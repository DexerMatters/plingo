//! URI-free scope graphs constructed incrementally from parser deltas.
//!
//! Applications define the traversal rule with a closure passed to
//! [`ScopeLayer::new`]. The layer owns graph snapshots, frame dependencies,
//! exact fact ownership, and graph patches. It remains a middle layer.

mod data;
mod engine;
mod layer;
mod query;

pub use data::{
    Scope, ScopeDatum, ScopeEdge, ScopeError, ScopePatch, ScopeProperty, ScopeReference,
};
pub use layer::{ScopeCx, ScopeLayer, ScopeLayerError};
pub use query::{
    PathExpr, QueryConfirmation, RecordedQuery, ResolutionPath, ScopeQuery,
};

#[cfg(test)]
mod tests;
