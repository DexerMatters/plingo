mod build;
mod lexing;
mod mode;

#[doc(hidden)]
pub mod __macro_private;
pub mod interface;

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    hash::Hash,
    marker::PhantomData,
    str::FromStr,
};

use fluent_uri::Uri;
use plingo_macros::layer;
use regex_automata::dfa::dense::DFA;
use regex_syntax::hir::HirKind;
use thiserror::Error;

pub use mode::{LexerState, State, StateAction, StateInfo};

use crate::{
    component::parse::identity::{eof_fingerprint, error_fingerprint, token_fingerprint},
    component::parse::{TokenData, grammar::TerminalId},
    component::source::TextChange,
    scheme::{
        change::{LayerChange, LayerChanges, ReplacementBatch, ReplacementChange},
        context::{Context, SnapshotId},
        error::ActionError,
        layer::{MiddleLayer, NonTopLayer, SnapshotLayer},
    },
    utils::{PrettyDisplay, Span},
};

use self::__macro_private::{
    BuildErrorToken, BuildToken, ScopeRegistration, TokenMatcher, WithHook,
};

pub trait TokenState: Send + Sync + 'static {
    fn display_name() -> &'static str;
    fn state_key() -> &'static str;
}

pub trait LexerRoot: TokenState + Hash + Eq + PartialEq + Sized + Send + Sync + 'static {
    type SlotValue: Clone + Eq + Hash + Send + Sync + 'static;

    fn state_registrations() -> Vec<ScopeRegistration<Self>>;
    fn slot_count() -> usize;
    fn recover_key(slots: &SlotStore<Self>) -> Option<&str>;
}

pub trait FromLexeme: Sized {
    type Error: Error + Send + Sync + 'static;

    fn from_lexeme(lexeme: &str) -> Result<Self, Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LexMoment {
    Normal,
    Eof,
}

pub struct Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
    index: usize,
    pack: fn(T) -> Root::SlotValue,
    as_ref: for<'a> fn(&'a Root::SlotValue) -> Option<&'a T>,
    _root: PhantomData<fn() -> Root>,
}

impl<Root, T> Copy for Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
}

impl<Root, T> Clone for Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<Root, T> Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
    pub const fn new(
        index: usize,
        pack: fn(T) -> Root::SlotValue,
        as_ref: for<'a> fn(&'a Root::SlotValue) -> Option<&'a T>,
    ) -> Self {
        Self {
            index,
            pack,
            as_ref,
            _root: PhantomData,
        }
    }

    pub const fn index(self) -> usize {
        self.index
    }
}

pub struct SlotStore<Root>
where
    Root: LexerRoot,
{
    values: Vec<Option<Root::SlotValue>>,
}

impl<Root> Clone for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
        }
    }
}

impl<Root> PartialEq for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<Root> Eq for SlotStore<Root> where Root: LexerRoot {}

impl<Root> Hash for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.values.hash(state);
    }
}

impl<Root> fmt::Debug for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotStore")
            .field("len", &self.values.len())
            .finish()
    }
}

impl<Root> Default for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn default() -> Self {
        Self {
            values: (0..Root::slot_count()).map(|_| None).collect(),
        }
    }
}

impl<Root> SlotStore<Root>
where
    Root: LexerRoot,
{
    pub fn get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.values
            .get(slot.index)
            .and_then(|value| value.as_ref())
            .and_then(|value| (slot.as_ref)(value))
    }

    pub fn set<T>(&mut self, slot: Slot<Root, T>, value: T)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        if let Some(entry) = self.values.get_mut(slot.index) {
            *entry = Some((slot.pack)(value));
        }
    }

    pub fn remove<T>(&mut self, slot: Slot<Root, T>)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        if let Some(entry) = self.values.get_mut(slot.index) {
            *entry = None;
        }
    }
}

pub struct WhenCx<'a, Root>
where
    Root: LexerRoot,
{
    lexeme: &'a str,
    moment: LexMoment,
    depth: usize,
    current: &'a SlotStore<Root>,
    parent: Option<&'a SlotStore<Root>>,
}

impl<'a, Root> WhenCx<'a, Root>
where
    Root: LexerRoot,
{
    pub fn new(
        lexeme: &'a str,
        moment: LexMoment,
        depth: usize,
        current: &'a SlotStore<Root>,
        parent: Option<&'a SlotStore<Root>>,
    ) -> Self {
        Self {
            lexeme,
            moment,
            depth,
            current,
            parent,
        }
    }

    pub fn lexeme(&self) -> &str {
        self.lexeme
    }

    pub fn moment(&self) -> LexMoment {
        self.moment
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.current.get(slot)
    }

    pub fn parent_get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.parent.and_then(|parent| parent.get(slot))
    }
}

pub struct WithCx<'a, Root>
where
    Root: LexerRoot,
{
    lexeme: &'a str,
    moment: LexMoment,
    depth: usize,
    target: &'a mut SlotStore<Root>,
    source: SlotStore<Root>,
    parent: Option<SlotStore<Root>>,
}

impl<'a, Root> WithCx<'a, Root>
where
    Root: LexerRoot,
{
    pub fn new(
        lexeme: &'a str,
        moment: LexMoment,
        depth: usize,
        target: &'a mut SlotStore<Root>,
        source: SlotStore<Root>,
        parent: Option<SlotStore<Root>>,
    ) -> Self {
        Self {
            lexeme,
            moment,
            depth,
            target,
            source,
            parent,
        }
    }

    pub fn lexeme(&self) -> &str {
        self.lexeme
    }

    pub fn moment(&self) -> LexMoment {
        self.moment
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.target.get(slot)
    }

    pub fn set<T>(&mut self, slot: Slot<Root, T>, value: T)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.target.set(slot, value);
    }

    pub fn remove<T>(&mut self, slot: Slot<Root, T>)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.target.remove(slot);
    }

    pub fn source_get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.source.get(slot)
    }

    pub fn parent_get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.parent.as_ref().and_then(|parent| parent.get(slot))
    }
}

#[derive(Clone)]
pub struct ResolvedToken<Root>
where
    Root: LexerRoot,
{
    pub terminal: TerminalId,
    pub precedence: usize,
    pub label: &'static str,
    pub empty: bool,
    pub action: TokenAction<Root>,
    pub skip: bool,
    pub(crate) build: BuildToken<Root>,
    pub(crate) minimum_length: usize,
    pub(crate) maximum_length: usize,
    pub(crate) when: Option<self::__macro_private::WhenGuard<Root>>,
    pub(crate) recover_when: Option<self::__macro_private::RecoverWhen>,
    pub(crate) with_hook: Option<WithHook<Root>>,
}

impl<Root> fmt::Debug for ResolvedToken<Root>
where
    Root: LexerRoot,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedToken")
            .field("precedence", &self.precedence)
            .field("label", &self.label)
            .field("empty", &self.empty)
            .field("action", &self.action)
            .field("skip", &self.skip)
            .field("minimum_length", &self.minimum_length)
            .field("maximum_length", &self.maximum_length)
            .finish_non_exhaustive()
    }
}

impl<Root> ResolvedToken<Root>
where
    Root: LexerRoot,
{
    pub fn minimum_length(&self) -> usize {
        self.minimum_length
    }

    pub fn maximum_length(&self) -> usize {
        self.maximum_length
    }

    pub fn build(&self, lexeme: &str) -> Result<Root, LexInterrupt> {
        (self.build)(lexeme)
    }
}

#[derive(Clone)]
pub enum TokenAction<Root>
where
    Root: LexerRoot,
{
    None,
    Enter { next: State<Root> },
    Exit,
}

impl<Root> fmt::Debug for TokenAction<Root>
where
    Root: LexerRoot,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Enter { next, .. } => f.debug_struct("Enter").field("next", next).finish(),
            Self::Exit => f.write_str("Exit"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LexToken<Root>
where
    Root: LexerRoot,
{
    pub id: usize,
    pub start: usize,
    pub length: usize,
    pub terminal: Option<TerminalId>,
    pub error: Option<LexErrorInfo>,
    pub value: Root,
}

#[derive(Debug, Clone)]
pub(crate) struct TokenMatch<Root>
where
    Root: LexerRoot,
{
    pub token_index: usize,
    pub start: usize,
    pub end: usize,
    pub lexeme: String,
    pub moment: LexMoment,
    pub value: Root,
    pub transition: StateAction<Root>,
}

#[derive(Debug, Error, Clone)]
pub enum LexInterrupt {
    #[error("Failed to parse token {token} from lexeme {lexeme:?}: {err}")]
    TokenParseError {
        token: &'static str,
        lexeme: String,
        err: String,
    },
    #[error("Parse error: {0}")]
    ParseError(String, String),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("No candidate")]
    NoCandidate,
    #[error("Missing state")]
    MissingState,
    #[error("Unsupported search")]
    UnsupportedSearch,
    #[error("Dead state")]
    DeadState,
    #[error("Quit state")]
    QuitState,
    #[error("End of input")]
    EndOfInput,

    #[error("Action error: {0}")]
    ActionError(ActionError),
}

impl LexInterrupt {
    pub fn token_parse_failed<E>(token: &'static str, lexeme: &str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::TokenParseError {
            token,
            lexeme: lexeme.to_string(),
            err: source.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LexErrorKind {
    UnexpectedInput,
    RequiredBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LexErrorInfo {
    pub kind: LexErrorKind,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Error)]
pub enum GenerateError {
    #[error("token generator for {token} could not compile the regex: {err}")]
    RegexCompile { token: &'static str, err: String },
    #[error("token generator for {token} could not produce an accepted sample")]
    NoAcceptedSample { token: &'static str },
    #[error("generate! does not know variant {state}::{variant}")]
    UnknownVariant {
        state: &'static str,
        variant: &'static str,
    },
    #[error("generate! does not support #[when(...)] variant {token}")]
    UnsupportedWhenVariant { token: &'static str },
    #[error("generate! does not support #[empty] variant {token}")]
    UnsupportedEmptyVariant { token: &'static str },
    #[error("failed to write generated token")]
    Write(#[source] fmt::Error),
}

#[derive(Debug)]
pub(crate) struct StateMatcher {
    pub(crate) dfa: DFA<Vec<u32>>,
    pub(crate) token_index_by_pattern: Vec<usize>,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TokenOccurrence {
    pub id: usize,
    pub start: usize,
    pub end: usize,
}

pub(crate) struct CompiledState<Root>
where
    Root: LexerRoot,
{
    pub(crate) info: StateInfo,
    pub(crate) matcher: StateMatcher,
    pub(crate) tokens: Vec<ResolvedToken<Root>>,
    pub(crate) recovery_error: BuildErrorToken<Root>,
    pub(crate) boundary_error: BuildErrorToken<Root>,
}

pub type TokenBatch = ReplacementBatch<TokenData>;
pub type TokenChange = ReplacementChange<Uri<&'static str>, TokenData>;

const SYNTHETIC_EOF_ID: usize = usize::MAX;

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
    state_instances: HashMap<Uri<&'static str>, Vec<LexerState<Root>>>,
    occurrences: HashMap<Uri<&'static str>, Vec<TokenOccurrence>>,
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

    #[snapshot]
    latest: LexerSnapshotState<Root>,
    arena: Vec<LexToken<Root>>,
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

    fn token_data_from_occurrences(
        &self,
        occurrences: &[TokenOccurrence],
        span: Option<Span>,
    ) -> Vec<TokenData> {
        let mut out = Vec::new();
        for (column, occurrence) in occurrences.iter().enumerate() {
            let Some(token) = self.arena.get(occurrence.id) else {
                continue;
            };
            if let Some(span) = span {
                if occurrence.start >= span.range.end() || occurrence.end <= span.range.start() {
                    continue;
                }
            }
            if let Some(error) = token.error {
                out.push(TokenData {
                    id: token.id,
                    terminal: None,
                    start: occurrence.start,
                    length: occurrence.end.saturating_sub(occurrence.start),
                    column,
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
                column,
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
                column: occurrences.len(),
                fingerprint: eof_fingerprint(),
            });
        }
        out
    }

    fn token_data_for_uri(
        &self,
        state: &LexerSnapshotState<Root>,
        uri: Uri<&'static str>,
    ) -> Vec<TokenData> {
        let Some(occurrences) = state.occurrences.get(&uri) else {
            return Vec::new();
        };
        self.token_data_from_occurrences(occurrences, None)
    }

    fn token_data_semantically_equal(a: &TokenData, b: &TokenData) -> bool {
        a.terminal == b.terminal && a.length == b.length && a.fingerprint == b.fingerprint
    }

    fn build_visible_batch(old_tokens: Vec<TokenData>, new_tokens: Vec<TokenData>) -> TokenBatch {
        let mut prefix_len = 0usize;
        while prefix_len < old_tokens.len()
            && prefix_len < new_tokens.len()
            && Self::token_data_semantically_equal(&old_tokens[prefix_len], &new_tokens[prefix_len])
        {
            prefix_len += 1;
        }

        let mut suffix_len = 0usize;
        while suffix_len < old_tokens.len().saturating_sub(prefix_len)
            && suffix_len < new_tokens.len().saturating_sub(prefix_len)
        {
            let old_idx = old_tokens.len() - suffix_len - 1;
            let new_idx = new_tokens.len() - suffix_len - 1;
            if !Self::token_data_semantically_equal(&old_tokens[old_idx], &new_tokens[new_idx]) {
                break;
            }
            suffix_len += 1;
        }

        TokenBatch {
            old_changed_range: prefix_len..old_tokens.len().saturating_sub(suffix_len),
            new_changed_range: prefix_len..new_tokens.len().saturating_sub(suffix_len),
            old_units: old_tokens,
            new_units: new_tokens,
            prefix_len,
            suffix_len,
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
            latest: LexerSnapshotState::default(),
            _lower: PhantomData,
            _snapshot: HashMap::new(),
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

    pub(crate) fn snapshot_state(&self, snapshot: Option<SnapshotId>) -> &LexerSnapshotState<Root> {
        self.state(snapshot).unwrap_or_else(|| self.latest_state())
    }

    pub(crate) fn token_span(&self, snapshot: Option<SnapshotId>, id: usize) -> Option<Span> {
        let state = self.snapshot_state(snapshot);
        for (&uri, occurrences) in &state.occurrences {
            for occurrence in occurrences {
                if occurrence.id == id {
                    return Span::new_uri(uri, occurrence.start, occurrence.end).ok();
                }
            }
        }
        None
    }

    pub(crate) fn tokens_in_span_snapshot(
        &self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Vec<LexToken<Root>>
    where
        Root: Clone,
    {
        let state = self.snapshot_state(snapshot);
        let Some(occurrences) = state.occurrences.get(&span.uri) else {
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

    pub(crate) fn token_data_in_span(
        &self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Vec<crate::component::parse::TokenData> {
        let state = self.snapshot_state(snapshot);
        let Some(occurrences) = state.occurrences.get(&span.uri) else {
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

#[layer(middle)]
impl<Root, Lower> MiddleLayer for Lexer<Root, Lower>
where
    Root: LexerRoot,
    Lower: NonTopLayer<Change = TokenChange> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Error = LexInterrupt;
    type Change = TextChange;

    fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move {
            let mut working = self.latest.clone();
            let mut lower_changes = Vec::new();

            let mut grouped: Vec<(Uri<&'static str>, Vec<TextChange>)> = Vec::new();
            for change in changes {
                let uri = *change.address();
                if let Some((_, batch)) =
                    grouped.iter_mut().find(|(group_uri, _)| *group_uri == uri)
                {
                    batch.push(change);
                } else {
                    grouped.push((uri, vec![change]));
                }
            }

            for (_, batch) in grouped {
                let uri_lower_changes = self.lex_changes(ctx, &mut working, &batch).await?;
                lower_changes.extend(uri_lower_changes);
            }

            self.latest = working.clone();
            if let Some(snapshot) = ctx.snapshot() {
                self.push_state(snapshot);
            }
            Ok(lower_changes)
        }
    }
}

#[derive(Debug, Error)]
pub enum LexerCreationError {
    #[error("Error occurred while parsing regex pattern {0} for token {1}: {2}")]
    RegexParsingError(String, String, regex_syntax::Error),
    #[error("Failed to build grouped regex matcher for state {state}: {source}")]
    RegexMatcherBuildError {
        state: String,
        #[source]
        source: regex_automata::dfa::dense::BuildError,
    },
    #[error("Regex pattern {1} for token {0} contains unsupported feature: {2:?}")]
    UnsupportedRegexFeature(String, String, HirKind),
    #[error("Token {0} with pattern {1} cannot be matched by any input string")]
    ImpossibleToken(String, String),
    #[error("Token {0} with pattern {1} can match the empty string, which is unsupported")]
    EmptyMatchToken(String, String),
    #[error("State {0} is referenced but not registered")]
    UnknownState(String),
}

#[derive(Debug)]
pub struct UnsupportedDefaultParseError {
    ty: &'static str,
}

impl UnsupportedDefaultParseError {
    pub fn new<T>() -> Self {
        Self {
            ty: std::any::type_name::<T>(),
        }
    }
}

impl fmt::Display for UnsupportedDefaultParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "type {} does not support default lexeme parsing",
            self.ty
        )
    }
}

impl Error for UnsupportedDefaultParseError {}

impl FromLexeme for String {
    type Error = std::convert::Infallible;

    fn from_lexeme(lexeme: &str) -> Result<Self, Self::Error> {
        Ok(lexeme.to_string())
    }
}

impl FromLexeme for Box<str> {
    type Error = std::convert::Infallible;

    fn from_lexeme(lexeme: &str) -> Result<Self, Self::Error> {
        Ok(lexeme.into())
    }
}

macro_rules! impl_from_lexeme_via_parse {
    ($($ty:ty),* $(,)?) => {
        $(
            impl FromLexeme for $ty {
                type Error = <Self as FromStr>::Err;

                fn from_lexeme(lexeme: &str) -> Result<Self, Self::Error> {
                    lexeme.parse()
                }
            }
        )*
    };
}

impl_from_lexeme_via_parse!(
    bool, i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64,
);
