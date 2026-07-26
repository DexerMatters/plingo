//! Parsing is split by responsibility: grammar construction, persistent parse data,
//! replay, and public inspection all share the same root-level API.

pub(crate) mod analyze;
pub(crate) mod build;
pub mod data;
pub(crate) mod diagnostics;
pub(crate) mod diff;
pub mod generator;
pub mod grammar;
pub(crate) mod identity;
pub mod interface;
pub(crate) mod parser;
pub(crate) mod parsing;
pub(crate) mod recovery;
pub(crate) mod types;

#[doc(hidden)]
pub mod __macro_private;

pub use data::ast::AstToken;
pub use data::green::{ErrorKind, ParseErrorInfo};
pub use identity::TokenFingerprint;
pub use parser::Parser;
pub use parsing::ParseError;
pub use types::{
    AstView, AstViewEntry, IncrementalParseStats, ParseAddress, ParseChange, ParseChanges,
    ParseForest, ParsePath, ParseUnit, ParserConfig, ParserSnapshotState, TokenData,
    TokenOccurrenceId,
};
