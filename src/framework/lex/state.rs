//! Compiled lexer grammar and persistent per-document lexical roots.

use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
};

use fluent_uri::Uri;

use crate::{
    framework::{
        lex::{
            IncrementalLexStats, LexInterrupt, LexToken, LexerCreationError, LexerRoot,
            LexerState, LexicalDocument, TokenOccurrenceId, TokenPatch,
            __macro_private::{BuildErrorToken, TokenMatcher},
            build,
            lexed::document_id,
            mode::{State, StateInfo},
            token::{CompiledState, StateMatcher},
        },
        parse::{
            TokenData,
            grammar::TerminalId,
        },
        source::{SourceDelta, SourceSplice},
    },
    utils::{PrettyDisplay, Span},
};

/// Result of one committed lexer derivation.  The component publishes its root
/// handles directly; this wrapper keeps the mutation boundary explicit.
pub(crate) struct LexDocument<R: LexerRoot> {
    pub(crate) document: Arc<LexicalDocument<R>>,
    pub(crate) patch: TokenPatch,
}

fn is_rope_char_boundary(source: &ropey::Rope, offset: usize) -> bool {
    offset <= source.len_bytes()
        && source
            .try_byte_to_char(offset)
            .map(|character| source.char_to_byte(character) == offset)
            .unwrap_or(false)
}

fn validate_source_delta(
    previous: &ropey::Rope,
    next: &ropey::Rope,
    splices: &[SourceSplice],
) -> Result<(), LexInterrupt> {
    let mut old_cursor = 0;
    let mut new_cursor = 0;
    for splice in splices {
        if splice.old_range.start < old_cursor
            || splice.new_range.start < new_cursor
            || splice.old_range.start > splice.old_range.end
            || splice.new_range.start > splice.new_range.end
            || splice.old_range.end > previous.len_bytes()
            || splice.new_range.end > next.len_bytes()
            || !is_rope_char_boundary(previous, splice.old_range.start)
            || !is_rope_char_boundary(previous, splice.old_range.end)
            || !is_rope_char_boundary(next, splice.new_range.start)
            || !is_rope_char_boundary(next, splice.new_range.end)
        {
            return Err(LexInterrupt::InternalError(
                "source delta does not match adjacent source revisions".to_string(),
            ));
        }
        old_cursor = splice.old_range.end;
        new_cursor = splice.new_range.end;
    }
    Ok(())
}

fn apply_evolving_splice(
    snapshot: &mut ropey::Rope,
    old_range: std::ops::Range<usize>,
    inserted: &str,
) -> Result<(), LexInterrupt> {
    if old_range.end > snapshot.len_bytes()
        || !is_rope_char_boundary(snapshot, old_range.start)
        || !is_rope_char_boundary(snapshot, old_range.end)
    {
        return Err(LexInterrupt::InternalError(
            "source delta no longer matches the lexer snapshot".to_string(),
        ));
    }
    let start = snapshot.byte_to_char(old_range.start);
    let end = snapshot.byte_to_char(old_range.end);
    snapshot.remove(start..end);
    snapshot.insert(start, inserted);
    Ok(())
}

fn translate_offset(offset: usize, shift: isize) -> Result<usize, LexInterrupt> {
    offset.checked_add_signed(shift).ok_or_else(|| {
        LexInterrupt::InternalError("source delta coordinate translation overflowed".to_string())
    })
}

impl<Root: LexerRoot + fmt::Display + Clone> PrettyDisplay<Lexer<Root>> for LexToken<Root> {
    fn pretty_fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
        _context: &Lexer<Root>,
    ) -> core::fmt::Result {
        use color_print::cwrite;
        if self.error.is_some() {
            cwrite!(
                formatter,
                "<dim>[{}]\t</dim><red>Error</red>: {}",
                self.length,
                self.value
            )
        } else {
            cwrite!(
                formatter,
                "<dim>[{}]\t</dim><green>Token</green>: {}",
                self.length,
                self.value
            )
        }
    }
}

impl<Root: LexerRoot + fmt::Display + Clone> PrettyDisplay<Lexer<Root>> for usize {
    fn pretty_fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
        context: &Lexer<Root>,
    ) -> core::fmt::Result {
        use color_print::cwrite;
        match context.token(*self) {
            Some(token) if token.error.is_some() => cwrite!(
                formatter,
                "<dim>[{}]\t</dim><red>Error</red>: {}",
                token.length,
                token.value
            ),
            Some(token) => cwrite!(
                formatter,
                "<dim>[{}]\t</dim><green>Token</green>: {}",
                token.length,
                token.value
            ),
            None => cwrite!(formatter, "<red>Missing token {}</red>", self),
        }
    }
}

#[derive(Debug)]
pub struct LexerSnapshotState<Root: LexerRoot> {
    pub(super) documents: HashMap<Uri<String>, Arc<LexicalDocument<Root>>>,
    pub(super) incremental_stats: HashMap<Uri<String>, IncrementalLexStats>,
}

impl<Root: LexerRoot> Clone for LexerSnapshotState<Root> {
    fn clone(&self) -> Self {
        Self {
            documents: self.documents.clone(),
            incremental_stats: self.incremental_stats.clone(),
        }
    }
}

impl<Root: LexerRoot> Default for LexerSnapshotState<Root> {
    fn default() -> Self {
        Self {
            documents: HashMap::new(),
            incremental_stats: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct Lexer<Root: LexerRoot> {
    compiled_states: Vec<CompiledState<Root>>,
    state_ids: HashMap<String, State<Root>>,
    pub(crate) latest: Arc<LexerSnapshotState<Root>>,
    /// Command-local DFA counters drained by persistent replay.
    pub(crate) dfa_scratch: std::cell::Cell<(u64, u64)>,
}

impl<Root: LexerRoot> Lexer<Root> {
    pub fn new() -> Result<Self, LexerCreationError> {
        let registrations = Root::state_registrations();
        let state_ids = registrations
            .iter()
            .enumerate()
            .map(|(index, registration)| (registration.type_name.clone(), State::new(index)))
            .collect::<HashMap<_, _>>();
        let mut compiled_states = Vec::with_capacity(registrations.len());
        for registration in &registrations {
            let mut state_tokens = Vec::new();
            let mut patterns = Vec::new();
            let mut token_index_by_pattern = Vec::new();
            for spec in (registration.rules)() {
                let regex = match spec.matcher {
                    TokenMatcher::Regex(regex) => Some(regex),
                    TokenMatcher::Empty => None,
                };
                let resolved = build::resolve_token(spec, &state_ids)?;
                if let Some(regex) = regex {
                    patterns.push(regex);
                    token_index_by_pattern.push(state_tokens.len());
                }
                state_tokens.push(resolved);
            }
            let matcher = build::build_state_matcher(
                registration.display_name,
                &patterns,
                token_index_by_pattern,
            )?;
            compiled_states.push(CompiledState {
                info: StateInfo {
                    name: registration.display_name,
                    type_name: registration.type_name.clone(),
                },
                matcher,
                tokens: state_tokens,
                recovery_error: registration.recovery_error_builder.clone(),
                boundary_error: registration.boundary_error_builder.clone(),
            });
        }
        Ok(Self {
            compiled_states,
            state_ids,
            latest: Arc::new(LexerSnapshotState::default()),
            dfa_scratch: std::cell::Cell::new((0, 0)),
        })
    }

    /// Processes normalized source splices left-to-right.  Each splice sees the
    /// preceding persistent root, so distant edits retain their intervening
    /// unchanged islands by pointer.
    pub(crate) fn derive_document(
        &mut self,
        uri: Uri<String>,
        source: Arc<ropey::Rope>,
        delta: &SourceDelta,
    ) -> Result<LexDocument<Root>, LexInterrupt>
    where
        Root: Clone,
    {
        let mut next = (*self.latest).clone();
        let root_state = self.state_id_of::<Root>().ok_or(LexInterrupt::MissingState)?;
        let previous = next.documents.get(&uri).cloned().unwrap_or_else(|| {
            Arc::new(LexicalDocument::empty(document_id(&uri.to_string()), root_state.clone()))
        });
        if previous.source.as_ref() == source.as_ref() {
            return Ok(LexDocument {
                document: previous.clone(),
                patch: TokenPatch::unchanged(previous.structure_revision),
            });
        }

        let initializing = !next.documents.contains_key(&uri);
        let replacement = SourceSplice {
            old_range: 0..previous.source.len_bytes(),
            new_range: 0..source.len_bytes(),
        };
        let splices: Vec<SourceSplice> = match delta {
            SourceDelta::Load { .. } => vec![replacement],
            SourceDelta::Edit { .. } if initializing => vec![replacement],
            SourceDelta::Edit { splices } => splices.to_vec(),
        };
        validate_source_delta(previous.source.as_ref(), source.as_ref(), &splices)?;

        let mut document = (*previous).clone();
        let mut snapshot = previous.source.as_ref().clone();
        let mut cumulative_shift = 0isize;
        let mut patches = crate::framework::lex::incremental::PatchBuilder::new(
            document.structure_revision,
        );
        let mut total_relexed = 0usize;
        let mut total_reused = 0usize;
        let mut first_restart = None;
        let mut first_rank = None;
        let old_semantic_len = document.semantic.len();

        for splice in &splices {
            let start = translate_offset(splice.old_range.start, cumulative_shift)?;
            let end = translate_offset(splice.old_range.end, cumulative_shift)?;
            let inserted = source.byte_slice(splice.new_range.clone()).to_string();
            let evolving = SourceSplice {
                old_range: start..end,
                new_range: start..start.checked_add(inserted.len()).ok_or_else(|| {
                    LexInterrupt::InternalError("source insertion range overflowed".to_string())
                })?,
            };
            let restart_rank = document
                .lexical
                .lexical_rank_at_byte(evolving.old_range.start as u64);
            let restart = document.lexical_start(restart_rank);
            apply_evolving_splice(&mut snapshot, evolving.old_range.clone(), &inserted)?;
            let local = self.relex_splice(
                &uri,
                &mut document,
                Arc::new(snapshot.clone()),
                &evolving,
            )?;
            total_relexed = total_relexed.saturating_add(local.replayed);
            total_reused = total_reused.saturating_add(local.reused);
            first_restart.get_or_insert(restart);
            first_rank.get_or_insert(restart_rank);
            patches.absorb(local);
            let inserted_len = isize::try_from(inserted.len()).map_err(|_| {
                LexInterrupt::InternalError("source insertion length overflows isize".to_string())
            })?;
            let removed_len = isize::try_from(splice.old_range.len()).map_err(|_| {
                LexInterrupt::InternalError("source removal length overflows isize".to_string())
            })?;
            cumulative_shift = cumulative_shift
                .checked_add(inserted_len - removed_len)
                .ok_or_else(|| {
                    LexInterrupt::InternalError(
                        "source delta cumulative shift overflows".to_string(),
                    )
                })?;
        }
        if snapshot != *source {
            return Err(LexInterrupt::InternalError(
                "source delta did not produce the observed document text".to_string(),
            ));
        }
        let patch = patches.freeze(document.structure_revision);
        if !patch.structure_unchanged() {
            // Same-terminal value and layout-only edits preserve parser
            // structure; only membership/order/terminal changes wake the
            // parser (plan §3 revision-domain contract).
            document.semantic_revision = crate::framework::lex::SemanticRevisionId(
                document
                    .semantic_revision
                    .0
                    .checked_add(1)
                    .expect("semantic revision overflow"),
            );
        }
        let document = Arc::new(document);
        next.incremental_stats.insert(
            uri.clone(),
            IncrementalLexStats {
                restart_byte: first_restart.unwrap_or(0),
                restart_occurrence: first_rank.unwrap_or(0),
                relexed: total_relexed,
                reused: total_reused,
                old_tokens: old_semantic_len,
                new_tokens: document.semantic.len(),
            },
        );
        next.documents.insert(uri, Arc::clone(&document));
        self.latest = Arc::new(next);
        Ok(LexDocument { document, patch })
    }

    pub fn incremental_stats(&self, uri: Uri<String>) -> Option<IncrementalLexStats> {
        self.latest.incremental_stats.get(&uri).copied()
    }

    pub(crate) fn forget_document(&mut self, uri: Uri<String>) {
        let latest = Arc::make_mut(&mut self.latest);
        latest.documents.remove(&uri);
        latest.incremental_stats.remove(&uri);
    }

    pub fn state_info(&self) -> impl ExactSizeIterator<Item = &StateInfo> {
        self.compiled_states.iter().map(|state| &state.info)
    }

    pub fn resolved_tokens(&self) -> impl ExactSizeIterator<Item = &[crate::framework::lex::ResolvedToken<Root>]> {
        self.compiled_states.iter().map(|state| state.tokens.as_slice())
    }

    pub fn tokens_in_state(&self, state: State<Root>) -> Option<&[crate::framework::lex::ResolvedToken<Root>]> {
        self.compiled_states.get(state.id).map(|state| state.tokens.as_slice())
    }

    pub(crate) fn state_matcher(&self, state: State<Root>) -> Option<&StateMatcher> {
        self.compiled_states.get(state.id).map(|state| &state.matcher)
    }

    /// Explicit lookup façade. IDs are document-local, so the first matching
    /// current document is returned only for legacy single-document callers.
    pub fn token(&self, index: usize) -> Option<LexToken<Root>>
    where
        Root: Clone,
    {
        self.latest
            .documents
            .values()
            .find_map(|document| document.lexical_token(TokenOccurrenceId(index as u64)))
    }

    pub fn terminal_of(&self, index: usize) -> Option<TerminalId>
    where
        Root: Clone,
    {
        self.token(index)
            .and_then(|token| token.error.is_none().then_some(token.terminal).flatten())
    }

    pub fn token_span(&self, id: usize) -> Option<Span> {
        let occurrence = TokenOccurrenceId(id as u64);
        self.latest.documents.iter().find_map(|(uri, document)| {
            let rank = document.lexical_rank_of(occurrence)?;
            let start = document.lexical_start(rank);
            let token = document.lexical_at(rank)?;
            Span::new_uri(uri.clone(), start, start.saturating_add(token.byte_len as usize)).ok()
        })
    }

    pub fn tokens_in_span(&self, span: Span) -> Vec<LexToken<Root>>
    where
        Root: Clone,
    {
        self.latest
            .documents
            .get(&span.uri)
            .map(|document| document.tokens_in_span(&span))
            .unwrap_or_default()
    }

    pub fn token_data_in_span(&self, span: Span) -> Vec<TokenData> {
        let Some(document) = self.latest.documents.get(&span.uri) else {
            return Vec::new();
        };
        let start_rank = document.lexical.lexical_rank_at_byte(span.range.start() as u64);
        let mut data = Vec::new();
        for rank in start_rank..document.lexical.len() {
            let start = document.lexical_start(rank);
            let Some(token) = document.lexical_at(rank) else {
                break;
            };
            if start >= span.range.end() {
                break;
            }
            if token.is_semantic() && start.saturating_add(token.byte_len as usize) > span.range.start() {
                if let Some(semantic_rank) = document.semantic_rank_of(token.id)
                    && let Some(token_data) = document.token_data_at_semantic_rank(semantic_rank)
                {
                    data.push(token_data);
                }
            }
        }
        data
    }

    pub(crate) fn state_id_of<S: crate::framework::lex::TokenState>(&self) -> Option<State<Root>> {
        self.state_ids.get(S::state_key()).cloned()
    }

    pub(crate) fn recovery_error_builder(
        &self,
        state: State<Root>,
    ) -> Option<&BuildErrorToken<Root>> {
        self.compiled_states
            .get(state.id)
            .map(|state| &state.recovery_error)
    }

    pub(crate) fn boundary_error_builder(
        &self,
        state: State<Root>,
    ) -> Option<&BuildErrorToken<Root>> {
        self.compiled_states
            .get(state.id)
            .map(|state| &state.boundary_error)
    }
}
