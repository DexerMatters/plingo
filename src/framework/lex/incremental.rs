//! Bounded lexer replay over persistent lexical and semantic tapes.
//!
//! Every source splice preserves the old prefix/suffix root by pointer and
//! scans only until an exact canonical-state boundary converges.  The replay
//! window itself constructs the token patch directly; there is no post-hoc
//! whole-vector projection or diff.

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    sync::Arc,
};

use fluent_uri::Uri;

use crate::framework::{
    lex::{
        CanonicalLexerState, IncrementalLexStats, LexInterrupt, Lexer, LexerRoot,
        LexicalDocument, LexicalOccurrence, ParseTokenRef, ScannedToken, TapeSplice,
        TokenLayoutEntry, TokenOccurrenceId, TokenPatch,
        cursor::RopeCursor,
    },
    source::SourceSplice,
    tape::StableTape,
};

#[derive(Default)]
pub(crate) struct LocalPatch {
    pub(crate) splices: Vec<TapeSplice>,
    pub(crate) inserted: BTreeSet<TokenOccurrenceId>,
    pub(crate) updated: BTreeSet<TokenOccurrenceId>,
    pub(crate) removed: BTreeSet<TokenOccurrenceId>,
    pub(crate) replayed: usize,
    pub(crate) reused: usize,
}

impl LocalPatch {
    fn structure_changed(&self) -> bool {
        !self.splices.is_empty()
    }
}

/// Command-local coalescer.  Source normalization keeps unrelated splice
/// windows disjoint.  Adjacent windows are joined only when their retained
/// anchors prove adjacency; all identifier sets are canonicalized on freeze.
pub(crate) struct PatchBuilder {
    old_structure: crate::framework::lex::TokenStructureRevisionId,
    splices: Vec<TapeSplice>,
    inserted: BTreeSet<TokenOccurrenceId>,
    updated: BTreeSet<TokenOccurrenceId>,
    removed: BTreeSet<TokenOccurrenceId>,
}

impl PatchBuilder {
    pub(crate) fn new(structure: crate::framework::lex::TokenStructureRevisionId) -> Self {
        Self {
            old_structure: structure,
            splices: Vec::new(),
            inserted: BTreeSet::new(),
            updated: BTreeSet::new(),
            removed: BTreeSet::new(),
        }
    }

    pub(crate) fn absorb(&mut self, local: LocalPatch) {
        for splice in local.splices {
            if let Some(previous) = self.splices.last_mut()
                && previous.after == splice.before
            {
                let mut removed = Vec::with_capacity(previous.removed.len() + splice.removed.len());
                removed.extend(previous.removed.iter().copied());
                removed.extend(splice.removed.iter().copied());
                let mut inserted = Vec::with_capacity(previous.inserted.len() + splice.inserted.len());
                inserted.extend(previous.inserted.iter().copied());
                inserted.extend(splice.inserted.iter().copied());
                previous.removed = removed.into();
                previous.inserted = inserted.into();
                previous.after = splice.after;
                continue;
            }
            self.splices.push(splice);
        }
        self.inserted.extend(local.inserted);
        self.updated.extend(local.updated);
        self.removed.extend(local.removed);
    }

    pub(crate) fn freeze(
        self,
        structure: crate::framework::lex::TokenStructureRevisionId,
    ) -> TokenPatch {
        let mut inserted = self.inserted;
        let mut updated = self.updated;
        let mut removed = self.removed;
        // A retained occurrence can be structurally replaced in-place (same
        // ID, new terminal).  That is a fact update plus an order splice, not
        // both insertion and removal.  Newly inserted then removed IDs cancel.
        let cancelled: Vec<_> = inserted.intersection(&removed).copied().collect();
        for occurrence in cancelled {
            inserted.remove(&occurrence);
            removed.remove(&occurrence);
        }
        for occurrence in inserted.iter().chain(removed.iter()) {
            updated.remove(occurrence);
        }
        TokenPatch {
            old_structure: self.old_structure,
            new_structure: structure,
            order_splices: self.splices.into(),
            inserted: inserted.into_iter().collect::<Vec<_>>().into(),
            updated: updated.into_iter().collect::<Vec<_>>().into(),
            removed: removed.into_iter().collect::<Vec<_>>().into(),
        }
    }
}

impl<Root> Lexer<Root>
where
    Root: LexerRoot + Clone,
{
    pub(crate) fn relex_splice(
        &mut self,
        uri: &Uri<String>,
        document: &mut LexicalDocument<Root>,
        next_source: Arc<ropey::Rope>,
        splice: &SourceSplice,
    ) -> Result<LocalPatch, LexInterrupt> {
        let old_source = Arc::clone(&document.source);
        let old_lexical_len = document.lexical.len();
        let old_semantic_len = document.semantic.len();
        let (restart_rank, restart_lookup_depth) = document
            .lexical
            .lexical_rank_at_byte_detailed(splice.old_range.start as u64);
        let restart_offset = document.lexical_start(restart_rank);
        let old_semantic_start = usize::try_from(
            document.lexical.metric_before(restart_rank).semantic_count,
        )
        .expect("semantic rank exceeds usize");
        let start_state = document.state_before_rank(restart_rank);
        let old_suffix_source_start = splice.old_range.end;
        let new_change_end = splice.new_range.end;
        let net_shift = isize::try_from(next_source.len_bytes())
            .ok()
            .and_then(|next| isize::try_from(old_source.len_bytes()).ok().and_then(|old| next.checked_sub(old)))
            .ok_or_else(|| LexInterrupt::InternalError("source length delta overflows isize".to_string()))?;

        // Take the interner without cloning its buckets: replay interns only
        // the states it actually visits, and the original entries remain
        // present because `take` moves the whole cache out and back.
        let mut interner = std::mem::take(&mut document.state_interner);
        let mut provisional = Vec::new();
        let mut convergence: Option<usize> = None;
        let mut convergence_checks = 0u64;
        let input = RopeCursor::new(Arc::clone(&next_source));
        let final_state = match self.lex_cont(start_state, &input, |scanned, state| {
            let canonical = interner.intern(state);
            let width = match u32::try_from(scanned.end.saturating_sub(scanned.start)) {
                Ok(width) => width,
                Err(_) => return false,
            };
            provisional.push(LexicalOccurrence {
                id: TokenOccurrenceId(0),
                byte_len: width,
                terminal: scanned.terminal,
                skip: scanned.skip,
                value: Arc::new(scanned.value),
                error: scanned.error,
                state_after: Arc::clone(&canonical),
            });
            if state.offset < new_change_end {
                return true;
            }
            convergence_checks = convergence_checks.saturating_add(1);
            let Some(old_offset) = state.offset.checked_add_signed(-net_shift) else {
                return true;
            };
            if old_offset < old_suffix_source_start {
                return true;
            }
            let candidate_rank = document.lexical.lexical_rank_at_byte(old_offset as u64);
            if document.lexical_start(candidate_rank) != old_offset {
                return true;
            }
            let old_state = document.state_before_rank(candidate_rank);
            let old_canonical = CanonicalLexerState::from_state(&old_state);
            if canonical.as_ref() == &old_canonical {
                convergence = Some(candidate_rank);
                return false;
            }
            true
        }) {
            Ok(final_state) => final_state,
            Err(error) => {
                // Restore the (growth-only) cache before propagating.
                document.state_interner = interner;
                return Err(error);
            }
        };

        // A zero-token suffix can converge at EOF or immediately after an
        // empty source replacement.  The exact boundary/state proof is the
        // same as the callback path.
        if convergence.is_none() && final_state.offset >= new_change_end {
            convergence_checks = convergence_checks.saturating_add(1);
            if let Some(old_offset) = final_state.offset.checked_add_signed(-net_shift)
                && old_offset >= old_suffix_source_start
            {
                let candidate_rank = document.lexical.lexical_rank_at_byte(old_offset as u64);
                if document.lexical_start(candidate_rank) == old_offset {
                    let old_state = document.state_before_rank(candidate_rank);
                    let current = CanonicalLexerState::from_state(&final_state);
                    if current == CanonicalLexerState::from_state(&old_state) {
                        convergence = Some(candidate_rank);
                    }
                }
            }
        }

        let old_suffix_rank = convergence.unwrap_or(old_lexical_len);
        let old_semantic_end = usize::try_from(
            document.lexical.metric_before(old_suffix_rank).semantic_count,
        )
        .expect("semantic rank exceeds usize");
        let old_range = restart_rank..old_suffix_rank;
        let old_window: Vec<_> = document.lexical_range(old_range.clone()).cloned().collect();
        let old_semantic_window: Vec<_> = document
            .semantic
            .iter_range(old_semantic_start..old_semantic_end)
            .cloned()
            .collect();

        if let Err(error) =
            Self::reconcile_occurrence_ids(document, &old_window, &mut provisional)
        {
            document.state_interner = interner;
            return Err(error);
        }
        let new_semantic_window: Vec<_> = provisional
            .iter()
            .filter(|token| token.is_semantic())
            .map(ParseTokenRef::from)
            .collect();
        let new_semantic_len = new_semantic_window.len();
        let mut local = Self::direct_patch(
            document,
            &old_window,
            &provisional,
            &old_semantic_window,
            &new_semantic_window,
            old_semantic_start,
            old_semantic_end,
        );

        let tape_checkpoint = document.tape_ids.checkpoint();
        let lexical_replacement = StableTape::from_entries(provisional.clone(), &mut document.tape_ids);
        let (lexical, lexical_index) = document.lexical.splice_with_index(
            &document.lexical_index,
            old_range.clone(),
            &lexical_replacement,
            &mut document.tape_ids,
        );
        let layout_replacement = StableTape::from_entries(
            provisional.iter().map(TokenLayoutEntry::from).collect::<Vec<_>>(),
            &mut document.tape_ids,
        );
        let (layout, layout_index) = document.layout.splice_with_index(
            &document.layout_index,
            old_range.clone(),
            &layout_replacement,
            &mut document.tape_ids,
        );
        let semantic_replacement =
            StableTape::from_entries(new_semantic_window.clone(), &mut document.tape_ids);
        let (semantic, semantic_index) = document.semantic.splice_with_index(
            &document.semantic_index,
            old_semantic_start..old_semantic_end,
            &semantic_replacement,
            &mut document.tape_ids,
        );
        document.lexical = lexical;
        document.lexical_index = lexical_index;
        document.semantic = semantic;
        document.layout = layout;
        document.layout_index = layout_index;
        document.semantic_index = semantic_index;
        document.source = next_source;
        document.state_interner = interner;
        document.layout_revision = crate::framework::lex::LayoutRevisionId(
            document
                .layout_revision
                .0
                .checked_add(1)
                .expect("layout revision overflow"),
        );
        if !local.updated.is_empty() {
            document.value_revision = crate::framework::lex::TokenValueRevisionId(
                document
                    .value_revision
                    .0
                    .checked_add(1)
                    .expect("token value revision overflow"),
            );
        }
        // Structure advances only when the ordered semantic sequence
        // (terminal / membership / error kind) actually differs — a
        // same-shape value replacement bumps value+layout revisions but
        // keeps the parser cold (plan §7 revision domains, §18 row
        // `Number 1 -> 7`).
        let structure_unchanged = old_semantic_window.len() == new_semantic_window.len()
            && old_semantic_window
                .iter()
                .zip(new_semantic_window.iter())
                .all(
                    |(old_entry, new_entry)| {
                        old_entry.terminal == new_entry.terminal
                            && old_entry.error == new_entry.error
                    },
                );
        if !structure_unchanged {
            document.structure_revision = crate::framework::lex::TokenStructureRevisionId(
                document
                    .structure_revision
                    .0
                    .checked_add(1)
                    .expect("structure revision overflow"),
            );
        }

        let (dfa_transitions, source_bytes_examined) = self.dfa_scratch.replace((0, 0));
        let replayed = provisional.len();
        let reused = old_lexical_len.saturating_sub(old_suffix_rank);
        let created = document.tape_ids.created_since(tape_checkpoint);
        crate::framework::workspace::record_lexer_work(&uri.to_string(), |work| {
            work.checkpoint_lookups += 2;
            // One B-tree descent for byte->rank plus one cached prefix-metric
            // read for the byte offset (plan §19: measured, not fabricated).
            work.checkpoint_lookup_depth += (restart_lookup_depth + 1) as u64;
            work.restart_bytes += restart_offset as u64;
            work.restart_occurrences += restart_rank as u64;
            work.dfa_transitions += dfa_transitions;
            work.source_bytes_examined += source_bytes_examined;
            work.lexical_entries_visited += (old_window.len() + replayed) as u64;
            work.semantic_entries_visited +=
                (old_semantic_window.len() + new_semantic_len) as u64;
            work.tokens_replayed += replayed as u64;
            work.tokens_reused += reused as u64;
            work.tokens_inserted += local.inserted.len() as u64;
            work.tokens_removed += local.removed.len() as u64;
            work.token_fact_writes += (local.inserted.len() + local.updated.len()) as u64;
            work.convergence_checks += convergence_checks;
            work.convergence_candidates += convergence_checks;
            work.eof_replays += u64::from(convergence.is_none());
            work.transferred_tape_intervals += u64::from(old_suffix_rank < old_lexical_len);
            work.tape_nodes_created += created;
            work.tape_nodes_reused += u64::from(old_suffix_rank < old_lexical_len);
        });
        local.replayed = replayed;
        local.reused = reused;
        Ok(local)
    }

    fn reconcile_occurrence_ids(
        document: &mut LexicalDocument<Root>,
        old: &[LexicalOccurrence<Root>],
        new: &mut [LexicalOccurrence<Root>],
    ) -> Result<(), LexInterrupt> {
        let mut pairs = vec![None; new.len()];
        let mut prefix = 0usize;
        while prefix < old.len()
            && prefix < new.len()
            && Self::exact_segment_eq(&old[prefix], &new[prefix])
        {
            pairs[prefix] = Some(prefix);
            prefix += 1;
        }
        let mut old_end = old.len();
        let mut new_end = new.len();
        while old_end > prefix
            && new_end > prefix
            && Self::exact_segment_eq(&old[old_end - 1], &new[new_end - 1])
        {
            old_end -= 1;
            new_end -= 1;
            pairs[new_end] = Some(old_end);
        }
        if old_end - prefix == new_end - prefix
            && old[prefix..old_end]
                .iter()
                .zip(&new[prefix..new_end])
                .all(|(old, new)| old.structural_eq(new))
        {
            for (old_index, new_index) in (prefix..old_end).zip(prefix..new_end) {
                pairs[new_index] = Some(old_index);
            }
        }

        for (new_index, token) in new.iter_mut().enumerate() {
            if let Some(old_index) = pairs[new_index] {
                token.id = old[old_index].id;
                continue;
            }
            let id = document.next_occurrence;
            document.next_occurrence = document.next_occurrence.checked_add(1).ok_or_else(|| {
                LexInterrupt::InternalError("token occurrence identity space exhausted".to_string())
            })?;
            token.id = TokenOccurrenceId(id);
        }
        Ok(())
    }

    fn exact_segment_eq(left: &LexicalOccurrence<Root>, right: &LexicalOccurrence<Root>) -> bool {
        left.exact_payload_eq(right)
            && CanonicalLexerState::ptr_or_exact_eq(&left.state_after, &right.state_after)
    }

    fn direct_patch(
        document: &LexicalDocument<Root>,
        old_lexical: &[LexicalOccurrence<Root>],
        new_lexical: &[LexicalOccurrence<Root>],
        old_semantic: &[ParseTokenRef],
        new_semantic: &[ParseTokenRef],
        old_semantic_start: usize,
        old_semantic_end: usize,
    ) -> LocalPatch {
        let mut patch = LocalPatch::default();
        let mut prefix = 0usize;
        while prefix < old_semantic.len()
            && prefix < new_semantic.len()
            && old_semantic[prefix] == new_semantic[prefix]
        {
            prefix += 1;
        }
        let mut old_end = old_semantic.len();
        let mut new_end = new_semantic.len();
        while old_end > prefix
            && new_end > prefix
            && old_semantic[old_end - 1] == new_semantic[new_end - 1]
        {
            old_end -= 1;
            new_end -= 1;
        }
        if prefix != old_end || prefix != new_end {
            let before = if prefix > 0 {
                Some(old_semantic[prefix - 1].occurrence)
            } else {
                old_semantic_start
                    .checked_sub(1)
                    .and_then(|rank| document.semantic.get(rank))
                    .map(|token| token.occurrence)
            };
            let after = if old_end < old_semantic.len() {
                Some(old_semantic[old_end].occurrence)
            } else {
                document
                    .semantic
                    .get(old_semantic_end)
                    .map(|token| token.occurrence)
            };
            patch.splices.push(TapeSplice {
                before,
                removed: old_semantic[prefix..old_end]
                    .iter()
                    .map(|token| token.occurrence)
                    .collect::<Vec<_>>()
                    .into(),
                inserted: new_semantic[prefix..new_end]
                    .iter()
                    .map(|token| token.occurrence)
                    .collect::<Vec<_>>()
                    .into(),
                after,
            });
        }

        let old_semantic_ids: BTreeSet<_> = old_semantic.iter().map(|token| token.occurrence).collect();
        let new_semantic_ids: BTreeSet<_> = new_semantic.iter().map(|token| token.occurrence).collect();
        patch.inserted.extend(new_semantic_ids.difference(&old_semantic_ids).copied());
        patch.removed.extend(old_semantic_ids.difference(&new_semantic_ids).copied());

        let old_by_id: HashMap<_, _> = old_lexical.iter().map(|token| (token.id, token)).collect();
        for token in new_lexical {
            if !token.is_semantic() || !old_semantic_ids.contains(&token.id) {
                continue;
            }
            let Some(old) = old_by_id.get(&token.id) else {
                continue;
            };
            if !old.exact_payload_eq(token) {
                patch.updated.insert(token.id);
            }
        }
        patch
    }
}


