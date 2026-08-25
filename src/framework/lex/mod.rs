//! The built-in reactive lexer (plan §8.2): a `Tokens` map view, the
//! generic lexer pipeline, and `install_lexer`. The pure DFA machinery
//! (scan/state/token/build/mode/context/incremental) lives here.
//!
//! The reactive lexer observes the [`SourceText`] each open document and,
//! per URI, re-lexes the whole document on any change, publishing [`Tokens`]
//! for that URI. One child computation per URI keeps edits to one document
//! from re-running another's lexer child (matrix 1).

mod build;
mod component;
pub mod cursor;
pub mod lexed;
mod context;
mod incremental;
mod mode;
mod scan;
mod state;
mod token;

#[doc(hidden)]
pub mod __macro_private;

pub use component::{TokenVec, Tokens, install_lexer};
pub use context::{Slot, SlotStore, WhenCx, WithCx};
pub use mode::{LexerState, State, StateAction, StateInfo};
pub use state::{Lexer, LexerSnapshotState};
pub use token::{
    FromLexeme, GenerateError, IncrementalLexStats, LexErrorInfo, LexErrorKind, LexInterrupt,
    LexMoment, LexerWork, LexToken, LexerCreationError, LexerRoot, ResolvedToken, TokenAction,
    TokenState, UnsupportedDefaultParseError,
};
pub use lexed::{
    LayoutRevisionId, LexedDocument, LexedDocuments, SemanticRevisionId, SemanticToken,
    SemanticTokenDocument, SemanticTokenDocuments, StableDocumentId, SyntheticTokenId, TapeSplice,
    TokenFact, TokenFactId, TokenFactKey, TokenFacts, TokenFingerprint, TokenLayoutDocument,
    TokenLayoutDocuments, TokenOccurrenceId, TokenPatch, TokenStructureRevisionId,
    TokenValueRevisionId, observe_token,
};
pub(crate) use lexed::{
    CanonicalLexerState, LexicalDocument, LexicalOccurrence, ParseTokenRef, TokenLayoutEntry,
};
pub(crate) use token::ScannedToken;
