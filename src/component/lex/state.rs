//! The lexer owns compiled grammar state, token arenas, and versioned snapshots.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;

use crate::{
    component::{
        parse::{
            TokenData,
            grammar::TerminalId,
            identity::{eof_fingerprint, error_fingerprint, token_fingerprint},
        },
        source::{SourceDelta, SourceSplice},
    },
    scheme::change::AddressChange,
    utils::{PrettyDisplay, Span},
};

/// The lexer result for one committed source revision. `changes` is the exact
/// sparse token delta between the adjacent token revisions.
pub(crate) struct LexDocument {
    pub tokens: Vec<TokenData>,
    pub changes: Vec<AddressChange<Uri<&'static str>, TokenData>>,
}

fn validate_source_delta(
    previous: &str,
    next: &str,
    splices: &[SourceSplice],
) -> Result<(), LexInterrupt> {
    let mut old_cursor = 0;
    let mut new_cursor = 0;
    for splice in splices {
        if splice.old_range.start < old_cursor
            || splice.new_range.start < new_cursor
            || splice.old_range.start > splice.old_range.end
            || splice.new_range.start > splice.new_range.end
            || splice.old_range.end > previous.len()
            || splice.new_range.end > next.len()
            || !previous.is_char_boundary(splice.old_range.start)
            || !previous.is_char_boundary(splice.old_range.end)
            || !next.is_char_boundary(splice.new_range.start)
            || !next.is_char_boundary(splice.new_range.end)
            || previous[splice.old_range.clone()] != *splice.removed
            || next[splice.new_range.clone()] != *splice.inserted
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

fn apply_evolving_splice(snapshot: &mut String, splice: &SourceSplice) -> Result<(), LexInterrupt> {
    if splice.old_range.end > snapshot.len()
        || !snapshot.is_char_boundary(splice.old_range.start)
        || !snapshot.is_char_boundary(splice.old_range.end)
        || snapshot[splice.old_range.clone()] != *splice.removed
    {
        return Err(LexInterrupt::InternalError(
            "source delta no longer matches the lexer snapshot".to_string(),
        ));
    }
    snapshot.replace_range(splice.old_range.clone(), &splice.inserted);
    Ok(())
}

fn translate_offset(offset: usize, shift: isize) -> Result<usize, LexInterrupt> {
    offset.checked_add_signed(shift).ok_or_else(|| {
        LexInterrupt::InternalError("source delta coordinate translation overflowed".to_string())
    })
}

use super::{
    __macro_private::{BuildErrorToken, TokenMatcher},
    IncrementalLexStats, LexErrorInfo, LexInterrupt, LexToken, LexerCreationError, LexerRoot,
    ResolvedToken, TokenState, build,
    mode::{LexerState, State, StateInfo},
    token::{CompiledState, SYNTHETIC_EOF_ID, StateMatcher, TokenOccurrence},
};

impl<Root: LexerRoot + fmt::Display> PrettyDisplay<Lexer<Root>> for LexToken<Root> {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        _context: &Lexer<Root>,
    ) -> core::fmt::Result {
        use color_print::cwrite;
        if self.error.is_some() {
            cwrite!(
                f,
                "<dim>[{}]\t</dim><red>Error</red>: {}",
                self.length,
                self.value
            )
        } else {
            cwrite!(
                f,
                "<dim>[{}]\t</dim><green>Token</green>: {}",
                self.length,
                self.value
            )
        }
    }
}

impl<Root: LexerRoot + fmt::Display> PrettyDisplay<Lexer<Root>> for usize {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        context: &Lexer<Root>,
    ) -> core::fmt::Result {
        use color_print::cwrite;
        match context.token(*self) {
            Some(token) if token.error.is_some() => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><red>Error</red>: {}",
                    token.length,
                    token.value
                )
            }
            Some(token) => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><green>Token</green>: {}",
                    token.length,
                    token.value
                )
            }
            None => cwrite!(f, "<red>Missing token {}</red>", self),
        }
    }
}

#[derive(Debug)]
pub struct LexerSnapshotState<Root>
where
    Root: LexerRoot,
{
    pub(super) state_instances: HashMap<Uri<&'static str>, Arc<Vec<LexerState<Root>>>>,
    pub(super) occurrences: HashMap<Uri<&'static str>, Arc<Vec<TokenOccurrence>>>,
    pub(super) next_occurrence: HashMap<Uri<&'static str>, usize>,
    pub(super) sources: HashMap<Uri<&'static str>, Arc<str>>,
    pub(super) incremental_stats: HashMap<Uri<&'static str>, IncrementalLexStats>,
    _root: PhantomData<Root>,
}

impl<Root> Clone for LexerSnapshotState<Root>
where
    Root: LexerRoot,
{
    fn clone(&self) -> Self {
        Self {
            state_instances: self.state_instances.clone(),
            occurrences: self.occurrences.clone(),
            next_occurrence: self.next_occurrence.clone(),
            sources: self.sources.clone(),
            incremental_stats: self.incremental_stats.clone(),
            _root: PhantomData,
        }
    }
}

impl<Root> Default for LexerSnapshotState<Root>
where
    Root: LexerRoot,
{
    fn default() -> Self {
        Self {
            state_instances: HashMap::new(),
            occurrences: HashMap::new(),
            next_occurrence: HashMap::new(),
            sources: HashMap::new(),
            incremental_stats: HashMap::new(),
            _root: PhantomData,
        }
    }
}

#[derive(Clone)]
pub struct Lexer<Root>
where
    Root: LexerRoot,
{
    compiled_states: Vec<CompiledState<Root>>,
    state_ids: HashMap<String, State<Root>>,
    skip_terminals: HashSet<TerminalId>,
    latest: Arc<LexerSnapshotState<Root>>,
    pub(super) arena: Vec<LexToken<Root>>,
}

impl<Root: LexerRoot> Lexer<Root> {
    fn materialize_token(&self, occurrence: TokenOccurrence) -> Option<LexToken<Root>>
    where
        Root: Clone,
    {
        let token = self.arena.get(occurrence.id)?.clone();
        Some(LexToken {
            start: occurrence.start,
            length: occurrence.end.saturating_sub(occurrence.start),
            ..token
        })
    }

    pub(super) fn token_data_from_occurrences(
        &self,
        occurrences: &[TokenOccurrence],
        span: Option<Span>,
    ) -> Vec<TokenData> {
        let mut out = Vec::new();
        for occurrence in occurrences {
            let Some(token) = self.arena.get(occurrence.id) else {
                continue;
            };
            if let Some(span) = span
                && (occurrence.start >= span.range.end() || occurrence.end <= span.range.start())
            {
                continue;
            }
            if let Some(error) = token.error {
                out.push(TokenData {
                    id: token.id,
                    terminal: None,
                    start: occurrence.start,
                    length: occurrence.end.saturating_sub(occurrence.start),
                    column: occurrence.column,
                    fingerprint: error_fingerprint(
                        &error,
                        occurrence.end.saturating_sub(occurrence.start),
                    ),
                });
                continue;
            }
            let Some(terminal) = token.terminal else {
                continue;
            };
            if self.is_skip_terminal(terminal) {
                continue;
            }
            out.push(TokenData {
                id: token.id,
                terminal: Some(terminal),
                start: occurrence.start,
                length: occurrence.end.saturating_sub(occurrence.start),
                column: occurrence.column,
                fingerprint: token_fingerprint(
                    Some(terminal),
                    &token.value,
                    occurrence.end.saturating_sub(occurrence.start),
                ),
            });
        }

        let eof_start = occurrences
            .last()
            .map(|occurrence| occurrence.end)
            .unwrap_or(0);
        let include_eof = span
            .is_none_or(|span| eof_start >= span.range.start() && eof_start <= span.range.end());
        if include_eof {
            out.push(TokenData {
                id: SYNTHETIC_EOF_ID,
                terminal: None,
                start: eof_start,
                length: 0,
                column: SYNTHETIC_EOF_ID,
                fingerprint: eof_fingerprint(),
            });
        }
        out
    }

    pub(super) fn token_data_for_uri(
        &self,
        state: &LexerSnapshotState<Root>,
        uri: Uri<&'static str>,
    ) -> Vec<TokenData> {
        let Some(occurrences) = state.occurrences.get(&uri) else {
            return Vec::new();
        };
        self.token_data_from_occurrences(occurrences, None)
    }

    pub(super) fn token_data_semantically_equal(&self, a: &TokenData, b: &TokenData) -> bool {
        if a.id == SYNTHETIC_EOF_ID || b.id == SYNTHETIC_EOF_ID {
            return a.id == b.id;
        }
        match (self.arena.get(a.id), self.arena.get(b.id)) {
            (Some(a_token), Some(b_token)) => {
                a.terminal == b.terminal
                    && a.length == b.length
                    && a_token.error == b_token.error
                    && a_token.value == b_token.value
            }
            _ => false,
        }
    }

    pub fn new() -> Result<Self, LexerCreationError> {
        let registrations = Root::state_registrations();
        let state_ids = registrations
            .iter()
            .enumerate()
            .map(|(index, registration)| (registration.type_name.clone(), State::new(index)))
            .collect::<HashMap<_, _>>();
        let mut compiled_states = Vec::with_capacity(registrations.len());
        let mut skip_terminals = HashSet::new();
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
                if resolved.skip {
                    skip_terminals.insert(resolved.terminal);
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
            skip_terminals,
            arena: Vec::new(),
            latest: Arc::new(LexerSnapshotState::default()),
        })
    }

    /// Incrementally materializes one document from its authoritative ordered
    /// source edits. Each splice reuses lexer checkpoints before its own start;
    /// multiple distant edits therefore retain their independent unchanged
    /// islands instead of becoming one broad source replacement.
    pub(crate) fn derive_document(
        &mut self,
        uri: Uri<&'static str>,
        source: Arc<str>,
        delta: &SourceDelta,
    ) -> Result<LexDocument, LexInterrupt> {
        let mut next = (*self.latest).clone();
        let initializing = !next.sources.contains_key(&uri);
        let previous = next
            .sources
            .get(&uri)
            .cloned()
            .unwrap_or_else(|| Arc::from(""));
        if previous.as_ref() == source.as_ref() {
            return Ok(LexDocument {
                tokens: self.token_data_for_uri(&next, uri),
                changes: Vec::new(),
            });
        }

        let initial_splice = SourceSplice {
            old_range: 0..0,
            new_range: 0..source.len(),
            removed: Arc::from(""),
            inserted: Arc::clone(&source),
        };
        let splices = if initializing {
            std::slice::from_ref(&initial_splice)
        } else {
            delta.splices.as_ref()
        };
        validate_source_delta(previous.as_ref(), source.as_ref(), splices)?;
        let mut snapshot = previous.to_string();
        let mut cumulative_shift = 0isize;
        let mut changes = Vec::with_capacity(splices.len());
        for splice in splices {
            let start = translate_offset(splice.old_range.start, cumulative_shift)?;
            let end = translate_offset(splice.old_range.end, cumulative_shift)?;
            let evolving = SourceSplice {
                old_range: start..end,
                new_range: start..start + splice.inserted.len(),
                removed: Arc::clone(&splice.removed),
                inserted: Arc::clone(&splice.inserted),
            };
            apply_evolving_splice(&mut snapshot, &evolving)?;
            if let Some(change) = self.lex_uri(
                &mut next,
                uri,
                snapshot.clone(),
                std::slice::from_ref(&evolving),
            )? {
                changes.push(change);
            }
            let inserted = isize::try_from(splice.inserted.len()).map_err(|_| {
                LexInterrupt::InternalError("source insertion length overflows isize".to_string())
            })?;
            let removed = isize::try_from(splice.removed.len()).map_err(|_| {
                LexInterrupt::InternalError("source removal length overflows isize".to_string())
            })?;
            cumulative_shift = cumulative_shift
                .checked_add(inserted - removed)
                .ok_or_else(|| {
                    LexInterrupt::InternalError(
                        "source delta cumulative shift overflows".to_string(),
                    )
                })?;
        }
        if snapshot != source.as_ref() {
            return Err(LexInterrupt::InternalError(
                "source delta did not produce the observed document text".to_string(),
            ));
        }
        let tokens = self.token_data_for_uri(&next, uri);
        self.latest = Arc::new(next);
        Ok(LexDocument { tokens, changes })
    }

    pub fn incremental_stats(&self, uri: Uri<&'static str>) -> Option<IncrementalLexStats> {
        self.latest.incremental_stats.get(&uri).copied()
    }

    pub(crate) fn forget_document(&mut self, uri: Uri<&'static str>) {
        let latest = Arc::make_mut(&mut self.latest);
        latest.state_instances.remove(&uri);
        latest.occurrences.remove(&uri);
        latest.next_occurrence.remove(&uri);
        latest.sources.remove(&uri);
        latest.incremental_stats.remove(&uri);
    }

    pub(crate) fn reset_documents(&mut self) {
        self.latest = Arc::new(LexerSnapshotState::default());
        self.arena.clear();
    }

    pub fn state_info(&self) -> impl ExactSizeIterator<Item = &StateInfo> {
        self.compiled_states.iter().map(|state| &state.info)
    }

    pub fn resolved_tokens(&self) -> impl ExactSizeIterator<Item = &[ResolvedToken<Root>]> {
        self.compiled_states
            .iter()
            .map(|state| state.tokens.as_slice())
    }

    pub fn tokens_in_state(&self, state: State<Root>) -> Option<&[ResolvedToken<Root>]> {
        self.compiled_states
            .get(state.id)
            .map(|state| state.tokens.as_slice())
    }

    pub(crate) fn state_matcher(&self, state: State<Root>) -> Option<&StateMatcher> {
        self.compiled_states
            .get(state.id)
            .map(|state| &state.matcher)
    }

    pub fn alloc_token(
        &mut self,
        start: usize,
        length: usize,
        terminal: Option<TerminalId>,
        error: Option<LexErrorInfo>,
        value: Root,
    ) -> usize {
        let id = self.arena.len();
        self.arena.push(LexToken {
            id,
            start,
            length,
            terminal,
            error,
            value,
        });
        id
    }

    pub fn token(&self, index: usize) -> Option<&LexToken<Root>> {
        self.arena.get(index)
    }

    pub fn terminal_of(&self, index: usize) -> Option<TerminalId> {
        self.token(index)
            .and_then(|token| token.error.is_none().then_some(token.terminal).flatten())
    }

    pub fn token_span(&self, id: usize) -> Option<Span> {
        for (&uri, occurrences) in &self.latest.occurrences {
            for occurrence in occurrences.iter() {
                if occurrence.id == id {
                    return Span::new_uri(uri, occurrence.start, occurrence.end).ok();
                }
            }
        }
        None
    }

    pub fn tokens_in_span(&self, span: Span) -> Vec<LexToken<Root>>
    where
        Root: Clone,
    {
        let Some(occurrences) = self.latest.occurrences.get(&span.uri) else {
            return Vec::new();
        };
        occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.start < span.range.end() && occurrence.end > span.range.start()
            })
            .filter_map(|occurrence| self.materialize_token(*occurrence))
            .collect()
    }

    pub fn token_data_in_span(&self, span: Span) -> Vec<crate::component::parse::TokenData> {
        let Some(occurrences) = self.latest.occurrences.get(&span.uri) else {
            return Vec::new();
        };
        self.token_data_from_occurrences(occurrences, Some(span))
    }

    fn is_skip_terminal(&self, terminal: TerminalId) -> bool {
        self.skip_terminals.contains(&terminal)
    }

    pub fn state_id_of<S: TokenState>(&self) -> Option<State<Root>> {
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
