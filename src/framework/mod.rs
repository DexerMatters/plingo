//! The built-in reactive framework (plan §3): source, lexer, parser,
//! scope-graph, and workspace pipelines on top of the `reactive` engine.

pub mod lex;
pub mod parse;
pub mod scope;
pub mod source;
pub(crate) mod tape;
pub mod workspace;

pub mod change;

pub use lex::Lexer;
pub use parse::Parser;
pub use scope::{
    PathExpr, PathOrder, ResolutionPath, Scope, ScopeDomain, ScopeGraph, ScopeNode, ScopePath,
    ScopeRequirements, declarations, declare, edge, ensure_scope, observe_node, observe_scope,
    outgoing, partition_visible, reference, remove_scope, resolve, resolve_name, scope,
    snapshot_declarations, snapshot_node, snapshot_nodes, snapshot_outgoing, snapshot_scope,
};
pub use source::{SourceEdit, SourceEdits, SourceRevisions, SourceSnapshot, source_snapshot};
pub use workspace::{Workspace, WorkspaceReport};
