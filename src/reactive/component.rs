//! First-class reactive components (follow-up plan §6.1 / Cut C).
//!
//! A component is ONE named computation with TYPED PORTS whose identity is
//! `(definition marker TypeId, exact driving element)` — never a callsite,
//! installation ordinal, or worker order. The [`#[component]`](macro@crate::__component)
//! macro generates a zero-sized definition marker plus an installer; the
//! runtime stamps every evaluation with the definition id so reaction
//! graphs, retirement, and duplicate-install rejection all key off the
//! authored definition.
//!
//! Port kinds (Cut C scope):
//!
//! | Port | Meaning |
//! |---|---|
//! | `EachKey<V>` | one instance per present map key (membership lifecycle) |
//! | `Read<V>` | exact recorded reads through the view's observe handle |
//! | `Write<V>` | owned writes through the view's emit handle |
//!
//! Raw effect constructors stay crate-internal: application code reaches
//! effects only through these ports inside a `#[component]` body.

use crate::reactive::kind::{
    GraphEmit, GraphView, MapView, TreeEmit, TreeView, ViewKind,
};
use crate::reactive::{Error, Result};
use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

/// The runtime record of one installed component definition.
#[derive(Clone, Debug)]
pub(crate) struct DefinitionEntry {
    /// Module-qualified authored path (`module::function`).
    pub descriptor: &'static str,
    /// The driving-port kind wire name.
    pub driver: &'static str,
}

/// Per-engine registry of installed definitions. A second installer for the
/// same marker is a deterministic error before anything mutates.
#[derive(Default)]
pub(crate) struct DefinitionRegistry {
    by_marker: HashMap<TypeId, DefinitionEntry>,
}

impl DefinitionRegistry {
    pub(crate) fn register(
        &mut self,
        marker: TypeId,
        descriptor: &'static str,
        driver: &'static str,
    ) -> Result<()> {
        match self.by_marker.get(&marker) {
            Some(existing) => Err(Error::DuplicateComponent {
                descriptor: existing.descriptor.to_string(),
            }),
            None => {
                self.by_marker
                    .insert(marker, DefinitionEntry { descriptor, driver });
                Ok(())
            }
        }
    }

    pub(crate) fn descriptor_of(&self, marker: &TypeId) -> Option<&'static str> {
        self.by_marker.get(marker).map(|entry| entry.descriptor)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_marker.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_marker.is_empty()
    }

    /// Definitions in deterministic order for snapshots/reports.
    pub(crate) fn descriptors(&self) -> Vec<&'static str> {
        let mut rows: Vec<(String, &'static str)> = self
            .by_marker
            .values()
            .map(|entry| (entry.descriptor.to_string(), entry.descriptor))
            .collect();
        rows.sort();
        rows.into_iter().map(|(_, descriptor)| descriptor).collect()
    }
}

/// Implemented by the zero-sized marker the `#[component]` macro generates.
///
/// The descriptor is the module-qualified authored path; the registry uses
/// it for duplicate-install rejection and reaction attribution.
pub trait ComponentDefinition {
    #[doc(hidden)]
    fn __descriptor() -> &'static str;
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// Exact-value reads over one view, recorded as ordinary reactive deps.
pub struct Read<V: ViewKind> {
    _marker: PhantomData<fn() -> V>,
}

impl<V: ViewKind> Read<V> {
    /// Crate-internal attachment: only generated trampolines construct
    /// ports, always inside a running evaluation.
    #[doc(hidden)]
    pub fn __attach() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<V> Read<V>
where
    V: MapView + ViewKind<Observe = crate::reactive::kind::MapObserve<V>>,
{
    /// Reads one committed value, recording the exact element dep.
    pub fn get(&self, key: &V::Input) -> Result<Option<Arc<V::Output>>> {
        crate::reactive::kind::observe_view::<V>().and_then(|handle| handle.get(key))
    }
}

/// A generated automatic node output.
///
/// One output port owns one logical node per component instance. The runtime
/// derives its identity from the component definition, exact driving key, and
/// output-port ordinal; reevaluation therefore reuses the same node without
/// exposing identity construction to component bodies.
pub struct Output<V: ViewKind> {
    node: crate::reactive::view::Node<V>,
}

impl<V: ViewKind> Output<V> {
    /// Constructs one output at the generated component boundary.
    #[doc(hidden)]
    pub fn __attach<M, K>(key: K, port: u16) -> Result<Self>
    where
        M: ComponentDefinition + 'static,
        K: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
    {
        Ok(Self {
            node: crate::reactive::plain::automatic_node_id::<V, M, K>(key, port)?,
        })
    }

    pub fn node(&self) -> crate::reactive::view::Node<V> {
        self.node.clone()
    }
}

impl<V> Output<V>
where
    V: TreeView + ViewKind<Emit = TreeEmit<V>>,
{
    /// Publishes only this output node's payload.
    pub fn set_payload(&self, payload: V::Payload) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.put(
            crate::reactive::kind::TreeKey::Payload(self.node.clone()),
            Some(crate::reactive::kind::TreeFact::Payload(payload)),
        )
    }

    /// Publishes this output node's parent fact.
    pub fn set_parent(&self, parent: Option<crate::reactive::view::Node<V>>) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.put(
            crate::reactive::kind::TreeKey::Parent(self.node.clone()),
            Some(crate::reactive::kind::TreeFact::Parent(parent)),
        )
    }

    /// Replaces this output node's child order and link facts.
    pub fn set_children(&self, children: Vec<crate::reactive::view::Node<V>>) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.set_children(self.node.clone(), children)
    }

    /// Makes this output node the sole root for one forest domain.
    pub fn set_root(&self, key: &V::Key) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.replace_roots(key, &[self.node.clone()])
    }
}

impl<V> Output<V>
where
    V: GraphView + ViewKind<Emit = GraphEmit<V>>,
{
    /// Publishes this output node's graph payload.
    pub fn set_node(&self, payload: V::NodePayload) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.set_node(self.node.clone(), payload)
    }

    /// Adds a labelled edge from this output node.
    pub fn link(&self, label: V::Label, target: crate::reactive::view::Node<V>) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.link(self.node.clone(), label, target)
    }

    /// Removes a labelled edge from this output node.
    pub fn unlink(&self, label: V::Label, target: crate::reactive::view::Node<V>) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.unlink(self.node.clone(), label, target)
    }

    /// Retracts this output node's graph payload.
    pub fn remove_node(&self) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.remove_node(self.node.clone())
    }
}

/// Descriptive alias for generated node output ports.
pub type NodeOutput<V> = Output<V>;

/// Owned writes to one view, recorded as ordinary reactive writes.
pub struct Write<V: ViewKind> {
    _marker: PhantomData<fn() -> V>,
}

impl<V: ViewKind> Write<V> {
    #[doc(hidden)]
    pub fn __attach() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<V> Write<V>
where
    V: MapView + ViewKind<Emit = crate::reactive::kind::MapEmit<V>>,
{
    /// Upserts one entry under this component's ownership.
    pub fn insert(&self, key: V::Input, value: V::Output) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.insert(key, value)
    }

    /// Removes one entry; ownership of the key leaves this component.
    pub fn remove(&self, key: V::Input) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.remove(key)
    }
}

/// Membership-lifecycle driver: one instance per present map key.
///
/// The instance exists iff the key exists. A payload update reruns this
/// instance ONLY when its body records a read of that payload.
pub struct EachKey<V: ViewKind> {
    _marker: PhantomData<fn() -> V>,
}
