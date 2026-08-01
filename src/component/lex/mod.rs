//! Lexing keeps declarative state configuration, persistent token state, sparse
//! transactions, and DFA scanning separate while exposing one stable API.

mod build;
mod context;
mod incremental;
mod mode;
pub mod node;
mod scan;
mod state;
mod token;

#[doc(hidden)]
pub mod __macro_private;

pub use context::{Slot, SlotStore, WhenCx, WithCx};
pub use mode::{LexerState, State, StateAction, StateInfo};
pub use node::{
    LexDiagnostics, LexStats, LexerNode, TokenArtifact, TokenEntryKey, TokenKey, TokenLexeme,
    TokenOrder,
};
pub use state::{Lexer, LexerSnapshotState};
pub use token::{
    FromLexeme, GenerateError, IncrementalLexStats, LexErrorInfo, LexErrorKind, LexInterrupt,
    LexMoment, LexToken, LexerCreationError, LexerRoot, ResolvedToken, TokenAction, TokenState,
    UnsupportedDefaultParseError,
};
