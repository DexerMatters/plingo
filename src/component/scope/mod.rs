//! Built-in, typed scope allocation plus graph-native scope facts.
//!
//! Language semantics are implemented by `component::elaborate::ElaboratorNode`
//! passes, not by user callbacks inside this module.

mod data;
pub mod node;
mod query;

pub use data::{
    DatumSelector, Scope, ScopeAllocation, ScopeDatum, ScopeDomain, ScopeEdge, ScopeError,
    ScopeOwner, ScopeProperty, ScopeReference,
};
pub use node::{
    ResolutionKey, ResolutionNode, ScopeAllocations, ScopeCatalogNode, ScopeCatalogStamp,
    ScopeDatums, ScopeEdges, ScopeHandle, ScopeReferences, ScopeResolution, SourceRequirements,
};
pub use plingo_macros::{label_regex, relative_label_regex};
pub(crate) use query::resolve_indexed;
pub use query::{PathExpr, RelativeRegex, ResolutionPath};
