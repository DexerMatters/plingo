//! The built-in reactive lexer (plan §8.2): a `Tokens` map view, the
//! hand-written generic `LexerComponent`, and `install_lexer`. The pure DFA
//! machinery (scan/state/token/build/mode/context/incremental) moved here
//! from `component::lex`, with `node.rs` (node-graph glue) deleted.
//!
//! The reactive lexer observes the [`SourceText`] each open document and,
//! per uri, re-lexes the whole document on any change, publishing
//! [`Tokens`] for that uri. One child visitor per uri keeps edits to one
//! document from re-running another's lexer child (matrix 1).

mod build;
mod component;
mod context;
mod incremental;
mod mode;
mod scan;
mod state;
mod token;

#[doc(hidden)]
pub mod __macro_private;

pub use component::{LexerComponent, TokenVec, Tokens, install_lexer};
pub use context::{Slot, SlotStore, WhenCx, WithCx};
pub use mode::{LexerState, State, StateAction, StateInfo};
pub use state::{Lexer, LexerSnapshotState};
pub use token::{
    FromLexeme, GenerateError, IncrementalLexStats, LexErrorInfo, LexErrorKind, LexInterrupt,
    LexMoment, LexToken, LexerCreationError, LexerRoot, ResolvedToken, TokenAction, TokenState,
    UnsupportedDefaultParseError,
};