//! Typed view kinds (plan §5): Map, List, Tree, Graph, Box.
//!
//! A view declares one *kind witness* as its single tuple field; the
//! `#[view]` macro reads the witness and generates the fact codec, the
//! emit handle API, and the observe handle API for that kind. Reactive and
//! incremental granularity is the smallest unit of each structure (map
//! entry, list slot + length, tree node + root list, graph node + edge
//! bucket, box cell) — every unit is one ordinary engine fact.
//!
//! The witness types exist only in types, never at runtime. Handles are
//! cheap value types (an [`EffectContext`] clone plus a marker); obtaining
//! one inside an effect or command registers the view and binds it to the
//! active computation. Using a stale handle after its invocation retires is
//! rejected by the engine's existing write-ownership checks.

use std::marker::PhantomData;
use std::sync::Arc;

use crate::reactive::plain::{EffectContext, Temporal};
use crate::reactive::view::{Node, View};
use crate::reactive::{Error, Result};

/// Bounds every encoded fact key.
pub trait KeyBounds:
    Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static
{
}
impl<T: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static> KeyBounds for T {}

/// Bounds every encoded fact payload component.
pub trait ValueBounds: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static {}
impl<T: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static> ValueBounds for T {}

// ---------------------------------------------------------------------------
// Kind witnesses
// ---------------------------------------------------------------------------

/// Witness of a keyed-entry view: one fact per present entry `K -> V`.
#[allow(non_camel_case_types)]
pub struct Map<Domain, Entry>(PhantomData<(Domain, Entry)>);

/// Witness of an ordered-list view under a domain key: one fact per slot
/// `(K, index)` plus one length fact per `K`.
#[allow(non_camel_case_types)]
pub struct List<Domain, Item>(PhantomData<(Domain, Item)>);

/// Witness of a rooted-forest view under a domain key: one fact per node
/// plus one root-list fact per `K`.
#[allow(non_camel_case_types)]
pub struct Tree<Domain, Payload>(PhantomData<(Domain, Payload)>);

/// Witness of a labelled-multigraph view: one fact per node payload and
/// one fact per labelled edge bucket.
#[allow(non_camel_case_types)]
pub struct Graph<NodePayload, Label>(PhantomData<(NodePayload, Label)>);

/// Witness of a single-cell view.
#[allow(non_camel_case_types)]
pub struct Box<Payload>(PhantomData<Payload>);

// ---------------------------------------------------------------------------
// Fact codecs
// ---------------------------------------------------------------------------

/// The fact-key space of a list-kind view: slot facts and length facts.
///
/// The length fact is the wake-on-growth companion: iterating records the
/// slot dependencies *and* the length dependency, so an append wakes
/// iterators while an in-place equal-value slot write stays cold (T4).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ListKey<K: KeyBounds> {
    /// One ordered slot under the domain key.
    Slot(K, u32),
    /// The length fact under the domain key.
    Len(K),
}

/// The fact-value space of a list-kind view.
#[derive(Clone, Debug, PartialEq)]
pub enum ListFact<I: ValueBounds> {
    /// One slot's item.
    Item(I),
    /// One domain key's list length.
    Len(u32),
}

/// The fact-key space of a tree-kind view (plan §11 granular facts).
///
/// Every semantic dimension is its own fact, so a payload-only edit does
/// not rewrite a node's child order or parent, and a child-order edit does
/// not rewrite payloads. Link ids are the stable child/root node ids, so
/// an order change never renumbers unrelated links.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TreeKey<K: KeyBounds, N: KeyBounds> {
    /// One node's payload fact.
    Payload(N),
    /// One node's optional parent fact.
    Parent(N),
    /// One node's ordered child-link root (link ids in emission order).
    ChildOrder(N),
    /// One child link of one node.
    ChildLink(N, u64),
    /// One domain key's ordered root-link root.
    RootOrder(K),
    /// One root link of one domain key.
    RootLink(K, u64),
}

/// The fact-value space of a tree-kind view.
#[derive(Clone, Debug, PartialEq)]
pub enum TreeFact<N: KeyBounds, P: ValueBounds> {
    /// One node's payload.
    Payload(P),
    /// One node's optional parent identity.
    Parent(Option<N>),
    /// One node's ordered child-link ids.
    Order(Arc<[u64]>),
    /// One child link's target identity.
    Link(N),
    /// One domain key's ordered root-link ids.
    RootOrder(Arc<[u64]>),
    /// One root link's root identity.
    RootLink(N),
}

/// The fact-key space of a graph-kind view.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GraphKey<Id: KeyBounds, L: KeyBounds> {
    /// One node payload fact.
    Node(Id),
    /// One labelled outgoing-edge bucket of one node.
    Bucket(Id, L),
}

/// The fact-value space of a graph-kind view.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphFact<P: ValueBounds, Id: KeyBounds> {
    /// One node's payload.
    Node(P),
    /// One edge bucket's targets in link order (deduplicated).
    Targets(Vec<Id>),
}

// ---------------------------------------------------------------------------
// Kind traits
// ---------------------------------------------------------------------------

/// A map-kind view: keyed entries, one fact per entry.
///
/// `Input` is the entry key and `Output` the entry payload; presence is
/// the fact itself. Writes are strictly owned (T5).
pub trait MapView: View {}

/// A list-kind view: ordered slots plus one length fact per domain key.
pub trait ListView: View<Input = ListKey<Self::Key>, Output = ListFact<Self::Item>> {
    /// The domain key grouping one ordered list.
    type Key: KeyBounds;
    /// The payload of one slot.
    type Item: ValueBounds;
}

/// A tree-kind view: rooted forests with one fact per node and one root
/// list per domain key.
pub trait TreeView:
    View<Input = TreeKey<Self::Key, Node<Self>>, Output = TreeFact<Node<Self>, Self::Payload>>
{
    /// The domain key grouping one forest.
    type Key: KeyBounds;
    /// The payload of one node.
    type Payload: ValueBounds;
}

/// A graph-kind view: node payloads plus labelled edge buckets.
///
/// Edge buckets are independently owned facts, so two components may write
/// different labels of the same node (multi-producer by ownership, §5.7).
pub trait GraphView:
    View<
        Input = GraphKey<Node<Self>, Self::Label>,
        Output = GraphFact<Self::NodePayload, Node<Self>>,
    >
{
    /// The payload of one node.
    type NodePayload: ValueBounds;
    /// The label multiplexing edge buckets of one node.
    type Label: KeyBounds;
}

/// A box-kind view: one cell.
pub trait BoxView: View<Input = ()> {}

/// Binds the handle pair of one view kind.
///
/// Only the `#[view]` macro implements this trait; the associated types are
/// the kind-specific handles whose methods record exact per-fact effects
/// through the bound [`EffectContext`].
pub trait ViewKind: View {
    /// The write-side handle constructed by [`emit_view`].
    type Emit: EmitHandle<Self>;
    /// The read-side handle constructed by [`observe_view`].
    type Observe: ObserveHandle<Self>;
    /// The patch handle constructed by [`emit_patch`] for kinds that
    /// support per-key publication. Kinds without patch semantics name
    /// [`NoPatch`] until their phase lands (plan §5.5).
    type Patch;
}

/// Construction seam implemented by a patch handle.
#[doc(hidden)]
pub trait PatchHandle<V: View>: Sized {
    /// Binds this patch handle to an effect context.
    fn construct(effect: EffectContext) -> Self;
}

/// Construction seam implemented by every emit handle.
#[doc(hidden)]
pub trait EmitHandle<V: View>: Sized {
    /// Binds this handle type to an effect context.
    fn construct(effect: EffectContext) -> Self;
}

/// Construction seam implemented by every observe handle.
#[doc(hidden)]
pub trait ObserveHandle<V: View>: Sized {
    /// Binds this handle type to an effect context.
    fn construct(effect: EffectContext) -> Self;
}
// ---------------------------------------------------------------------------
// Handle constructors (the runtime boundary)
// ---------------------------------------------------------------------------

/// Declares an emit effect over view `V` and returns its kind-specific
/// handle.
///
/// Obtaining the handle registers the view in the active computation;
/// every subsequent method call records the exact facts written. Calling
/// outside an active effect or command is the existing `context_for`
/// error.
#[track_caller]
pub fn emit_view<V: ViewKind>() -> Result<V::Emit> {
    let context = crate::reactive::plain::context_for("emit_view", V::name())?;
    V::__register(&context)?;
    Ok(<V::Emit as EmitHandle<V>>::construct(context))
}

/// Declares a PATCH effect over a view `V` and returns its kind-specific
/// handle (plan §5.5).
///
/// Patch handles record per-key operations instead of replace-whole-view
/// publications: untouched facts are neither read nor rewritten, and the
/// invocation retains ownership of exactly the keys it mentioned.
#[track_caller]
pub fn emit_patch<V>() -> Result<V::Patch>
where
    V: ViewKind,
    V::Patch: PatchHandle<V>,
{
    let context = crate::reactive::plain::context_for("emit_patch", V::name())?;
    V::__register(&context)?;
    Ok(<V::Patch as PatchHandle<V>>::construct(context))
}

/// Placeholder patch type for kinds whose patch handles arrive with their
/// consuming phases (list/tree/graph splices; plan §5.5 ledger note).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPatch;

/// The map-kind patch handle: per-key upsert and remove.
pub struct MapPatch<V: MapView> {
    context: crate::reactive::plain::EffectContext,
    _marker: std::marker::PhantomData<fn() -> V>,
}

impl<V: MapView> Clone for MapPatch<V> {
    fn clone(&self) -> Self {
        Self {
            context: self.context.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<V: MapView> std::fmt::Debug for MapPatch<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MapPatch")
    }
}

impl<V: MapView> MapPatch<V> {
    /// Inserts or updates one key's fact.
    pub fn upsert(&self, key: V::Input, value: V::Output) -> Result<()> {
        self.context.emit_patch::<V>(key, Some(value))
    }

    /// Removes one key's fact; removing an absent key is a no-op.
    pub fn remove(&self, key: V::Input) -> Result<()> {
        self.context.emit_patch::<V>(key, None)
    }
}

impl<V: MapView> PatchHandle<V> for MapPatch<V> {
    fn construct(context: EffectContext) -> Self {
        Self {
            context,
            _marker: std::marker::PhantomData,
        }
    }
}

/// The tree-kind patch handle: sparse node and root-list publication.
pub struct TreePatch<V: TreeView> {
    context: EffectContext,
    _marker: std::marker::PhantomData<fn() -> V>,
}

impl<V: TreeView> Clone for TreePatch<V> {
    fn clone(&self) -> Self {
        Self {
            context: self.context.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<V: TreeView> std::fmt::Debug for TreePatch<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TreePatch")
    }
}

impl<V: TreeView> PatchHandle<V> for TreePatch<V> {
    fn construct(context: EffectContext) -> Self {
        Self {
            context,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<V: TreeView> TreePatch<V> {
    /// Upserts one encoded node or root-list fact.
    pub fn upsert(
        &self,
        key: TreeKey<V::Key, Node<V>>,
        value: TreeFact<Node<V>, V::Payload>,
    ) -> Result<()> {
        self.context.emit_patch::<V>(key, Some(value))
    }

    /// Removes one encoded node or root-list fact.
    pub fn remove(&self, key: TreeKey<V::Key, Node<V>>) -> Result<()> {
        self.context.emit_patch::<V>(key, None)
    }
}
/// Reads one committed fact WITHOUT registering a reactive dependency.
///
/// This is the read-modify-write base for output owners that need to inspect
/// their own prior publication before deciding what to write (plan §5.5,
/// barrier-solutions §2.3). Unlike [`observe_view`], no `ReadDep` is recorded.
#[track_caller]
pub fn peek_view<V: ViewKind>() -> Result<V::Observe> {
    let context = crate::reactive::plain::peek_context_for("peek_view", V::name())?;
    V::__register(&context)?;
    Ok(<V::Observe as ObserveHandle<V>>::construct(context))
}

/// Declares an observe effect over view `V` and returns its kind-specific
/// handle.
#[track_caller]
pub fn observe_view<V: ViewKind>() -> Result<V::Observe> {
    let context = crate::reactive::plain::context_for("observe_view", V::name())?;
    V::__register(&context)?;
    Ok(<V::Observe as ObserveHandle<V>>::construct(context))
}

fn foreign_encoding() -> Error {
    Error::Internal("kind handle observed a foreign fact encoding".into())
}

/// The invocation-scoped pending-write overlay.
///
/// An invocation is a pure function from committed state to buffered
/// writes, so a handle's own earlier writes are invisible to committed
/// reads — and handles created at different call sites of one invocation
/// must still see each other's writes. The overlay therefore lives on the
/// ACTIVE EVALUATION FRAME (see `EffectContext::pending_put`), shared by
/// every handle of that invocation (plan §5.3). The resulting same-view
/// read-modify-write is not a computation cycle: the engine exempts it,
/// and equal writes keep re-evaluations cold (T4).
struct Pending<V: View>(
    std::marker::PhantomData<fn() -> V>,
);

impl<V: View> Pending<V> {
    fn new() -> Self {
        Self(std::marker::PhantomData)
    }

    fn write(
        &self,
        effect: &EffectContext,
        key: V::Input,
        value: Option<V::Output>,
    ) -> Result<()> {
        effect.pending_put::<V>(
            key.clone(),
            value.as_ref().map(|value| std::sync::Arc::new(value.clone())),
        );
        effect.emit::<V>(key, value)
    }

    fn read(&self, effect: &EffectContext, key: &V::Input) -> Result<Option<V::Output>> {
        if let Some(pending) = effect
            .pending_get::<V>(key)
            .map(|pending| pending.map(|arc| (*arc).clone()))
        {
            return Ok(pending);
        }
        Ok(effect
            .peek::<V>(key.clone())?
            .map(|shared| (*shared).clone()))
    }
}

// ---------------------------------------------------------------------------
// Map handles
// ---------------------------------------------------------------------------

/// Write-side handle of a map-kind view.
pub struct MapEmit<V: MapView>(EffectContext, PhantomData<fn() -> V>);

impl<V: MapView> EmitHandle<V> for MapEmit<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: MapView> MapEmit<V> {
    /// Inserts or updates one entry (an upsert).
    pub fn insert(&self, key: V::Input, value: V::Output) -> Result<()> {
        self.0.emit::<V>(key, Some(value))
    }

    /// Retracts one entry.
    pub fn remove(&self, key: V::Input) -> Result<()> {
        self.0.emit::<V>(key, None)
    }
}

/// Read-side handle of a map-kind view.
pub struct MapObserve<V: MapView>(EffectContext, PhantomData<fn() -> V>);

impl<V: MapView> ObserveHandle<V> for MapObserve<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: MapView> MapObserve<V> {
    /// Reads one entry's current payload.
    pub fn get(&self, key: &V::Input) -> Result<Option<Arc<V::Output>>> {
        self.0.observe::<V>(key.clone(), Temporal::Current)
    }

    /// Reads one entry's committed payload from the previous epoch.
    pub fn get_previous(&self, key: &V::Input) -> Result<Option<Arc<V::Output>>> {
        self.0.observe::<V>(key.clone(), Temporal::Previous)
    }

    /// Enumerates current keys and depends only on key insertion/removal.
    pub fn keys(&self) -> Result<Vec<V::Input>> {
        self.0.inputs_keyset::<V>(Temporal::Current)
    }

    /// Enumerates previous keys and depends only on key insertion/removal.
    pub fn keys_previous(&self) -> Result<Vec<V::Input>> {
        self.0.inputs_keyset::<V>(Temporal::Previous)
    }
}

// ---------------------------------------------------------------------------
// List handles
// ---------------------------------------------------------------------------

/// Write-side handle of a list-kind view.
pub struct ListEmit<V: ListView>(EffectContext, Pending<V>, PhantomData<fn() -> V>);

impl<V: ListView> EmitHandle<V> for ListEmit<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, Pending::new(), PhantomData)
    }
}

impl<V: ListView> ListEmit<V> {
    fn write_slot(&self, key: &V::Key, index: u32, item: V::Item) -> Result<()> {
        self.1.write(
            &self.0,
            ListKey::Slot(key.clone(), index),
            Some(ListFact::Item(item)),
        )
    }

    fn retract_slot(&self, key: &V::Key, index: u32) -> Result<()> {
        self.1.write(&self.0, ListKey::Slot(key.clone(), index), None)
    }

    fn write_len(&self, key: &V::Key, len: u32) -> Result<()> {
        self.1
            .write(&self.0, ListKey::Len(key.clone()), Some(ListFact::Len(len)))
    }

    /// Reads the pending-or-committed length under `key`.
    pub(crate) fn committed_len(&self, key: &V::Key) -> Result<usize> {
        let fact = self.1.read(&self.0, &ListKey::Len(key.clone()))?;
        Ok(match fact {
            Some(ListFact::Len(len)) => len as usize,
            _ => 0,
        })
    }

    /// Reads one pending-or-committed slot item.
    fn committed_item(&self, key: &V::Key, index: usize) -> Result<Option<V::Item>> {
        let fact = self
            .1
            .read(&self.0, &ListKey::Slot(key.clone(), index as u32))?;
        Ok(match fact {
            Some(ListFact::Item(item)) => Some(item),
            _ => None,
        })
    }

    /// Appends one item, growing the list by one slot.
    pub fn push(&self, key: &V::Key, item: V::Item) -> Result<()> {
        let len = self.committed_len(key)? as u32;
        self.write_slot(key, len, item)?;
        self.write_len(key, len.saturating_add(1))
    }

    /// Overwrites one slot in place. An equal value stays cold (T4).
    pub fn set(&self, key: &V::Key, index: usize, item: V::Item) -> Result<()> {
        self.write_slot(key, index as u32, item)
    }

    /// Removes one slot, shifting the tail left by one. Reorder-heavy data
    /// belongs in a map view instead (plan §9).
    pub fn remove(&self, key: &V::Key, index: usize) -> Result<()> {
        let len = self.committed_len(key)?;
        if index >= len {
            return Ok(());
        }
        for j in (index + 1)..len {
            match self.committed_item(key, j)? {
                Some(item) => self.write_slot(key, (j - 1) as u32, item)?,
                None => self.retract_slot(key, (j - 1) as u32)?,
            }
        }
        self.retract_slot(key, (len - 1) as u32)?;
        self.write_len(key, (len - 1) as u32)
    }

    /// Replaces the whole list under `key`.
    ///
    /// Every maintained slot is written on every evaluation — the engine
    /// retracts facts absent from an invocation's candidate set, so an
    /// incremental emitter must keep its full fact set in play. Equal slot
    /// values still publish nothing at commit time (T4).
    pub fn replace(&self, key: &V::Key, items: impl IntoIterator<Item = V::Item>) -> Result<()> {
        let items: Vec<V::Item> = items.into_iter().collect();
        let old_len = self.committed_len(key)?;
        for (index, item) in items.iter().enumerate() {
            self.write_slot(key, index as u32, item.clone())?;
        }
        for index in items.len()..old_len {
            self.retract_slot(key, index as u32)?;
        }
        self.write_len(key, items.len() as u32)
    }

    /// Retracts every slot and zeroes the length under `key`. Facts owned
    /// by other invocations keep their owners (T5).
    pub fn clear(&self, key: &V::Key) -> Result<()> {
        let old_len = self.committed_len(key)?;
        for index in 0..old_len {
            self.retract_slot(key, index as u32)?;
        }
        self.write_len(key, 0)
    }
}

/// Read-side handle of a list-kind view.
pub struct ListObserve<V: ListView>(EffectContext, PhantomData<fn() -> V>);

impl<V: ListView> ObserveHandle<V> for ListObserve<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: ListView> ListObserve<V> {
    /// Reads one slot's current item (one fact read).
    pub fn get(&self, key: &V::Key, index: usize) -> Result<Option<Arc<V::Item>>> {
        let fact =
            self.0
                .observe::<V>(ListKey::Slot(key.clone(), index as u32), Temporal::Current)?;
        Ok(match fact.as_deref() {
            Some(ListFact::Item(item)) => Some(Arc::new(item.clone())),
            _ => None,
        })
    }

    /// Reads the current length under `key` (one fact read).
    pub fn len(&self, key: &V::Key) -> Result<usize> {
        let fact = self
            .0
            .observe::<V>(ListKey::Len(key.clone()), Temporal::Current)?;
        Ok(match fact.as_deref() {
            Some(ListFact::Len(len)) => *len as usize,
            _ => 0,
        })
    }

    /// Reads all slots plus the length fact: `len + 1` dependencies, so an
    /// append wakes iterators while equal-value slot updates do not.
    pub fn iter(&self, key: &V::Key) -> Result<Vec<Arc<V::Item>>> {
        let len = self.len(key)?;
        let mut items = Vec::with_capacity(len);
        for index in 0..len {
            if let Some(item) = self.get(key, index)? {
                items.push(item);
            }
        }
        Ok(items)
    }

    /// Reads all slots plus the length fact from the previous epoch.
    pub fn iter_previous(&self, key: &V::Key) -> Result<Vec<Arc<V::Item>>> {
        let fact = self
            .0
            .observe::<V>(ListKey::Len(key.clone()), Temporal::Previous)?;
        let len = match fact.as_deref() {
            Some(ListFact::Len(len)) => *len as usize,
            _ => 0,
        };
        let mut items = Vec::with_capacity(len);
        for index in 0..len {
            let fact = self.0.observe::<V>(
                ListKey::Slot(key.clone(), index as u32),
                Temporal::Previous,
            )?;
            if let Some(ListFact::Item(item)) = fact.as_deref() {
                items.push(Arc::new(item.clone()));
            }
        }
        Ok(items)
    }

    /// Enumerates the domain keys that own list facts (a domain read).
    pub fn domains(&self) -> Result<Vec<V::Key>> {
        let mut seen: Vec<V::Key> = Vec::new();
        for input in self.0.inputs::<V>(Temporal::Current)? {
            let key = match input {
                ListKey::Slot(key, _) | ListKey::Len(key) => key,
            };
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        Ok(seen)
    }
}

// ---------------------------------------------------------------------------
// Tree handles
// ---------------------------------------------------------------------------

/// Write-side handle of a tree-kind view.
pub struct TreeEmit<V: TreeView>(EffectContext, Pending<V>, PhantomData<fn() -> V>);

impl<V: TreeView> EmitHandle<V> for TreeEmit<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, Pending::new(), PhantomData)
    }
}

impl<V: TreeView> TreeEmit<V> {
    /// Writes one node's payload, parent, order, and per-child links as
    /// separate facts. An equal fact stays cold per dimension (T4).
    fn write_node(
        &self,
        id: Node<V>,
        parent: Option<Node<V>>,
        payload: V::Payload,
        children: Vec<Node<V>>,
    ) -> Result<()> {
        self.1.write(
            &self.0,
            TreeKey::Payload(id),
            Some(TreeFact::Payload(payload)),
        )?;
        self.1.write(
            &self.0,
            TreeKey::Parent(id),
            Some(TreeFact::Parent(parent)),
        )?;
        self.write_order_links(id, &children)
    }

    /// Writes one node's ordered child-link ids and one link fact per child.
    fn write_order_links(&self, id: Node<V>, children: &[Node<V>]) -> Result<()> {
        let order: Arc<[u64]> = children.iter().map(|child| child.raw_id()).collect();
        self.1.write(
            &self.0,
            TreeKey::ChildOrder(id),
            Some(TreeFact::Order(order)),
        )?;
        for &child in children {
            self.1.write(
                &self.0,
                TreeKey::ChildLink(id, child.raw_id()),
                Some(TreeFact::Link(child)),
            )?;
        }
        Ok(())
    }

    /// Publishes one root with a freshly minted identity and appends it to
    /// the domain key's root list.
    pub fn root(&self, key: &V::Key, payload: V::Payload) -> Result<Node<V>> {
        let id = crate::reactive::plain::fresh_node_id::<V>()?;
        self.write_node(id, None, payload, Vec::new())?;
        self.append_roots(key, &[id])?;
        Ok(id)
    }

    /// Publishes one child under `parent` with a freshly minted identity
    /// and appends it to the parent's ordered child list.
    pub fn child(&self, parent: Node<V>, payload: V::Payload) -> Result<Node<V>> {
        let id = crate::reactive::plain::fresh_node_id::<V>()?;
        let mut children = self.read_children(&parent)?;
        // Re-running the same builder (identical call site, identical
        // deterministic id) must not append the child twice. The order is
        // still written every time so ownership (T5) survives re-runs;
        // the equal-value write stays cold (T4).
        if !children.contains(&id) {
            children.push(id);
        }
        self.write_order_links(parent, &children)?;
        self.write_node(id, Some(parent), payload, Vec::new())?;
        Ok(id)
    }

    /// Rewrites one node's payload, keeping its parent and children.
    pub fn set_payload(&self, id: Node<V>, payload: V::Payload) -> Result<()> {
        self.1.write(
            &self.0,
            TreeKey::Payload(id),
            Some(TreeFact::Payload(payload)),
        )
    }

    /// Rewrites one node's ordered children, keeping its parent and
    /// payload. Only the order fact plus changed link facts are written.
    /// Removed links are retracted; retained links stay cold (T4); the
    /// membership test is set-based, never a per-link vector scan; and the
    /// order fact is written on every call so ownership (T5) survives
    /// re-running builders (equal values stay cold).
    pub fn set_children(&self, id: Node<V>, children: Vec<Node<V>>) -> Result<()> {
        let before: Arc<[u64]> = match self.read_fact(&id)? {
            Some(TreeFact::Order(order)) => order,
            _ => Arc::from([]),
        };
        let after: Arc<[u64]> = children.iter().map(|child| child.raw_id()).collect();
        if before.as_ref() != after.as_ref() {
            let retained: std::collections::HashSet<u64> = after.iter().copied().collect();
            for link in before.iter().filter(|link| !retained.contains(link)) {
                self.1.write(&self.0, TreeKey::ChildLink(id, *link), None)?;
            }
        }
        self.write_order_links(id, &children)?;
        Ok(())
    }

    /// Applies one canonical ordered splice to `id`'s child list (plan
    /// §15.3). The contiguous run between the `before` anchor (exclusive)
    /// and the `after` anchor (exclusive) in the committed order must
    /// equal `removed` exactly; that run is replaced in place by
    /// `inserted`.
    ///
    /// Anchors are validated against the current order, so overlapping
    /// splices of one node fail at the second splice (its anchors no
    /// longer bound the run). Distinct splices of the same node in one
    /// command coalesce because the overlay reads the intermediate order.
    /// Only the order fact plus the retracted/inserted link facts are
    /// written; untouched links stay cold (T4).
    pub fn splice_children(
        &self,
        id: Node<V>,
        before: Option<Node<V>>,
        removed: &[Node<V>],
        inserted: &[Node<V>],
        after: Option<Node<V>>,
    ) -> Result<()> {
        let before_id = before.map(|node| node.raw_id());
        let after_id = after.map(|node| node.raw_id());
        let removed_ids: Vec<u64> = removed.iter().map(|node| node.raw_id()).collect();
        let inserted_ids: Vec<u64> = inserted.iter().map(|node| node.raw_id()).collect();

        let old: Arc<[u64]> = match self.read_fact(&id)? {
            Some(TreeFact::Order(order)) => order,
            _ => Arc::from([]),
        };
        let start = match before_id {
            Some(before) => old.iter().position(|link| *link == before).ok_or_else(|| {
                crate::reactive::Error::TopologyViolation {
                    view: V::name().to_string(),
                    message: format!(
                        "splice before-anchor {before} absent from child order of {id:?}"
                    ),
                }
            })? + 1,
            None => 0,
        };
        let end = match after_id {
            Some(after) => old.iter().position(|link| *link == after).ok_or_else(|| {
                crate::reactive::Error::TopologyViolation {
                    view: V::name().to_string(),
                    message: format!(
                        "splice after-anchor {after} absent from child order of {id:?}"
                    ),
                }
            })?,
            None => old.len(),
        };
        if end < start || old[start..end] != removed_ids[..] {
            return Err(crate::reactive::Error::TopologyViolation {
                view: V::name().to_string(),
                message: format!(
                    "splice removed run {removed_ids:?} does not equal committed run {:?} of {id:?}",
                    &old[start..end]
                ),
            });
        }
        // A link occupies exactly one order position: reject attempts to
        // re-insert a link that survives outside the spliced run.
        for inserted in &inserted_ids {
            if old[..start].contains(inserted) || old[end..].contains(inserted) {
                return Err(crate::reactive::Error::TopologyViolation {
                    view: V::name().to_string(),
                    message: format!(
                        "splice inserts {inserted} which already occupies another position of {id:?}"
                    ),
                });
            }
        }

        for removed in &removed_ids {
            self.1.write(&self.0, TreeKey::ChildLink(id, *removed), None)?;
        }
        let mut next: Arc<[u64]> = Arc::from(
            old[..start]
                .iter()
                .chain(inserted_ids.iter())
                .chain(old[end..].iter())
                .copied()
                .collect::<Vec<_>>(),
        );
        // Always write the order so ownership (T5) survives re-runs; an
        // unchanged order stays cold (T4).
        self.1.write(&self.0, TreeKey::ChildOrder(id), Some(TreeFact::Order(next)))?;
        for &child in inserted {
            self.1.write(
                &self.0,
                TreeKey::ChildLink(id, child.raw_id()),
                Some(TreeFact::Link(child)),
            )?;
        }
        Ok(())
    }

    /// Writes one complete node's payload/parent/order/link facts with a
    /// caller-chosen stable identity (the arena-backed publication path).
    /// Equal dimension facts stay cold (T4).
    pub fn set_node(
        &self,
        id: Node<V>,
        parent: Option<Node<V>>,
        payload: V::Payload,
        children: Vec<Node<V>>,
    ) -> Result<()> {
        self.write_node(id, parent, payload, children)
    }

    /// Retracts one node's payload, parent, and order facts. Descendant
    /// facts keep their own owners and are retracted when their writers
    /// retire (`retract_invocation`).
    pub fn remove_subtree(&self, id: Node<V>) -> Result<()> {
        self.1.write(&self.0, TreeKey::Payload(id), None)?;
        self.1.write(&self.0, TreeKey::Parent(id), None)?;
        self.1.write(&self.0, TreeKey::ChildOrder(id), None)
    }

    /// Writes one encoded tree fact directly (crate ABI for generated
    /// façades; equal writes stay cold, T4).
    #[doc(hidden)]
    pub fn put(
        &self,
        key: TreeKey<V::Key, Node<V>>,
        value: Option<TreeFact<Node<V>, V::Payload>>,
    ) -> Result<()> {
        self.1.write(&self.0, key, value)
    }

    /// Replaces the root-list facts of `key`. Removed root links are
    /// retracted (so a closed or replaced root never leaks its `RootLink`
    /// fact), retained links stay cold (T4), and the membership test is
    /// set-based rather than a vector scan. The order fact is written on
    /// every call — equal values stay cold while ownership (T5) survives
    /// re-running builders.
    pub fn replace_roots(&self, key: &V::Key, roots: &[Node<V>]) -> Result<()> {
        let before: Arc<[u64]> = match self.1.read(&self.0, &TreeKey::RootOrder(key.clone()))? {
            Some(TreeFact::RootOrder(order)) => order,
            _ => Arc::from([]),
        };
        let after: Arc<[u64]> = roots.iter().map(|root| root.raw_id()).collect();
        if before.as_ref() != after.as_ref() {
            let retained: std::collections::HashSet<u64> = after.iter().copied().collect();
            for link in before.iter().filter(|link| !retained.contains(link)) {
                self.1
                    .write(&self.0, TreeKey::RootLink(key.clone(), *link), None)?;
            }
        }
        self.1.write(
            &self.0,
            TreeKey::RootOrder(key.clone()),
            Some(TreeFact::RootOrder(after)),
        )?;
        for &root in roots {
            self.1.write(
                &self.0,
                TreeKey::RootLink(key.clone(), root.raw_id()),
                Some(TreeFact::RootLink(root)),
            )?;
        }
        Ok(())
    }

    fn append_roots(&self, key: &V::Key, ids: &[Node<V>]) -> Result<()> {
        let mut roots = self.read_roots(key)?;
        // Re-running the same builder with deterministic identities must
        // not duplicate the root list. The list is still written every
        // time so ownership (T5) survives re-runs; equal values stay cold
        // (T4).
        for id in ids {
            if !roots.contains(id) {
                roots.push(*id);
            }
        }
        self.replace_roots(key, &roots)
    }

    fn read_fact(&self, id: &Node<V>) -> Result<Option<TreeFact<Node<V>, V::Payload>>> {
        let fact = self.1.read(&self.0, &TreeKey::ChildOrder(*id))?;
        Ok(fact)
    }

    fn read_parent(&self, id: &Node<V>) -> Result<Option<Node<V>>> {
        Ok(match self.1.read(&self.0, &TreeKey::Parent(*id))? {
            Some(TreeFact::Parent(parent)) => parent,
            _ => None,
        })
    }

    fn read_payload(&self, id: &Node<V>) -> Result<Option<V::Payload>> {
        Ok(match self.1.read(&self.0, &TreeKey::Payload(*id))? {
            Some(TreeFact::Payload(payload)) => Some(payload),
            _ => None,
        })
    }

    fn read_children(&self, id: &Node<V>) -> Result<Vec<Node<V>>> {
        let Some(order) = self.read_fact(id)? else {
            return Ok(Vec::new());
        };
        let TreeFact::Order(order) = order else {
            return Ok(Vec::new());
        };
        let mut children = Vec::with_capacity(order.len());
        for link in order.iter() {
            match self.1.read(&self.0, &TreeKey::ChildLink(*id, *link))? {
                Some(TreeFact::Link(child)) => children.push(child),
                _ => {}
            }
        }
        Ok(children)
    }

    fn read_roots(&self, key: &V::Key) -> Result<Vec<Node<V>>> {
        let Some(fact) = self.1.read(&self.0, &TreeKey::RootOrder(key.clone()))? else {
            return Ok(Vec::new());
        };
        let TreeFact::RootOrder(order) = fact else {
            return Ok(Vec::new());
        };
        let mut roots = Vec::with_capacity(order.len());
        for link in order.iter() {
            match self
                .1
                .read(&self.0, &TreeKey::RootLink(key.clone(), *link))?
            {
                Some(TreeFact::RootLink(root)) => roots.push(root),
                _ => {}
            }
        }
        Ok(roots)
    }
}

/// Read-side handle of a tree-kind view.
pub struct TreeObserve<V: TreeView>(EffectContext, PhantomData<fn() -> V>);

impl<V: TreeView> ObserveHandle<V> for TreeObserve<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: TreeView> TreeObserve<V> {
    fn fact_of(
        &self,
        key: TreeKey<V::Key, Node<V>>,
        temporal: Temporal,
    ) -> Result<Option<TreeFact<Node<V>, V::Payload>>> {
        let fact = self.0.observe::<V>(key, temporal)?;
        Ok(fact.map(|fact| (*fact).clone()))
    }

    /// Reads one encoded tree fact without recording a dependency.
    #[doc(hidden)]
    pub fn fact_peek(
        &self,
        key: TreeKey<V::Key, Node<V>>,
    ) -> Result<Option<Arc<TreeFact<Node<V>, V::Payload>>>> {
        self.0.peek::<V>(key)
    }
    /// Reads one node's payload (one fact read).
    pub fn payload(&self, id: Node<V>) -> Result<Option<Arc<V::Payload>>> {
        Ok(match self.fact_of(TreeKey::Payload(id), Temporal::Current)? {
            Some(TreeFact::Payload(payload)) => Some(Arc::new(payload)),
            _ => None,
        })
    }

    /// Reads one node's ordered children (the order fact plus each link
    /// encountered; each read is a separate dependency).
    pub fn children(&self, id: Node<V>) -> Result<Vec<Node<V>>> {
        let Some(order) = self.fact_of(TreeKey::ChildOrder(id), Temporal::Current)? else {
            return Ok(Vec::new());
        };
        let TreeFact::Order(order) = order else {
            return Ok(Vec::new());
        };
        let mut children = Vec::with_capacity(order.len());
        for link in order.iter() {
            match self.fact_of(TreeKey::ChildLink(id, *link), Temporal::Current)? {
                Some(TreeFact::Link(child)) => children.push(child),
                _ => {}
            }
        }
        Ok(children)
    }

    /// Reads one node's parent from its own fact (no domain scan).
    pub fn parent(&self, id: Node<V>) -> Result<Option<Node<V>>> {
        Ok(match self.fact_of(TreeKey::Parent(id), Temporal::Current)? {
            Some(TreeFact::Parent(parent)) => parent,
            _ => None,
        })
    }

    /// Reads one domain key's committed root list.
    pub fn roots(&self, key: &V::Key) -> Result<Vec<Node<V>>> {
        let Some(order) = self.fact_of(TreeKey::RootOrder(key.clone()), Temporal::Current)? else {
            return Ok(Vec::new());
        };
        let TreeFact::RootOrder(order) = order else {
            return Ok(Vec::new());
        };
        let mut roots = Vec::with_capacity(order.len());
        for link in order.iter() {
            match self.fact_of(TreeKey::RootLink(key.clone(), *link), Temporal::Current)? {
                Some(TreeFact::RootLink(root)) => roots.push(root),
                _ => {}
            }
        }
        Ok(roots)
    }

    /// Reads one node's payload from the previous epoch.
    pub fn payload_previous(&self, id: Node<V>) -> Result<Option<Arc<V::Payload>>> {
        Ok(match self.fact_of(TreeKey::Payload(id), Temporal::Previous)? {
            Some(TreeFact::Payload(payload)) => Some(Arc::new(payload)),
            _ => None,
        })
    }

    /// Reads one node's previous-epoch children.
    pub fn children_previous(&self, id: Node<V>) -> Result<Vec<Node<V>>> {
        let Some(order) = self.fact_of(TreeKey::ChildOrder(id), Temporal::Previous)? else {
            return Ok(Vec::new());
        };
        let TreeFact::Order(order) = order else {
            return Ok(Vec::new());
        };
        let mut children = Vec::with_capacity(order.len());
        for link in order.iter() {
            match self.fact_of(TreeKey::ChildLink(id, *link), Temporal::Previous)? {
                Some(TreeFact::Link(child)) => children.push(child),
                _ => {}
            }
        }
        Ok(children)
    }

    /// Reads one encoded tree fact directly (crate ABI for generated
    /// façades that decode payload+children in one read).
    #[doc(hidden)]
    pub fn fact(
        &self,
        key: TreeKey<V::Key, Node<V>>,
        temporal: Temporal,
    ) -> Result<Option<Arc<TreeFact<Node<V>, V::Payload>>>> {
        self.0.observe::<V>(key, temporal)
    }

    /// Enumerates encoded fact keys (crate ABI for generated façades that
    /// aggregate across domains). The dependency is the view's fact
    /// keyset (plan §15.2): payload-value changes alone do not wake the
    /// enumerator, only structural insert/remove of keys.
    #[doc(hidden)]
    pub fn all_keys(&self, temporal: Temporal) -> Result<Vec<TreeKey<V::Key, Node<V>>>> {
        self.0.inputs_keyset::<V>(temporal)
    }

    /// Reads one node's previous-epoch parent from its own fact.
    pub fn parent_previous(&self, id: Node<V>) -> Result<Option<Node<V>>> {
        Ok(match self.fact_of(TreeKey::Parent(id), Temporal::Previous)? {
            Some(TreeFact::Parent(parent)) => parent,
            _ => None,
        })
    }

    /// Enumerates the previous epoch's forest domains (a root-domain read,
    /// plan §15.2): depends on the RootOrder keyset so only root-list
    /// insert/remove wakes it, never payload or child values.
    pub fn domains_previous(&self) -> Result<Vec<V::Key>> {
        let mut seen: Vec<V::Key> = Vec::new();
        for input in self.0.inputs_keyset::<V>(Temporal::Previous)? {
            if let TreeKey::RootOrder(key) = input
                && !seen.contains(&key) {
                    seen.push(key);
                }
        }
        Ok(seen)
    }

    /// Enumerates the domain keys that own forests (a root-domain read,
    /// plan §15.2): depends on the RootOrder keyset so only root-list
    /// insert/remove wakes it, never payload or child values.
    pub fn domains(&self) -> Result<Vec<V::Key>> {
        let mut seen: Vec<V::Key> = Vec::new();
        for input in self.0.inputs_keyset::<V>(Temporal::Current)? {
            if let TreeKey::RootOrder(key) = input
                && !seen.contains(&key) {
                    seen.push(key);
                }
        }
        Ok(seen)
    }
}

// ---------------------------------------------------------------------------
// Graph handles
// ---------------------------------------------------------------------------

/// Write-side handle of a graph-kind view.
pub struct GraphEmit<V: GraphView>(EffectContext, Pending<V>, PhantomData<fn() -> V>);

impl<V: GraphView> EmitHandle<V> for GraphEmit<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, Pending::new(), PhantomData)
    }
}

impl<V: GraphView> GraphEmit<V> {
    fn write_bucket(
        &self,
        from: &Node<V>,
        label: &V::Label,
        targets: Vec<Node<V>>,
    ) -> Result<()> {
        self.1.write(
            &self.0,
            GraphKey::Bucket(*from, label.clone()),
            Some(GraphFact::Targets(targets)),
        )
    }

    /// Allocates a fresh node identity and publishes its payload.
    pub fn mint(&self, payload: V::NodePayload) -> Result<Node<V>> {
        let id = crate::reactive::plain::fresh_node_id::<V>()?;
        self.set_node(id, payload)?;
        Ok(id)
    }

    /// Publishes or replaces one node's payload.
    pub fn node(&self, id: Node<V>, payload: V::NodePayload) -> Result<()> {
        self.set_node(id, payload)
    }

    /// Publishes or replaces one node's payload (alias of [`node`]).
    pub fn set_node(&self, id: Node<V>, payload: V::NodePayload) -> Result<()> {
        self.1
            .write(&self.0, GraphKey::Node(id), Some(GraphFact::Node(payload)))
    }

    /// Adds one labelled edge, appending to the bucket when absent. The
    /// bucket is rewritten on every evaluation (absent candidates retract);
    /// an equal bucket publishes nothing at commit time (T4).
    pub fn link(&self, from: Node<V>, label: V::Label, to: Node<V>) -> Result<()> {
        let mut targets = self.read_bucket(&from, &label)?;
        if !targets.contains(&to) {
            targets.push(to);
        }
        self.write_bucket(&from, &label, targets)
    }

    /// Removes one labelled edge if present.
    pub fn unlink(&self, from: Node<V>, label: V::Label, to: Node<V>) -> Result<()> {
        let targets: Vec<Node<V>> = self
            .read_bucket(&from, &label)?
            .into_iter()
            .filter(|target| *target != to)
            .collect();
        self.write_bucket(&from, &label, targets)
    }

    /// Retracts one node fact. Buckets referencing it keep their owners;
    /// readers that need liveness filter through [`GraphObserve::payload`].
    pub fn remove_node(&self, id: Node<V>) -> Result<()> {
        self.1.write(&self.0, GraphKey::Node(id), None)
    }

    fn read_bucket(&self, from: &Node<V>, label: &V::Label) -> Result<Vec<Node<V>>> {
        let fact = self
            .1
            .read(&self.0, &GraphKey::Bucket(*from, label.clone()))?;
        Ok(match fact {
            Some(GraphFact::Targets(targets)) => targets,
            _ => Vec::new(),
        })
    }
}

/// Read-side handle of a graph-kind view.
pub struct GraphObserve<V: GraphView>(EffectContext, PhantomData<fn() -> V>);

impl<V: GraphView> ObserveHandle<V> for GraphObserve<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: GraphView> GraphObserve<V> {
    fn fact_of(
        &self,
        key: GraphKey<Node<V>, V::Label>,
        temporal: Temporal,
    ) -> Result<Option<GraphFact<V::NodePayload, Node<V>>>> {
        let fact = self.0.observe::<V>(key, temporal)?;
        Ok(fact.map(|fact| (*fact).clone()))
    }

    /// Reads one node's payload (one fact read).
    pub fn payload(&self, id: Node<V>) -> Result<Option<Arc<V::NodePayload>>> {
        Ok(match self.fact_of(GraphKey::Node(id), Temporal::Current)? {
            Some(GraphFact::Node(payload)) => Some(Arc::new(payload)),
            _ => None,
        })
    }

    /// Reads all targets in one labelled edge bucket — exactly one fact
    /// read, replacing the legacy full-domain scan.
    pub fn outgoing(&self, from: Node<V>, label: &V::Label) -> Result<Vec<Node<V>>> {
        Ok(match self.fact_of(GraphKey::Bucket(from, label.clone()), Temporal::Current)? {
            Some(GraphFact::Targets(targets)) => targets,
            _ => Vec::new(),
        })
    }

    /// Enumerates the known node identities (a coarse domain read; kept
    /// for tests and whole-graph consumers, plan §10.4).
    pub fn nodes(&self) -> Result<Vec<Node<V>>> {
        let mut nodes = Vec::new();
        for input in self.0.inputs::<V>(Temporal::Current)? {
            if let GraphKey::Node(id) = input {
                nodes.push(id);
            }
        }
        Ok(nodes)
    }

    /// Reads one node's previous-epoch payload.
    pub fn payload_previous(&self, id: Node<V>) -> Result<Option<Arc<V::NodePayload>>> {
        Ok(match self.fact_of(GraphKey::Node(id), Temporal::Previous)? {
            Some(GraphFact::Node(payload)) => Some(Arc::new(payload)),
            _ => None,
        })
    }

    /// Reads one bucket's previous-epoch targets.
    pub fn outgoing_previous(
        &self,
        from: Node<V>,
        label: &V::Label,
    ) -> Result<Vec<Node<V>>> {
        Ok(match self.fact_of(GraphKey::Bucket(from, label.clone()), Temporal::Previous)? {
            Some(GraphFact::Targets(targets)) => targets,
            _ => Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Box handles
// ---------------------------------------------------------------------------

/// Write-side handle of a box-kind view.
pub struct BoxEmit<V: BoxView>(EffectContext, PhantomData<fn() -> V>);

impl<V: BoxView> EmitHandle<V> for BoxEmit<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: BoxView> BoxEmit<V> {
    /// Writes the cell.
    pub fn set(&self, value: V::Output) -> Result<()> {
        self.0.emit::<V>((), Some(value))
    }

    /// Retracts the cell.
    pub fn clear(&self) -> Result<()> {
        self.0.emit::<V>((), None)
    }
}

/// Read-side handle of a box-kind view.
pub struct BoxObserve<V: BoxView>(EffectContext, PhantomData<fn() -> V>);

impl<V: BoxView> ObserveHandle<V> for BoxObserve<V> {
    fn construct(effect: EffectContext) -> Self {
        Self(effect, PhantomData)
    }
}

impl<V: BoxView> BoxObserve<V> {
    /// Reads the cell's current payload.
    pub fn get(&self) -> Result<Option<Arc<V::Output>>> {
        self.0.observe::<V>((), Temporal::Current)
    }

    /// Reads the cell's previous-epoch payload.
    pub fn get_previous(&self) -> Result<Option<Arc<V::Output>>> {
        self.0.observe::<V>((), Temporal::Previous)
    }
}
