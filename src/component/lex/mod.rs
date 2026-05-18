mod lexing;
mod mode;

#[doc(hidden)]
pub mod __macro_private;

use std::{any::Any, collections::HashMap, error::Error, fmt, str::FromStr};

use regex_automata::{
    MatchKind,
    dfa::{StartKind, dense::DFA},
};
use regex_syntax::hir::{Hir, HirKind, Look};
use thiserror::Error;

pub use mode::{LexerState, StateAction, StateId, StateInfo};

use self::__macro_private::{BuildToken, StateDirective, StateRegistration, TokenSpec};

pub trait TokenState: Send + Sync + 'static {
    fn display_name() -> &'static str;
    fn state_key() -> &'static str;
}

pub trait FromLexeme: Sized {
    type Error: Error + Send + Sync + 'static;

    fn from_lexeme(lexeme: &str) -> Result<Self, Self::Error>;
}

pub struct Token {
    type_name: &'static str,
    variant_name: &'static str,
    value: Box<dyn __macro_private::TokenValue>,
}

impl Token {
    #[doc(hidden)]
    pub fn new<T>(variant_name: &'static str, value: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            type_name: std::any::type_name::<T>(),
            variant_name,
            value: Box::new(value),
        }
    }

    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    pub fn variant_name(&self) -> &'static str {
        self.variant_name
    }

    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.value.as_any().downcast_ref::<T>()
    }

    pub fn into_any(self) -> Box<dyn Any + Send + Sync> {
        self.value.into_any()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("type_name", &self.type_name)
            .field("variant_name", &self.variant_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedToken {
    pub precedence: usize,
    pub label: &'static str,
    pub action: StateAction,
    pub skip: bool,
    pub(crate) build: BuildToken,
    pub(crate) minimum_length: usize,
    pub(crate) maximum_length: usize,
}

impl ResolvedToken {
    pub fn minimum_length(&self) -> usize {
        self.minimum_length
    }

    pub fn maximum_length(&self) -> usize {
        self.maximum_length
    }

    pub fn build(&self, lexeme: &str) -> Result<Token, LexError> {
        (self.build)(lexeme)
    }
}

#[derive(Debug)]
pub(crate) struct StateMatcher {
    pub(crate) dfa: DFA<Vec<u32>>,
    pub(crate) token_index_by_pattern: Vec<usize>,
}

#[derive(Debug)]
pub struct Lexer {
    root: StateId,
    state_info: Vec<StateInfo>,
    tokens: Vec<Vec<ResolvedToken>>,
    state_matchers: Vec<StateMatcher>,
    states: Vec<LexerState>,
}

impl Lexer {
    pub fn new<Root: TokenState>() -> Result<Self, LexerCreationError> {
        let registrations = collect_state_registrations();
        let state_ids = registrations
            .iter()
            .enumerate()
            .map(|(index, registration)| (registration.type_name, StateId(index)))
            .collect::<HashMap<_, _>>();

        let root_type = Root::state_key();
        let Some(&root) = state_ids.get(root_type) else {
            return Err(LexerCreationError::UnknownState(root_type.to_string()));
        };

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
            root,
            state_info: states,
            tokens,
            state_matchers,
            states: vec![LexerState::new(root)],
        })
    }

    pub fn root(&self) -> StateId {
        self.root
    }

    pub fn state_info(&self) -> &[StateInfo] {
        &self.state_info
    }

    pub fn tokens(&self) -> &[Vec<ResolvedToken>] {
        &self.tokens
    }

    pub fn tokens_in_state(&self, state: StateId) -> Option<&[ResolvedToken]> {
        self.tokens.get(state.0).map(Vec::as_slice)
    }

    pub(crate) fn state_matcher(&self, state: StateId) -> Option<&StateMatcher> {
        self.state_matchers.get(state.0)
    }

    pub fn state_id_of<S: TokenState>(&self) -> Option<StateId> {
        let type_name = S::state_key();
        self.state_info
            .iter()
            .position(|state| state.type_name == type_name)
            .map(StateId)
    }

    pub fn states(&self) -> &[LexerState] {
        &self.states
    }
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

#[derive(Debug, Error)]
pub enum LexError {
    #[error("Unexpected end of input while in state {state}")]
    UnexpectedEndOfInput { state: &'static str },
    #[error("Unexpected token at offset {offset}")]
    UnexpectedToken { offset: usize },
    #[error("Missing token {token} at offset {offset}")]
    MissingToken { token: &'static str, offset: usize },
    #[error("Cannot leave root lexer state at offset {offset}")]
    CannotLeaveRootState { offset: usize },
    #[error("Failed to parse token {token} from lexeme {lexeme:?}: {source}")]
    TokenParseError {
        token: &'static str,
        lexeme: String,
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl LexError {
    pub fn token_parse_failed<E>(token: &'static str, lexeme: &str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::TokenParseError {
            token,
            lexeme: lexeme.to_string(),
            source: Box::new(source),
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

fn collect_state_registrations() -> Vec<&'static StateRegistration> {
    let mut registrations = ::inventory::iter::<StateRegistration>
        .into_iter()
        .collect::<Vec<_>>();
    registrations.sort_by(|left, right| left.type_name.cmp(right.type_name));
    registrations
}

fn resolve_token(
    spec: TokenSpec,
    state_ids: &HashMap<&'static str, StateId>,
) -> Result<ResolvedToken, LexerCreationError> {
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
    state_ids: &HashMap<&'static str, StateId>,
) -> Result<StateAction, LexerCreationError> {
    match action {
        StateDirective::None => Ok(StateAction::None),
        StateDirective::Enter(target) => state_ids
            .get(target)
            .copied()
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
