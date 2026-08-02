//! A demand-driven graph of versioned views.
//!
//! This facade exposes the typed graph kernel.  Runtime implementation details
//! are separated by responsibility: contracts, erased identity/state,
//! transaction engine, graph publication/lifecycle, and regression tests.
//!
//! Ordinary language authors use the higher-level [`Component`] API instead;
//! this module remains the kernel vocabulary used by built-in components and
//! advanced framework implementors.

mod actor;
mod api;
mod engine;
mod graph;
mod identity;
mod state;

// Private runtime prelude shared by the implementation modules.  These are not
// exposed from `scheme::node`; consumers interact only with the typed facade
// re-exports below.

pub use actor::{GraphActor, GraphActorError, GraphHandle, GraphRuntime};
pub use api::{
    Command, DefinitionEdge, EdgeKind, IndexedRelation, InputNode, NodeError, NodeInspection,
    NodeKey, NodeProvider, NodeSchema, NodeValue, PortDeclaration, PortKind, ProviderState,
    ReadGraph, Relation, SnapshotId, View, ViewFamily,
};
pub(crate) use engine::ErasedProvider;
pub use engine::{CommandCx, DeriveCx, ReclaimCx};
pub use graph::{
    DemandLease, EffectFailure, Graph, GraphReader, RelationEffectResult, RelationSubscription,
    RelationUpdate, Snapshot, Subscription, ViewUpdate,
};
pub(crate) use identity::{KeyId, TaskId};

#[cfg(test)]
#[path = "../../tests/unit/scheme_node.rs"]
mod tests;
