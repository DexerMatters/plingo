mod lexing;
mod mode;

#[doc(hidden)]
pub mod __macro_private;
pub mod policy;

use std::{collections::HashMap, error::Error, fmt, hash::Hash, marker::PhantomData, str::FromStr};

use fluent_uri::Uri;
use indexmap::IndexSet;
use plingo_macros::layer;
use regex_automata::{
    MatchKind,
    dfa::{StartKind, dense::DFA},
};
use regex_syntax::hir::{Hir, HirKind, Look};
use thiserror::Error;

pub use mode::{LexerState, State, StateAction, StateInfo};

use crate::{
    scheme::{
        ActionError, Context, LayerDeltas, MiddleLayer, NonTopLayer, SnapshotId, SnapshotLayer,
    },
    utils::{PrettyDisplay, Span},
};

use self::__macro_private::{BuildToken, StateDirective, StateRegistration, TokenSpec};

pub trait TokenState: Send + Sync + 'static {
    fn display_name() -> &'static str;
    fn state_key() -> &'static str;
}

pub trait StateTokens: TokenState + Sized {
    fn token_specs() -> Vec<TokenSpec<Self>>;

    fn state_registration() -> StateRegistration<Self> {
        StateRegistration::new(Self::display_name(), Self::state_key(), Self::token_specs)
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
    pub precedence: usize,
    pub label: &'static str,
    pub action: StateAction,
    pub skip: bool,
    pub(crate) build: BuildToken<Root>,
    pub(crate) minimum_length: usize,
    pub(crate) maximum_length: usize,
    pub(crate) captures_context: bool,
    pub(crate) validate: Option<fn(&str, Option<&str>) -> bool>,
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
    Token(usize, Root),
    Error(usize, ErrorToken),
}

impl<Root> Entry<Root>
where
    Root: LexerRoot,
{
    pub fn is_token(&self) -> bool {
        matches!(self, Self::Token(_, _))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_, _))
    }

    pub fn length(&self) -> usize {
        match self {
            Self::Token(length, _) | Self::Error(length, _) => *length,
        }
    }
}

impl<Root: LexerRoot + fmt::Display, Lower> PrettyDisplay<Lexer<Root, Lower>> for Entry<Root> {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        context: &Lexer<Root, Lower>,
    ) -> core::fmt::Result {
        use color_print::cwrite;
        match self {
            Entry::Token(length, token) => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><green>Token</green>: {}",
                    length,
                    token
                )
            }
            Entry::Error(length, error) => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><red>Error</red>: {}",
                    length,
                    error.pretty(context)
                )
            }
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
            Entry::Token(length, token) => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><green>Token</green>: {}",
                    length,
                    token
                )
            }
            Entry::Error(length, error) => {
                cwrite!(
                    f,
                    "<dim>[{}]\t</dim><red>Error</red>: {}",
                    length,
                    error.pretty(context)
                )
            }
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
            _root: PhantomData,
        }
    }
}

#[derive(Debug)]
#[layer]
pub struct Lexer<Root, Lower = ()>
where
    Root: LexerRoot,
{
    tokens: Vec<Vec<ResolvedToken<Root>>>,
    state_matchers: Vec<StateMatcher>,
    state_info: Vec<StateInfo>,

    #[snapshot]
    latest: LexerSnapshotState<Root>,
    arena: IndexSet<Entry<Root>>,
    _lower: PhantomData<fn() -> Lower>,
}

impl<Root: LexerRoot, Lower> Lexer<Root, Lower> {
    pub fn new() -> Result<Self, LexerCreationError> {
        let registrations = Root::state_registrations();
        let state_ids = registrations
            .iter()
            .enumerate()
            .map(|(index, registration)| (registration.type_name, State::new(index)))
            .collect::<HashMap<_, _>>();
        let states = registrations
            .iter()
            .map(|registration| StateInfo {
                name: registration.display_name,
                type_name: registration.type_name,
            })
            .collect::<Vec<_>>();

        let mut tokens = Vec::with_capacity(registrations.len());
        let mut state_matchers = Vec::with_capacity(registrations.len());
        for registration in &registrations {
            let mut state_tokens = Vec::new();
            let mut patterns = Vec::new();
            for spec in (registration.rules)() {
                patterns.push(spec.regex);
                state_tokens.push(resolve_token(spec, &state_ids)?);
            }
            state_matchers.push(build_state_matcher(registration.display_name, &patterns)?);
            tokens.push(state_tokens);
        }

        Ok(Self {
            state_info: states,
            tokens,
            arena: IndexSet::new(),
            state_matchers,
            latest: LexerSnapshotState::default(),
            _lower: PhantomData,
            _snapshot: HashMap::new(),
        })
    }

    pub fn state_info(&self) -> &[StateInfo] {
        &self.state_info
    }

    pub fn tokens(&self) -> &[Vec<ResolvedToken<Root>>] {
        &self.tokens
    }

    pub fn tokens_in_state(&self, state: State) -> Option<&[ResolvedToken<Root>]> {
        self.tokens.get(state.id).map(Vec::as_slice)
    }

    pub(crate) fn state_matcher(&self, state: State) -> Option<&StateMatcher> {
        self.state_matchers.get(state.id)
    }

    pub fn alloc(&mut self, entry: Entry<Root>) -> usize {
        let index = self.arena.insert_full(entry).0;
        index
    }

    pub fn get(&self, index: usize) -> &Entry<Root> {
        // SAFETY: The index is guaranteed to be valid because it's only
        // produced by `alloc`, which inserts into the arena and returns the
        // index.
        self.arena.get_index(index).unwrap()
    }

    pub(crate) fn snapshot_state(&self, snapshot: Option<SnapshotId>) -> &LexerSnapshotState<Root> {
        self.state(snapshot).unwrap_or_else(|| self.latest_state())
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

        let start = span.range.start().min(token_ids.len());
        let end = span.range.end().min(token_ids.len());
        token_ids[start..end]
            .iter()
            .map(|&token_id| self.get(token_id).clone())
            .collect()
    }

    pub fn state_id_of<S: TokenState>(&self) -> Option<State> {
        let type_name = S::state_key();
        self.state_info
            .iter()
            .position(|state| state.type_name == type_name)
            .map(|p| State::new(p))
    }
}

#[layer(middle)]
impl<Root, Lower> MiddleLayer for Lexer<Root, Lower>
where
    Root: LexerRoot,
    Lower: NonTopLayer<_Key = Span, _Value = usize> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Key = Span;
    type Error = LexInterrupt;
    type Value = usize;

    fn pass(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<LayerDeltas<Self::Lower>, Self::Error>> + Send {
        async move {
            let mut working = self.latest.clone();
            let mut lower_deltas = Vec::new();
            for delta in deltas {
                match self.lex_delta(ctx, &mut working, delta).await {
                    Ok(deltas) => lower_deltas.extend(deltas),
                    Err(err) => return Err(err),
                }
            }
            self.latest = working.clone();
            if let Some(snapshot) = ctx.snapshot() {
                self.push_state(snapshot);
            }
            Ok(lower_deltas)
        }
    }
}

pub fn lift_state_registrations<Root, Nested>(
    wrap: fn(Nested) -> Root,
) -> Vec<StateRegistration<Root>>
where
    Root: Send + Sync + 'static,
    Nested: LexerRoot + 'static,
{
    Nested::state_registrations()
        .into_iter()
        .map(|registration| {
            StateRegistration::new(
                registration.display_name,
                registration.type_name,
                move || {
                    (registration.rules)()
                        .into_iter()
                        .map(|spec| TokenSpec {
                            regex: spec.regex,
                            precedence: spec.precedence,
                            label: spec.label,
                            action: spec.action,
                            skip: spec.skip,
                            build: std::sync::Arc::new(move |lexeme| {
                                (spec.build)(lexeme).map(wrap)
                            }),
                            captures_context: spec.captures_context,
                            validate: spec.validate,
                        })
                        .collect()
                },
            )
        })
        .collect()
}

#[derive(Debug, Error)]
pub enum LexerCreationError {
    #[error("Error occurred while parsing regex pattern {0} for token {1}: {2}")]
    RegexParsingError(String, String, regex_syntax::Error),
    #[error("Failed to build grouped regex matcher for state {state}: {source}")]
    RegexMatcherBuildError {
        state: &'static str,
        #[source]
        source: regex_automata::dfa::dense::BuildError,
    },
    #[error("Regex pattern {1} for token {0} contains unsupported feature: {2:?}")]
    UnsupportedRegexFeature(String, String, HirKind),
    #[error("Token {0} with pattern {1} cannot be matched by any input string")]
    ImpossibleToken(String, String),
    #[error("State {0} is referenced but not registered")]
    UnknownState(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ErrorToken {
    UnexpectedEndOfInput {
        state: usize,
    },
    UnexpectedToken {
        start: usize,
        end: usize,
        expected_state: usize,
    },
    MissingToken {
        state: usize,
        token: usize,
        offset: usize,
    },
}

impl<Root: LexerRoot, Lower> PrettyDisplay<Lexer<Root, Lower>> for ErrorToken {
    fn pretty_fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
        context: &Lexer<Root, Lower>,
    ) -> core::fmt::Result {
        use color_print::cwrite;

        match self {
            Self::UnexpectedEndOfInput { state } => {
                let tokens = context
                    .tokens_in_state(State::new(*state))
                    .ok_or_else(|| fmt::Error)?;
                cwrite!(
                    f,
                    "Unexpected end of input, expected one of the following tokens: {}",
                    tokens
                        .iter()
                        .map(|token| token.label)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Self::UnexpectedToken {
                start,
                end,
                expected_state,
            } => {
                let tokens = context
                    .tokens_in_state(State::new(*expected_state))
                    .ok_or_else(|| fmt::Error)?;
                cwrite!(
                    f,
                    "Unexpected token from offset {} to {}, expected one of the following tokens: {}",
                    start,
                    end,
                    tokens
                        .iter()
                        .map(|token| token.label)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Self::MissingToken {
                state,
                token,
                offset,
            } => {
                let token = context
                    .tokens_in_state(State::new(*state))
                    .and_then(|tokens| tokens.get(*token))
                    .ok_or_else(|| fmt::Error)?;
                cwrite!(f, "Missing token {} at offset {}", token.label, offset)
            }
        }
    }
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

fn resolve_token<Root>(
    spec: TokenSpec<Root>,
    state_ids: &HashMap<&'static str, State>,
) -> Result<ResolvedToken<Root>, LexerCreationError> {
    let hir = regex_syntax::parse(spec.regex).map_err(|error| {
        LexerCreationError::RegexParsingError(spec.regex.to_string(), spec.label.to_string(), error)
    })?;

    if let Some(kind) = find_unsupported_regex_features(&hir) {
        return Err(LexerCreationError::UnsupportedRegexFeature(
            spec.label.to_string(),
            spec.regex.to_string(),
            kind,
        ));
    }

    let minimum_length = hir.properties().minimum_len().ok_or_else(|| {
        LexerCreationError::ImpossibleToken(spec.label.to_string(), spec.regex.to_string())
    })?;
    let maximum_length = hir.properties().maximum_len().unwrap_or(usize::MAX);

    Ok(ResolvedToken {
        precedence: spec.precedence,
        label: spec.label,
        action: resolve_action(spec.action, state_ids)?,
        skip: spec.skip,
        build: spec.build,
        minimum_length,
        maximum_length,
        captures_context: spec.captures_context,
        validate: spec.validate,
    })
}

fn build_state_matcher(
    state: &'static str,
    patterns: &[&'static str],
) -> Result<StateMatcher, LexerCreationError> {
    let dfa = DFA::builder()
        .configure(
            DFA::config()
                .start_kind(StartKind::Anchored)
                .match_kind(MatchKind::All),
        )
        .build_many(patterns)
        .map_err(|source| LexerCreationError::RegexMatcherBuildError { state, source })?;
    let token_index_by_pattern = (0..patterns.len()).collect();
    Ok(StateMatcher {
        dfa,
        token_index_by_pattern,
    })
}

fn resolve_action(
    action: StateDirective,
    state_ids: &HashMap<&'static str, State>,
) -> Result<StateAction, LexerCreationError> {
    match action {
        StateDirective::None => Ok(StateAction::None),
        StateDirective::Enter(target) => state_ids
            .get(target)
            .cloned()
            .map(StateAction::Enter)
            .ok_or_else(|| LexerCreationError::UnknownState(target.to_string())),
        StateDirective::Leave => Ok(StateAction::Leave),
    }
}

fn find_unsupported_regex_features(hir: &Hir) -> Option<HirKind> {
    match hir.kind() {
        HirKind::Alternation(parts) | HirKind::Concat(parts) => {
            parts.iter().find_map(find_unsupported_regex_features)
        }
        HirKind::Capture(_) => Some(hir.kind().clone()),
        HirKind::Look(Look::Start) => Some(hir.kind().clone()),
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) | HirKind::Look(_) => None,
        HirKind::Repetition(rep) => find_unsupported_regex_features(&rep.sub),
    }
}
