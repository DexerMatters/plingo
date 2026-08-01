//! Public lexer vocabulary and the compact compiled-token representation.

use std::{error::Error, fmt, hash::Hash, str::FromStr};

use regex_automata::dfa::dense::DFA;
use regex_syntax::hir::HirKind;
use thiserror::Error;

use crate::component::parse::grammar::TerminalId;

use super::{
    __macro_private::{self, BuildErrorToken, BuildToken, ScopeRegistration, WithHook},
    SlotStore,
    mode::{State, StateAction, StateInfo},
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncrementalLexStats {
    pub restart_byte: usize,
    pub restart_occurrence: usize,
    pub relexed: usize,
    pub reused: usize,
    pub old_tokens: usize,
    pub new_tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LexMoment {
    Normal,
    Eof,
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
    pub(crate) when: Option<__macro_private::WhenGuard<Root>>,
    pub(crate) recover_when: Option<__macro_private::RecoverWhen>,
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

#[derive(Debug, Clone)]
pub(crate) struct StateMatcher {
    pub(crate) dfa: DFA<Vec<u32>>,
    pub(crate) token_index_by_pattern: Vec<usize>,
}

#[derive(Clone, Debug, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TokenOccurrence {
    pub id: usize,
    pub column: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone)]
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

pub(super) const SYNTHETIC_EOF_ID: usize = usize::MAX;

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
