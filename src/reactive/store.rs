//! Per-shape stores whose reconciliation is a pure diff (§5.2, T4).
//!
//! Each store holds three snapshots: `committed` (the last committed
//! state), `working` (the current epoch's tentative state), and
//! `round_base` (the state at the start of the current round). An op is
//! validated for ownership against `working`, applied to `working`, and
//! reported as a delta only when the value differs from `round_base`.
//! Equal candidate writes keep the committed `Arc` and revision (T4,
//! identity preservation).
//!
//! Snapshots share `Arc`s by clone, so rollback is a drop-and-reclone and
//! unchanged facts keep their allocation identity across epochs.
//!
//! # Ownership (§4.4, T5)
//!
//! Payload facts (`Box::Value`, `Map::Entry(k)`, `Tree::Node(i)`,
//! `Graph::Node(i)`, `Graph::Edge(e)`) are owned by the producer that
//! first publishes them in the current lineage; removal releases the
//! owner slot, and re-creation starts a new lineage. A write to a fact
//! owned by another producer is a deterministic validation error that
//! aborts the epoch.
//!
//! Structural facts (`Map::Keys`, `Tree::Roots`/`Children(p)`/`Parent(i)`,
//! `Graph::Nodes`/`Bucket(s,l)`) are the shared union structure of the
//! view: every producer mutates them through the op algebra, and the
//! merged result is deterministic because ops apply in deterministic
//! order. This is what makes multi-producer views ordinary for every
//! shape, not a Graph-shaped exception.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::reactive::error::{Error, Producer, Result};
use crate::reactive::value::{KeySpec, KeyValue, Value};
use crate::reactive::view::{
    BoxFactKey, GraphEdgeKey, GraphFactKey, MapFactKey, NodeId, TreeFactKey,
};

/// One materialized fact: an optional value (None is `⊥`), its revision,
/// and its owning producer.
#[derive(Clone)]
pub(crate) struct FactEntry {
    pub value: Option<Arc<dyn Value>>,
    pub revision: u64,
    pub owner: Producer,
}

/// One fact's value change within a round (vs the round base).
#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub key: Arc<dyn KeyValue>,
    pub prev: Option<Arc<dyn Value>>,
    pub next: Option<Arc<dyn Value>>,
}

/// A type-erased write op, as recorded by the authoring surface. The
/// target view is carried by the engine's dispatch, not the op itself.
#[derive(Clone, Debug)]
pub(crate) enum WriteKind {
    BoxSet(Arc<dyn Value>),
    BoxClear,
    MapSet {
        key: Arc<dyn KeyValue>,
        value: Arc<dyn Value>,
    },
    MapRemove {
        key: Arc<dyn KeyValue>,
    },
    MapRekey {
        from: Arc<dyn KeyValue>,
        to: Arc<dyn KeyValue>,
    },
    TreeInsertNode {
        id: NodeId,
        data: Option<Arc<dyn Value>>,
    },
    /// Insert-or-update: one op, no read-before-write (so re-ensuring a
    /// node never creates a fact-level self-dependency).
    TreeUpsertNode {
        id: NodeId,
        data: Arc<dyn Value>,
    },
    TreeUpdateNode {
        id: NodeId,
        data: Arc<dyn Value>,
    },
    TreeRemoveNode {
        id: NodeId,
    },
    TreeReorderChildren {
        parent: NodeId,
        order: Vec<NodeId>,
    },
    TreeMoveNode {
        id: NodeId,
        parent: NodeId,
    },
    GraphInsertNode {
        id: NodeId,
        data: Option<Arc<dyn Value>>,
    },
    /// Insert-or-update: one op, no read-before-write.
    GraphUpsertNode {
        id: NodeId,
        data: Arc<dyn Value>,
    },
    GraphUpdateNode {
        id: NodeId,
        data: Arc<dyn Value>,
    },
    GraphRemoveNode {
        id: NodeId,
    },
    GraphInsertEdge {
        source: NodeId,
        label: Arc<dyn KeyValue>,
        target: NodeId,
        data: Arc<dyn Value>,
    },
    GraphRemoveEdge {
        source: NodeId,
        label: Arc<dyn KeyValue>,
        target: NodeId,
    },
    GraphReplaceBucket {
        source: NodeId,
        label: Arc<dyn KeyValue>,
        targets: Vec<NodeId>,
    },
}

/// The shape-generic store interface used by the engine.
pub(crate) trait DynStore: Send + Sync {
    fn begin_epoch(&self);
    fn apply(&self, writer: Producer, instance: u32, op: &WriteKind) -> Result<Vec<Change>>;
    /// Retracts one fact (a retired visitor's contribution dies with it).
    fn retract(&self, writer: Producer, key: &dyn KeyValue) -> Result<Vec<Change>>;
    /// Applies the round's deferred ops and advances the round base.
    /// Returns the deferred ops' changes (attributed to their writers).
    fn end_round(&self) -> Result<Vec<(u32, Change)>, Error>;
    fn commit(&self);
    fn rollback(&self);
    /// Read the current epoch's working value of one fact (Observed).
    fn read(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>>;
    /// Read the committed value of one fact (Previous, snapshots).
    fn read_committed(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>>;
    /// Test hook: the committed revision of one fact.
    #[allow(dead_code)]
    fn debug_revision(&self, fact: &dyn KeyValue) -> Option<u64>;
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Downcasts an erased key; a mismatch is an internal bug, not user error.
fn downcast_key<'a, K: KeySpec>(key: &'a dyn KeyValue) -> Result<&'a K, Error> {
    key.as_any()
        .downcast_ref::<K>()
        .ok_or_else(|| Error::Internal("key type mismatch in store".into()))
}

/// Downcasts an erased label.
fn downcast_label<'a, L: KeySpec>(label: &'a dyn KeyValue) -> Result<&'a L, Error> {
    label
        .as_any()
        .downcast_ref::<L>()
        .ok_or_else(|| Error::Internal("label type mismatch in store".into()))
}

/// Attaches the store's view name to an ownership error raised without one.
fn with_view(error: Error, name: &str) -> Error {
    match error {
        Error::OwnershipViolation { view, fact, writer, owner } if view.is_empty() => {
            Error::OwnershipViolation {
                view: name.to_string(),
                fact,
                writer,
                owner,
            }
        }
        other => other,
    }
}

/// One op whose precondition was not met at application time. Structure
/// created by the same round (a node inserted by another visitor, an edge
/// endpoint created in the same round) is legal, so the op is deferred to
/// the round end, where the final candidate state decides (topology is a
/// round-level property, not an application-order accident).
struct PendingOp {
    instance: u32,
    writer: Producer,
    op: WriteKind,
}

/// The three-snapshot lifecycle shared by every store.
struct StoreState<S: Clone> {
    committed: S,
    working: S,
    round_base: S,
    pending: Vec<PendingOp>,
    /// The instance id of the op currently being applied (for pending
    /// attribution); u32::MAX when not applying.
    pending_instance: u32,
}

impl<S: Clone> StoreState<S> {
    fn new(initial: S) -> Self {
        StoreState {
            committed: initial.clone(),
            working: initial.clone(),
            round_base: initial,
            pending: Vec::new(),
            pending_instance: u32::MAX,
        }
    }
    fn begin_epoch(&mut self) {
        self.working = self.committed.clone();
        self.round_base = self.committed.clone();
        self.pending.clear();
    }
    fn end_round(&mut self) {
        self.round_base = self.working.clone();
    }
    fn commit(&mut self) {
        self.committed = self.working.clone();
    }
    fn rollback(&mut self) {
        self.working = self.committed.clone();
        self.round_base = self.committed.clone();
        self.pending.clear();
    }
}

/// Applies one value write to `working`, reporting the delta against
/// `base` (the round base). Owned facts must be written by their owner; an
/// absent owned fact is claimed by the first publisher of the current
/// lineage. Removing a fact (value `None`) releases the owner slot.
/// Structural facts are exempt from ownership. An equal write keeps the
/// committed `Arc` and revision (T4).
fn put(
    working: &mut HashMap<u32, FactEntry>,
    base: &HashMap<u32, FactEntry>,
    writer: Producer,
    structural: bool,
    ordinal: u32,
    key: Arc<dyn KeyValue>,
    value: Option<Arc<dyn Value>>,
) -> Result<Option<Change>, Error> {
    if !structural {
        if let Some(entry) = working.get(&ordinal) {
            if entry.owner != writer {
                return Err(Error::OwnershipViolation {
                    view: String::new(),
                    fact: format!("{key:?}"),
                    writer: writer.label(),
                    owner: entry.owner.label(),
                });
            }
        }
    }
    let prev = base.get(&ordinal).and_then(|entry| entry.value.clone());
    let next = match value {
        None => {
            if working.remove(&ordinal).is_some() {
                None
            } else {
                return Ok(None);
            }
        }
        Some(value) => {
            let entry = working.entry(ordinal).or_insert_with(|| FactEntry {
                value: None,
                revision: 0,
                owner: if structural { Producer::Structural } else { writer },
            });
            let equal = match &entry.value {
                Some(existing) => existing.value_eq(value.as_ref()),
                None => false,
            };
            if !equal {
                entry.value = Some(value);
                entry.revision += 1;
            }
            entry.value.clone()
        }
    };
    let changed = match (&prev, &next) {
        (Some(a), Some(b)) => !a.value_eq(b.as_ref()),
        (None, None) => false,
        _ => true,
    };
    Ok(if changed {
        Some(Change { key, prev, next })
    } else {
        None
    })
}

// ---------------------------------------------------------------------------
// Box store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BoxSnap {
    facts: HashMap<u32, FactEntry>,
}

pub(crate) struct BoxStore {
    state: Mutex<StoreState<BoxSnap>>,
    name: &'static str,
}

impl BoxStore {
    fn apply_inner(
        &self,
        state: &mut StoreState<BoxSnap>,
        writer: Producer,
        op: &WriteKind,
    ) -> Result<Vec<Change>> {
        let value = match op {
            WriteKind::BoxSet(value) => Some(value.clone()),
            WriteKind::BoxClear => None,
            _ => return Err(Error::Internal("op/shape mismatch: box".into())),
        };
        let StoreState { working, round_base, .. } = state;
        let change = put(
            &mut working.facts,
            &round_base.facts,
            writer,
            false,
            0,
            box_key(),
            value,
        )
        .map_err(|e| with_view(e, self.name))?;
        Ok(change.into_iter().collect())
    }

    pub(crate) fn new(name: &'static str) -> Self {
        BoxStore {
            state: Mutex::new(StoreState::new(BoxSnap {
                facts: HashMap::new(),
            })),
            name,
        }
    }
}

impl DynStore for BoxStore {
    fn begin_epoch(&self) {
        self.state.lock().begin_epoch();
    }
    fn apply(&self, writer: Producer, _instance: u32, op: &WriteKind) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        self.apply_inner(&mut state, writer, op)
    }
    fn retract(&self, writer: Producer, _key: &dyn KeyValue) -> Result<Vec<Change>> {
        self.apply(writer, u32::MAX, &WriteKind::BoxClear)
    }
    fn end_round(&self) -> Result<Vec<(u32, Change)>, Error> {
        let mut state = self.state.lock();
        let pending = std::mem::take(&mut state.pending);
        let mut out = Vec::new();
        for op in pending {
            state.pending_instance = op.instance;
            let changes = self.apply_inner(&mut state, op.writer, &op.op)?;
            state.pending_instance = u32::MAX;
            for change in changes {
                out.push((op.instance, change));
            }
        }
        state.end_round();
        Ok(out)
    }
    fn commit(&self) {
        self.state.lock().commit();
    }
    fn rollback(&self) {
        self.state.lock().rollback();
    }
    fn read(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let _ = fact;
        state
            .working
            .facts
            .get(&0)
            .and_then(|entry| entry.value.clone())
    }
    fn read_committed(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let _ = fact;
        state
            .committed
            .facts
            .get(&0)
            .and_then(|entry| entry.value.clone())
    }
    fn debug_revision(&self, _fact: &dyn KeyValue) -> Option<u64> {
        let state = self.state.lock();
        state.committed.facts.get(&0).map(|entry| entry.revision)
    }
}

fn box_key() -> Arc<dyn KeyValue> {
    Arc::new(BoxFactKey::Value)
}

// ---------------------------------------------------------------------------
// Map store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MapSnap<K: KeySpec> {
    /// Key → entry ordinal, in rank order (rank retained across rekey).
    entries: IndexMap<K, u32>,
    facts: HashMap<u32, FactEntry>,
    /// Next entry ordinal; ordinal 0 is the Keys fact. Never reused.
    counter: u32,
}

pub(crate) struct MapStore<K: KeySpec> {
    state: Mutex<StoreState<MapSnap<K>>>,
    name: &'static str,
    _marker: std::marker::PhantomData<K>,
}

impl<K: KeySpec> MapStore<K> {
    fn apply_inner(
        &self,
        state: &mut StoreState<MapSnap<K>>,
        writer: Producer,
        op: &WriteKind,
    ) -> Result<Vec<Change>> {
        match op {
            WriteKind::MapSet { key, value } => {
                self.set(state, writer, key.as_ref(), value.clone())
            }
            WriteKind::MapRemove { key } => self.remove(state, writer, key.as_ref()),
            WriteKind::MapRekey { from, to } => {
                self.rekey(state, writer, from.as_ref(), to.as_ref())
            }
            _ => Err(Error::Internal("op/shape mismatch: map".into())),
        }
    }

    pub(crate) fn new(name: &'static str) -> Self {
        MapStore {
            state: Mutex::new(StoreState::new(MapSnap {
                entries: IndexMap::new(),
                facts: HashMap::new(),
                counter: 1,
            })),
            name,
            _marker: std::marker::PhantomData,
        }
    }

    fn entry_ordinal(snap: &MapSnap<K>, key: &K) -> Option<u32> {
        snap.entries.get(key).copied()
    }

    fn keys_value(snap: &MapSnap<K>) -> Arc<dyn Value> {
        let keys: Vec<K> = snap.entries.keys().cloned().collect();
        Arc::new(keys)
    }

    fn set(
        &self,
        state: &mut StoreState<MapSnap<K>>,
        writer: Producer,
        key: &dyn KeyValue,
        value: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        let key = downcast_key::<K>(key)?.clone();
        let mut changes = Vec::new();
        if let Some(ordinal) = Self::entry_ordinal(&state.working, &key) {
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                false,
                ordinal,
                Arc::new(MapFactKey::Entry(key.clone())),
                Some(value),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        } else {
            let ordinal = state.working.counter;
            state.working.counter += 1;
            state.working.entries.insert(key.clone(), ordinal);
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                false,
                ordinal,
                Arc::new(MapFactKey::Entry(key.clone())),
                Some(value),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
            let keys = Self::keys_value(&state.working);
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                0,
                Arc::new(MapFactKey::<K>::Keys),
                Some(keys),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        Ok(changes)
    }

    fn remove(
        &self,
        state: &mut StoreState<MapSnap<K>>,
        writer: Producer,
        key: &dyn KeyValue,
    ) -> Result<Vec<Change>, Error> {
        let key = downcast_key::<K>(key)?.clone();
        let mut changes = Vec::new();
        if let Some(ordinal) = Self::entry_ordinal(&state.working, &key) {
            state.working.entries.shift_remove(&key);
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                false,
                ordinal,
                Arc::new(MapFactKey::Entry(key.clone())),
                None,
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
            let keys = Self::keys_value(&state.working);
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                0,
                Arc::new(MapFactKey::<K>::Keys),
                Some(keys),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        Ok(changes)
    }

    fn rekey(
        &self,
        state: &mut StoreState<MapSnap<K>>,
        writer: Producer,
        from: &dyn KeyValue,
        to: &dyn KeyValue,
    ) -> Result<Vec<Change>, Error> {
        let from = downcast_key::<K>(from)?.clone();
        let to = downcast_key::<K>(to)?.clone();
        let mut changes = Vec::new();
        if let Some(index) = state.working.entries.get_index_of(&from) {
            if to != from && state.working.entries.contains_key(&to) {
                return Err(Error::TopologyViolation {
                    view: self.name.to_string(),
                    message: format!("rekey target key {to:?} already exists"),
                });
            }
            // The old lineage dies; the new key starts a new lineage at a
            // fresh ordinal, sharing the value Arc. The entry RANK (map
            // position) is retained.
            if let Some(ordinal) = state.working.entries.shift_remove(&from) {
                let value = state
                    .working
                    .facts
                    .get(&ordinal)
                    .and_then(|entry| entry.value.clone());
                if let Some(change) = put(
                    &mut state.working.facts,
                    &state.round_base.facts,
                    writer,
                    false,
                    ordinal,
                    Arc::new(MapFactKey::Entry(from.clone())),
                    None,
                )
                .map_err(|e| with_view(e, self.name))?
                {
                    changes.push(change);
                }
                let new_ordinal = state.working.counter;
                state.working.counter += 1;
                state.working.entries.insert(to.clone(), new_ordinal);
                let last = state.working.entries.len() - 1;
                state.working.entries.move_index(last, index.min(last));
                if let Some(value) = value {
                    if let Some(change) = put(
                        &mut state.working.facts,
                        &state.round_base.facts,
                        writer,
                        false,
                        new_ordinal,
                        Arc::new(MapFactKey::Entry(to.clone())),
                        Some(value),
                    )
                    .map_err(|e| with_view(e, self.name))?
                    {
                        changes.push(change);
                    }
                }
            }
            let keys = Self::keys_value(&state.working);
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                0,
                Arc::new(MapFactKey::<K>::Keys),
                Some(keys),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        Ok(changes)
    }
}

impl<K: KeySpec> DynStore for MapStore<K> {
    fn begin_epoch(&self) {
        self.state.lock().begin_epoch();
    }
    fn apply(&self, writer: Producer, _instance: u32, op: &WriteKind) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        self.apply_inner(&mut state, writer, op)
    }
    fn retract(&self, writer: Producer, key: &dyn KeyValue) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        if let Some(key) = key.as_any().downcast_ref::<MapFactKey<K>>() {
            match key {
                MapFactKey::Entry(key) => self.remove(&mut state, writer, key),
                MapFactKey::Keys => Ok(Vec::new()),
            }
        } else {
            Err(Error::Internal("retract key type mismatch: map".into()))
        }
    }
    fn end_round(&self) -> Result<Vec<(u32, Change)>, Error> {
        let mut state = self.state.lock();
        let pending = std::mem::take(&mut state.pending);
        let mut out = Vec::new();
        for op in pending {
            state.pending_instance = op.instance;
            let changes = self.apply_inner(&mut state, op.writer, &op.op)?;
            state.pending_instance = u32::MAX;
            for change in changes {
                out.push((op.instance, change));
            }
        }
        state.end_round();
        Ok(out)
    }
    fn commit(&self) {
        self.state.lock().commit();
    }
    fn rollback(&self) {
        self.state.lock().rollback();
    }
    fn read(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<MapFactKey<K>>() {
            match key {
                MapFactKey::Keys => Some(0),
                MapFactKey::Entry(key) => Self::entry_ordinal(&state.working, key),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| {
            state
                .working
                .facts
                .get(&ordinal)
                .and_then(|entry| entry.value.clone())
        })
    }
    fn read_committed(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<MapFactKey<K>>() {
            match key {
                MapFactKey::Keys => Some(0),
                MapFactKey::Entry(key) => Self::entry_ordinal(&state.committed, key),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| {
            state
                .committed
                .facts
                .get(&ordinal)
                .and_then(|entry| entry.value.clone())
        })
    }
    fn debug_revision(&self, fact: &dyn KeyValue) -> Option<u64> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<MapFactKey<K>>() {
            match key {
                MapFactKey::Keys => Some(0),
                MapFactKey::Entry(key) => Self::entry_ordinal(&state.committed, key),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| state.committed.facts.get(&ordinal).map(|e| e.revision))
    }
}

// ---------------------------------------------------------------------------
// Tree store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TreeSnap {
    /// Node id → node-fact ordinal.
    nodes: HashMap<NodeId, u32>,
    /// Parent id → children-fact ordinal (lazy: materialized on first write).
    children: HashMap<NodeId, u32>,
    /// Node id → parent-fact ordinal (lazy).
    parents: HashMap<NodeId, u32>,
    facts: HashMap<u32, FactEntry>,
    /// Next ordinal; ordinal 0 is the Roots fact. Never reused.
    counter: u32,
}

pub(crate) struct TreeStore {
    state: Mutex<StoreState<TreeSnap>>,
    name: &'static str,
}

impl TreeStore {
    fn apply_inner(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        op: &WriteKind,
    ) -> Result<Vec<Change>> {
        match op {
            WriteKind::TreeInsertNode { id, data } => {
                self.insert_node(state, writer, *id, data.clone())
            }
            WriteKind::TreeUpdateNode { id, data } => {
                self.update_node(state, writer, *id, data.clone())
            }
            WriteKind::TreeUpsertNode { id, data } => {
                self.upsert_node(state, writer, *id, data.clone())
            }
            WriteKind::TreeRemoveNode { id } => self.remove_node(state, writer, *id),
            WriteKind::TreeReorderChildren { parent, order } => {
                self.reorder_children(state, writer, *parent, order)
            }
            WriteKind::TreeMoveNode { id, parent } => {
                self.move_node(state, writer, *id, *parent)
            }
            _ => Err(Error::Internal("op/shape mismatch: tree".into())),
        }
    }

    pub(crate) fn new(name: &'static str) -> Self {
        TreeStore {
            state: Mutex::new(StoreState::new(TreeSnap {
                nodes: HashMap::new(),
                children: HashMap::new(),
                parents: HashMap::new(),
                facts: HashMap::new(),
                counter: 1,
            })),
            name,
        }
    }

    fn roots_value(snap: &TreeSnap) -> Vec<NodeId> {
        snap.facts
            .get(&0)
            .and_then(|entry| entry.value.clone())
            .and_then(|value| {
                value
                    .as_any()
                    .downcast_ref::<Vec<NodeId>>()
                    .map(|roots| roots.clone())
            })
            .unwrap_or_default()
    }

    fn children_value(snap: &TreeSnap, parent: NodeId) -> Vec<NodeId> {
        snap.children
            .get(&parent)
            .and_then(|ordinal| snap.facts.get(ordinal))
            .and_then(|entry| entry.value.clone())
            .and_then(|value| {
                value
                    .as_any()
                    .downcast_ref::<Vec<NodeId>>()
                    .map(|kids| kids.clone())
            })
            .unwrap_or_default()
    }

    fn parent_value(snap: &TreeSnap, id: NodeId) -> Option<NodeId> {
        snap.parents
            .get(&id)
            .and_then(|ordinal| snap.facts.get(ordinal))
            .and_then(|entry| entry.value.clone())
            .and_then(|value| {
                value
                    .as_any()
                    .downcast_ref::<Option<NodeId>>()
                    .cloned()
                    .flatten()
            })
    }

    fn insert_node(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        id: NodeId,
        data: Option<Arc<dyn Value>>,
    ) -> Result<Vec<Change>, Error> {
        let mut changes = Vec::new();
        if state.working.nodes.contains_key(&id) {
            return Ok(changes); // ensure semantics: existing node untouched
        }
        let ordinal = state.working.counter;
        state.working.counter += 1;
        state.working.nodes.insert(id, ordinal);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(TreeFactKey::Node(id)),
            data,
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        // A fresh node with no parent is a root.
        let mut roots = Self::roots_value(&state.working);
        roots.push(id);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            0,
            Arc::new(TreeFactKey::Roots),
            Some(Arc::new(roots)),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        Ok(changes)
    }

    fn update_node(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        id: NodeId,
        data: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        if state.working.nodes.contains_key(&id) {
            self.update_node_inner(state, writer, id, data)
        } else {
            // The node may be created by the same round: defer.
            state.pending.push(PendingOp {
                instance: state.pending_instance,
                writer,
                op: WriteKind::TreeUpdateNode { id, data },
            });
            Ok(Vec::new())
        }
    }

    fn update_node_inner(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        id: NodeId,
        data: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        let ordinal = state.working.nodes[&id];
        let change = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(TreeFactKey::Node(id)),
            Some(data),
        )
        .map_err(|e| with_view(e, self.name))?;
        Ok(change.into_iter().collect())
    }

    fn upsert_node(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        id: NodeId,
        data: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        if state.working.nodes.contains_key(&id) {
            self.update_node_inner(state, writer, id, data)
        } else {
            self.insert_node(state, writer, id, Some(data))
        }
    }

    fn remove_node(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        id: NodeId,
    ) -> Result<Vec<Change>, Error> {
        let mut changes = Vec::new();
        let Some(ordinal) = state.working.nodes.remove(&id) else {
            return Ok(changes);
        };
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(TreeFactKey::Node(id)),
            None,
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        if let Some(parent_ordinal) = state.working.parents.remove(&id) {
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                parent_ordinal,
                Arc::new(TreeFactKey::Parent(id)),
                None,
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        if let Some(children_ordinal) = state.working.children.remove(&id) {
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                children_ordinal,
                Arc::new(TreeFactKey::Children(id)),
                None,
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        // Detach from the old parent's children list.
        if let Some(old_parent) = Self::parent_value(&state.working, id) {
            let mut kids = Self::children_value(&state.working, old_parent);
            kids.retain(|kid| *kid != id);
            let ordinal = *state.working.children.get(&old_parent).unwrap();
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                ordinal,
                Arc::new(TreeFactKey::Children(old_parent)),
                Some(Arc::new(kids)),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        // The removed node's children become roots.
        let mut roots = Self::roots_value(&state.working);
        roots.retain(|root| *root != id);
        for kid in Self::children_value(&state.working, id) {
            if let Some(ordinal) = state.working.parents.remove(&kid) {
                if let Some(change) = put(
                    &mut state.working.facts,
                    &state.round_base.facts,
                    writer,
                    true,
                    ordinal,
                    Arc::new(TreeFactKey::Parent(kid)),
                    None,
                )
                .map_err(|e| with_view(e, self.name))?
                {
                    changes.push(change);
                }
            }
            roots.push(kid);
        }
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            0,
            Arc::new(TreeFactKey::Roots),
            Some(Arc::new(roots)),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        Ok(changes)
    }

    fn reorder_children(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        parent: NodeId,
        order: &[NodeId],
    ) -> Result<Vec<Change>, Error> {
        if !state.working.nodes.contains_key(&parent) {
            // The node may be created by the same round: defer.
            state.pending.push(PendingOp {
                instance: state.pending_instance,
                writer,
                op: WriteKind::TreeReorderChildren {
                    parent,
                    order: order.to_vec(),
                },
            });
            return Ok(Vec::new());
        }
        let current = Self::children_value(&state.working, parent);
        let mut sorted_current = current.clone();
        sorted_current.sort_unstable();
        let mut sorted_order = order.to_vec();
        sorted_order.sort_unstable();
        if sorted_current != sorted_order {
            // The children may be re-parented by the same round: defer.
            state.pending.push(PendingOp {
                instance: state.pending_instance,
                writer,
                op: WriteKind::TreeReorderChildren {
                    parent,
                    order: order.to_vec(),
                },
            });
            return Ok(Vec::new());
        }
        let ordinal = *state
            .working
            .children
            .entry(parent)
            .or_insert_with(|| {
                let ordinal = state.working.counter;
                state.working.counter += 1;
                ordinal
            });
        let change = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            ordinal,
            Arc::new(TreeFactKey::Children(parent)),
            Some(Arc::new(order.to_vec())),
        )
        .map_err(|e| with_view(e, self.name))?;
        Ok(change.into_iter().collect())
    }

    fn move_node(
        &self,
        state: &mut StoreState<TreeSnap>,
        writer: Producer,
        id: NodeId,
        parent: NodeId,
    ) -> Result<Vec<Change>, Error> {
        if !state.working.nodes.contains_key(&id) || !state.working.nodes.contains_key(&parent) {
            // The nodes may be created by the same round: defer.
            state.pending.push(PendingOp {
                instance: state.pending_instance,
                writer,
                op: WriteKind::TreeMoveNode { id, parent },
            });
            return Ok(Vec::new());
        }
        if id == parent {
            return Err(Error::TopologyViolation {
                view: self.name.to_string(),
                message: "a node cannot be its own parent".into(),
            });
        }
        let mut changes = Vec::new();
        let old_parent = Self::parent_value(&state.working, id);
        if old_parent == Some(parent) {
            return Ok(changes); // no-op
        }
        let was_root = old_parent.is_none();
        let parent_ordinal = *state
            .working
            .parents
            .entry(id)
            .or_insert_with(|| {
                let ordinal = state.working.counter;
                state.working.counter += 1;
                ordinal
            });
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            parent_ordinal,
            Arc::new(TreeFactKey::Parent(id)),
            Some(Arc::new(Some(parent))),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        if let Some(old_parent) = old_parent {
            let mut kids = Self::children_value(&state.working, old_parent);
            kids.retain(|kid| *kid != id);
            let ordinal = *state.working.children.get(&old_parent).unwrap();
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                ordinal,
                Arc::new(TreeFactKey::Children(old_parent)),
                Some(Arc::new(kids)),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        let new_ordinal = *state
            .working
            .children
            .entry(parent)
            .or_insert_with(|| {
                let ordinal = state.working.counter;
                state.working.counter += 1;
                ordinal
            });
        let mut kids = Self::children_value(&state.working, parent);
        kids.push(id);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            new_ordinal,
            Arc::new(TreeFactKey::Children(parent)),
            Some(Arc::new(kids)),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        if was_root {
            let mut roots = Self::roots_value(&state.working);
            roots.retain(|root| *root != id);
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                true,
                0,
                Arc::new(TreeFactKey::Roots),
                Some(Arc::new(roots)),
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
        }
        Ok(changes)
    }
}

impl DynStore for TreeStore {
    fn begin_epoch(&self) {
        self.state.lock().begin_epoch();
    }
    fn apply(&self, writer: Producer, instance: u32, op: &WriteKind) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        state.pending_instance = instance;
        let result = self.apply_inner(&mut state, writer, op);
        state.pending_instance = u32::MAX;
        result
    }
    fn retract(&self, writer: Producer, key: &dyn KeyValue) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        match key.as_any().downcast_ref::<TreeFactKey>() {
            Some(TreeFactKey::Node(id)) => self.remove_node(&mut state, writer, *id),
            _ => Ok(Vec::new()),
        }
    }
    fn end_round(&self) -> Result<Vec<(u32, Change)>, Error> {
        let mut state = self.state.lock();
        let pending = std::mem::take(&mut state.pending);
        let mut out = Vec::new();
        for op in pending {
            state.pending_instance = op.instance;
            let changes = self.apply_inner(&mut state, op.writer, &op.op)?;
            state.pending_instance = u32::MAX;
            for change in changes {
                out.push((op.instance, change));
            }
        }
        state.end_round();
        Ok(out)
    }
    fn commit(&self) {
        self.state.lock().commit();
    }
    fn rollback(&self) {
        self.state.lock().rollback();
    }
    fn read(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<TreeFactKey>() {
            match key {
                TreeFactKey::Roots => Some(0),
                TreeFactKey::Node(id) => state.working.nodes.get(id).copied(),
                TreeFactKey::Children(parent) => state.working.children.get(parent).copied(),
                TreeFactKey::Parent(id) => state.working.parents.get(id).copied(),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| {
            state
                .working
                .facts
                .get(&ordinal)
                .and_then(|entry| entry.value.clone())
        })
    }
    fn read_committed(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<TreeFactKey>() {
            match key {
                TreeFactKey::Roots => Some(0),
                TreeFactKey::Node(id) => state.committed.nodes.get(id).copied(),
                TreeFactKey::Children(parent) => state.committed.children.get(parent).copied(),
                TreeFactKey::Parent(id) => state.committed.parents.get(id).copied(),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| {
            state
                .committed
                .facts
                .get(&ordinal)
                .and_then(|entry| entry.value.clone())
        })
    }
    fn debug_revision(&self, fact: &dyn KeyValue) -> Option<u64> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<TreeFactKey>() {
            match key {
                TreeFactKey::Roots => Some(0),
                TreeFactKey::Node(id) => state.committed.nodes.get(id).copied(),
                TreeFactKey::Children(parent) => state.committed.children.get(parent).copied(),
                TreeFactKey::Parent(id) => state.committed.parents.get(id).copied(),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| state.committed.facts.get(&ordinal).map(|e| e.revision))
    }
}

// ---------------------------------------------------------------------------
// Graph store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GraphSnap<L: KeySpec> {
    /// Node id → node-fact ordinal.
    nodes: HashMap<NodeId, u32>,
    /// Edge triple → edge-fact ordinal (insertion order preserved).
    edges: IndexMap<(NodeId, L, NodeId), u32>,
    /// (source, label) → bucket-fact ordinal (insertion order preserved).
    buckets: IndexMap<(NodeId, L), u32>,
    facts: HashMap<u32, FactEntry>,
    /// Next ordinal; ordinal 0 is the Nodes registry. Never reused.
    counter: u32,
}

pub(crate) struct GraphStore<L: KeySpec> {
    state: Mutex<StoreState<GraphSnap<L>>>,
    name: &'static str,
    _marker: std::marker::PhantomData<L>,
}

impl<L: KeySpec> GraphStore<L> {
    fn apply_inner(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        op: &WriteKind,
    ) -> Result<Vec<Change>> {
        match op {
            WriteKind::GraphInsertNode { id, data } => {
                self.insert_node(state, writer, *id, data.clone())
            }
            WriteKind::GraphUpdateNode { id, data } => {
                self.update_node(state, writer, *id, data.clone())
            }
            WriteKind::GraphUpsertNode { id, data } => {
                self.upsert_node(state, writer, *id, data.clone())
            }
            WriteKind::GraphRemoveNode { id } => self.remove_node(state, writer, *id),
            WriteKind::GraphInsertEdge {
                source,
                label,
                target,
                data,
            } => self.insert_edge(
                state,
                writer,
                *source,
                label.as_ref(),
                *target,
                data.clone(),
            ),
            WriteKind::GraphRemoveEdge {
                source,
                label,
                target,
            } => self.remove_edge(state, writer, *source, label.as_ref(), *target),
            WriteKind::GraphReplaceBucket {
                source,
                label,
                targets,
            } => self.replace_bucket(state, writer, *source, label.as_ref(), targets),
            _ => Err(Error::Internal("op/shape mismatch: graph".into())),
        }
    }

    pub(crate) fn new(name: &'static str) -> Self {
        GraphStore {
            state: Mutex::new(StoreState::new(GraphSnap {
                nodes: HashMap::new(),
                edges: IndexMap::new(),
                buckets: IndexMap::new(),
                facts: HashMap::new(),
                counter: 1,
            })),
            name,
            _marker: std::marker::PhantomData,
        }
    }

    fn nodes_value(snap: &GraphSnap<L>) -> Vec<NodeId> {
        snap.facts
            .get(&0)
            .and_then(|entry| entry.value.clone())
            .and_then(|value| {
                value
                    .as_any()
                    .downcast_ref::<Vec<NodeId>>()
                    .map(|nodes| nodes.clone())
            })
            .unwrap_or_default()
    }

    fn bucket_value(snap: &GraphSnap<L>, source: NodeId, label: &L) -> Vec<GraphEdgeKey<L>> {
        snap.buckets
            .get(&(source, label.clone()))
            .and_then(|ordinal| snap.facts.get(ordinal))
            .and_then(|entry| entry.value.clone())
            .and_then(|value| {
                value
                    .as_any()
                    .downcast_ref::<Vec<GraphEdgeKey<L>>>()
                    .map(|edges| edges.clone())
            })
            .unwrap_or_default()
    }

    fn insert_node(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        id: NodeId,
        data: Option<Arc<dyn Value>>,
    ) -> Result<Vec<Change>, Error> {
        let mut changes = Vec::new();
        if state.working.nodes.contains_key(&id) {
            return Ok(changes);
        }
        let ordinal = state.working.counter;
        state.working.counter += 1;
        state.working.nodes.insert(id, ordinal);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(GraphFactKey::<L>::Node(id)),
            data,
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        let mut nodes = Self::nodes_value(&state.working);
        nodes.push(id);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            0,
            Arc::new(GraphFactKey::<L>::Nodes),
            Some(Arc::new(nodes)),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        Ok(changes)
    }

    fn update_node(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        id: NodeId,
        data: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        let ordinal = state.working.nodes.get(&id).copied().ok_or_else(|| {
            Error::TopologyViolation {
                view: self.name.to_string(),
                message: format!("update of missing node {id:?}"),
            }
        })?;
        let change = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(GraphFactKey::<L>::Node(id)),
            Some(data),
        )
        .map_err(|e| with_view(e, self.name))?;
        Ok(change.into_iter().collect())
    }
    fn upsert_node(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        id: NodeId,
        data: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        if state.working.nodes.contains_key(&id) {
            self.update_node(state, writer, id, data)
        } else {
            self.insert_node(state, writer, id, Some(data))
        }
    }


    fn remove_node(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        id: NodeId,
    ) -> Result<Vec<Change>, Error> {
        let mut changes = Vec::new();
        let Some(ordinal) = state.working.nodes.remove(&id) else {
            return Ok(changes);
        };
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(GraphFactKey::<L>::Node(id)),
            None,
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        let mut nodes = Self::nodes_value(&state.working);
        nodes.retain(|node| *node != id);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            0,
            Arc::new(GraphFactKey::<L>::Nodes),
            Some(Arc::new(nodes)),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        // Incident edges die with the node (insertion order, deterministic).
        let incident: Vec<(NodeId, L, NodeId)> = state
            .working
            .edges
            .keys()
            .filter(|(s, _, t)| *s == id || *t == id)
            .cloned()
            .collect();
        for triple in incident {
            let (source, label, target) = triple.clone();
            if let Some(edge_ordinal) = state.working.edges.shift_remove(&triple) {
                if let Some(change) = put(
                    &mut state.working.facts,
                    &state.round_base.facts,
                    writer,
                    false,
                    edge_ordinal,
                    Arc::new(GraphFactKey::Edge(GraphEdgeKey {
                        source,
                        label: label.clone(),
                        target,
                    })),
                    None,
                )
                .map_err(|e| with_view(e, self.name))?
                {
                    changes.push(change);
                }
                if let Some(bucket_ordinal) = state.working.buckets.get(&(source, label.clone())) {
                    let mut edges = Self::bucket_value(&state.working, source, &label);
                    edges.retain(|edge| edge.target != target);
                    if let Some(change) = put(
                        &mut state.working.facts,
                        &state.round_base.facts,
                        writer,
                        true,
                        *bucket_ordinal,
                        Arc::new(GraphFactKey::Bucket(source, label.clone())),
                        Some(Arc::new(edges)),
                    )
                    .map_err(|e| with_view(e, self.name))?
                    {
                        changes.push(change);
                    }
                }
            }
        }
        // Buckets sourced at the removed node die (insertion order).
        let source_buckets: Vec<(NodeId, L)> = state
            .working
            .buckets
            .keys()
            .filter(|(s, _)| *s == id)
            .cloned()
            .collect();
        for (source, label) in source_buckets {
            if let Some(bucket_ordinal) =
                state.working.buckets.shift_remove(&(source, label.clone()))
            {
                if let Some(change) = put(
                    &mut state.working.facts,
                    &state.round_base.facts,
                    writer,
                    true,
                    bucket_ordinal,
                    Arc::new(GraphFactKey::Bucket(source, label)),
                    None,
                )
                .map_err(|e| with_view(e, self.name))?
                {
                    changes.push(change);
                }
            }
        }
        Ok(changes)
    }

    fn insert_edge(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        source: NodeId,
        label: &dyn KeyValue,
        target: NodeId,
        data: Arc<dyn Value>,
    ) -> Result<Vec<Change>, Error> {
        let label = downcast_label::<L>(label)?.clone();
        if !state.working.nodes.contains_key(&source) || !state.working.nodes.contains_key(&target) {
            // The endpoints may be created by the same round: defer.
            state.pending.push(PendingOp {
                instance: state.pending_instance,
                writer,
                op: WriteKind::GraphInsertEdge {
                    source,
                    label: Arc::new(label.clone()),
                    target,
                    data,
                },
            });
            return Ok(Vec::new());
        }
        let triple = (source, label.clone(), target);
        let mut changes = Vec::new();
        if let Some(ordinal) = state.working.edges.get(&triple) {
            let change = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                false,
                *ordinal,
                Arc::new(GraphFactKey::Edge(GraphEdgeKey {
                    source,
                    label: label.clone(),
                    target,
                })),
                Some(data),
            )
            .map_err(|e| with_view(e, self.name))?;
            changes.extend(change);
            return Ok(changes);
        }
        let ordinal = state.working.counter;
        state.working.counter += 1;
        state.working.edges.insert(triple.clone(), ordinal);
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            false,
            ordinal,
            Arc::new(GraphFactKey::Edge(GraphEdgeKey {
                source,
                label: label.clone(),
                target,
            })),
            Some(data),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        let bucket_ordinal = *state
            .working
            .buckets
            .entry((source, label.clone()))
            .or_insert_with(|| {
                let ordinal = state.working.counter;
                state.working.counter += 1;
                ordinal
            });
        let mut edges = Self::bucket_value(&state.working, source, &label);
        edges.push(GraphEdgeKey {
            source,
            label: label.clone(),
            target,
        });
        if let Some(change) = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            bucket_ordinal,
            Arc::new(GraphFactKey::Bucket(source, label)),
            Some(Arc::new(edges)),
        )
        .map_err(|e| with_view(e, self.name))?
        {
            changes.push(change);
        }
        Ok(changes)
    }

    fn remove_edge(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        source: NodeId,
        label: &dyn KeyValue,
        target: NodeId,
    ) -> Result<Vec<Change>, Error> {
        let label = downcast_label::<L>(label)?.clone();
        let triple = (source, label.clone(), target);
        let mut changes = Vec::new();
        if let Some(ordinal) = state.working.edges.shift_remove(&triple) {
            if let Some(change) = put(
                &mut state.working.facts,
                &state.round_base.facts,
                writer,
                false,
                ordinal,
                Arc::new(GraphFactKey::Edge(GraphEdgeKey {
                    source,
                    label: label.clone(),
                    target,
                })),
                None,
            )
            .map_err(|e| with_view(e, self.name))?
            {
                changes.push(change);
            }
            if let Some(bucket_ordinal) = state.working.buckets.get(&(source, label.clone())) {
                let mut edges = Self::bucket_value(&state.working, source, &label);
                edges.retain(|edge| edge.target != target);
                if let Some(change) = put(
                    &mut state.working.facts,
                    &state.round_base.facts,
                    writer,
                    true,
                    *bucket_ordinal,
                    Arc::new(GraphFactKey::Bucket(source, label.clone())),
                    Some(Arc::new(edges)),
                )
                .map_err(|e| with_view(e, self.name))?
                {
                    changes.push(change);
                }
            }
        }
        Ok(changes)
    }

    fn replace_bucket(
        &self,
        state: &mut StoreState<GraphSnap<L>>,
        writer: Producer,
        source: NodeId,
        label: &dyn KeyValue,
        targets: &[NodeId],
    ) -> Result<Vec<Change>, Error> {
        let label = downcast_label::<L>(label)?.clone();
        let mut seen = std::collections::HashSet::new();
        let mut missing = false;
        if !state.working.nodes.contains_key(&source) {
            missing = true;
        }
        for target in targets {
            if !seen.insert(*target) {
                return Err(Error::TopologyViolation {
                    view: self.name.to_string(),
                    message: format!("duplicate target {target:?} in bucket"),
                });
            }
            if !state.working.edges.contains_key(&(source, label.clone(), *target)) {
                missing = true;
            }
        }
        if missing {
            // The structure may be created by the same round: defer.
            state.pending.push(PendingOp {
                instance: state.pending_instance,
                writer,
                op: WriteKind::GraphReplaceBucket {
                    source,
                    label: Arc::new(label.clone()),
                    targets: targets.to_vec(),
                },
            });
            return Ok(Vec::new());
        }
        let bucket_ordinal = *state
            .working
            .buckets
            .entry((source, label.clone()))
            .or_insert_with(|| {
                let ordinal = state.working.counter;
                state.working.counter += 1;
                ordinal
            });
        let edges: Vec<GraphEdgeKey<L>> = targets
            .iter()
            .map(|target| GraphEdgeKey {
                source,
                label: label.clone(),
                target: *target,
            })
            .collect();
        let change = put(
            &mut state.working.facts,
            &state.round_base.facts,
            writer,
            true,
            bucket_ordinal,
            Arc::new(GraphFactKey::Bucket(source, label)),
            Some(Arc::new(edges)),
        )
        .map_err(|e| with_view(e, self.name))?;
        Ok(change.into_iter().collect())
    }
}

impl<L: KeySpec> DynStore for GraphStore<L> {
    fn begin_epoch(&self) {
        self.state.lock().begin_epoch();
    }
    fn apply(&self, writer: Producer, instance: u32, op: &WriteKind) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        state.pending_instance = instance;
        let result = self.apply_inner(&mut state, writer, op);
        state.pending_instance = u32::MAX;
        result
    }
    fn retract(&self, writer: Producer, key: &dyn KeyValue) -> Result<Vec<Change>> {
        let mut state = self.state.lock();
        match key.as_any().downcast_ref::<GraphFactKey<L>>() {
            Some(GraphFactKey::Node(id)) => self.remove_node(&mut state, writer, *id),
            Some(GraphFactKey::Edge(edge)) => self.remove_edge(
                &mut state,
                writer,
                edge.source,
                &edge.label,
                edge.target,
            ),
            _ => Ok(Vec::new()),
        }
    }
    fn end_round(&self) -> Result<Vec<(u32, Change)>, Error> {
        let mut state = self.state.lock();
        let pending = std::mem::take(&mut state.pending);
        let mut out = Vec::new();
        for op in pending {
            state.pending_instance = op.instance;
            let changes = self.apply_inner(&mut state, op.writer, &op.op)?;
            state.pending_instance = u32::MAX;
            for change in changes {
                out.push((op.instance, change));
            }
        }
        state.end_round();
        Ok(out)
    }
    fn commit(&self) {
        self.state.lock().commit();
    }
    fn rollback(&self) {
        self.state.lock().rollback();
    }
    fn read(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<GraphFactKey<L>>() {
            match key {
                GraphFactKey::Nodes => Some(0),
                GraphFactKey::Node(id) => state.working.nodes.get(id).copied(),
                GraphFactKey::Edge(edge) => state
                    .working
                    .edges
                    .get(&(edge.source, edge.label.clone(), edge.target))
                    .copied(),
                GraphFactKey::Bucket(source, label) => state
                    .working
                    .buckets
                    .get(&(*source, label.clone()))
                    .copied(),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| {
            state
                .working
                .facts
                .get(&ordinal)
                .and_then(|entry| entry.value.clone())
        })
    }
    fn read_committed(&self, fact: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<GraphFactKey<L>>() {
            match key {
                GraphFactKey::Nodes => Some(0),
                GraphFactKey::Node(id) => state.committed.nodes.get(id).copied(),
                GraphFactKey::Edge(edge) => state
                    .committed
                    .edges
                    .get(&(edge.source, edge.label.clone(), edge.target))
                    .copied(),
                GraphFactKey::Bucket(source, label) => state
                    .committed
                    .buckets
                    .get(&(*source, label.clone()))
                    .copied(),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| {
            state
                .committed
                .facts
                .get(&ordinal)
                .and_then(|entry| entry.value.clone())
        })
    }
    fn debug_revision(&self, fact: &dyn KeyValue) -> Option<u64> {
        let state = self.state.lock();
        let ordinal = if let Some(key) = fact.as_any().downcast_ref::<GraphFactKey<L>>() {
            match key {
                GraphFactKey::Nodes => Some(0),
                GraphFactKey::Node(id) => state.committed.nodes.get(id).copied(),
                GraphFactKey::Edge(edge) => state
                    .committed
                    .edges
                    .get(&(edge.source, edge.label.clone(), edge.target))
                    .copied(),
                GraphFactKey::Bucket(source, label) => state
                    .committed
                    .buckets
                    .get(&(*source, label.clone()))
                    .copied(),
            }
        } else {
            None
        };
        ordinal.and_then(|ordinal| state.committed.facts.get(&ordinal).map(|e| e.revision))
    }
}
