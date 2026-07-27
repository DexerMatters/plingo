//! URI-free scope graph facts and node derivations.

mod data;
mod engine;
pub mod node;
mod query;

pub use data::{
    Scope, ScopeDatum, ScopeEdge, ScopeError, ScopeFrameKey, ScopeProperty, ScopeReference,
};
pub use node::{
    DatumSelector, ResolutionKey, ResolutionNode, ScopeCx, ScopeDatums, ScopeEdges, ScopeHandle,
    ScopeKey, ScopeNode, ScopeReferences, ScopeResolution, ScopeStamp, ScopeTaskKey,
    SourceRequirements,
};
pub use query::{PathExpr, ResolutionPath};
