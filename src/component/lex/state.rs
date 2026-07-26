//! The lexer owns compiled grammar state, token arenas, and versioned snapshots.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;
use plingo_macros::layer;

use crate::{
    component::{
        parse::{
            TokenData,
            grammar::TerminalId,
            identity::{eof_fingerprint, error_fingerprint, token_fingerprint},
        },
        source::TextChunk,
    },
    scheme::{
        change::{ChangeSet, LayerChanges},
        context::{Context, SnapshotId},
        layer::{MiddleLayer, NonTopLayer, SnapshotLayer},
    },
    utils::{PrettyDisplay, Span},
};

use super::{
    __macro_private::{BuildErrorToken, TokenMatcher},
    IncrementalLexStats, LexErrorInfo, LexInterrupt, LexToken, LexerConfig, LexerCreationError,
    LexerRoot, ResolvedToken, TokenState, build,
    mode::{LexerState, State, StateInfo},
    token::{CompiledState, SYNTHETIC_EOF_ID, StateMatcher, TokenOccurrence},
};

impl<Root: LexerRoot + fmt::Display, Lower> PrettyDisplay<Lexer<Root, Lower>> for LexToken<Root> {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        _context: &Lexer<Root, Lower>,
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

impl<Root: LexerRoot + fmt::Display, Lower> PrettyDisplay<Lexer<Root, Lower>> for usize {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        context: &Lexer<Root, Lower>,
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

#[layer]
pub struct Lexer<Root, Lower = ()>
where
    Root: LexerRoot,
{
    compiled_states: Vec<CompiledState<Root>>,
    state_ids: HashMap<String, State<Root>>,
    skip_terminals: HashSet<TerminalId>,
    pub config: LexerConfig,

    #[snapshot]
    latest: Arc<LexerSnapshotState<Root>>,
    pub(super) arena: Vec<LexToken<Root>>,
    _lower: PhantomData<fn() -> Lower>,
}

impl<Root: LexerRoot, Lower> Lexer<Root, Lower> {
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
                && (occurrence.start >= span.range.end() || occurrence.end <= span.range.start()) {
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
            config: LexerConfig::default(),
            arena: Vec::new(),
            latest: Arc::new(LexerSnapshotState::default()),
            _lower: PhantomData,
            _snapshot: Default::default(),
        })
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

    pub(crate) fn snapshot_state(
        &self,
        snapshot: Option<SnapshotId>,
    ) -> Result<&LexerSnapshotState<Root>, LexInterrupt> {
        match snapshot {
            Some(snapshot) => self
                .state(Some(snapshot))
                .ok_or(LexInterrupt::MissingSnapshot(snapshot)),
            None => Ok(self.latest_state()),
        }
    }

    pub(crate) fn token_span(
        &self,
        snapshot: Option<SnapshotId>,
        id: usize,
    ) -> Result<Option<Span>, LexInterrupt> {
        let state = self.snapshot_state(snapshot)?;
        for (&uri, occurrences) in &state.occurrences {
            for occurrence in occurrences.iter() {
                if occurrence.id == id {
                    return Ok(Span::new_uri(uri, occurrence.start, occurrence.end).ok());
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn tokens_in_span_snapshot(
        &self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Result<Vec<LexToken<Root>>, LexInterrupt>
    where
        Root: Clone,
    {
        let state = self.snapshot_state(snapshot)?;
        let Some(occurrences) = state.occurrences.get(&span.uri) else {
            return Ok(Vec::new());
        };
        Ok(occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.start < span.range.end() && occurrence.end > span.range.start()
            })
            .filter_map(|occurrence| self.materialize_token(*occurrence))
            .collect())
    }

    pub(crate) fn token_data_in_span(
        &self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Result<Vec<crate::component::parse::TokenData>, LexInterrupt> {
        let state = self.snapshot_state(snapshot)?;
        let Some(occurrences) = state.occurrences.get(&span.uri) else {
            return Ok(Vec::new());
        };
        Ok(self.token_data_from_occurrences(occurrences, Some(span)))
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

#[layer(middle)]
impl<Root, Lower> MiddleLayer for Lexer<Root, Lower>
where
    Root: LexerRoot,
    Lower: NonTopLayer<Address = Uri<&'static str>, Unit = TokenData> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Error = LexInterrupt;
    type Address = Uri<&'static str>;
    type Unit = TextChunk;

    fn pass(
        &mut self,
        _ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move {
            let revision = changes.revision;
            if changes.changes.is_empty() {
                self.push_state(revision.target);
                return Ok(ChangeSet::empty(revision));
            }
            let mut working = (*self.latest).clone();
            let mut lower_changes = Vec::new();
            for change in changes.changes {
                let mut snapshot = working
                    .sources
                    .get(&change.address)
                    .map_or_else(String::new, ToString::to_string);
                for splice in change.splices.iter().rev() {
                    let inserted = splice
                        .inserted
                        .iter()
                        .map(|chunk| chunk.0.as_ref())
                        .collect::<String>();
                    snapshot.replace_range(splice.old_range.clone(), &inserted);
                }
                if let Some(change) =
                    self.lex_uri(&mut working, change.address, snapshot, &change.splices)?
                {
                    lower_changes.push(change);
                }
            }

            self.latest = Arc::new(working);
            self.push_state(revision.target);
            Ok(ChangeSet {
                revision,
                changes: lower_changes,
            })
        }
    }
}
