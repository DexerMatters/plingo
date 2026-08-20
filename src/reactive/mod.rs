//! The detached reactive engine (§5 of `docs/agent/reactive-engine-plan.md`).
//!
//! A self-contained, theory-grounded dependency engine: shared views with
//! exact fact dependencies, epoch transactions with round-based
//! propagation, deterministic under any worker count, with per-fact
//! ownership and cycle rejection. It never imports the legacy kernel
//! (`crate::scheme`, `crate::component` internals, `crate::visual`).
//!
//! The authoring surface ([`prelude`]) exposes only views, handles, and
//! visitor methods; the engine derives the dependency relation from
//! executed reads and writes (G3).

pub mod api;
pub mod engine;
pub mod error;
pub mod store;
pub mod trace;
pub mod value;
pub mod view;

pub use api::{Emitted, EmittedHandle, Observed, ObservedHandle, Previous, PreviousHandle, RunContext};
pub use engine::{
    CommandReport, Component, Engine, EngineBuilder, ExternalOp, RawChange, Snapshot, Subscriber,
    SnapshotBox, SnapshotGraph, SnapshotMap, SnapshotTree,
};
pub use error::{Error, Result};
pub use view::{
    AbstractTreeView, BoxFactKey, BoxView, GraphEdgeKey, GraphFactKey, GraphView, MapFactKey,
    MapView, NodeId, ShapeKind, TreeFactKey, TreeView, ViewSpec,
};

#[cfg(test)]
mod tests;

/// The one-import authoring surface.
pub mod prelude {
    pub use super::api::{
        BoxEmittedExt, BoxObservedExt, BoxPreviousExt, Emitted, EmittedHandle, GraphEmittedExt,
        GraphObservedExt, GraphPreviousExt, MapEmittedExt, MapObservedExt, MapPreviousExt,
        Observed, ObservedHandle, Previous, PreviousHandle, RunContext, TreeEmittedExt,
        TreeObservedExt, TreePreviousExt,
    };
    pub use super::engine::{
        CommandReport, Component, Engine, EngineBuilder, ExternalOp, RawChange, Snapshot,
        SnapshotBox, SnapshotGraph, SnapshotMap, SnapshotTree, Subscriber,
    };
    pub use super::error::{Error, Result};
    pub use super::view::{
        AbstractTreeShape, AbstractTreeView, BoxFactKey, BoxShape, BoxView, GraphEdgeKey,
        GraphFactKey, GraphShape, GraphView, MapFactKey, MapShape, MapView, NodeId, ShapeKind,
        ShapeSpec, TreeFactKey, TreeShape, TreeView, ViewSpec,
    };
}
