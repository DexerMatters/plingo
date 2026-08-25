//! Public lexer vocabulary and the compact compiled-token representation.

use std::{error::Error, fmt, hash::Hash, str::FromStr};

use regex_automata::dfa::dense::DFA;
use regex_syntax::hir::HirKind;
use thiserror::Error;

use crate::framework::parse::grammar::TerminalId;

use super::{
    __macro_private::{self, BuildErrorToken, BuildToken, ScopeRegistration, WithHook},
    SlotStore,
    mode::{State, StateAction, StateInfo},
};

pub trait TokenState: Send + Sync + 'static {
    fn display_name() -> &'static str;
    fn state_key() -> &'static str;
}

pub trait LexerRoot:
    TokenState + Hash + Eq + PartialEq + Clone + Sized + Send + Sync + 'static
{
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

/// Deterministic lexer work counters for one document command (plan §10.1).
/// Counters roll back with their command and never enter reactive facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LexerWork {
    /// Lexer component invocations for this document.
    pub component_runs: u64,
    /// Checkpoint/occurrence-index lookups.
    pub checkpoint_lookups: u64,
    /// Cumulative lookup depth (entries inspected by index searches).
    pub checkpoint_lookup_depth: u64,
    /// Restart byte offsets chosen for replay.
    pub restart_bytes: u64,
    /// Restart occurrence indexes chosen for replay.
    pub restart_occurrences: u64,
    /// Source bytes examined by DFA scanning or replay decisions.
    pub source_bytes_examined: u64,
    /// DFA transitions executed.
    pub dfa_transitions: u64,
    /// Lexical entries decoded inside replay windows.
    pub lexical_entries_visited: u64,
    /// Semantic entries inspected while constructing an exact patch.
    pub semantic_entries_visited: u64,
    /// Tokens re-lexed inside replay windows.
    pub tokens_replayed: u64,
    /// New tokens inserted into the publication.
    pub tokens_inserted: u64,
    /// Old tokens removed from the publication.
    pub tokens_removed: u64,
    /// Suffix tokens reused without re-lexing.
    pub tokens_reused: u64,
    /// Retained suffix entries physically visited after convergence.
    pub retained_suffix_entries_visited: u64,
    /// Exact token-fact candidate writes.
    pub token_fact_writes: u64,
    /// Convergence candidates considered.
    pub convergence_candidates: u64,
    /// Convergence proofs tested against retained state.
    pub convergence_checks: u64,
    /// Replays that ran to document EOF without converging.
    pub eof_replays: u64,
    /// Tape intervals transferred unchanged.
    pub transferred_tape_intervals: u64,
    /// Scratch bytes requested from reusable buffers.
    pub scratch_bytes_requested: u64,
    /// Persistent tape nodes created.
    pub tape_nodes_created: u64,
    /// Persistent tape nodes reused by pointer.
    pub tape_nodes_reused: u64,
    /// Persistent radix/HAMT nodes created.
    pub radix_nodes_created: u64,
    /// Persistent radix/HAMT nodes reused by pointer.
    pub radix_nodes_reused: u64,
    /// Explicit forbidden complete-tape walks on a local edit.
    pub full_tape_iterations: u64,
    /// Explicit forbidden complete semantic projections on a local edit.
    pub full_projection_fallbacks: u64,
    /// Explicit forbidden document-vector reconstructions on a local edit.
    pub document_vector_rebuilds: u64,
}

impl LexerWork {
    /// Merges another counter set into this one (checked addition).
    pub fn merge(&mut self, other: &Self) {
        self.component_runs += other.component_runs;
        self.checkpoint_lookups += other.checkpoint_lookups;
        self.checkpoint_lookup_depth += other.checkpoint_lookup_depth;
        self.restart_bytes += other.restart_bytes;
        self.restart_occurrences += other.restart_occurrences;
        self.source_bytes_examined += other.source_bytes_examined;
        self.dfa_transitions += other.dfa_transitions;
        self.lexical_entries_visited += other.lexical_entries_visited;
        self.semantic_entries_visited += other.semantic_entries_visited;
        self.tokens_replayed += other.tokens_replayed;
        self.tokens_inserted += other.tokens_inserted;
        self.tokens_removed += other.tokens_removed;
        self.tokens_reused += other.tokens_reused;
        self.retained_suffix_entries_visited += other.retained_suffix_entries_visited;
        self.token_fact_writes += other.token_fact_writes;
        self.convergence_candidates += other.convergence_candidates;
        self.convergence_checks += other.convergence_checks;
        self.eof_replays += other.eof_replays;
        self.transferred_tape_intervals += other.transferred_tape_intervals;
        self.scratch_bytes_requested += other.scratch_bytes_requested;
        self.tape_nodes_created += other.tape_nodes_created;
        self.tape_nodes_reused += other.tape_nodes_reused;
        self.radix_nodes_created += other.radix_nodes_created;
        self.radix_nodes_reused += other.radix_nodes_reused;
        self.full_tape_iterations += other.full_tape_iterations;
        self.full_projection_fallbacks += other.full_projection_fallbacks;
        self.document_vector_rebuilds += other.document_vector_rebuilds;
    }
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

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LexToken<Root>
where
    Root: LexerRoot,
{
    pub(crate) id: usize,
    pub start: usize,
    pub length: usize,
    pub terminal: Option<TerminalId>,
    pub error: Option<LexErrorInfo>,
    pub value: Root,
}
impl<Root> fmt::Debug for LexToken<Root>
where
    Root: LexerRoot + fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LexToken")
            .field("start", &self.start)
            .field("length", &self.length)
            .field("terminal", &self.terminal)
            .field("error", &self.error)
            .field("value", &self.value)
            .finish()
    }
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

/// One scanner output before it receives a document-local occurrence identity.
/// Scanner output is deliberately transient; the persistent lexical tape owns
/// the committed value and checkpoint state.
#[derive(Debug)]
pub(crate) struct ScannedToken<Root>
where
    Root: LexerRoot,
{
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) terminal: Option<TerminalId>,
    pub(crate) skip: bool,
    pub(crate) error: Option<LexErrorKind>,
    pub(crate) value: Root,
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
