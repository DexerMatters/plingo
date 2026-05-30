use regex_automata::{Anchored, Input, dfa::Automaton};

use crate::{
    component::{
        lex::{
            BestMatch, Entry, ErrorToken, LexInterrupt, LexedToken, Lexer, LexerRoot,
            LexerSnapshotState, LexerState, MatchReport, State, StateAction,
        },
        source::Source,
    },
    scheme::{Delta, LayerDeltas, MiddleLayer, NonTopLayer},
    utils::{RangeOrPoint, Span},
};

impl<Root, Lower> Lexer<Root, Lower>
where
    Root: LexerRoot,
    Lower: NonTopLayer<_Key = Span, _Value = usize> + Send + Sync + 'static,
{
    pub(crate) fn lex_delta<'a>(
        &'a mut self,
        ctx: &'a crate::scheme::Context,
        state: &'a mut LexerSnapshotState<Root>,
        delta: Delta<Span, usize>,
    ) -> impl Future<
        Output = Result<LayerDeltas<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt>,
    > + Send
    + 'a {
        let is_insert = matches!(&delta, Delta::Insert { .. });
        async move {
            let key = *delta.key();
            let snapshot = ctx
                .post::<Source<Lexer<Root, Lower>>, _>(Span {
                    uri: key.uri,
                    range: RangeOrPoint::Range(0, usize::MAX),
                })
                .await
                .map_err(LexInterrupt::ActionError)?;
            self.apply_delta(state, key, snapshot.to_string(), is_insert)
        }
    }

    fn apply_delta(
        &mut self,
        snapshot_state: &mut LexerSnapshotState<Root>,
        key: Span,
        snapshot: String,
        is_insert: bool,
    ) -> Result<LayerDeltas<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt> {
        let uri = key.uri;
        let root_state = self
            .state_id_of::<Root>()
            .ok_or(LexInterrupt::MissingState)?;

        snapshot_state
            .state_instances
            .entry(uri)
            .or_insert_with(|| vec![LexerState::new(root_state)]);
        snapshot_state.token_instances.entry(uri).or_default();

        let point = key.range.start();

        let restart_token_pos = {
            let states = &snapshot_state.state_instances[&uri];
            let tokens = &snapshot_state.token_instances[&uri];
            debug_assert_eq!(states.len(), tokens.len() + 1);
            states
                .windows(2)
                .position(|window| point < window[1].offset)
                .unwrap_or(tokens.len())
        };

        let restart_state_pos = restart_token_pos;

        let key_len = key.range.end().saturating_sub(key.range.start());
        let net_shift: isize = if is_insert {
            key_len as isize
        } else {
            -(key_len as isize)
        };

        let (start_state, old_states, old_tokens) = {
            let states = snapshot_state.state_instances.get_mut(&uri).unwrap();
            let tokens = snapshot_state.token_instances.get_mut(&uri).unwrap();
            debug_assert_eq!(states.len(), tokens.len() + 1);

            let start_state = states[restart_state_pos].clone();
            let old_states: Vec<LexerState> =
                states[restart_state_pos + 1..].iter().cloned().collect();
            let old_tokens: Vec<usize> = tokens[restart_token_pos..].to_vec();

            states.truncate(restart_state_pos + 1);
            tokens.truncate(restart_token_pos);

            (start_state, old_states, old_tokens)
        };

        let mut new_token_ids: Vec<usize> = Vec::new();
        let mut new_states: Vec<LexerState> = Vec::new();
        let mut converged_at_old_state: Option<usize> = None;

        self.lex_cont(start_state, snapshot, |token_id, state| {
            new_token_ids.push(token_id);
            new_states.push(state.clone());

            for (old_idx, old_state) in old_states.iter().enumerate() {
                let shifted_old_offset = if net_shift >= 0 {
                    old_state.offset + net_shift as usize
                } else {
                    old_state.offset.saturating_sub((-net_shift) as usize)
                };
                if state.state_stack == old_state.state_stack && state.offset == shifted_old_offset
                {
                    converged_at_old_state = Some(old_idx);
                    return false;
                }
            }
            true
        })?;

        let old_replaced_count = match converged_at_old_state {
            Some(old_idx) => old_idx + 1,
            None => old_tokens.len(),
        };

        let del_start = restart_token_pos;
        let del_end = restart_token_pos + old_replaced_count;

        {
            let states = snapshot_state.state_instances.get_mut(&uri).unwrap();
            let tokens = snapshot_state.token_instances.get_mut(&uri).unwrap();
            debug_assert_eq!(states.len(), restart_state_pos + 1);
            debug_assert_eq!(tokens.len(), restart_token_pos);

            states.extend(new_states);

            if let Some(old_idx) = converged_at_old_state {
                for os in &old_states[old_idx + 1..] {
                    let mut adjusted = os.clone();
                    if net_shift >= 0 {
                        adjusted.offset = (adjusted.offset as isize + net_shift) as usize;
                    } else {
                        adjusted.offset = adjusted.offset.saturating_sub((-net_shift) as usize);
                    }
                    if adjusted.offset >= states.last().map(|s| s.offset).unwrap_or(0) {
                        states.push(adjusted);
                    }
                }
            }

            tokens.extend(new_token_ids.iter().copied());
            tokens.extend_from_slice(&old_tokens[old_replaced_count..]);

            debug_assert_eq!(states.len(), tokens.len() + 1);
        }

        let mut deltas = Vec::new();
        if old_replaced_count > 0 {
            deltas.push(Delta::Delete {
                key: Span {
                    uri,
                    range: RangeOrPoint::from_range(del_start, del_end),
                },
            });
        }
        if !new_token_ids.is_empty() {
            deltas.push(Delta::Insert {
                key: Span {
                    uri,
                    range: RangeOrPoint::Point(del_start),
                },
                value: new_token_ids.len(),
            });
        }

        Ok(deltas)
    }
}

impl<Root: LexerRoot, Lower> Lexer<Root, Lower> {
    pub(crate) fn lex_cont(
        &mut self,
        start_state: LexerState,
        input_str: String,
        mut cont: impl FnMut(usize, &LexerState) -> bool,
    ) -> Result<(), LexInterrupt> {
        let mut input = Input::new(input_str.as_bytes());
        let mut state = start_state;
        let mut unexpected_input = String::new();

        while state.offset < input.end() {
            let MatchReport { best, .. } = self.select_best_match(&state, &mut input);
            match best {
                Some(best_match) => {
                    if !unexpected_input.is_empty() {
                        let taken = std::mem::take(&mut unexpected_input);
                        let error = ErrorToken::UnexpectedToken {
                            start: state.offset - taken.chars().count(),
                            end: state.offset,
                            expected_state: state.current_state().unwrap().id,
                        };
                        let token_id = self.alloc(Entry::Error(taken.chars().count(), error));
                        if !cont(token_id, &state) {
                            return Ok(());
                        }
                    }
                    match self.next_token(&mut state, &best_match, &mut input) {
                        Ok(token) => {
                            let length = best_match.end - best_match.start;
                            let token_id = self.alloc(Entry::Token {
                                length,
                                terminal: token.terminal,
                                value: token.value,
                            });
                            if !cont(token_id, &state) {
                                return Ok(());
                            }
                        }
                        Err(LexInterrupt::ParseError(err, lexeme)) => {
                            let token = &self.tokens[state.current_state().unwrap().id]
                                [best_match.token_index];
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
                    unexpected_input.push(input.haystack()[state.offset] as char);
                    state.offset += 1;
                }
            }
        }

        if !unexpected_input.is_empty() {
            let taken = std::mem::take(&mut unexpected_input);
            let error = ErrorToken::UnexpectedToken {
                start: state.offset - taken.chars().count(),
                end: state.offset,
                expected_state: state.current_state().unwrap().id,
            };
            let token_id = self.alloc(Entry::Error(taken.chars().count(), error));
            if !cont(token_id, &state) {
                return Ok(());
            }
        }

        let eof = self.alloc(Entry::EOF);
        cont(eof, &state);

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
    let lexeme_end = match_end + 1;

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
