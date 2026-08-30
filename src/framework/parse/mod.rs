//! The built-in reactive parser (plan §3.2): pure grammar/replay and
//! snapshot machinery live here; parser publication is layered on top of it.
//! The former node-graph glue has been removed.
//!
//! This module is the versioned home of the parser: `grammar.rs`,
//! `build.rs`, `types.rs`, `analyze.rs`, `diagnostics.rs`, `recovery.rs`,
//! `parser.rs`, `parsing/`, and `data/` all live under it.

pub(crate) mod analyze;
pub(crate) mod build;
pub(crate) mod component;
#[doc(hidden)]
pub mod data;
pub mod delta;
pub(crate) mod diagnostics;
#[doc(hidden)]
pub mod grammar;
pub(crate) mod identity;
pub(crate) mod parser;
pub(crate) mod parsing;
pub(crate) mod recovery;
pub mod recovery_policy;
pub(crate) mod types;

#[doc(hidden)]
pub mod __macro_private;

pub use component::{
    AstSnapshots, ParseDiagnostics, ParseUnit, ParseUnits, ParserTreeStatuses, install_parser,
    install_parser_tree,
};
#[doc(hidden)]
pub use data::ast::AstToken;
#[doc(hidden)]
pub use data::green::{ErrorKind, ParseErrorInfo};
pub use delta::{
    KeyDelta, OrderedDelta, ParseDelta, ParseDiagnosticKey, ParsedStatus, RecoverySegmentId,
    TokenAnchor,
};
pub use parser::Parser;
pub use parsing::ParseError;
pub use recovery_policy::{
    ErrorRegion, MissingToken, ParserRecoveryPolicy, RecoveryProduct, RegionalFallbackPolicy,
    SkippedToken,
};
pub use types::{
    AstLookupError, AstSnapshot, AstTokenSnapshotEntry, BoundaryTrace, DocumentSnapshot,
    FullReplayReason, IncrementalParseStats, ParseStatus, ParserConfig, ParserWork, ResolvedAst,
};
pub(crate) use types::{ParserSnapshotState, TokenData};
