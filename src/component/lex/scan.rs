//! DFA scanning and token commitment. This code never decides transaction shape;
//! incremental replay supplies the restart state and owns convergence.

use std::collections::HashSet;

use regex_automata::{Anchored, Input, dfa::Automaton};

use super::{
    LexErrorInfo, LexErrorKind, LexInterrupt, LexMoment, Lexer, LexerRoot, LexerState, State,
    StateAction, TokenAction, WhenCx, WithCx, token::TokenMatch,
};

impl<Root: LexerRoot> Lexer<Root> {
    pub(crate) fn lex_cont(
        &mut self,
        start_state: LexerState<Root>,
        input_str: impl AsRef<str>,
        mut cont: impl FnMut(usize, &LexerState<Root>, usize, usize) -> bool,
    ) -> Result<LexerState<Root>, LexInterrupt> {
        let mut input = Input::new(input_str.as_ref().as_bytes());
        let mut state = start_state;
        let mut unexpected_start: Option<usize> = None;
        let mut zero_progress = HashSet::from([state.clone()]);

        while state.offset < input.end() {
            match self.select_step(&state, &mut input, LexMoment::Normal)? {
                Some(step) => {
                    let start = step.start;
                    let end = step.end;
                    if let Some(start) = unexpected_start.take()
                        && !self.emit_state_error(
                            &state,
                            LexErrorInfo {
                                kind: LexErrorKind::UnexpectedInput,
                                start,
                                end: state.offset,
                            },
                            false,
                            &mut cont,
                        )?
                    {
                        return Ok(state);
                    }

                    if let Some(token_id) = self.commit_match(&mut state, step)?
                        && !cont(token_id, &state, start, end)
                    {
                        return Ok(state);
                    }
                    if start == end {
                        if !zero_progress.insert(state.clone()) {
                            return Err(LexInterrupt::InternalError(format!(
                                "empty token cycle at byte {}",
                                state.offset
                            )));
                        }
                    } else {
                        zero_progress.clear();
                        zero_progress.insert(state.clone());
                    }
                }
                None => {
                    unexpected_start.get_or_insert(state.offset);
                    state.offset += 1;
                    zero_progress.clear();
                    zero_progress.insert(state.clone());
                }
            }
        }

        if let Some(start) = unexpected_start.take()
            && !self.emit_state_error(
                &state,
                LexErrorInfo {
                    kind: LexErrorKind::UnexpectedInput,
                    start,
                    end: state.offset,
                },
                false,
                &mut cont,
            )?
        {
            return Ok(state);
        }

        while let Some(step) = self.select_step(&state, &mut input, LexMoment::Eof)? {
            let start = step.start;
            let end = step.end;
            if let Some(token_id) = self.commit_match(&mut state, step)?
                && !cont(token_id, &state, start, end)
            {
                return Ok(state);
            }
            if !zero_progress.insert(state.clone()) {
                return Err(LexInterrupt::InternalError(format!(
                    "empty token cycle at byte {}",
                    state.offset
                )));
            }
        }

        if state.parent_state().is_some()
            && !self.emit_state_error(
                &state,
                LexErrorInfo {
                    kind: LexErrorKind::RequiredBoundary,
                    start: state.offset,
                    end: state.offset,
                },
                true,
                &mut cont,
            )?
        {
            return Ok(state);
        }

        Ok(state)
    }

    fn emit_state_error(
        &mut self,
        state: &LexerState<Root>,
        info: LexErrorInfo,
        boundary: bool,
        cont: &mut impl FnMut(usize, &LexerState<Root>, usize, usize) -> bool,
    ) -> Result<bool, LexInterrupt> {
        let current = state.current_state()?;
        let builder = if boundary {
            self.boundary_error_builder(current)
        } else {
            self.recovery_error_builder(current)
        }
        .ok_or(LexInterrupt::MissingState)?
        .clone();
        let value = builder(info)?;
        let token_id = self.alloc_token(
            info.start,
            info.end.saturating_sub(info.start),
            None,
            Some(info),
            value,
        );
        Ok(cont(token_id, state, info.start, info.end))
    }

    fn select_step(
        &self,
        state: &LexerState<Root>,
        input: &mut Input,
        moment: LexMoment,
    ) -> Result<Option<TokenMatch<Root>>, LexInterrupt> {
        let current = state.current_state()?;
        // Empty transitions are checked first so scope exits and boundary actions
        // happen at the byte where they become valid.
        if let Some(step) = self.scan_empty_state(state, current.clone(), moment)? {
            return Ok(Some(step));
        }
        if moment == LexMoment::Eof {
            return Ok(None);
        }
        self.scan_regex_state(state, current, input.haystack(), state.offset)
    }

    fn scan_empty_state(
        &self,
        lexer_state: &LexerState<Root>,
        state: State<Root>,
        moment: LexMoment,
    ) -> Result<Option<TokenMatch<Root>>, LexInterrupt> {
        let root_state = self
            .state_id_of::<Root>()
            .ok_or(LexInterrupt::MissingState)?;
        let tokens = self
            .tokens_in_state(state.clone())
            .ok_or(LexInterrupt::MissingState)?;
        let mut best: Option<(u8, usize, TokenMatch<Root>)> = None;

        for (token_index, token) in tokens.iter().enumerate() {
            if !token.empty {
                continue;
            }
            let current_slots = lexer_state.current_slots()?;
            let when_cx = WhenCx::new(
                "",
                moment,
                lexer_state.depth(),
                current_slots,
                lexer_state.parent_slots(),
            );
            if token.when.as_ref().is_some_and(|when| !when(&when_cx)) {
                continue;
            }

            let (rank, transition) = match &token.action {
                TokenAction::None => (2u8, StateAction::None),
                TokenAction::Enter { next } => (1u8, StateAction::Enter(State::new(next.id))),
                TokenAction::Exit => {
                    if state.id == root_state.id {
                        continue;
                    }
                    (0u8, StateAction::Exit)
                }
            };
            // Exit, enter, then ordinary actions give nested scopes a stable
            // precedence; declaration order breaks remaining ties.
            let candidate = TokenMatch {
                token_index,
                start: lexer_state.offset,
                end: lexer_state.offset,
                lexeme: String::new(),
                moment,
                value: token.build("")?,
                transition,
            };
            let should_replace = match &best {
                None => true,
                Some((best_rank, best_token_index, _)) => {
                    rank < *best_rank || (rank == *best_rank && token_index < *best_token_index)
                }
            };
            if should_replace {
                best = Some((rank, token_index, candidate));
            }
        }

        Ok(best.map(|(_, _, candidate)| candidate))
    }

    fn scan_regex_state(
        &self,
        lexer_state: &LexerState<Root>,
        state: State<Root>,
        haystack: &[u8],
        offset: usize,
    ) -> Result<Option<TokenMatch<Root>>, LexInterrupt> {
        let current_state = state.clone();
        let matcher = self
            .state_matcher(state.clone())
            .ok_or(LexInterrupt::MissingState)?;
        let tokens = self
            .tokens_in_state(state)
            .ok_or(LexInterrupt::MissingState)?;
        let raw_match_ends = collect_raw_match_ends(
            &matcher.dfa,
            &matcher.token_index_by_pattern,
            tokens,
            haystack,
            offset,
            lexer_state,
            LexMoment::Normal,
            None,
        );

        let mut best: Option<(u8, usize, usize, TokenMatch<Root>)> = None;
        for (token_index, match_ends) in raw_match_ends.into_iter().enumerate() {
            let Some(token) = tokens.get(token_index) else {
                continue;
            };
            for raw_end in match_ends {
                let Some(end) = self.recover_match_end(
                    current_state.clone(),
                    haystack,
                    token_index,
                    raw_end,
                    lexer_state,
                ) else {
                    continue;
                };
                let Ok(lexeme) = std::str::from_utf8(&haystack[offset..end]) else {
                    continue;
                };
                let value = token.build(lexeme)?;
                let (rank, transition) = match &token.action {
                    TokenAction::None => (2u8, StateAction::None),
                    TokenAction::Enter { next } => (1u8, StateAction::Enter(State::new(next.id))),
                    TokenAction::Exit => {
                        if current_state.id
                            == self
                                .state_id_of::<Root>()
                                .ok_or(LexInterrupt::MissingState)?
                                .id
                        {
                            continue;
                        }
                        (0u8, StateAction::Exit)
                    }
                };

                let candidate = TokenMatch {
                    token_index,
                    start: offset,
                    end,
                    lexeme: lexeme.to_string(),
                    moment: LexMoment::Normal,
                    value,
                    transition,
                };
                let should_replace = match &best {
                    None => true,
                    Some((best_rank, best_end, best_token_index, _)) => {
                        rank < *best_rank
                            || (rank == *best_rank
                                && (end > *best_end
                                    || (end == *best_end && token_index < *best_token_index)))
                    }
                };
                if should_replace {
                    best = Some((rank, end, token_index, candidate));
                }
            }
        }

        Ok(best.map(|(_, _, _, candidate)| candidate))
    }

    fn recover_match_end(
        &self,
        state: State<Root>,
        haystack: &[u8],
        token_index: usize,
        raw_end: usize,
        lexer_state: &LexerState<Root>,
    ) -> Option<usize> {
        let token = self
            .tokens_in_state(state.clone())
            .and_then(|tokens| tokens.get(token_index))?;
        let Some(recover_when) = token.recover_when.as_ref() else {
            return Some(raw_end);
        };

        let mut last_success_end = raw_end;
        loop {
            let rest = std::str::from_utf8(&haystack[last_success_end..]).ok()?;
            let recover_chars = recover_when(rest, lexer_state.current_key());
            if recover_chars == 0 {
                return Some(last_success_end);
            }

            let recovered_bytes = char_len_to_byte_len(rest, recover_chars);
            if recovered_bytes == 0 {
                return Some(last_success_end);
            }

            let resume_offset = last_success_end.saturating_add(recovered_bytes);
            let resumed_end = self.raw_match_end_for_token(
                state.clone(),
                haystack,
                resume_offset,
                token_index,
                lexer_state,
            );
            let Some(resumed_end) = resumed_end else {
                return Some(last_success_end);
            };
            if resumed_end <= resume_offset {
                return Some(last_success_end);
            }

            last_success_end = resumed_end;
        }
    }

    fn raw_match_end_for_token(
        &self,
        state: State<Root>,
        haystack: &[u8],
        offset: usize,
        token_index: usize,
        lexer_state: &LexerState<Root>,
    ) -> Option<usize> {
        let matcher = self.state_matcher(state.clone())?;
        let tokens = self.tokens_in_state(state)?;
        collect_raw_match_ends(
            &matcher.dfa,
            &matcher.token_index_by_pattern,
            tokens,
            haystack,
            offset,
            lexer_state,
            LexMoment::Normal,
            Some(token_index),
        )
        .into_iter()
        .nth(token_index)
        .and_then(|match_ends| match_ends.into_iter().last())
    }

    fn commit_match(
        &mut self,
        state: &mut LexerState<Root>,
        step: TokenMatch<Root>,
    ) -> Result<Option<usize>, LexInterrupt> {
        let Some(token) = self
            .tokens_in_state(state.current_state()?)
            .and_then(|tokens| tokens.get(step.token_index))
        else {
            return Err(LexInterrupt::NoCandidate);
        };
        let old_state = state.clone();
        let skip = token.skip;
        let terminal = token.terminal;
        let label = token.label;
        let with_hook = token.with_hook.clone();
        let source_slots = old_state.current_slots()?.clone();
        let parent_slots = old_state.parent_slots_cloned();
        state.offset = step.end;
        state.apply_action(step.transition);
        if let Some(with_hook) = with_hook {
            let depth = state.depth();
            let target_slots = state.current_slots_mut()?;
            let mut cx = WithCx::new(
                &step.lexeme,
                step.moment,
                depth,
                target_slots,
                source_slots,
                parent_slots,
            );
            with_hook(&mut cx);
        }

        if step.start == step.end && *state == old_state {
            return Err(LexInterrupt::InternalError(format!(
                "empty token {label} did not change lexer state",
            )));
        }

        if skip {
            return Ok(None);
        }

        Ok(Some(self.alloc_token(
            step.start,
            step.end.saturating_sub(step.start),
            Some(terminal),
            None,
            step.value,
        )))
    }
}

fn collect_raw_match_ends<A: Automaton, R: LexerRoot>(
    dfa: &A,
    token_index_by_pattern: &[usize],
    tokens: &[crate::component::lex::ResolvedToken<R>],
    haystack: &[u8],
    search_start: usize,
    lexer_state: &LexerState<R>,
    moment: LexMoment,
    target_token: Option<usize>,
) -> Vec<Vec<usize>> {
    let mut input = Input::new(haystack);
    input.set_range(search_start..haystack.len());
    input.set_anchored(Anchored::Yes);

    let search_end = input.end();
    let Ok(mut dfa_state) = dfa.start_state_forward(&input) else {
        return vec![Vec::new(); tokens.len()];
    };
    let mut best_ends = vec![Vec::new(); tokens.len()];

    for (offset_delta, &byte) in haystack[search_start..search_end].iter().enumerate() {
        dfa_state = dfa.next_state(dfa_state, byte);
        if dfa.is_special_state(dfa_state) {
            let match_end = search_start + offset_delta;
            record_match_ends(
                dfa,
                token_index_by_pattern,
                dfa_state,
                match_end,
                tokens,
                haystack,
                search_start,
                lexer_state,
                moment,
                target_token,
                &mut best_ends,
            );
            if dfa.is_dead_state(dfa_state) || dfa.is_quit_state(dfa_state) {
                return best_ends;
            }
        }
    }

    let dfa_state = dfa.next_eoi_state(dfa_state);
    record_match_ends(
        dfa,
        token_index_by_pattern,
        dfa_state,
        search_end,
        tokens,
        haystack,
        search_start,
        lexer_state,
        moment,
        target_token,
        &mut best_ends,
    );

    best_ends
}

fn record_match_ends<A: Automaton, R: LexerRoot>(
    dfa: &A,
    token_index_by_pattern: &[usize],
    dfa_state: regex_automata::util::primitives::StateID,
    match_end: usize,
    tokens: &[crate::component::lex::ResolvedToken<R>],
    haystack: &[u8],
    search_start: usize,
    lexer_state: &LexerState<R>,
    moment: LexMoment,
    target_token: Option<usize>,
    best_ends: &mut [Vec<usize>],
) {
    if !dfa.is_match_state(dfa_state) {
        return;
    }

    for pattern_index in 0..dfa.match_len(dfa_state) {
        let token_index = token_index_by_pattern[dfa.match_pattern(dfa_state, pattern_index)];
        if target_token.is_some_and(|target| target != token_index) {
            continue;
        }
        let Some(token) = tokens.get(token_index) else {
            continue;
        };

        if let Some(when) = &token.when {
            let Ok(lexeme) = std::str::from_utf8(&haystack[search_start..match_end]) else {
                continue;
            };
            let current_slots = match lexer_state.current_slots() {
                Ok(slots) => slots,
                Err(_) => continue,
            };
            let when_cx = WhenCx::new(
                lexeme,
                moment,
                lexer_state.depth(),
                current_slots,
                lexer_state.parent_slots(),
            );
            if !when(&when_cx) {
                continue;
            }
        }

        let ends = &mut best_ends[token_index];
        if ends.last().copied() != Some(match_end) {
            ends.push(match_end);
        }
    }
}

fn char_len_to_byte_len(rest: &str, char_len: usize) -> usize {
    if char_len == 0 {
        return 0;
    }

    rest.char_indices()
        .nth(char_len)
        .map(|(offset, _)| offset)
        .unwrap_or(rest.len())
}
