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

fn entries_semantically_equal<Root: LexerRoot>(a: &Entry<Root>, b: &Entry<Root>) -> bool {
    match (a, b) {
        (Entry::Token { terminal: t1, value: v1, .. },
         Entry::Token { terminal: t2, value: v2, .. }) => t1 == t2 && v1 == v2,
        (Entry::EOF, Entry::EOF) => true,
        (Entry::Error(l1, _), Entry::Error(l2, _)) => l1 == l2,
        _ => false,
    }
}

fn shift_range((start, end): (usize, usize), shift: isize) -> (usize, usize) {
    if shift >= 0 {
        let shift = shift as usize;
        (start.saturating_add(shift), end.saturating_add(shift))
    } else {
        let shift = (-shift) as usize;
        (start.saturating_sub(shift), end.saturating_sub(shift))
    }
}

impl<Root, Lower> Lexer<Root, Lower>
where
    Root: LexerRoot,
    Lower: NonTopLayer<_Key = Span, _Value = usize> + Send + Sync + 'static,
{
    #[allow(dead_code)]
    pub(crate) fn lex_delta<'a>(
        &'a mut self,
        ctx: &'a crate::scheme::Context,
        state: &'a mut LexerSnapshotState<Root>,
        delta: Delta<Span, usize>,
    ) -> impl Future<
        Output = Result<LayerDeltas<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt>,
    > + Send
    + 'a {
        async move { self.lex_deltas(ctx, state, std::slice::from_ref(&delta)).await }
    }

    pub(crate) fn lex_deltas<'a>(
        &'a mut self,
        ctx: &'a crate::scheme::Context,
        state: &'a mut LexerSnapshotState<Root>,
        deltas: &'a [Delta<Span, usize>],
    ) -> impl Future<
        Output = Result<LayerDeltas<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt>,
    > + Send
    + 'a {
        async move {
            let Some(first) = deltas.first() else {
                return Ok(Vec::new());
            };
            let uri = first.key().uri;
            let snapshot = ctx
                .post::<Source<Lexer<Root, Lower>>, _>(Span {
                    uri,
                    range: RangeOrPoint::Range(0, usize::MAX),
                })
                .await
                .map_err(LexInterrupt::ActionError)?;
            self.apply_deltas(state, uri, snapshot.to_string(), deltas)
        }
    }

    fn apply_deltas(
        &mut self,
        snapshot_state: &mut LexerSnapshotState<Root>,
        uri: fluent_uri::Uri<&'static str>,
        snapshot: String,
        deltas: &[Delta<Span, usize>],
    ) -> Result<LayerDeltas<<Lexer<Root, Lower> as MiddleLayer>::Lower>, LexInterrupt> {
        let root_state = self
            .state_id_of::<Root>()
            .ok_or(LexInterrupt::MissingState)?;

        snapshot_state
            .state_instances
            .entry(uri)
            .or_insert_with(|| vec![LexerState::new(root_state)]);
        snapshot_state.token_instances.entry(uri).or_default();
        snapshot_state.token_ranges.entry(uri).or_default();
        let old_visible_tokens = self.token_data_for_uri(snapshot_state, uri);

        let restart_point = deltas
            .iter()
            .map(|delta| delta.key().range.start())
            .min()
            .unwrap_or(0);
        let net_shift: isize = deltas
            .iter()
            .map(|delta| match delta {
                Delta::Insert { value, .. } => *value as isize,
                Delta::Delete { key } => {
                    -(key.range.end().saturating_sub(key.range.start()) as isize)
                }
            })
            .sum();

        let restart_token_pos = {
            let states = &snapshot_state.state_instances[&uri];
            let tokens = &snapshot_state.token_instances[&uri];
            debug_assert_eq!(states.len(), tokens.len() + 1);
            states
                .windows(2)
                .position(|window| restart_point < window[1].offset)
                .unwrap_or(tokens.len())
        };

        let restart_state_pos = restart_token_pos;

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

        let mut new_token_ids: Vec<usize> = Vec::new();
        let mut new_states: Vec<LexerState> = Vec::new();
        let mut new_ranges: Vec<(usize, usize)> = Vec::new();
        self.lex_cont(start_state, snapshot, |token_id, state, start, end| {
            new_token_ids.push(token_id);
            new_states.push(state.clone());
            new_ranges.push((start, end));
            true
        })?;

        if new_token_ids.len() == old_tokens.len()
            && new_token_ids
                .iter()
                .zip(&old_tokens)
                .all(|(&new_id, &old_id)| {
                    entries_semantically_equal(self.get(new_id), self.get(old_id))
                })
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

            for os in &old_states {
                let mut adjusted = os.clone();
                adjusted.offset = if net_shift >= 0 {
                    (adjusted.offset as isize + net_shift) as usize
                } else {
                    adjusted.offset.saturating_sub((-net_shift) as usize)
                };
                if adjusted.offset >= states.last().map(|s| s.offset).unwrap_or(0) {
                    states.push(adjusted);
                }
            }
            tokens.extend_from_slice(&old_tokens);
            ranges.extend(old_ranges.iter().copied().map(|(start, end)| {
                shift_range((start, end), net_shift)
            }));

            debug_assert_eq!(states.len(), tokens.len() + 1);
            debug_assert_eq!(tokens.len(), ranges.len());
            let new_visible_tokens = self.token_data_for_uri(snapshot_state, uri);
            snapshot_state.visible_batches.insert(
                uri,
                Self::build_visible_batch(old_visible_tokens, new_visible_tokens),
            );
            return Ok(Vec::new());
        }

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

            states.extend(new_states);

            tokens.extend(new_token_ids.iter().copied());
            // The freshly lexed tail is authoritative; do not splice in the
            // old suffix here, or we can reintroduce an EOF before the tail.
            ranges.extend(new_ranges.iter().copied());

            debug_assert_eq!(states.len(), tokens.len() + 1);
            debug_assert_eq!(tokens.len(), ranges.len());
        }

        let new_visible_tokens = self.token_data_for_uri(snapshot_state, uri);
        let visible_batch = Self::build_visible_batch(old_visible_tokens, new_visible_tokens);
        let changed = visible_batch.is_changed();
        snapshot_state.visible_batches.insert(uri, visible_batch);

        if changed {
            Ok(deltas.to_vec())
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
