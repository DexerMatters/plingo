//! A demand-driven graph of versioned views.
//!
//! This facade exposes the typed graph API.  Runtime implementation details are
//! separated by responsibility: contracts, erased identity/state, transaction
//! engine, graph publication/lifecycle, and regression tests.

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
    Command, ComponentState, DefinitionEdge, EdgeKind, IndexedRelation, InputNode, NodeError,
    NodeInspection, NodeKey, NodeProvider, NodeSchema, NodeValue, PortDeclaration, PortKind,
    ReadGraph, Relation, SnapshotId, View, ViewFamily,
};
pub use engine::{CommandCx, DeriveCx, ReclaimCx};
pub use graph::{
    DemandLease, EffectFailure, Graph, GraphReader, RelationEffectResult, RelationSubscription,
    RelationUpdate, Snapshot, Subscription, ViewUpdate,
};

#[cfg(test)]
#[path = "../../tests/unit/scheme_node.rs"]
mod tests;
