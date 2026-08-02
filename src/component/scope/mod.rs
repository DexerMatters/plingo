//! Typed scope allocation and graph facts.
//!
//! Language components explicitly construct domain-owned scopes; this module
//! stores their identity, data, and edges without interpreting them.

pub(crate) mod context;
mod data;
mod node;
mod query;

pub use context::ScopeView;
pub use data::{
    ScopeAllocation, ScopeDefinitions, ScopeDomain, ScopeEdge, ScopeEdges, ScopeEntries, ScopeId,
    ScopeProperty, ScopeStructure,
};
pub use node::{ScopeAllocations, SourceRequirements};
pub use plingo_macros::{ScopeDomain, lregex, scope_path};
pub use query::{
    FilteredScopeQuery, OrderedScopeQuery, PathExpr, PathOrder, ResolutionPath, ScopePath,
    ScopePathQuery, ScopeQuery, ScopeResolution, ShadowResponse, Unset,
};
