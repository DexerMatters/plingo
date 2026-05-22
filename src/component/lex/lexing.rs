use regex_automata::{Anchored, Input, dfa::Automaton};

use crate::component::lex::{
    BestMatch, Entry, ErrorToken, LexInterrupt, Lexer, LexerRoot, LexerState, MatchReport, State,
    StateAction,
};

impl<Root: LexerRoot> Lexer<Root> {
    pub(crate) fn lex_cont(
        &mut self,
        start_state: LexerState,
        input_str: impl AsRef<str>,
        mut cont: impl FnMut(usize) -> bool,
    ) -> Option<LexInterrupt> {
        let mut input = Input::new(input_str.as_ref().as_bytes());
        let mut state = start_state;
        let mut unexpected_input = String::new();

        while state.offset < input.end() {
            let MatchReport { best, .. } = self.select_best_match(&state, &mut input);
            match best {
                Some(best_match) => {
                    if !unexpected_input.is_empty() {
                        let taken = std::mem::take(&mut unexpected_input);
                        let error = ErrorToken::UnexpectedToken {
                            start: state.offset - taken.len(),
                            end: state.offset,
                            expected_state: state.current_state().unwrap().id(),
                        };
                        let token_id = self.alloc(Entry::Error(taken.len(), error));
                        if !cont(token_id) {
                            break;
                        }
                    }
                    match self.next_token(&mut state, &best_match, &mut input) {
                        Ok(token) => {
                            let length = best_match.end - best_match.start;
                            let token_id = self.alloc(Entry::Token(length, token));
                            if !cont(token_id) {
                                break;
                            }
                        }
                        Err(LexInterrupt::ParseError(err, lexeme)) => {
                            let token = &self.tokens[state.current_state().unwrap().id()]
                                [best_match.token_index];
                            return Some(LexInterrupt::TokenParseError {
                                token: token.label,
                                lexeme,
                                err,
                            });
                        }
                        Err(LexInterrupt::InternalError(err)) => {
                            return Some(LexInterrupt::TokenParseError {
                                token: "<internal>",
                                lexeme: String::new(),
                                err,
                            });
                        }
                        Err(other) => return Some(other),
                    }
                }
                None => {
                    unexpected_input.push(input.haystack()[state.offset] as char);
                    state.offset += 1;
                }
            }
        }

        None
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
    ) -> Result<Root, LexInterrupt> {
        let current_state = state.current_state()?;
        let Some(token) = self
            .tokens_in_state(current_state)
            .and_then(|tokens| tokens.get(*token_index))
        else {
            return Err(LexInterrupt::NoCandidate);
        };

        state.offset = *end;
        let action = match token.action {
            StateAction::Enter(s) if token.has_payload => {
                StateAction::Enter(State::IdWithCapture {
                    id: s.id(),
                    start: *start,
                    end: *end,
                })
            }
            other => other,
        };
        state.apply_action(action);

        let Ok(lexeme) = std::str::from_utf8(&input.haystack()[*start..*end]) else {
            return Err(LexInterrupt::InternalError(
                "Invalid UTF-8 in input".to_string(),
            ));
        };

        token
            .build(lexeme)
            .map_err(|e| LexInterrupt::ParseError(e.to_string(), lexeme.to_string()))
    }

    pub(crate) fn select_best_match(
        &self,
        last_state: &LexerState,
        input: &mut Input,
    ) -> MatchReport {
        let Some(current_state) = last_state.current_state().ok() else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: LexInterrupt::MissingState,
            };
        };
        let Some(matcher) = self.state_matcher(current_state) else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: LexInterrupt::MissingState,
            };
        };
        let Some(tokens) = self.tokens_in_state(current_state) else {
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

        let capture: Option<&[u8]> = last_state
            .current_capture()
            .and_then(|(cs, ce)| haystack.get(cs..ce));

        for (offset, &byte) in haystack[search_start..search_end].iter().enumerate() {
            dfa_state = matcher.dfa.next_state(dfa_state, byte);
            if matcher.dfa.is_special_state(dfa_state) {
                let absolute_offset = search_start + offset;
                let lexeme_end = absolute_offset + 1;
                record_best_match(
                    &matcher.dfa,
                    &matcher.token_index_by_pattern,
                    dfa_state,
                    absolute_offset,
                    tokens,
                    haystack,
                    search_start,
                    lexeme_end,
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

        if stop_reason == LexInterrupt::EndOfInput {
            dfa_state = matcher.dfa.next_eoi_state(dfa_state);
            record_best_match(
                &matcher.dfa,
                &matcher.token_index_by_pattern,
                dfa_state,
                search_end,
                tokens,
                haystack,
                search_start,
                search_end,
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
    lexeme_end: usize,
    capture: Option<&[u8]>,
    best: &mut Option<(usize, usize)>,
) {
    if !dfa.is_match_state(dfa_state) {
        return;
    }

    let capture_str = capture.and_then(|b| std::str::from_utf8(b).ok());

    for pattern_index in 0..dfa.match_len(dfa_state) {
        let token_index = token_index_by_pattern[dfa.match_pattern(dfa_state, pattern_index)];

        if let Some(token) = tokens.get(token_index) {
            if let Some(validate) = token.validate {
                if let Ok(lexeme_str) = std::str::from_utf8(&haystack[search_start..lexeme_end]) {
                    if !validate(lexeme_str, capture_str) {
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
