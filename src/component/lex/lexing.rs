use regex_automata::{Anchored, Input, dfa::Automaton};

use crate::component::lex::{
    BestMatch, Interrupt, LexError, Lexer, LexerState, MatchReport, Token,
};

impl Lexer {
    pub(crate) fn next_wildcard(&self, mut state: LexerState) -> LexerState {
        state.offset += 1;
        state
    }

    pub(crate) fn next_token(
        &self,
        mut state: LexerState,
        BestMatch {
            token_index,
            start,
            end,
        }: BestMatch,
        input: &mut Input,
    ) -> (LexerState, Result<Token, Interrupt>) {
        let Some(current_state) = state.current_state() else {
            return (state, Err(Interrupt::NoCandidate));
        };
        let Some(token) = self
            .tokens_in_state(current_state)
            .and_then(|tokens| tokens.get(token_index))
        else {
            return (state, Err(Interrupt::NoCandidate));
        };

        state.offset = end;
        state.apply_action(token.action);

        let Ok(lexeme) = std::str::from_utf8(&input.haystack()[start..end]) else {
            return (state, Err(Interrupt::ParseError));
        };

        (
            state,
            token.build(lexeme).map_err(|_| Interrupt::ParseError),
        )
    }

    pub(crate) fn select_best_match(
        &self,
        last_state: &LexerState,
        input: &mut Input,
    ) -> MatchReport {
        let Some(matcher) = last_state
            .current_state()
            .and_then(|state_id| self.state_matcher(state_id))
        else {
            return MatchReport {
                best: None,
                stop_offset: last_state.offset,
                stop_reason: Interrupt::MissingState,
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
                stop_reason: Interrupt::UnsupportedSearch,
            };
        };
        let mut best: Option<(usize, usize)> = None;
        let mut stop_offset = search_end;
        let mut stop_reason = Interrupt::EndOfInput;

        for (offset, &byte) in haystack[search_start..search_end].iter().enumerate() {
            dfa_state = matcher.dfa.next_state(dfa_state, byte);
            if matcher.dfa.is_special_state(dfa_state) {
                let absolute_offset = search_start + offset;
                record_best_match(
                    &matcher.dfa,
                    &matcher.token_index_by_pattern,
                    dfa_state,
                    absolute_offset,
                    &mut best,
                );
                if matcher.dfa.is_dead_state(dfa_state) {
                    stop_offset = absolute_offset + 1;
                    stop_reason = Interrupt::DeadState;
                    break;
                }
                if matcher.dfa.is_quit_state(dfa_state) {
                    stop_offset = absolute_offset + 1;
                    stop_reason = Interrupt::QuitState;
                    break;
                }
            }
        }

        if stop_reason == Interrupt::EndOfInput {
            dfa_state = matcher.dfa.next_eoi_state(dfa_state);
            record_best_match(
                &matcher.dfa,
                &matcher.token_index_by_pattern,
                dfa_state,
                search_end,
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

fn record_best_match<A: Automaton>(
    dfa: &A,
    token_index_by_pattern: &[usize],
    dfa_state: regex_automata::util::primitives::StateID,
    match_end: usize,
    best: &mut Option<(usize, usize)>,
) {
    if !dfa.is_match_state(dfa_state) {
        return;
    }

    for pattern_index in 0..dfa.match_len(dfa_state) {
        let token_index = token_index_by_pattern[dfa.match_pattern(dfa_state, pattern_index)];
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
