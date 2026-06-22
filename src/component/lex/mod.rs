mod build;
mod lexing;
mod mode;

#[doc(hidden)]
pub mod __macro_private;
pub mod interface;

use std::{collections::HashMap, error::Error, fmt, hash::Hash, marker::PhantomData, str::FromStr};

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
    BuildErrorToken, BuildLiftedToken, BuildToken, StateBoundary, StateRegistration, TokenSpec,
    WrapLiftedToken,
};

pub trait TokenState: Send + Sync + 'static {
    fn display_name() -> &'static str;
    fn state_key() -> &'static str;
}

pub trait StateTokens: TokenState + Sized {
    fn token_specs() -> Vec<TokenSpec<Self>>;
    fn error_builder() -> BuildErrorToken<Self>;

    fn state_registration() -> StateRegistration<Self> {
        let error_builder = Self::error_builder();
        StateRegistration::new(
            Self::display_name(),
            Self::state_key(),
            Self::token_specs,
            error_builder.clone(),
            error_builder,
            None,
        )
    }
}

pub trait LexerRoot: TokenState + Hash + Eq + PartialEq + Sized + Send + Sync + 'static {
    fn state_registrations() -> Vec<StateRegistration<Self>>;
}

pub trait FromLexeme: Sized {
    type Error: Error + Send + Sync + 'static;

    fn from_lexeme(lexeme: &str) -> Result<Self, Self::Error>;
}

#[derive(Clone)]
pub struct ResolvedToken<Root> {
    pub terminal: TerminalId,
    pub precedence: usize,
    pub label: &'static str,
    pub action: StateAction,
    pub skip: bool,
    pub(crate) build: BuildToken<Root>,
    pub(crate) minimum_length: usize,
    pub(crate) maximum_length: usize,
    pub(crate) captures_context: bool,
    pub(crate) validate: Option<self::__macro_private::ValidateLexeme>,
}

impl<Root> fmt::Debug for ResolvedToken<Root> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedToken")
            .field("precedence", &self.precedence)
            .field("label", &self.label)
            .field("action", &self.action)
            .field("skip", &self.skip)
            .field("minimum_length", &self.minimum_length)
            .field("maximum_length", &self.maximum_length)
            .finish_non_exhaustive()
    }
}

impl<Root> ResolvedToken<Root> {
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
pub struct LexedToken<Root> {
    pub terminal: TerminalId,
    pub value: Root,
}

#[derive(Debug, Clone)]
pub struct BestMatch {
    pub token_index: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct MatchReport {
    pub best: Option<BestMatch>,
    pub stop_offset: usize,
    pub stop_reason: LexInterrupt,
}

impl MatchReport {
    pub fn best_match(&self) -> Option<&BestMatch> {
        self.best.as_ref()
    }

    pub fn has_match(&self) -> bool {
        self.best.is_some()
    }
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
    #[error("generate! does not support #[from(...)] variant {token}")]
    UnsupportedFromVariant { token: &'static str },
    #[error("generate! does not support validated variant {token}")]
    UnsupportedValidatedVariant { token: &'static str },
    #[error("failed to write generated token")]
    Write(#[source] fmt::Error),
}

#[derive(Debug)]
pub(crate) struct StateMatcher {
    pub(crate) dfa: DFA<Vec<u32>>,
    pub(crate) token_index_by_pattern: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Entry<Root>
where
    Root: LexerRoot,
{
    Token {
        length: usize,
        terminal: TerminalId,
        value: Root,
    },
    EOF,
    Error {
        length: usize,
        info: LexErrorInfo,
        value: Root,
    },
}

pub type TokenBatch = ReplacementBatch<TokenData>;
pub type TokenChange = ReplacementChange<Uri<&'static str>, TokenData>;

impl<Root> Entry<Root>
where
    Root: LexerRoot,
{
    pub fn is_token(&self) -> bool {
        matches!(self, Self::Token { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn length(&self) -> usize {
        match self {
            Self::Token { length, .. } | Self::Error { length, .. } => *length,
            Self::EOF => 0,
        }
    }
}

impl<Root: LexerRoot + fmt::Display, Lower> PrettyDisplay<Lexer<Root, Lower>> for Entry<Root> {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        _context: &Lexer<Root, Lower>,
    ) -> core::fmt::Result {
        use color_print::cwrite;
        match self {
            Entry::Token { length, value, .. } => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><green>Token</green>: {}",
                    length,
                    value
                )
            }
            Entry::Error { length, value, .. } => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><red>Error</red>: {}",
                    length,
                    value
                )
            }
            Entry::EOF => cwrite!(f, "<dim>[0]\t</dim><green>EOF</green>"),
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
        match context.get(*self) {
            Entry::Token { length, value, .. } => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><green>Token</green>: {}",
                    length,
                    value
                )
            }
            Entry::Error { length, value, .. } => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><red>Error</red>: {}",
                    length,
                    value
                )
            }
            Entry::EOF => cwrite!(f, "<dim>[0]\t</dim><green>EOF</green>"),
        }
    }
}

#[derive(Debug)]
pub struct LexerSnapshotState<Root>
where
    Root: LexerRoot,
{
    state_instances: HashMap<Uri<&'static str>, Vec<LexerState>>,
    token_instances: HashMap<Uri<&'static str>, Vec<usize>>,
    token_ranges: HashMap<Uri<&'static str>, Vec<(usize, usize)>>,
    _root: PhantomData<Root>,
}

impl<Root> Clone for LexerSnapshotState<Root>
where
    Root: LexerRoot,
{
    fn clone(&self) -> Self {
        Self {
            state_instances: self.state_instances.clone(),
            token_instances: self.token_instances.clone(),
            token_ranges: self.token_ranges.clone(),
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
            token_instances: HashMap::new(),
            token_ranges: HashMap::new(),
            _root: PhantomData,
        }
    }
}

#[layer]
pub struct Lexer<Root, Lower = ()>
where
    Root: LexerRoot,
{
    tokens: Vec<Vec<ResolvedToken<Root>>>,
    state_matchers: Vec<StateMatcher>,
    state_info: Vec<StateInfo>,
    state_boundaries: Vec<Option<StateBoundary>>,
    state_recovery_errors: Vec<BuildErrorToken<Root>>,
    state_boundary_errors: Vec<BuildErrorToken<Root>>,

    #[snapshot]
    latest: LexerSnapshotState<Root>,
    arena: Vec<Entry<Root>>,
    _lower: PhantomData<fn() -> Lower>,
}

impl<Root: LexerRoot, Lower> Lexer<Root, Lower> {
    fn token_data_from_instances(
        &self,
        token_ids: &[usize],
        token_ranges: &[(usize, usize)],
    ) -> Vec<TokenData> {
        let mut out = Vec::new();
        for (column, (&id, &(start, _end))) in token_ids.iter().zip(token_ranges.iter()).enumerate()
        {
            let Some(entry) = self.arena.get(id) else {
                continue;
            };
            let data = match entry {
                Entry::Token {
                    length,
                    terminal,
                    value,
                } if !self.is_skip_terminal(*terminal) => Some(TokenData {
                    id,
                    terminal: Some(*terminal),
                    start,
                    length: *length,
                    column,
                    fingerprint: token_fingerprint(Some(*terminal), value, *length),
                }),
                Entry::EOF => Some(TokenData {
                    id,
                    terminal: None,
                    start,
                    length: 0,
                    column,
                    fingerprint: eof_fingerprint(),
                }),
                Entry::Error { length, info, .. } => Some(TokenData {
                    id,
                    terminal: None,
                    start,
                    length: *length,
                    column,
                    fingerprint: error_fingerprint(info, *length),
                }),
                _ => None,
            };
            if let Some(data) = data {
                out.push(data);
            }
        }
        out
    }

    fn token_data_for_uri(
        &self,
        state: &LexerSnapshotState<Root>,
        uri: Uri<&'static str>,
    ) -> Vec<TokenData> {
        let Some(token_ids) = state.token_instances.get(&uri) else {
            return Vec::new();
        };
        let Some(token_ranges) = state.token_ranges.get(&uri) else {
            return Vec::new();
        };
        self.token_data_from_instances(token_ids, token_ranges)
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
        let states = registrations
            .iter()
            .map(|registration| StateInfo {
                name: registration.display_name,
                type_name: registration.type_name.clone(),
            })
            .collect::<Vec<_>>();

        let mut tokens = Vec::with_capacity(registrations.len());
        let mut state_matchers = Vec::with_capacity(registrations.len());
        let mut state_boundaries = Vec::with_capacity(registrations.len());
        let mut state_recovery_errors = Vec::with_capacity(registrations.len());
        let mut state_boundary_errors = Vec::with_capacity(registrations.len());
        for registration in &registrations {
            let mut state_tokens = Vec::new();
            let mut patterns = Vec::new();
            for spec in (registration.rules)() {
                patterns.push(spec.regex);
                state_tokens.push(build::resolve_token(spec, &state_ids)?);
            }
            state_boundaries.push(registration.boundary);
            state_recovery_errors.push(registration.recovery_error_builder.clone());
            state_boundary_errors.push(registration.boundary_error_builder.clone());
            state_matchers.push(build::build_state_matcher(
                registration.display_name,
                &patterns,
            )?);
            tokens.push(state_tokens);
        }

        Ok(Self {
            state_info: states,
            tokens,
            arena: Vec::new(),
            state_matchers,
            state_boundaries,
            state_recovery_errors,
            state_boundary_errors,
            latest: LexerSnapshotState::default(),
            _lower: PhantomData,
            _snapshot: HashMap::new(),
        })
    }

    pub fn state_info(&self) -> &[StateInfo] {
        &self.state_info
    }

    pub fn resolved_tokens(&self) -> &[Vec<ResolvedToken<Root>>] {
        &self.tokens
    }

    pub fn tokens_in_state(&self, state: State) -> Option<&[ResolvedToken<Root>]> {
        self.tokens.get(state.id).map(Vec::as_slice)
    }

    pub(crate) fn state_matcher(&self, state: State) -> Option<&StateMatcher> {
        self.state_matchers.get(state.id)
    }

    pub fn alloc(&mut self, entry: Entry<Root>) -> usize {
        let index = self.arena.len();
        self.arena.push(entry);
        index
    }

    pub fn get(&self, index: usize) -> &Entry<Root> {
        self.arena.get(index).unwrap()
    }

    pub fn terminal_of(&self, index: usize) -> Option<TerminalId> {
        match self.get(index) {
            Entry::Token { terminal, .. } => Some(*terminal),
            Entry::EOF | Entry::Error { .. } => None,
        }
    }

    pub(crate) fn snapshot_state(&self, snapshot: Option<SnapshotId>) -> &LexerSnapshotState<Root> {
        self.state(snapshot).unwrap_or_else(|| self.latest_state())
    }

    pub(crate) fn entry_span(&self, snapshot: Option<SnapshotId>, id: usize) -> Option<Span> {
        let state = self.snapshot_state(snapshot);
        for (&uri, token_ids) in &state.token_instances {
            let Some(token_ranges) = state.token_ranges.get(&uri) else {
                continue;
            };
            for (&token_id, &(start, end)) in token_ids.iter().zip(token_ranges.iter()) {
                if token_id == id {
                    return Span::new_uri(uri, start, end).ok();
                }
            }
        }
        None
    }

    pub(crate) fn entries_in_span(
        &self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Vec<Entry<Root>>
    where
        Root: Clone,
    {
        let state = self.snapshot_state(snapshot);
        let Some(token_ids) = state.token_instances.get(&span.uri) else {
            return Vec::new();
        };
        let Some(token_ranges) = state.token_ranges.get(&span.uri) else {
            return Vec::new();
        };

        token_ids
            .iter()
            .zip(token_ranges.iter())
            .filter_map(|(&token_id, &(start, end))| {
                if start < span.range.end() && end > span.range.start() {
                    self.arena.get(token_id).cloned()
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) fn token_data_in_span(
        &self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Vec<crate::component::parse::TokenData> {
        let state = self.snapshot_state(snapshot);
        let Some(token_ids) = state.token_instances.get(&span.uri) else {
            return Vec::new();
        };
        let Some(token_ranges) = state.token_ranges.get(&span.uri) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (column, (&id, &(start, end))) in token_ids.iter().zip(token_ranges.iter()).enumerate()
        {
            let Some(entry) = self.arena.get(id) else {
                continue;
            };
            let include = match entry {
                Entry::Token { terminal, .. } => {
                    !self.is_skip_terminal(*terminal)
                        && start < span.range.end()
                        && end > span.range.start()
                }
                Entry::Error { .. } => start < span.range.end() && end > span.range.start(),
                Entry::EOF => start >= span.range.start() && start <= span.range.end(),
            };

            if include {
                match entry {
                    Entry::Token {
                        length,
                        terminal,
                        value,
                    } => {
                        out.push(TokenData {
                            id,
                            terminal: Some(*terminal),
                            start,
                            length: *length,
                            column,
                            fingerprint: token_fingerprint(Some(*terminal), value, *length),
                        });
                    }
                    Entry::EOF => {
                        out.push(TokenData {
                            id,
                            terminal: None,
                            start,
                            length: 0,
                            column,
                            fingerprint: eof_fingerprint(),
                        });
                    }
                    Entry::Error { length, info, .. } => {
                        out.push(TokenData {
                            id,
                            terminal: None,
                            start,
                            length: *length,
                            column,
                            fingerprint: error_fingerprint(info, *length),
                        });
                    }
                }
            }
        }

        out
    }

    fn is_skip_terminal(&self, terminal: TerminalId) -> bool {
        self.tokens.iter().any(|state_tokens| {
            state_tokens
                .iter()
                .any(|t| t.terminal == terminal && t.skip)
        })
    }

    pub fn state_id_of<S: TokenState>(&self) -> Option<State> {
        let type_name = S::state_key();
        self.state_info
            .iter()
            .position(|state| state.type_name == type_name)
            .map(|p| State::new(p))
    }

    pub(crate) fn state_boundary(&self, state: State) -> Option<StateBoundary> {
        self.state_boundaries.get(state.id).copied().flatten()
    }

    pub(crate) fn recovery_error_builder(&self, state: State) -> Option<&BuildErrorToken<Root>> {
        self.state_recovery_errors.get(state.id)
    }

    pub(crate) fn boundary_error_builder(&self, state: State) -> Option<&BuildErrorToken<Root>> {
        self.state_boundary_errors.get(state.id)
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

pub fn lift_state_registrations<Root, Nested>(
    build_outer: BuildLiftedToken<Root, Nested>,
    wrap_nested: Option<WrapLiftedToken<Root, Nested>>,
    boundary_error_builder: BuildErrorToken<Root>,
    synthetic_key: &'static str,
    boundary: Option<StateBoundary>,
    terminal: TerminalId,
    label: &'static str,
    outer_validate: Option<self::__macro_private::ValidateLexeme>,
) -> Vec<StateRegistration<Root>>
where
    Root: Send + Sync + 'static,
    Nested: LexerRoot + 'static,
{
    build::lift_state_registrations(
        build_outer,
        wrap_nested,
        boundary_error_builder,
        synthetic_key,
        boundary,
        terminal,
        label,
        outer_validate,
    )
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
