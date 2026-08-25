//! The detached reactive engine (§5 of `docs/agent/reactive-engine-plan.md`).
//!
//! A self-contained, theory-grounded dependency engine: shared views with
//! exact fact dependencies, epoch transactions with round-based
//! propagation, deterministic under any worker count, with per-fact
//! ownership and cycle rejection. It has no dependency on parser or visual
//! framework internals.
//!
//! The authoring surface (`prelude`) exposes the handle constructors, the
//! kind witnesses and handles, opaque plans/running roots, and committed
//! snapshots. View dependencies are captured from handle effects executed
//! during the computation (plan §5.3).

use std::sync::Arc;

pub mod api;
pub mod kind;
pub(crate) mod engine;
pub(crate) mod error;
pub(crate) mod plain;
pub(crate) mod store;
pub(crate) mod trace;
pub(crate) mod value;
pub mod view;

/// ABI types used by generated view implementations.
/// Reads one committed fact WITHOUT recording a reactive dependency.
/// For read-modify-write publication owners inspecting their own prior output.
#[doc(hidden)]
pub fn peek_committed<V: View>(input: V::Input) -> Result<Option<Arc<V::Output>>>
where
    V::Output: Send + Sync + 'static,
{
    crate::reactive::plain::peek_committed_pub::<V>(input)
}

#[doc(hidden)]
pub mod __macro_private {
    #[track_caller]
    pub fn fresh_node_id<V: super::view::View>() -> super::Result<super::view::Node<V>> {
        super::plain::fresh_node_id::<V>()
    }
    pub use super::plain::{EffectContext, Temporal};
}
pub use api::{Planned, Running, run, run_each_key};
pub use plain::{state_cell, StateCell, StateValue};
#[doc(hidden)]
pub use plain::__snapshot_pub;
pub use engine::{
    CommandReport, Engine, EngineWork, InvocationIdentity, InvocationWork, KeyedFamily, Snapshot,
};
pub use error::{Error, Result};
pub use kind::{emit_patch, emit_view, observe_view};
pub use view::View;

#[cfg(test)]
mod tests;

/// The one-import authoring surface for plain reactive functions.
pub mod prelude {
    pub use super::api::{run, run_each_child, run_each_child_of, run_each_key, Planned, Running};
    pub use super::engine::{CommandReport, Engine, InvocationIdentity, InvocationWork, Snapshot};
    pub use super::error::{Error, Result};
    pub use super::kind::{
        EmitHandle, Graph, GraphEmit, GraphObserve, GraphView, List, ListEmit, ListKey, ListView,
        Map, MapEmit, MapObserve, MapView, ObserveHandle, Tree, TreeEmit, TreeObserve, TreeView,
        ViewKind, emit_view, observe_view,
    };
    /// The box-kind witness is deliberately absent from the glob (it would
    /// shadow `std::boxed::Box`); import it from `reactive::kind` where a
    /// box view is declared.
    pub use super::kind::BoxView;
    pub use super::view::View;
}
