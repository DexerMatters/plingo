//! Sparse source transactions are relexed from a stable checkpoint and emit only
//! changed token segments; occurrence identities carry the unchanged suffix.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    component::{
        lex::{
            IncrementalLexStats, LexInterrupt, Lexer, LexerRoot, LexerSnapshotState, LexerState,
        },
        source::SourceSplice,
    },
    scheme::change::{AddressChange, Splice},
};

use super::token::TokenOccurrence;

fn shift_occurrence(occurrence: TokenOccurrence, shift: isize) -> TokenOccurrence {
    TokenOccurrence {
        id: occurrence.id,
        column: occurrence.column,
        start: occurrence.start.saturating_add_signed(shift),
        end: occurrence.end.saturating_add_signed(shift),
    }
}

fn shift_state<Root: LexerRoot>(state: &LexerState<Root>, shift: isize) -> LexerState<Root> {
    let mut shifted = state.clone();
    shifted.offset = shifted.offset.saturating_add_signed(shift);
    shifted
}

impl<Root> Lexer<Root>
where
    Root: LexerRoot,
{
    pub(crate) fn lex_uri(
        &mut self,
        state: &mut LexerSnapshotState<Root>,
        uri: fluent_uri::Uri<&'static str>,
        snapshot: String,
        source_splices: &[SourceSplice],
    ) -> Result<
        Option<AddressChange<fluent_uri::Uri<&'static str>, crate::component::parse::TokenData>>,
        LexInterrupt,
    > {
        let total_start = Instant::now();
        self.relex(
            state,
            uri,
            snapshot,
            source_splices,
            total_start,
            Duration::ZERO,
        )
    }

    fn relex(
        &mut self,
        snapshot_state: &mut LexerSnapshotState<Root>,
        uri: fluent_uri::Uri<&'static str>,
        snapshot: String,
        source_splices: &[SourceSplice],
        total_start: Instant,
        fetch_source_elapsed: Duration,
    ) -> Result<
        Option<AddressChange<fluent_uri::Uri<&'static str>, crate::component::parse::TokenData>>,
        LexInterrupt,
    > {
        let root_state = self
            .state_id_of::<Root>()
            .ok_or(LexInterrupt::MissingState)?;

        snapshot_state
            .state_instances
            .entry(uri)
            .or_insert_with(|| Arc::new(vec![LexerState::new(root_state)]));
        snapshot_state.occurrences.entry(uri).or_default();

        let old_source = snapshot_state
            .sources
            .get(&uri)
            .cloned()
            .unwrap_or_else(|| Arc::from(""));
        if old_source.as_ref() == snapshot {
            return Ok(None);
        }

        let old_visible_start = Instant::now();
        let old_visible_tokens = self.token_data_for_uri(snapshot_state, uri);
        let old_visible_elapsed = old_visible_start.elapsed();

        let delta_scan_start = Instant::now();
        let Some(first_splice) = source_splices.first() else {
            return Err(LexInterrupt::InternalError(
                "changed source revision has no source delta".to_string(),
            ));
        };
        let Some(last_splice) = source_splices.last() else {
            unreachable!("first source splice implies a last splice");
        };
        // Source owns the exact ordered old/new coordinate map. Replay starts
        // before the first changed byte and convergence is considered only
        // beyond the final changed byte, retaining untouched islands between
        // distant splices for token re-anchoring below.
        let restart_point = first_splice.old_range.start;
        let new_change_end = last_splice.new_range.end;
        let net_shift = snapshot.len() as isize - old_source.len() as isize;
        let delta_scan_elapsed = delta_scan_start.elapsed();

        let restart_lookup_start = Instant::now();
        let restart_token_pos = {
            let states = &snapshot_state.state_instances[&uri];
            let occurrences = &snapshot_state.occurrences[&uri];
            debug_assert_eq!(states.len(), occurrences.len() + 1);
            // Restart before the first affected token; this is the last state
            // whose input and scope stack are certainly unchanged.
            states[1..].partition_point(|state| state.offset < restart_point)
        };
        let restart_lookup_elapsed = restart_lookup_start.elapsed();

        let old_suffix_snapshot_start = Instant::now();
        let (start_state, old_states, old_occurrences) = {
            let states = snapshot_state
                .state_instances
                .get(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let occurrences = snapshot_state
                .occurrences
                .get(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            debug_assert_eq!(states.len(), occurrences.len() + 1);

            let start_state = states[restart_token_pos].clone();
            let old_states = states[restart_token_pos + 1..].to_vec();
            let old_occurrences = occurrences[restart_token_pos..].to_vec();
            (start_state, old_states, old_occurrences)
        };
        let restart_offset = start_state.offset;
        let old_suffix_snapshot_elapsed = old_suffix_snapshot_start.elapsed();

        let mut new_occurrences = Vec::new();
        let mut new_states = Vec::new();
        // The exact source suffix and complete lexer state stack are the
        // convergence proof. Error tokens are ordinary deterministic outputs
        // of that state/input pair and may therefore be reused as well.
        let old_suffix_is_clean = vec![true; old_occurrences.len() + 1];

        let mut convergence: Option<(usize, usize)> = None;
        let occurrence_start = snapshot_state
            .next_occurrence
            .get(&uri)
            .copied()
            .unwrap_or(0);
        let mut next_occurrence = occurrence_start;
        let mut occurrence_overflow = false;
        let replay_start = Instant::now();
        let final_state =
            self.lex_cont(start_state, &snapshot, |token_id, state, start, end| {
                if next_occurrence == usize::MAX {
                    occurrence_overflow = true;
                    return false;
                }
                new_occurrences.push(TokenOccurrence {
                    id: token_id,
                    column: next_occurrence,
                    start,
                    end,
                });
                next_occurrence += 1;
                new_states.push(state.clone());
                if state.offset >= new_change_end {
                    // Offset alignment plus the complete state stack is the exact
                    // lexical convergence proof; token text is not guessed here.
                    let old_offset = if net_shift >= 0 {
                        state.offset.checked_sub(net_shift as usize)
                    } else {
                        state.offset.checked_add((-net_shift) as usize)
                    };
                    if let Some(old_offset) = old_offset {
                        let first = old_states.partition_point(|old| old.offset < old_offset);
                        let end = old_states.partition_point(|old| old.offset <= old_offset);
                        if let Some(index) = (first..end).rev().find(|&index| {
                            old_states[index].state_stack == state.state_stack
                                && old_suffix_is_clean[index + 1]
                        }) {
                            convergence = Some((new_occurrences.len(), index + 1));
                            return false;
                        }
                    }
                }
                true
            })?;
        if occurrence_overflow {
            return Err(LexInterrupt::InternalError(
                "token occurrence identity space exhausted".to_string(),
            ));
        }
        if convergence.is_none() && final_state.offset >= new_change_end {
            let old_offset = if net_shift >= 0 {
                final_state.offset.checked_sub(net_shift as usize)
            } else {
                final_state.offset.checked_add((-net_shift) as usize)
            };
            if let Some(old_offset) = old_offset {
                let first = old_states.partition_point(|old| old.offset < old_offset);
                let end = old_states.partition_point(|old| old.offset <= old_offset);
                if let Some(index) = (first..end).rev().find(|&index| {
                    old_states[index].state_stack == final_state.state_stack
                        && old_suffix_is_clean[index + 1]
                }) {
                    convergence = Some((new_occurrences.len(), index + 1));
                }
            }
        }

        // No convergence means replay reached EOF. This remains an exact
        // incremental replay from the edit checkpoint.
        let replay_elapsed = replay_start.elapsed();

        let (new_prefix_len, old_suffix_start_index) =
            convergence.unwrap_or((new_occurrences.len(), old_occurrences.len()));

        let snapshot_len = snapshot.len();
        let new_visible_start = Instant::now();
        let mut old_window =
            self.token_data_from_occurrences(&old_occurrences[..old_suffix_start_index], None);
        let mut new_window =
            self.token_data_from_occurrences(&new_occurrences[..new_prefix_len], None);
        old_window.pop();
        new_window.pop();
        let new_visible_elapsed = new_visible_start.elapsed();
        let batch_diff_start = Instant::now();
        let mut anchors = Vec::new();
        let mut old_index = 0;
        let mut new_index = 0;
        let mut old_gap_start = 0;
        let mut new_gap_start = 0;
        for (gap, (old_gap_end, new_gap_end, final_gap)) in source_splices
            .iter()
            .map(|splice| (splice.old_range.start, splice.new_range.start, false))
            .chain(std::iter::once((old_source.len(), snapshot_len, true)))
            .enumerate()
        {
            while old_index < old_window.len() {
                let old = &old_window[old_index];
                let old_end = old.start + old.length;
                if old.start < old_gap_start {
                    old_index += 1;
                    continue;
                }
                if old.start > old_gap_end
                    || (old.start == old_gap_end && (!final_gap || old.length != 0))
                {
                    break;
                }
                if old_end > old_gap_end {
                    old_index += 1;
                    continue;
                }
                let expected_start = new_gap_start + old.start - old_gap_start;
                while new_index < new_window.len() && new_window[new_index].start < expected_start {
                    new_index += 1;
                }
                if let Some(found) = (new_index..new_window.len())
                    .take_while(|&index| new_window[index].start == expected_start)
                    .find(|&index| {
                        new_window[index].length == old.length
                            && new_window[index].start + new_window[index].length <= new_gap_end
                            && self.token_data_semantically_equal(old, &new_window[index])
                    })
                {
                    anchors.push((old_index, found));
                    new_index = found + 1;
                }
                old_index += 1;
            }
            if let Some(splice) = source_splices.get(gap) {
                old_gap_start = splice.old_range.end;
                new_gap_start = splice.new_range.end;
            }
        }

        let occurrence_by_id = new_occurrences[..new_prefix_len]
            .iter()
            .enumerate()
            .map(|(index, occurrence)| (occurrence.id, index))
            .collect::<HashMap<_, _>>();
        for &(old, new) in &anchors {
            if let Some(&occurrence) = occurrence_by_id.get(&new_window[new].id) {
                new_occurrences[occurrence].id = old_window[old].id;
                new_occurrences[occurrence].column = old_window[old].column;
                new_window[new].id = old_window[old].id;
                new_window[new].column = old_window[old].column;
            }
        }

        let old_visible_len = old_visible_tokens.len();
        let base = old_visible_tokens[..old_visible_len.saturating_sub(1)]
            .partition_point(|token| token.start < restart_offset);
        let new_visible_len = old_visible_len - old_window.len() + new_window.len();
        let mut splices = Vec::new();
        let mut old_cursor = 0;
        let mut new_cursor = 0;
        for (old, new) in anchors
            .iter()
            .copied()
            .chain(std::iter::once((old_window.len(), new_window.len())))
        {
            if old_cursor != old || new_cursor != new {
                splices.push(Splice {
                    old_range: base + old_cursor..base + old,
                    new_range: base + new_cursor..base + new,
                    removed: Arc::from(old_window[old_cursor..old].to_vec()),
                    inserted: Arc::from(new_window[new_cursor..new].to_vec()),
                });
            }
            old_cursor = old + usize::from(old < old_window.len());
            new_cursor = new + usize::from(new < new_window.len());
        }
        let prefix_len = splices
            .first()
            .map_or(old_visible_len, |splice| splice.old_range.start);
        let suffix_len = splices.last().map_or(old_visible_len, |splice| {
            old_visible_len - splice.old_range.end
        });
        let changed = !splices.is_empty();
        let batch_diff_elapsed = batch_diff_start.elapsed();

        let state_splice_start = Instant::now();
        {
            let states = snapshot_state
                .state_instances
                .get_mut(&uri)
                .map(Arc::make_mut)
                .ok_or(LexInterrupt::MissingState)?;
            let occurrences = snapshot_state
                .occurrences
                .get_mut(&uri)
                .map(Arc::make_mut)
                .ok_or(LexInterrupt::MissingState)?;
            states.truncate(restart_token_pos + 1);
            occurrences.truncate(restart_token_pos);
            states.extend(new_states.iter().take(new_prefix_len).cloned());
            states.extend(
                old_states
                    .iter()
                    .skip(old_suffix_start_index)
                    .map(|state| shift_state(state, net_shift)),
            );
            occurrences.extend(new_occurrences.iter().take(new_prefix_len).copied());
            occurrences.extend(
                old_occurrences
                    .iter()
                    .skip(old_suffix_start_index)
                    .copied()
                    .map(|occurrence| shift_occurrence(occurrence, net_shift)),
            );
            debug_assert_eq!(states.len(), occurrences.len() + 1);
        }
        let state_splice_elapsed = state_splice_start.elapsed();
        snapshot_state.sources.insert(uri, Arc::from(snapshot));
        snapshot_state.next_occurrence.insert(uri, next_occurrence);
        snapshot_state.incremental_stats.insert(
            uri,
            IncrementalLexStats {
                restart_byte: restart_point,
                restart_occurrence: restart_token_pos,
                relexed: new_prefix_len,
                reused: old_occurrences.len().saturating_sub(old_suffix_start_index),
                old_tokens: old_visible_len,
                new_tokens: new_visible_len,
            },
        );
        let total_elapsed = total_start.elapsed();

        eprintln!(
            "[lex-replay] uri={} total={:?} fetch_source={:?} old_visible={:?} delta_scan={:?} restart_lookup={:?} old_suffix={:?} replay={:?} splice={:?} new_visible={:?} batch_diff={:?} status={} changed={} restart_byte={} restart_token={} change_end={} net_shift={} relexed={} reused={} old_tokens={} new_tokens={} prefix={} suffix={}",
            uri,
            total_elapsed,
            fetch_source_elapsed,
            old_visible_elapsed,
            delta_scan_elapsed,
            restart_lookup_elapsed,
            old_suffix_snapshot_elapsed,
            replay_elapsed,
            state_splice_elapsed,
            new_visible_elapsed,
            batch_diff_elapsed,
            if convergence.is_some() {
                "converged"
            } else {
                "eof"
            },
            changed,
            restart_point,
            restart_token_pos,
            new_change_end,
            net_shift,
            new_prefix_len,
            old_occurrences.len().saturating_sub(old_suffix_start_index),
            old_visible_len,
            new_visible_len,
            prefix_len,
            suffix_len,
        );

        if changed {
            Ok(Some(AddressChange {
                address: uri,
                old_extent: old_visible_len,
                new_extent: new_visible_len,
                splices,
            }))
        } else {
            Ok(None)
        }
    }
}
