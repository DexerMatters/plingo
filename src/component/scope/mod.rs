//! Typed semantic scope allocation and graph facts.
//!
//! Language rules explicitly construct domain-owned scopes; this module stores
//! their identity, data, edges, and closure facts without interpreting them.

mod data;
pub mod node;
mod query;

pub use data::{ScopeAllocation, ScopeDomain, ScopeEdge, ScopeId, ScopeLifecycle, ScopeProperty};
pub use node::{ScopeAllocations, ScopeData, ScopeEdges, ScopeLifecycles, SourceRequirements};
pub(crate) use node::{ScopeCatalogNode, ScopeHandle};
pub use plingo_macros::{ScopeDomain, lregex, rlregex};
pub(crate) use query::resolve_indexed;
pub use query::{PathExpr, PathOrder, RelativeRegex, ResolutionPath};
