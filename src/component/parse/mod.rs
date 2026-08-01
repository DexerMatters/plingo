//! Parsing is split by responsibility: grammar construction, persistent parse data,
//! replay, and public inspection all share the same root-level API.

pub(crate) mod analyze;
pub(crate) mod build;
pub mod data;
pub(crate) mod diagnostics;
pub mod generator;
pub mod grammar;
pub(crate) mod identity;
pub mod node;
pub(crate) mod parser;
pub(crate) mod parsing;
pub(crate) mod recovery;
pub(crate) mod types;

#[doc(hidden)]
pub mod __macro_private;

pub use data::ast::AstToken;
pub use data::green::{ErrorKind, ParseErrorInfo};
pub use identity::TokenFingerprint;
pub use node::{
    AstArtifact, AstLocation, ParseCandidate, ParseCandidates, ParseDiagnostics, ParseRoots,
    ParseSnapshot, ParseStats, ParseStatusView, ParsedAst, ParserNode,
};
pub use parser::Parser;
pub use parsing::ParseError;
pub use types::{
    AstKey, AstLookupError, AstSnapshot, AstSnapshotEntry, AstTokenSnapshotEntry,
    IncrementalParseStats, ParseSnapshotId, ParseStatus, ParserConfig, ParserSnapshotState,
    ResolvedAst, TokenData, TokenOccurrenceId,
};
