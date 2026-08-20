//! The authoring surface (§5.3): `Observed` / `Previous` / `Emitted`
//! handles, the visitor family of §5.2, nested visitors, and
//! write-outside-visitor rejection.
//!
//! The author writes ordinary functions over view handles; the engine
//! derives the dependency relation from the reads and writes executed
//! under each visitor. No relation context, task id, epoch, rank, worker
//! count, or timestamp appears here.
//!
//! Handle methods live in one extension trait per shape and handle family
//! (`BoxObservedExt`, `MapEmittedExt`, ...). Method resolution picks the
//! single applicable impl from the handle's view shape, so the same method
//! names are safe across shapes and no engine identifier appears in the
//! author's code beyond the prelude traits.

use std::any::TypeId;
use std::marker::PhantomData;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::reactive::engine::{Shared, downcast_value, fresh_identity};
use crate::reactive::error::{Error, Result};
use crate::reactive::store::{DynStore, WriteKind};
use crate::reactive::trace::{ChildKey, InstanceId, record_read, record_write, ACTIVE};
use crate::reactive::value::Value;
use crate::reactive::view::{
    BoxFactKey, BoxShape, GraphEdgeKey, GraphFactKey, GraphShape, MapFactKey, MapShape, NodeId,
    TreeFactKey, TreeShape, ViewSpec,
};


impl<V: ViewSpec> Clone for ObservedHandle<V> {
    fn clone(&self) -> Self {
        ObservedHandle {
            shared: Arc::clone(&self.shared),
            view: self.view,
            name: self.name,
            store: Arc::clone(&self.store),
            _marker: PhantomData,
        }
    }
}

impl<V: ViewSpec> Clone for PreviousHandle<V> {
    fn clone(&self) -> Self {
        PreviousHandle {
            shared: Arc::clone(&self.shared),
            view: self.view,
            name: self.name,
            store: Arc::clone(&self.store),
            _marker: PhantomData,
        }
    }
}

impl<V: ViewSpec> Clone for EmittedHandle<V> {
    fn clone(&self) -> Self {
        EmittedHandle {
            shared: Arc::clone(&self.shared),
            view: self.view,
            name: self.name,
            _marker: PhantomData,
        }
    }
}

/// The context handed to a component's root run by the engine.
pub struct RunContext<'a> {
    pub(crate) shared: &'a Arc<Shared>,
    /// The active component's registration ordinal (test hooks).
    #[allow(dead_code)]
    pub(crate) component: u32,
    /// The active instance's id (test hooks).
    #[allow(dead_code)]
    pub(crate) instance: u32,
}

impl RunContext<'_> {
    /// One observed (current-epoch) view handle.
    pub fn observed<V: ViewSpec>(&self) -> Result<ObservedHandle<V>, Error> {
        let (store, view, name) = self.shared.view_store::<V>()?;
        Ok(ObservedHandle {
            shared: Arc::clone(self.shared),
            view,
            name,
            store,
            _marker: PhantomData,
        })
    }

    /// One temporal (`Previous`, committed epoch t-1) view handle.
    pub fn previous<V: ViewSpec>(&self) -> Result<PreviousHandle<V>, Error> {
        let (store, view, name) = self.shared.view_store::<V>()?;
        Ok(PreviousHandle {
            shared: Arc::clone(self.shared),
            view,
            name,
            store,
            _marker: PhantomData,
        })
    }

    /// One emitted view handle. Writes attach to the active visitor.
    pub fn emitted<V: ViewSpec>(&self) -> Result<EmittedHandle<V>, Error> {
        let (_, view, name) = self.shared.view_store::<V>()?;
        Ok(EmittedHandle {
            shared: Arc::clone(self.shared),
            view,
            name,
            _marker: PhantomData,
        })
    }
}

/// A current-epoch observed view. All reads are recorded against the
/// active visitor (dynamic dependency capture, §4.1).
pub struct ObservedHandle<V: ViewSpec> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) view: u32,
    pub(crate) name: &'static str,
    pub(crate) store: Arc<dyn DynStore>,
    _marker: PhantomData<V>,
}

/// A temporal observed view: reads the committed epoch t-1 state, and the
/// read edges only schedule at the next epoch's start (§4.4).
pub struct PreviousHandle<V: ViewSpec> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) view: u32,
    pub(crate) name: &'static str,
    pub(crate) store: Arc<dyn DynStore>,
    _marker: PhantomData<V>,
}

/// An emitted view. Writes record against the active visitor and are
/// applied by the engine at round end (ownership-validated, diffed).
pub struct EmittedHandle<V: ViewSpec> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) view: u32,
    pub(crate) name: &'static str,
    _marker: PhantomData<V>,
}

// ---------------------------------------------------------------------------
// Shared handle machinery
// ---------------------------------------------------------------------------

impl<V: ViewSpec> ObservedHandle<V> {
    fn read(&self, fact: Arc<dyn crate::reactive::value::KeyValue>) -> Result<Option<Arc<dyn Value>>> {
        record_read(self.view, fact.clone(), false)?;
        Ok(self.store.read(fact.as_ref()))
    }

    /// Registers (or reuses) a child visitor and records it in the
    /// parent's run buffer (for retirement diffs). The caller then runs
    /// the child immediately.
    fn spawn_child(
        &self,
        kind: &'static str,
        key: Arc<dyn crate::reactive::value::KeyValue>,
        closure: Box<dyn FnMut() -> Result<()> + Send + Sync>,
    ) -> Result<InstanceId> {
        let parent = ACTIVE.with(|active| {
            let active = active.borrow();
            active
                .last()
                .map(|frame| frame.instance)
                .ok_or_else(|| Error::ReadOutsideVisitor {
                    view: self.name.to_string(),
                })
        })?;
        let rank = self.shared.view_rank(self.view);
        let child_key = ChildKey { kind, key };
        let child = self
            .shared
            .register_child(parent, child_key.clone(), rank, closure)?;
        // Record the registration for the round-end retirement diff.
        ACTIVE.with(|active| {
            let active = active.borrow();
            if let Some(frame) = active.last() {
                frame.buffer.lock().children.push(child_key);
            }
        });
        Ok(child)
    }
}

impl<V: ViewSpec> PreviousHandle<V> {
    fn read(&self, fact: Arc<dyn crate::reactive::value::KeyValue>) -> Result<Option<Arc<dyn Value>>> {
        record_read(self.view, fact.clone(), true)?;
        Ok(self.store.read_committed(fact.as_ref()))
    }
}

impl<V: ViewSpec> EmittedHandle<V> {
    /// Creates the handle for the active component's emitted view. The
    /// view must be registered (it is, during any run).
    pub fn new() -> Result<Self> {        let shared = ACTIVE.with(|active| {
            let active = active.borrow();
            let frame = active.last().ok_or_else(|| {
                Error::Internal("handle creation outside a visitor".into())
            })?;
            Ok::<Arc<Shared>, Error>(Arc::clone(&frame.shared))
        })?;
        let (_, view, name) = shared.view_store::<V>()?;
        Ok(EmittedHandle {
            shared,
            view,
            name,
            _marker: PhantomData,
        })
    }

    fn write(&self, kind: WriteKind) -> Result<()> {
        record_write(self.view, kind)
    }

    /// Mints a deterministic node identity: same component, view,
    /// allocation site (visitor path), and lane ⇒ same id, across epochs.
    /// The lane counts mints within one run, so stable code allocates
    /// stable ids (§5.6).
    pub fn fresh_node_id(&self) -> Result<NodeId> {
        let lane = ACTIVE.with(|active| {
            let active = active.borrow();
            let frame = active.last().ok_or_else(|| {
                Error::Internal("identity allocation outside a visitor".into())
            })?;
            let mut buffer = frame.buffer.lock();
            let lane = buffer.fresh_lane;
            buffer.fresh_lane += 1;
            Ok::<u64, Error>(lane)
        })?;
        let component = self.shared.active_component()?;
        let path = self.shared.active_path()?;
        Ok(NodeId(fresh_identity(
            component,
            TypeId::of::<V>(),
            &path,
            lane,
        )))
    }
}

// ---------------------------------------------------------------------------
// Box
// ---------------------------------------------------------------------------

/// Box observed-view methods.
pub trait BoxObservedExt<V: ViewSpec<Shape = BoxShape>> {
    /// Reads the box value (a presence read when absent).
    fn get(&self) -> Result<Option<Arc<V::Value>>>;
    /// One child visitor watching the box value.
    fn visit<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
}

impl<V: ViewSpec<Shape = BoxShape>> BoxObservedExt<V> for ObservedHandle<V> {
    fn get(&self) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(BoxFactKey::Value);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn visit<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let value = handle.get()?;
            f.lock()(value).map_err(Into::into)
        });
        let child = self.spawn_child("box", Arc::new(BoxFactKey::Value), closure)?;
        self.shared.run_instance(child);
        Ok(())
    }
}

/// Box previous-view methods.
pub trait BoxPreviousExt<V: ViewSpec<Shape = BoxShape>> {
    /// Reads the committed box value.
    fn get(&self) -> Result<Option<Arc<V::Value>>>;
}

impl<V: ViewSpec<Shape = BoxShape>> BoxPreviousExt<V> for PreviousHandle<V> {
    fn get(&self) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(BoxFactKey::Value);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
}

/// Box emitted-view methods.
pub trait BoxEmittedExt<V: ViewSpec<Shape = BoxShape>> {
    fn set(&self, value: V::Value) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

impl<V: ViewSpec<Shape = BoxShape>> BoxEmittedExt<V> for EmittedHandle<V> {
    fn set(&self, value: V::Value) -> Result<()> {
        self.write(WriteKind::BoxSet(Arc::new(value)))
    }
    fn clear(&self) -> Result<()> {
        self.write(WriteKind::BoxClear)
    }
}

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

/// Map observed-view methods.
pub trait MapObservedExt<V: ViewSpec<Shape = MapShape>> {
    /// Reads one entry (a presence read when the key is absent).
    fn get(&self, key: &V::Key) -> Result<Option<Arc<V::Value>>>;
    fn contains(&self, key: &V::Key) -> Result<bool>;
    /// Reads the ordered key registry.
    fn keys(&self) -> Result<Vec<V::Key>>;
    /// One child visitor per key, watching `entry(k)`.
    fn visit<F, E>(&self, key: V::Key, f: F) -> Result<()>
    where
        F: FnMut(V::Key, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
    /// Discovery over the key registry; one child visitor per present key.
    fn visit_each<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(V::Key, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
}

impl<V: ViewSpec<Shape = MapShape>> MapObservedExt<V> for ObservedHandle<V> {
    fn get(&self, key: &V::Key) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> =
            Arc::new(MapFactKey::Entry(key.clone()));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn contains(&self, key: &V::Key) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }
    fn keys(&self) -> Result<Vec<V::Key>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(MapFactKey::<V::Key>::Keys);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|keys: Arc<Vec<V::Key>>| (*keys).clone())
            .unwrap_or_default())
    }
    fn visit<F, E>(&self, key: V::Key, f: F) -> Result<()>
    where
        F: FnMut(V::Key, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let key_for_closure = key.clone();
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let value = handle.get(&key_for_closure)?;
            f.lock()(key_for_closure.clone(), value).map_err(Into::into)
        });
        let child_key: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(key);
        let child = self.spawn_child("map-entry", child_key, closure)?;
        self.shared.run_instance(child);
        Ok(())
    }
    fn visit_each<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(V::Key, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let keys = self.keys()?;
        let f = Arc::new(Mutex::new(f));
        for key in keys {
            let handle = Clone::clone(self);
            let f = Arc::clone(&f);
            let key_for_closure = key.clone();
            let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
                let value = handle.get(&key_for_closure)?;
                f.lock()(key_for_closure.clone(), value).map_err(Into::into)
            });
            let child_key: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(key);
            let child = self.spawn_child("map-entry", child_key, closure)?;
            self.shared.run_instance(child);
        }
        Ok(())
    }
}

/// Map previous-view methods.
pub trait MapPreviousExt<V: ViewSpec<Shape = MapShape>> {
    fn get(&self, key: &V::Key) -> Result<Option<Arc<V::Value>>>;
    fn keys(&self) -> Result<Vec<V::Key>>;
}

impl<V: ViewSpec<Shape = MapShape>> MapPreviousExt<V> for PreviousHandle<V> {
    fn get(&self, key: &V::Key) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> =
            Arc::new(MapFactKey::Entry(key.clone()));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn keys(&self) -> Result<Vec<V::Key>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(MapFactKey::<V::Key>::Keys);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|keys: Arc<Vec<V::Key>>| (*keys).clone())
            .unwrap_or_default())
    }
}

/// Map emitted-view methods.
pub trait MapEmittedExt<V: ViewSpec<Shape = MapShape>> {
    fn set(&self, key: V::Key, value: V::Value) -> Result<()>;
    fn remove(&self, key: V::Key) -> Result<()>;
    fn rekey(&self, from: V::Key, to: V::Key) -> Result<()>;
}

impl<V: ViewSpec<Shape = MapShape>> MapEmittedExt<V> for EmittedHandle<V> {
    fn set(&self, key: V::Key, value: V::Value) -> Result<()> {
        self.write(WriteKind::MapSet {
            key: Arc::new(key),
            value: Arc::new(value),
        })
    }
    fn remove(&self, key: V::Key) -> Result<()> {
        self.write(WriteKind::MapRemove {
            key: Arc::new(key),
        })
    }
    fn rekey(&self, from: V::Key, to: V::Key) -> Result<()> {
        self.write(WriteKind::MapRekey {
            from: Arc::new(from),
            to: Arc::new(to),
        })
    }
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

/// Tree observed-view methods.
pub trait TreeObservedExt<V: ViewSpec<Shape = TreeShape>> {
    /// Reads one node's payload.
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>>;
    /// Reads one node's ordered children.
    fn children(&self, id: NodeId) -> Result<Vec<NodeId>>;
    /// Reads one node's parent.
    fn parent(&self, id: NodeId) -> Result<Option<NodeId>>;
    /// Reads the ordered roots.
    fn roots(&self) -> Result<Vec<NodeId>>;
    /// One child visitor per node, watching `node(i)`.
    fn visit_node<F, E>(&self, id: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
    /// Discovery over the roots; one child per root, watching `node(i)`.
    fn visit_roots_each<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(NodeId) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
    /// Discovery over one node's children; one child per grandchild.
    fn visit_children_each<F, E>(&self, parent: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
    /// One child visitor per node, watching `parent(i)`.
    fn visit_parent<F, E>(&self, id: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<NodeId>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
}

impl<V: ViewSpec<Shape = TreeShape>> TreeObservedExt<V> for ObservedHandle<V> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Node(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn children(&self, id: NodeId) -> Result<Vec<NodeId>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Children(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|kids: Arc<Vec<NodeId>>| (*kids).clone())
            .unwrap_or_default())
    }
    fn parent(&self, id: NodeId) -> Result<Option<NodeId>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Parent(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .and_then(|parent: Arc<Option<NodeId>>| *parent))
    }
    fn roots(&self) -> Result<Vec<NodeId>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Roots);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|roots: Arc<Vec<NodeId>>| (*roots).clone())
            .unwrap_or_default())
    }
    fn visit_node<F, E>(&self, id: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let value = handle.node(id)?;
            f.lock()(id, value).map_err(Into::into)
        });
        let child = self.spawn_child("tree-node", Arc::new(TreeFactKey::Node(id)), closure)?;
        self.shared.run_instance(child);
        Ok(())
    }
    fn visit_roots_each<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(NodeId) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let roots = self.roots()?;
        let f = Arc::new(Mutex::new(f));
        for id in roots {
            let f = Arc::clone(&f);
            let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> =
                Box::new(move || f.lock()(id).map_err(Into::into));
            let child = self.spawn_child("tree-node", Arc::new(TreeFactKey::Node(id)), closure)?;
            self.shared.run_instance(child);
        }
        Ok(())
    }
    fn visit_children_each<F, E>(&self, parent: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let children = self.children(parent)?;
        let f = Arc::new(Mutex::new(f));
        for id in children {
            let f = Arc::clone(&f);
            let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> =
                Box::new(move || f.lock()(id).map_err(Into::into));
            let child = self.spawn_child("tree-node", Arc::new(TreeFactKey::Node(id)), closure)?;
            self.shared.run_instance(child);
        }
        Ok(())
    }
    fn visit_parent<F, E>(&self, id: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<NodeId>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let parent = handle.parent(id)?;
            f.lock()(id, parent).map_err(Into::into)
        });
        let child = self.spawn_child("tree-parent", Arc::new(TreeFactKey::Parent(id)), closure)?;
        self.shared.run_instance(child);
        Ok(())
    }
}

/// Tree previous-view methods.
pub trait TreePreviousExt<V: ViewSpec<Shape = TreeShape>> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>>;
    fn children(&self, id: NodeId) -> Result<Vec<NodeId>>;
    fn roots(&self) -> Result<Vec<NodeId>>;
}

impl<V: ViewSpec<Shape = TreeShape>> TreePreviousExt<V> for PreviousHandle<V> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Node(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn children(&self, id: NodeId) -> Result<Vec<NodeId>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Children(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|kids: Arc<Vec<NodeId>>| (*kids).clone())
            .unwrap_or_default())
    }
    fn roots(&self) -> Result<Vec<NodeId>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(TreeFactKey::Roots);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|roots: Arc<Vec<NodeId>>| (*roots).clone())
            .unwrap_or_default())
    }
}

/// Tree emitted-view methods.
pub trait TreeEmittedExt<V: ViewSpec<Shape = TreeShape>> {
    fn insert_node(&self, id: NodeId, data: V::Value) -> Result<()>;
    /// Ensures a node exists (no-op when it does).
    fn ensure_node(&self, id: NodeId) -> Result<()>;
    /// Insert-or-update: one op, no read-before-write.
    fn upsert_node(&self, id: NodeId, data: V::Value) -> Result<()>;
    fn update_node(&self, id: NodeId, data: V::Value) -> Result<()>;
    fn remove_node(&self, id: NodeId) -> Result<()>;
    fn reorder_children(&self, parent: NodeId, order: Vec<NodeId>) -> Result<()>;
    fn move_node(&self, id: NodeId, parent: NodeId) -> Result<()>;
}

impl<V: ViewSpec<Shape = TreeShape>> TreeEmittedExt<V> for EmittedHandle<V> {
    fn insert_node(&self, id: NodeId, data: V::Value) -> Result<()> {
        self.write(WriteKind::TreeInsertNode {
            id,
            data: Some(Arc::new(data)),
        })
    }
    fn ensure_node(&self, id: NodeId) -> Result<()> {
        self.write(WriteKind::TreeInsertNode { id, data: None })
    }
    fn upsert_node(&self, id: NodeId, data: V::Value) -> Result<()> {
        self.write(WriteKind::TreeUpsertNode {
            id,
            data: Arc::new(data),
        })
    }
    fn update_node(&self, id: NodeId, data: V::Value) -> Result<()> {
        self.write(WriteKind::TreeUpdateNode {
            id,
            data: Arc::new(data),
        })
    }
    fn remove_node(&self, id: NodeId) -> Result<()> {
        self.write(WriteKind::TreeRemoveNode { id })
    }
    fn reorder_children(&self, parent: NodeId, order: Vec<NodeId>) -> Result<()> {
        self.write(WriteKind::TreeReorderChildren { parent, order })
    }
    fn move_node(&self, id: NodeId, parent: NodeId) -> Result<()> {
        self.write(WriteKind::TreeMoveNode { id, parent })
    }
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

/// Graph observed-view methods.
pub trait GraphObservedExt<V: ViewSpec<Shape = GraphShape>> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>>;
    fn edge(&self, source: NodeId, label: &V::Label, target: NodeId) -> Result<Option<Arc<V::Edge>>>;
    /// Reads one outgoing bucket (ordered edge keys).
    fn outgoing(&self, source: NodeId, label: &V::Label) -> Result<Vec<GraphEdgeKey<V::Label>>>;
    /// Reads the ordered node registry.
    fn nodes(&self) -> Result<Vec<NodeId>>;
    fn visit_node<F, E>(&self, id: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
    fn visit_edge<F, E>(&self, source: NodeId, label: V::Label, target: NodeId, f: F) -> Result<()>
    where
        F: FnMut(GraphEdgeKey<V::Label>, Option<Arc<V::Edge>>) -> Result<(), E> + Send + Sync
            + 'static,
        E: Into<Error> + 'static;
    /// One child per (source, label) bucket, watching `bucket(s, l)`.
    fn visit_outgoing<F, E>(&self, source: NodeId, label: V::Label, f: F) -> Result<()>
    where
        F: FnMut(NodeId, V::Label, Vec<GraphEdgeKey<V::Label>>) -> Result<(), E> + Send + Sync
            + 'static,
        E: Into<Error> + 'static;
    /// Discovery over one bucket; one child per edge, watching `edge(e)`.
    fn visit_outgoing_each<F, E>(&self, source: NodeId, label: V::Label, f: F) -> Result<()>
    where
        F: FnMut(GraphEdgeKey<V::Label>, Option<Arc<V::Edge>>) -> Result<(), E> + Send + Sync
            + 'static,
        E: Into<Error> + 'static;
    /// Discovery over the node registry; one child per node.
    fn visit_nodes_each<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
}

impl<V: ViewSpec<Shape = GraphShape>> GraphObservedExt<V> for ObservedHandle<V> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(GraphFactKey::<V::Label>::Node(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn edge(
        &self,
        source: NodeId,
        label: &V::Label,
        target: NodeId,
    ) -> Result<Option<Arc<V::Edge>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> =
            Arc::new(GraphFactKey::Edge(GraphEdgeKey {
                source,
                label: label.clone(),
                target,
            }));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn outgoing(
        &self,
        source: NodeId,
        label: &V::Label,
    ) -> Result<Vec<GraphEdgeKey<V::Label>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> =
            Arc::new(GraphFactKey::Bucket(source, label.clone()));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|edges: Arc<Vec<GraphEdgeKey<V::Label>>>| (*edges).clone())
            .unwrap_or_default())
    }
    fn nodes(&self) -> Result<Vec<NodeId>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(GraphFactKey::<V::Label>::Nodes);
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|nodes: Arc<Vec<NodeId>>| (*nodes).clone())
            .unwrap_or_default())
    }
    fn visit_node<F, E>(&self, id: NodeId, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let value = handle.node(id)?;
            f.lock()(id, value).map_err(Into::into)
        });
        let child = self.spawn_child("graph-node", Arc::new(GraphFactKey::<V::Label>::Node(id)), closure)?;
        self.shared.run_instance(child);
        Ok(())
    }
    fn visit_edge<F, E>(&self, source: NodeId, label: V::Label, target: NodeId, f: F) -> Result<()>
    where
        F: FnMut(GraphEdgeKey<V::Label>, Option<Arc<V::Edge>>) -> Result<(), E> + Send + Sync
            + 'static,
        E: Into<Error> + 'static,
    {
        let edge = GraphEdgeKey {
            source,
            label: label.clone(),
            target,
        };
        let closure_edge = edge.clone();
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let value = handle.edge(closure_edge.source, &closure_edge.label, closure_edge.target)?;
            f.lock()(closure_edge.clone(), value).map_err(Into::into)
        });
        let child = self.spawn_child("graph-edge", Arc::new(GraphFactKey::<V::Label>::Edge(edge)), closure)?;
        self.shared.run_instance(child);
        Ok(())
    }
    fn visit_outgoing<F, E>(&self, source: NodeId, label: V::Label, f: F) -> Result<()>
    where
        F: FnMut(NodeId, V::Label, Vec<GraphEdgeKey<V::Label>>) -> Result<(), E> + Send + Sync
            + 'static,
        E: Into<Error> + 'static,
    {
        let handle = Clone::clone(self);
        let f = Arc::new(Mutex::new(f));
        let closure_label = label.clone();
        let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
            let edges = handle.outgoing(source, &closure_label)?;
            f.lock()(source, closure_label.clone(), edges).map_err(Into::into)
        });
        let child = self.spawn_child(
            "graph-bucket",
            Arc::new(GraphFactKey::Bucket(source, label.clone())),
            closure,
        )?;
        self.shared.run_instance(child);
        Ok(())
    }
    fn visit_outgoing_each<F, E>(&self, source: NodeId, label: V::Label, f: F) -> Result<()>
    where
        F: FnMut(GraphEdgeKey<V::Label>, Option<Arc<V::Edge>>) -> Result<(), E> + Send + Sync
            + 'static,
        E: Into<Error> + 'static,
    {
        let edges = self.outgoing(source, &label)?;
        let f = Arc::new(Mutex::new(f));
        for edge in edges {
            let closure_edge = edge.clone();
            let handle = Clone::clone(self);
            let f = Arc::clone(&f);
            let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
                let value = handle.edge(closure_edge.source, &closure_edge.label, closure_edge.target)?;
                f.lock()(closure_edge.clone(), value).map_err(Into::into)
            });
            let child =
                self.spawn_child("graph-edge", Arc::new(GraphFactKey::<V::Label>::Edge(edge)), closure)?;
            self.shared.run_instance(child);
        }
        Ok(())
    }
    fn visit_nodes_each<F, E>(&self, f: F) -> Result<()>
    where
        F: FnMut(NodeId, Option<Arc<V::Value>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let nodes = self.nodes()?;
        let f = Arc::new(Mutex::new(f));
        for id in nodes {
            let handle = Clone::clone(self);
            let f = Arc::clone(&f);
            let closure: Box<dyn FnMut() -> Result<()> + Send + Sync> = Box::new(move || {
                let value = handle.node(id)?;
                f.lock()(id, value).map_err(Into::into)
            });
            let child = self.spawn_child("graph-node", Arc::new(GraphFactKey::<V::Label>::Node(id)), closure)?;
            self.shared.run_instance(child);
        }
        Ok(())
    }
}

/// Graph previous-view methods.
pub trait GraphPreviousExt<V: ViewSpec<Shape = GraphShape>> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>>;
    fn outgoing(&self, source: NodeId, label: &V::Label) -> Result<Vec<GraphEdgeKey<V::Label>>>;
}

impl<V: ViewSpec<Shape = GraphShape>> GraphPreviousExt<V> for PreviousHandle<V> {
    fn node(&self, id: NodeId) -> Result<Option<Arc<V::Value>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> = Arc::new(GraphFactKey::<V::Label>::Node(id));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value)))
    }
    fn outgoing(
        &self,
        source: NodeId,
        label: &V::Label,
    ) -> Result<Vec<GraphEdgeKey<V::Label>>> {
        let fact: Arc<dyn crate::reactive::value::KeyValue> =
            Arc::new(GraphFactKey::Bucket(source, label.clone()));
        Ok(self
            .read(fact)?
            .and_then(|value| downcast_value(value))
            .map(|edges: Arc<Vec<GraphEdgeKey<V::Label>>>| (*edges).clone())
            .unwrap_or_default())
    }
}

/// Graph emitted-view methods.
pub trait GraphEmittedExt<V: ViewSpec<Shape = GraphShape>> {
    fn insert_node(&self, id: NodeId, data: V::Value) -> Result<()>;
    /// Ensures a node exists (no-op when it does).
    fn ensure_node(&self, id: NodeId) -> Result<()>;
    /// Insert-or-update: one op, no read-before-write.
    fn upsert_node(&self, id: NodeId, data: V::Value) -> Result<()>;
    fn update_node(&self, id: NodeId, data: V::Value) -> Result<()>;
    fn remove_node(&self, id: NodeId) -> Result<()>;
    fn insert_edge(
        &self,
        source: NodeId,
        label: V::Label,
        target: NodeId,
        data: V::Edge,
    ) -> Result<()>;
    fn remove_edge(&self, source: NodeId, label: V::Label, target: NodeId) -> Result<()>;
    fn replace_bucket(&self, source: NodeId, label: V::Label, targets: Vec<NodeId>) -> Result<()>;
}

impl<V: ViewSpec<Shape = GraphShape>> GraphEmittedExt<V> for EmittedHandle<V> {
    fn insert_node(&self, id: NodeId, data: V::Value) -> Result<()> {
        self.write(WriteKind::GraphInsertNode {
            id,
            data: Some(Arc::new(data)),
        })
    }
    fn ensure_node(&self, id: NodeId) -> Result<()> {
        self.write(WriteKind::GraphInsertNode { id, data: None })
    }
    fn upsert_node(&self, id: NodeId, data: V::Value) -> Result<()> {
        self.write(WriteKind::GraphUpsertNode {
            id,
            data: Arc::new(data),
        })
    }
    fn update_node(&self, id: NodeId, data: V::Value) -> Result<()> {
        self.write(WriteKind::GraphUpdateNode {
            id,
            data: Arc::new(data),
        })
    }
    fn remove_node(&self, id: NodeId) -> Result<()> {
        self.write(WriteKind::GraphRemoveNode { id })
    }
    fn insert_edge(
        &self,
        source: NodeId,
        label: V::Label,
        target: NodeId,
        data: V::Edge,
    ) -> Result<()> {
        self.write(WriteKind::GraphInsertEdge {
            source,
            label: Arc::new(label),
            target,
            data: Arc::new(data),
        })
    }
    fn remove_edge(&self, source: NodeId, label: V::Label, target: NodeId) -> Result<()> {
        self.write(WriteKind::GraphRemoveEdge {
            source,
            label: Arc::new(label),
            target,
        })
    }
    fn replace_bucket(&self, source: NodeId, label: V::Label, targets: Vec<NodeId>) -> Result<()> {
        self.write(WriteKind::GraphReplaceBucket {
            source,
            label: Arc::new(label),
            targets,
        })
    }
}

// Re-export the key trait for the prelude.

/// The author-facing observed-view handle (the plan's `Observed<V>`).
pub type Observed<V> = ObservedHandle<V>;

/// The author-facing temporal-view handle (the plan's `Previous<V>`).
pub type Previous<V> = PreviousHandle<V>;

/// The author-facing emitted-view handle (the plan's `Emitted<V>`).
pub type Emitted<V> = EmittedHandle<V>;
