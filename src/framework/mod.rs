//! The built-in reactive framework (plan §3): source, lexer, parser,
//! scope-graph, and workspace components on top of the `reactive` engine.

pub mod lex;
pub mod parse;
pub mod scope;
pub mod source;
pub mod workspace;

pub mod change;

pub use lex::Lexer;
pub use parse::Parser;
pub use scope::{
    PathExpr, PathOrder, ResolutionPath, ScopeDomain, ScopeGraph, ScopeGraphSnapshot, ScopeId,
    ScopeNode, ScopePath, ScopeRequirements, partition_visible,
};
pub use source::{SourceDelta, SourceEdit, SourceEdits, SourceSplice, SourceText};
pub use workspace::Workspace;