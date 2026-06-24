use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use regex_automata::{Anchored, Input, dfa::Automaton};

use crate::{
    component::{
        lex::{
            LexErrorInfo, LexErrorKind, LexInterrupt, LexMoment, Lexer, LexerRoot,
            LexerSnapshotState, LexerState, State, StateAction, TokenAction, TokenChange,
            TokenMatch, TokenOccurrence, WhenCx, WithCx,
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

fn shift_occurrence(occurrence: TokenOccurrence, shift: isize) -> TokenOccurrence {
    TokenOccurrence {
        id: occurrence.id,
        start: shift_offset(occurrence.start, shift),
        end: shift_offset(occurrence.end, shift),
    }
}

fn shift_state<Root: LexerRoot>(state: &LexerState<Root>, shift: isize) -> LexerState<Root> {
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
        snapshot_state.occurrences.entry(uri).or_default();

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
            let occurrences = &snapshot_state.occurrences[&uri];
            debug_assert_eq!(states.len(), occurrences.len() + 1);
            states
                .windows(2)
                .position(|window| restart_point <= window[1].offset)
                .unwrap_or(occurrences.len())
        };
        let restart_lookup_elapsed = restart_lookup_start.elapsed();

        let restart_state_pos = restart_token_pos;

        let old_suffix_snapshot_start = Instant::now();
        let (start_state, old_states, old_occurrences) = {
            let states = snapshot_state
                .state_instances
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let occurrences = snapshot_state
                .occurrences
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            debug_assert_eq!(states.len(), occurrences.len() + 1);

            let start_state = states[restart_state_pos].clone();
            let old_states = states[restart_state_pos + 1..].to_vec();
            let old_occurrences = occurrences[restart_token_pos..].to_vec();

            states.truncate(restart_state_pos + 1);
            occurrences.truncate(restart_token_pos);

            (start_state, old_states, old_occurrences)
        };
        let old_suffix_snapshot_elapsed = old_suffix_snapshot_start.elapsed();

        let mut new_occurrences = Vec::new();
        let mut new_states = Vec::new();
        let lookup_build_start = Instant::now();
        let mut old_state_lookup: HashMap<LexerState<Root>, Vec<usize>> = HashMap::new();
        for (index, state) in old_states.iter().enumerate() {
            old_state_lookup
                .entry(shift_state(state, net_shift))
                .or_default()
                .push(index);
        }
        let lookup_build_elapsed = lookup_build_start.elapsed();

        let mut convergence: Option<(usize, usize)> = None;
        let replay_start = Instant::now();
        let final_state = self.lex_cont(start_state, snapshot, |token_id, state, start, end| {
            new_occurrences.push(TokenOccurrence {
                id: token_id,
                start,
                end,
            });
            new_states.push(state.clone());
            if state.offset >= new_change_end {
                if let Some(old_state_index) = old_state_lookup
                    .get(state)
                    .and_then(|indices| indices.iter().rev().copied().next())
                {
                    convergence = Some((new_occurrences.len(), old_state_index + 1));
                    return false;
                }
            }
            true
        })?;
        if convergence.is_none() && final_state.offset >= new_change_end {
            if let Some(old_state_index) = old_state_lookup
                .get(&final_state)
                .and_then(|indices| indices.iter().rev().copied().next())
            {
                convergence = Some((new_occurrences.len(), old_state_index + 1));
            }
        }
        let replay_elapsed = replay_start.elapsed();

        let (new_prefix_len, old_suffix_start_index) =
            convergence.unwrap_or_else(|| (new_occurrences.len(), old_occurrences.len()));

        let state_splice_start = Instant::now();
        {
            let states = snapshot_state
                .state_instances
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            let occurrences = snapshot_state
                .occurrences
                .get_mut(&uri)
                .ok_or(LexInterrupt::MissingState)?;
            debug_assert_eq!(states.len(), restart_state_pos + 1);
            debug_assert_eq!(occurrences.len(), restart_token_pos);

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
            old_occurrences.len().saturating_sub(old_suffix_start_index),
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
        start_state: LexerState<Root>,
        input_str: String,
        mut cont: impl FnMut(usize, &LexerState<Root>, usize, usize) -> bool,
    ) -> Result<LexerState<Root>, LexInterrupt> {
        let mut input = Input::new(input_str.as_bytes());
        let mut state = start_state;
        let mut unexpected_start: Option<usize> = None;

        while state.offset < input.end() {
            match self.select_step(&state, &mut input, LexMoment::Normal)? {
                Some(step) => {
                    let start = step.start;
                    let end = step.end;
                    if let Some(start) = unexpected_start.take() {
                        if !self.emit_state_error(
                            &state,
                            LexErrorInfo {
                                kind: LexErrorKind::UnexpectedInput,
                                start,
                                end: state.offset,
                            },
                            false,
                            &mut cont,
                        )? {
                            return Ok(state);
                        }
                    }

                    if let Some(token_id) = self.commit_match(&mut state, step)? {
                        if !cont(token_id, &state, start, end) {
                            return Ok(state);
                        }
                    }
                }
                None => {
                    unexpected_start.get_or_insert(state.offset);
                    state.offset += 1;
                }
            }
        }

        if let Some(start) = unexpected_start.take() {
            if !self.emit_state_error(
                &state,
                LexErrorInfo {
                    kind: LexErrorKind::UnexpectedInput,
                    start,
                    end: state.offset,
                },
                false,
                &mut cont,
            )? {
                return Ok(state);
            }
        }

        while let Some(step) = self.select_step(&state, &mut input, LexMoment::Eof)? {
            let start = step.start;
            let end = step.end;
            if let Some(token_id) = self.commit_match(&mut state, step)? {
                if !cont(token_id, &state, start, end) {
                    return Ok(state);
                }
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
