//! A demand-driven graph of versioned views.
//!
//! This facade exposes the typed graph API.  Runtime implementation details are
//! separated by responsibility: contracts, erased identity/state, transaction
//! engine, graph publication/lifecycle, and regression tests.

mod api;
mod engine;
mod graph;
mod identity;
mod state;

// Private runtime prelude shared by the implementation modules.  These are not
// exposed from `scheme::node`; consumers interact only with the typed facade
// re-exports below.

pub use api::{
    Command, ComponentState, IndexedRelation, Node, NodeError, NodeKey, NodeValue, Relation,
    SnapshotId, View,
};
pub use engine::{CommandCx, DeriveCx, ReclaimCx};
pub use graph::{
    EffectFailure, Graph, RelationEffectResult, RelationSubscription, RelationUpdate,
    RequestHandle, Snapshot, Subscription, ViewUpdate,
};

#[cfg(test)]
mod tests;
