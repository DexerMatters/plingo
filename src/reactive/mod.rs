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

pub mod abstract_tree;
/// Ordinary `run` combinators: framework/test seam only after the Cut H
/// cutover (plan §7). Components own recursion and fan-out through child
/// calls; these helpers remain for crate-internal fixtures.
pub(crate) mod api;
pub mod component;
pub mod digest;
pub(crate) mod engine;
pub(crate) mod error;
pub mod framework_mount;
pub mod kind;
pub(crate) mod pathwork;
pub(crate) mod plain;
pub(crate) mod reaction;
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
    pub fn automatic_effect_node_id<V: super::view::View>() -> super::Result<super::view::Node<V>> {
        super::plain::automatic_effect_node_id::<V>()
    }
    pub use super::plain::{EffectContext, Temporal};
}
pub use abstract_tree::{
    AbstractTreeFamily, AbstractTreeNode, AstBox, ChildList, NodeSelector, RootSelector,
    SnapshotTree,
};
#[doc(hidden)]
/// Macro ABI for generated view implementations; see `abstract_tree`.
pub use abstract_tree::{TreeFact, TreeKey};
pub use component::{
    CaseChain, ComponentDefinition, Each, Effect, Effects, FamilyNode, GraphRender, Remove,
    Replace, Set, emit,
};
pub use digest::{FamilyState, SemanticDigest, render_diff};
pub use engine::{
    CommandReport, Engine, EngineWork, InvocationIdentity, InvocationWork, KeyedFamily, Snapshot,
};
pub use error::{Error, Result};
pub use kind::{emit_patch, emit_view, observe_view};
#[doc(hidden)]
pub use plain::__snapshot_pub;
pub use plain::{StateCell, StateValue, state_cell};
pub use reaction::{
    ElementEdge, EvaluatedComponent, OutputEdge, ReactionDigest, RetiredComponent, capture_enabled,
    disable_capture, enable_capture,
};
pub use view::View;

#[cfg(test)]
mod tests;

/// The one-import authoring surface for reactive applications.
///
/// Components take semantic inputs ([`Each`], [`AstBox<T>`] nodes, plain
/// values), read exact view effects, and return desired outputs. Raw
/// effect handles, ports, and free `run` combinators are framework
/// internals after the Cut H cutover (plan §7).
pub mod prelude {
    pub use super::abstract_tree::{
        AbstractTreeFamily, AbstractTreeNode, AstBox, ChildList, NodeSelector, RootSelector,
        SnapshotTree,
    };
    pub use super::component::{
        CaseChain, ComponentDefinition, Each, Effect, Effects, FamilyNode, GraphRender, Remove,
        Replace, Set, emit,
    };
    pub use super::engine::{Engine, Snapshot};
    pub use super::error::{Error, Result};
    pub use super::framework_mount::{
        BoxCell, MapEntries, MountComponent, MountComponentWithDomain, MountComponentWithProps,
        MountToken, MountTokenWithProps,
    };
    /// Not `std::boxed::Box`; import from `reactive::kind` where a box
    /// view is declared.
    pub use super::kind::BoxView;
    pub use super::kind::{
        Graph, GraphView, List, ListView, Map, MapView, Tree, TreeView, ViewKind,
    };
    pub use super::view::View;
    pub use crate::{abstract_tree, component, view};
}
