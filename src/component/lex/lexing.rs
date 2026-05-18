use regex_automata::{Anchored, Input, dfa::Automaton};

use crate::component::lex::{LexError, Lexer, LexerState, ResolvedToken, Token};

pub enum Interrupt {
    ParseError,
    NoCandidate,
}

impl Lexer {
    fn next(&self, mut state: LexerState, input: &str) -> (LexerState, Result<Token, Interrupt>) {
        match self.select_best_match(&state, input) {
            Some((resolved, match_end)) => {
                state.offset += match_end;
                state.apply_action(resolved.action);

                (
                    state,
                    resolved
                        .build(&input[..match_end])
                        .map_err(|_| Interrupt::ParseError),
                )
            }
            None => (state, Err(Interrupt::NoCandidate)),
        }
    }
    fn select_best_match(&self, state: &LexerState, input: &str) -> Option<(ResolvedToken, usize)> {
        let current_state = state.current_state()?;
        let matcher = self.state_matcher(current_state)?;
        let tokens = self.tokens_in_state(current_state)?;

        let input = Input::new(input).anchored(Anchored::Yes);
        let haystack = input.haystack();
        let mut dfa_state = matcher.dfa.start_state_forward(&input).ok()?;
        let mut best: Option<(usize, usize)> = None;
        let mut stopped_early = false;

        for (offset, &byte) in haystack.iter().enumerate() {
            dfa_state = matcher.dfa.next_state(dfa_state, byte);
            if matcher.dfa.is_special_state(dfa_state) {
                record_best_match(
                    &matcher.dfa,
                    &matcher.token_index_by_pattern,
                    dfa_state,
                    offset,
                    &mut best,
                );
                if matcher.dfa.is_dead_state(dfa_state) || matcher.dfa.is_quit_state(dfa_state) {
                    stopped_early = true;
                    break;
                }
            }
        }

        if !stopped_early {
            dfa_state = matcher.dfa.next_eoi_state(dfa_state);
            record_best_match(
                &matcher.dfa,
                &matcher.token_index_by_pattern,
                dfa_state,
                haystack.len(),
                &mut best,
            );
        }

        let (token_index, match_end) = best?;
        Some((tokens[token_index].clone(), match_end))
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
