use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use regex_automata::{Anchored, Input, dfa::Automaton};

use crate::{
    component::{
        lex::{
            BestMatch, Entry, ErrorToken, LexInterrupt, LexedToken, Lexer, LexerRoot,
            LexerSnapshotState, LexerState, MatchReport, State, StateAction, TokenChange,
        },
        source::{Source, TextChange},
    },
    scheme::{
        change::{LayerChange, LayerChanges, ReplacementChange},
        layer::{MiddleLayer, NonTopLayer},
    },
    utils::{RangeOrPoint, Span},
};

fn shift_offset(offset: usize, shift: isize) -> usize {
    if shift >= 0 {
        offset.saturating_add(shift as usize)
    } else {
        offset.saturating_sub((-shift) as usize)
    }
}

fn shift_range((start, end): (usize, usize), shift: isize) -> (usize, usize) {
    (shift_offset(start, shift), shift_offset(end, shift))
}

fn shift_state(state: &LexerState, shift: isize) -> LexerState {
    let mut shifted = state.clone();
    shifted.offset = shift_offset(shifted.offset, shift);
    shifted
}

impl<Root, Lower> Lexer<Root, Lower>
where
    Root: LexerRoot,
    Lower: NonTopLayer<Change = TokenChange> + Send + Sync + 'static,
{
    #[allow(dead_code)]
    pub(crate) fn lex_change<'a>(
        &'a mut self,
        ctx: &'a crate::scheme::context::Context,
        state: &'a mut LexerSnapshotState<Root>,
        change: TextChange,
    ) -> impl Future<
        Output = Result<LayerChanges<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt>,
    > + Send
    + 'a {
        async move {
            self.lex_changes(ctx, state, std::slice::from_ref(&change))
                .await
        }
    }

    pub(crate) fn lex_changes<'a>(
        &'a mut self,
        ctx: &'a crate::scheme::context::Context,
        state: &'a mut LexerSnapshotState<Root>,
        changes: &'a [TextChange],
    ) -> impl Future<
        Output = Result<LayerChanges<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt>,
    > + Send
    + 'a {
        async move {
            let Some(first) = changes.first() else {
                return Ok(Vec::new());
            };
            let uri = *first.address();
            let total_start = Instant::now();
            let fetch_source_start = Instant::now();
            let snapshot = ctx
                .call(
                    Source::<Lexer<Root, Lower>>::read_span,
                    Span {
                        uri,
                        range: RangeOrPoint::Range(0, usize::MAX),
                    },
                )
                .await
                .map_err(LexInterrupt::ActionError)?;
            let fetch_source_elapsed = fetch_source_start.elapsed();
            self.apply_deltas(
                state,
                uri,
                snapshot.to_string(),
                changes,
                total_start,
                fetch_source_elapsed,
            )
        }
    }

    fn apply_deltas(
        &mut self,
        snapshot_state: &mut LexerSnapshotState<Root>,
        uri: fluent_uri::Uri<&'static str>,
        snapshot: String,
        changes: &[TextChange],
        total_start: Instant,
        fetch_source_elapsed: Duration,
    ) -> Result<LayerChanges<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt> {
        let root_state = self
            .state_id_of::<Root>()
            .ok_or(LexInterrupt::MissingState)?;

        snapshot_state
            .state_instances
            .entry(uri)
            .or_insert_with(|| vec![LexerState::new(root_state)]);
        snapshot_state.token_instances.entry(uri).or_default();
        snapshot_state.token_ranges.entry(uri).or_default();
        let old_visible_start = Instant::now();
        let old_visible_tokens = self.token_data_for_uri(snapshot_state, uri);
        let old_visible_elapsed = old_visible_start.elapsed();

        let delta_scan_start = Instant::now();
        let restart_point = changes
            .iter()
            .map(|change| change.batch.old_changed_range.start)
            .min()
            .unwrap_or(0);
        let net_shift: isize = changes
            .iter()
            .map(|change| {
                change.batch.new_changed_range.len() as isize
                    - change.batch.old_changed_range.len() as isize
            })
            .sum();
        let new_change_end = changes
            .iter()
            .map(|change| change.batch.new_changed_range.end)
            .max()
            .unwrap_or(restart_point);
        let delta_scan_elapsed = delta_scan_start.elapsed();

        let restart_lookup_start = Instant::now();
        let restart_token_pos = {
            let states = &snapshot_state.state_instances[&uri];
            let tokens = &snapshot_state.token_instances[&uri];
            let ranges = &snapshot_state.token_ranges[&uri];
            debug_assert_eq!(states.len(), tokens.len() + 1);
            let position = states
                .windows(2)
                .position(|window| restart_point <= window[1].offset)
                .unwrap_or(tokens.len());
            if position == tokens.len()
                && tokens
                    .last()
                    .is_some_and(|&token_id| matches!(self.get(token_id), Entry::EOF))
                && ranges
                    .last()
                    .is_some_and(|&(start, end)| restart_point >= start && restart_point <= end)
            {
                tokens.len().saturating_sub(1)
            } else {
                position
            }
        };
        let restart_lookup_elapsed = restart_lookup_start.elapsed();

        let restart_state_pos = restart_token_pos;

        let old_suffix_snapshot_start = Instant::now();
        let (start_state, old_states, old_tokens, old_ranges) = {
            let states = snapshot_state
                .state_instances
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let tokens = snapshot_state
                .token_instances
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let ranges = snapshot_state
                .token_ranges
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            debug_assert_eq!(states.len(), tokens.len() + 1);
            debug_assert_eq!(tokens.len(), ranges.len());

            let start_state = states[restart_state_pos].clone();
            let mut old_states: Vec<LexerState> =
                states[restart_state_pos + 1..].iter().cloned().collect();
            let mut old_tokens: Vec<usize> = tokens[restart_token_pos..].to_vec();
            let mut old_ranges: Vec<(usize, usize)> = ranges[restart_token_pos..].to_vec();

            if restart_token_pos == tokens.len() && !old_tokens.is_empty() {
                old_tokens.pop();
                old_ranges.pop();
                if !old_states.is_empty() {
                    old_states.pop();
                }
            }

            states.truncate(restart_state_pos + 1);
            tokens.truncate(restart_token_pos);
            ranges.truncate(restart_token_pos);

            (start_state, old_states, old_tokens, old_ranges)
        };
        let old_suffix_snapshot_elapsed = old_suffix_snapshot_start.elapsed();

        let mut new_token_ids: Vec<usize> = Vec::new();
        let mut new_states: Vec<LexerState> = Vec::new();
        let mut new_ranges: Vec<(usize, usize)> = Vec::new();
        let lookup_build_start = Instant::now();
        let mut old_state_lookup: HashMap<LexerState, Vec<usize>> = HashMap::new();
        for (index, state) in old_states.iter().enumerate() {
            old_state_lookup
                .entry(shift_state(state, net_shift))
                .or_default()
                .push(index);
        }
        let old_boundary_is_eof = old_tokens
            .iter()
            .map(|&token_id| matches!(self.get(token_id), Entry::EOF))
            .collect::<Vec<_>>();
        let lookup_build_elapsed = lookup_build_start.elapsed();
        let mut convergence: Option<(usize, usize)> = None;
        let replay_start = Instant::now();
        self.lex_cont(start_state, snapshot, |token_id, state, start, end| {
            new_token_ids.push(token_id);
            new_states.push(state.clone());
            new_ranges.push((start, end));
            if state.offset >= new_change_end {
                let new_boundary_is_eof = start == end;
                if let Some(old_state_index) = old_state_lookup.get(state).and_then(|indices| {
                    indices
                        .iter()
                        .rev()
                        .copied()
                        .find(|&index| !old_boundary_is_eof[index] || new_boundary_is_eof)
                }) {
                    convergence = Some((new_token_ids.len(), old_state_index + 1));
                    return false;
                }
            }
            true
        })?;
        let replay_elapsed = replay_start.elapsed();

        let (new_prefix_len, old_suffix_start_index) =
            convergence.unwrap_or_else(|| (new_token_ids.len(), old_tokens.len()));

        let state_splice_start = Instant::now();
        {
            let states = snapshot_state
                .state_instances
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let tokens = snapshot_state
                .token_instances
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let ranges = snapshot_state
                .token_ranges
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            debug_assert_eq!(states.len(), restart_state_pos + 1);
            debug_assert_eq!(tokens.len(), restart_token_pos);
            debug_assert_eq!(tokens.len(), ranges.len());

            states.extend(new_states.iter().take(new_prefix_len).cloned());
            states.extend(
                old_states
                    .iter()
                    .skip(old_suffix_start_index)
                    .map(|state| shift_state(state, net_shift)),
            );

            tokens.extend(new_token_ids.iter().take(new_prefix_len).copied());
            tokens.extend(old_tokens.iter().skip(old_suffix_start_index).copied());
            ranges.extend(new_ranges.iter().take(new_prefix_len).copied());
            ranges.extend(
                old_ranges
                    .iter()
                    .skip(old_suffix_start_index)
                    .copied()
                    .map(|range| shift_range(range, net_shift)),
            );

            debug_assert_eq!(states.len(), tokens.len() + 1);
            debug_assert_eq!(tokens.len(), ranges.len());
        }
        let state_splice_elapsed = state_splice_start.elapsed();

        let new_visible_start = Instant::now();
        let new_visible_tokens = self.token_data_for_uri(snapshot_state, uri);
        let new_visible_elapsed = new_visible_start.elapsed();
        let batch_diff_start = Instant::now();
        let token_batch = Self::build_visible_batch(old_visible_tokens, new_visible_tokens);
        let changed = token_batch.is_changed();
        let old_visible_len = token_batch.old_units.len();
        let new_visible_len = token_batch.new_units.len();
        let prefix_len = token_batch.prefix_len;
        let suffix_len = token_batch.suffix_len;
        let batch_diff_elapsed = batch_diff_start.elapsed();
        let total_elapsed = total_start.elapsed();

        log::debug!(
            target: "Measure",
            "lex {} total={:?} fetch_source={:?} old_visible={:?} delta_scan={:?} restart_lookup={:?} old_suffix={:?} lookup_build={:?} replay={:?} splice={:?} new_visible={:?} batch_diff={:?} changed={} restart={} restart_token={} change_end={} net_shift={} new_prefix={} reused_suffix={} old_tokens={} new_tokens={} prefix={} suffix={}",
            uri,
            total_elapsed,
            fetch_source_elapsed,
            old_visible_elapsed,
            delta_scan_elapsed,
            restart_lookup_elapsed,
            old_suffix_snapshot_elapsed,
            lookup_build_elapsed,
            replay_elapsed,
            state_splice_elapsed,
            new_visible_elapsed,
            batch_diff_elapsed,
            changed,
            restart_point,
            restart_token_pos,
            new_change_end,
            net_shift,
            new_prefix_len,
            old_tokens.len().saturating_sub(old_suffix_start_index),
            old_visible_len,
            new_visible_len,
            prefix_len,
            suffix_len,
        );

        if changed {
            Ok(vec![ReplacementChange::new(uri, token_batch)])
        } else {
            Ok(Vec::new())
        }
    }
}

impl<Root: LexerRoot, Lower> Lexer<Root, Lower> {
    pub(crate) fn lex_cont(
        &mut self,
        start_state: LexerState,
        input_str: String,
        mut cont: impl FnMut(usize, &LexerState, usize, usize) -> bool,
    ) -> Result<(), LexInterrupt> {
        let mut input = Input::new(input_str.as_bytes());
        let mut state = start_state;
        let mut unexpected_input: Vec<u8> = Vec::new();

        while state.offset < input.end() {
            let MatchReport { best, .. } = self.select_best_match(&state, &mut input);
            match best {
                Some(best_match) => {
                    if !unexpected_input.is_empty() {
                        let taken = std::mem::take(&mut unexpected_input);
                        let current_state = state.current_state()?;
                        let error = ErrorToken::UnexpectedToken {
                            start: state.offset - taken.len(),
                            end: state.offset,
                            expected_state: current_state.id,
                        };
                        let token_id = self.alloc(Entry::Error(taken.len(), error));
                        if !cont(token_id, &state, state.offset - taken.len(), state.offset) {
                            return Ok(());
                        }
                    }
                    let is_skip = state
                        .current_state()
                        .ok()
                        .and_then(|s| self.tokens_in_state(s))
                        .and_then(|tokens| tokens.get(best_match.token_index))
                        .is_some_and(|t| t.skip);
                    match self.next_token(&mut state, &best_match, &mut input) {
                        Ok(token) => {
                            if is_skip {
                                continue;
                            }
                            let length = best_match.end - best_match.start;
                            let token_id = self.alloc(Entry::Token {
                                length,
                                terminal: token.terminal,
                                value: token.value,
                            });
                            if !cont(token_id, &state, best_match.start, best_match.end) {
                                return Ok(());
                            }
                        }
                        Err(LexInterrupt::ParseError(err, lexeme)) => {
                            let current_state = state.current_state()?;
                            let token = &self.tokens[current_state.id][best_match.token_index];
                            return Err(LexInterrupt::TokenParseError {
                                token: token.label,
                                lexeme,
                                err,
                            });
                        }
                        Err(LexInterrupt::InternalError(err)) => {
                            return Err(LexInterrupt::TokenParseError {
                                token: "<internal>",
                                lexeme: String::new(),
                                err,
                            });
                        }
                        Err(other) => return Err(other),
                    }
                }
                None => {
                    unexpected_input.push(input.haystack()[state.offset]);
                    state.offset += 1;
                }
            }
        }

        if !unexpected_input.is_empty() {
            let taken = std::mem::take(&mut unexpected_input);
            let current_state = state.current_state()?;
            let error = ErrorToken::UnexpectedToken {
                start: state.offset - taken.len(),
                end: state.offset,
                expected_state: current_state.id,
            };
            let token_id = self.alloc(Entry::Error(taken.len(), error));
            if !cont(token_id, &state, state.offset - taken.len(), state.offset) {
                return Ok(());
            }
        }

        let eof = self.alloc(Entry::EOF);
        if !cont(eof, &state, state.offset, state.offset) {
            return Ok(());
        }

        Ok(())
    }

    pub(crate) fn next_token(
        &self,
        state: &mut LexerState,
        BestMatch {
            token_index,
            start,
            end,
        }: &BestMatch,
        input: &mut Input,
    ) -> Result<LexedToken<Root>, LexInterrupt> {
        let current = state.current_state()?;
        let Some(token) = self
            .tokens_in_state(current)
            .and_then(|tokens| tokens.get(*token_index))
        else {
            return Err(LexInterrupt::NoCandidate);
        };

        let Ok(lexeme) = std::str::from_utf8(&input.haystack()[*start..*end]) else {
            return Err(LexInterrupt::InternalError(
                "Invalid UTF-8 in input".to_string(),
            ));
        };

        let action = match &token.action {
            StateAction::Enter(s) if token.captures_context => {
                StateAction::Enter(State::with_context(s.id, lexeme))
            }
            other => other.clone(),
        };

        state.offset = *end;
        state.apply_action(action);

        token
            .build(lexeme)
            .map(|value| LexedToken {
                terminal: token.terminal,
                value,
            })
            .map_err(|e| LexInterrupt::ParseError(e.to_string(), lexeme.to_string()))
    }

    pub(crate) fn select_best_match(
        &self,
        last_state: &LexerState,
        input: &mut Input,
    ) -> MatchReport {
        let Some(current) = last_state.current_state().ok() else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: LexInterrupt::MissingState,
            };
        };
        let Some(matcher) = self.state_matcher(current.clone()) else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: LexInterrupt::MissingState,
            };
        };
        let Some(tokens) = self.tokens_in_state(current) else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: LexInterrupt::MissingState,
            };
        };

        input.set_range(last_state.offset..input.end());
        input.set_anchored(Anchored::Yes);

        let haystack = input.haystack();
        let search_start = input.start();
        let search_end = input.end();
        let Ok(mut dfa_state) = matcher.dfa.start_state_forward(input) else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: LexInterrupt::UnsupportedSearch,
            };
        };
        let mut best: Option<(usize, usize)> = None;
        let mut stop_offset = search_end;
        let mut stop_reason = LexInterrupt::EndOfInput;

        let capture: Option<&[u8]> = last_state.current_context().map(|s| s.as_bytes());

        for (offset, &byte) in haystack[search_start..search_end].iter().enumerate() {
            dfa_state = matcher.dfa.next_state(dfa_state, byte);
            if matcher.dfa.is_special_state(dfa_state) {
                let absolute_offset = search_start + offset;
                record_best_match(
                    &matcher.dfa,
                    &matcher.token_index_by_pattern,
                    dfa_state,
                    absolute_offset,
                    tokens,
                    haystack,
                    search_start,
                    capture,
                    &mut best,
                );
                if matcher.dfa.is_dead_state(dfa_state) {
                    stop_offset = absolute_offset + 1;
                    stop_reason = LexInterrupt::DeadState;
                    break;
                }
                if matcher.dfa.is_quit_state(dfa_state) {
                    stop_offset = absolute_offset + 1;
                    stop_reason = LexInterrupt::QuitState;
                    break;
                }
            }
        }

        if matches!(stop_reason, LexInterrupt::EndOfInput) {
            dfa_state = matcher.dfa.next_eoi_state(dfa_state);
            record_best_match(
                &matcher.dfa,
                &matcher.token_index_by_pattern,
                dfa_state,
                search_end,
                tokens,
                haystack,
                search_start,
                capture,
                &mut best,
            );
        }

        MatchReport {
            best: best.map(|(token_index, match_end)| BestMatch {
                token_index,
                start: last_state.offset,
                end: match_end,
            }),
            stop_offset,
            stop_reason,
        }
    }
}

fn record_best_match<A: Automaton, R>(
    dfa: &A,
    token_index_by_pattern: &[usize],
    dfa_state: regex_automata::util::primitives::StateID,
    match_end: usize,
    tokens: &[crate::component::lex::ResolvedToken<R>],
    haystack: &[u8],
    search_start: usize,
    capture: Option<&[u8]>,
    best: &mut Option<(usize, usize)>,
) {
    if !dfa.is_match_state(dfa_state) {
        return;
    }

    let ctx = capture.and_then(|b| std::str::from_utf8(b).ok());
    let lexeme_end = match_end;

    for pattern_index in 0..dfa.match_len(dfa_state) {
        let token_index = token_index_by_pattern[dfa.match_pattern(dfa_state, pattern_index)];

        if let Some(token) = tokens.get(token_index) {
            if let Some(validate) = token.validate {
                if let Ok(lexeme) = std::str::from_utf8(&haystack[search_start..lexeme_end]) {
                    if !validate(lexeme, ctx) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
        }

        let should_replace = match *best {
            None => true,
            Some((best_token_index, best_end)) => {
                match_end > best_end || (match_end == best_end && token_index < best_token_index)
            }
        };
        if should_replace {
            *best = Some((token_index, match_end));
        }
    }
}
