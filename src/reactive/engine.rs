//! The engine: installation, epochs, rounds, deterministic worklists,
//! atomic commit, rollback, and cycle rejection (§5.5, T1–T6).
//!
//! One external command opens one epoch (a transaction, §4.2). Round 0
//! evaluates the readers of the normalized command delta (plus, on the
//! first epoch, every component root, and the `Previous` readers of the
//! previous epoch's delta). Each later round evaluates the readers of the
//! previous round's delta, against the coherent state at the start of the
//! round (T2). Rounds repeat until a round publishes an empty delta
//! (quiescence); then one snapshot, the dynamic graph, ownership index,
//! counters, and subscriptions commit atomically.
//!
//! Every ordering that can affect committed behavior comes from private
//! stable ordinals: view registration order, component registration order,
//! visitor paths, round application order, and first-change order of the
//! deltas. No `HashMap`/`HashSet` iteration and no thread interleaving can
//! influence committed order (T3).

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::reactive::api::{RunContext};
use crate::reactive::error::{Error, Producer, Result};
use crate::reactive::store::{Change, DynStore, WriteKind};
use crate::reactive::trace::{
    ChildKey, FactRef, Instance, InstanceId, InstanceKind, PathStep, Registry, RunBuffer,
    RunBufferHandle, ViewId, ACTIVE, Frame,
};
use crate::reactive::value::{KeyValue, Value};
use crate::reactive::view::{
    BoxView, GraphView, MapView, NodeId, TreeView, ViewSpec,
};

/// One raw fact change in deterministic (first-change) order.
#[derive(Clone, Debug)]
pub struct RawChange {
    pub view: ViewId,
    pub view_name: &'static str,
    pub key: Arc<dyn KeyValue>,
    pub prev: Option<Arc<dyn Value>>,
    pub next: Option<Arc<dyn Value>>,
}

impl RawChange {
    /// Human-readable change for tests and diagnostics.
    pub fn describe(&self) -> String {
        format!(
            "{} {:?}: {:?} -> {:?}",
            self.view_name, self.key, self.prev, self.next
        )
    }
}

/// One external command: a batch of patch ops applied as one epoch.
/// External patches are the one write path outside visitors and own their
/// facts as `external` (§5.3).
#[derive(Clone)]
pub struct ExternalOp {
    pub(crate) view: TypeId,
    pub(crate) kind: WriteKind,
}

impl ExternalOp {
    pub fn box_set<V: BoxView>(value: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::BoxSet(Arc::new(value)),
        }
    }
    pub fn box_clear<V: BoxView>() -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::BoxClear,
        }
    }
    pub fn map_set<V: MapView>(key: V::Key, value: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::MapSet {
                key: Arc::new(key),
                value: Arc::new(value),
            },
        }
    }
    pub fn map_remove<V: MapView>(key: V::Key) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::MapRemove {
                key: Arc::new(key),
            },
        }
    }
    pub fn map_rekey<V: MapView>(from: V::Key, to: V::Key) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::MapRekey {
                from: Arc::new(from),
                to: Arc::new(to),
            },
        }
    }
    pub fn tree_insert_node<V: TreeView>(id: NodeId, data: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::TreeInsertNode {
                id,
                data: Some(Arc::new(data)),
            },
        }
    }
    /// Insert-or-update: re-ensuring a document's node payload replaces
    /// it, so an edited rebuild updates the changed nodes in place.
    pub fn tree_upsert_node<V: TreeView>(id: NodeId, data: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::TreeUpsertNode {
                id,
                data: Arc::new(data),
            },
        }
    }
    pub fn tree_update_node<V: TreeView>(id: NodeId, data: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::TreeUpdateNode {
                id,
                data: Arc::new(data),
            },
        }
    }
    pub fn tree_remove_node<V: TreeView>(id: NodeId) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::TreeRemoveNode { id },
        }
    }
    pub fn tree_move_node<V: TreeView>(id: NodeId, parent: NodeId) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::TreeMoveNode { id, parent },
        }
    }
    pub fn tree_reorder_children<V: TreeView>(parent: NodeId, order: Vec<NodeId>) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::TreeReorderChildren { parent, order },
        }
    }
    pub fn graph_insert_node<V: GraphView>(id: NodeId, data: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::GraphInsertNode {
                id,
                data: Some(Arc::new(data)),
            },
        }
    }
    pub fn graph_update_node<V: GraphView>(id: NodeId, data: V::Value) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::GraphUpdateNode {
                id,
                data: Arc::new(data),
            },
        }
    }
    pub fn graph_remove_node<V: GraphView>(id: NodeId) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::GraphRemoveNode { id },
        }
    }
    pub fn graph_insert_edge<V: GraphView>(
        source: NodeId,
        label: V::Label,
        target: NodeId,
        data: V::Edge,
    ) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::GraphInsertEdge {
                source,
                label: Arc::new(label),
                target,
                data: Arc::new(data),
            },
        }
    }
    pub fn graph_remove_edge<V: GraphView>(source: NodeId, label: V::Label, target: NodeId) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::GraphRemoveEdge {
                source,
                label: Arc::new(label),
                target,
            },
        }
    }
    pub fn graph_replace_bucket<V: GraphView>(
        source: NodeId,
        label: V::Label,
        targets: Vec<NodeId>,
    ) -> Self {
        ExternalOp {
            view: TypeId::of::<V>(),
            kind: WriteKind::GraphReplaceBucket {
                source,
                label: Arc::new(label),
                targets,
            },
        }
    }
}

/// The report of one committed command.
#[derive(Clone, Debug)]
pub struct CommandReport {
    /// The committed epoch counter (0 when the command did no work).
    pub epoch: u64,
    /// Rounds executed (0 when no work was needed).
    pub rounds: u32,
    /// Total visitor runs in this epoch.
    pub runs: u64,
    changed: Vec<RawChange>,
}

impl CommandReport {
    /// The epoch's changed facts in deterministic first-change order.
    pub fn changed(&self) -> &[RawChange] {
        &self.changed
    }
    /// The changed facts of one view (by its registered name).
    pub fn changed_view(&self, view_name: &'static str) -> Vec<&RawChange> {
        self.changed
            .iter()
            .filter(|change| change.view_name == view_name)
            .collect()
    }
}

/// A subscription: called once per committed epoch with that view's
/// changed facts, in deterministic order.
pub type Subscriber = Box<dyn Fn(&[RawChange]) + Send + Sync>;

pub(crate) struct ViewEntry {
    pub name: &'static str,
    pub store: Arc<dyn DynStore>,
    pub rank: u32,
    /// Producer names: components that emit this view, plus "external".
    pub producers: Vec<String>,
    pub external: bool,
}

pub(crate) struct ComponentEntry {
    pub name: &'static str,
    pub component: Arc<dyn Component>,
    /// (view, is_previous) — the observation edges.
    pub observed: Vec<(ViewId, bool)>,
    pub emitted: Vec<ViewId>,
}

/// A component: the authored dependency spec (G3). The `#[component]`
/// macro implements this trait from the author's function signature.
pub trait Component: Send + Sync {
    /// The component's name (for diagnostics and identity derivation).
    fn name(&self) -> &'static str;
    /// Registers the observed/emitted views (authority validation input).
    fn install(&self, builder: &mut EngineBuilder) -> Result<()>;
    /// Runs the component body as the root visitor of this instance.
    fn run(&self, cx: &RunContext) -> Result<()>;
}

pub(crate) struct Counters {
    pub epoch: u64,
    /// The committed delta of the previous epoch (for `Previous` readers).
    pub last_epoch_changes: Vec<(ViewId, Arc<dyn KeyValue>)>,
}

struct Epoch {
    round: u32,
    /// Total runs this epoch (deduped).
    runs: u64,
    ran_this_round: HashSet<InstanceId>,
    worklist: Vec<InstanceId>,
    round_children_snapshot: HashMap<InstanceId, Vec<ChildKey>>,
    results: Vec<(InstanceId, RunBufferHandle)>,
    round_delta: Vec<RawChange>,
    round_seen: HashSet<(ViewId, u64)>,
    epoch_delta: Vec<RawChange>,
    epoch_seen: HashSet<(ViewId, u64)>,
    /// Rollback state.
    created_instances: Vec<InstanceId>,
    instances_len_at_start: usize,
    children_at_start: HashMap<(InstanceId, u64), Vec<(ChildKey, InstanceId)>>,
    reads_at_start: Vec<(InstanceId, Vec<FactRef>, Vec<(ViewId, Arc<dyn KeyValue>)>)>,
}

impl Epoch {
    fn new() -> Self {
        Epoch {
            round: 0,
            runs: 0,
            ran_this_round: HashSet::new(),
            worklist: Vec::new(),
            round_children_snapshot: HashMap::new(),
            results: Vec::new(),
            round_delta: Vec::new(),
            round_seen: HashSet::new(),
            epoch_delta: Vec::new(),
            epoch_seen: HashSet::new(),
            created_instances: Vec::new(),
            instances_len_at_start: 0,
            children_at_start: HashMap::new(),
            reads_at_start: Vec::new(),
        }
    }
}

/// The engine's shared interior-mutable state.
pub(crate) struct Shared {
    pub views: Mutex<Vec<ViewEntry>>,
    pub view_by_type: Mutex<HashMap<TypeId, ViewId>>,
    pub components: Mutex<Vec<ComponentEntry>>,
    pub registry: Mutex<Registry>,
    pub epoch: Mutex<Epoch>,
    pub subscriptions: Mutex<Vec<(ViewId, Subscriber)>>,
    pub counters: Mutex<Counters>,
    pub prepared: Mutex<bool>,
    pub external_views: Mutex<HashSet<ViewId>>,
    pub workers: usize,
    /// Maximum rounds per epoch (safety net; cycles are rejected, so
    /// legitimate epochs are bounded by the longest dependency chain).
    pub max_rounds: u32,
    /// Maximum total runs per epoch (safety net).
    pub max_runs: u64,
}

/// The reactive engine. One engine owns all views, components, and
/// subscriptions; commands run synchronously.
pub struct Engine {
    pub(crate) shared: Arc<Shared>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    /// A new engine with one worker (deterministic by construction).
    pub fn new() -> Self {
        Self::with_workers(1)
    }

    /// A new engine with the given worker count. Committed results are
    /// identical under any worker count (T3); `with_workers(0)` uses
    /// `std::thread::available_parallelism()`.
    pub fn with_workers(workers: usize) -> Self {
        let workers = if workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            workers
        };
        Engine {
            shared: Arc::new(Shared {
                views: Mutex::new(Vec::new()),
                view_by_type: Mutex::new(HashMap::new()),
                components: Mutex::new(Vec::new()),
                registry: Mutex::new(Registry::new()),
                epoch: Mutex::new(Epoch::new()),
                subscriptions: Mutex::new(Vec::new()),
                counters: Mutex::new(Counters {
                    epoch: 0,
                    last_epoch_changes: Vec::new(),
                }),
                prepared: Mutex::new(false),
                external_views: Mutex::new(HashSet::new()),
                workers,
                max_rounds: 4096,
                max_runs: 1_000_000,
            }),
        }
    }

    /// Declares a view as externally owned: external commands may patch it.
    pub fn external<V: ViewSpec>(&mut self) -> Result<()> {
        let view = {
            let mut builder = EngineBuilder {
                shared: &self.shared,
                component: None,
            };
            builder.register_view::<V>()?
        };
        self.shared.external_views.lock().insert(view);
        let mut views = self.shared.views.lock();
        views[view as usize].external = true;
        views[view as usize].producers.push("external".to_string());
        Ok(())
    }

    /// Installs a component: registers its views (idempotently) and its
    /// signature edges.
    pub fn install(&mut self, component: impl Component + 'static) -> Result<()> {
        let name = component.name();
        let component: Arc<dyn Component> = Arc::new(component);
        let component_id = {
            let mut components = self.shared.components.lock();
            let id = components.len() as u32;
            components.push(ComponentEntry {
                name,
                component: Arc::clone(&component),
                observed: Vec::new(),
                emitted: Vec::new(),
            });
            id
        };
        let mut builder = EngineBuilder {
            shared: &self.shared,
            component: Some(component_id),
        };
        component.install(&mut builder)?;
        Ok(())
    }

    /// Subscribes to one view's committed changes.
    pub fn subscribe<V: ViewSpec>(&mut self, subscriber: Subscriber) -> Result<()> {
        let view = {
            let mut builder = EngineBuilder {
                shared: &self.shared,
                component: None,
            };
            builder.register_view::<V>()?
        };
        self.shared.subscriptions.lock().push((view, subscriber));
        Ok(())
    }

    /// Runs one external command (one epoch).
    pub fn command(&mut self, ops: Vec<ExternalOp>) -> Result<CommandReport> {
        let shared = Arc::clone(&self.shared);
        shared.prepare()?;
        // Begin the epoch.
        let first = shared.counters.lock().epoch == 0;
        {
            let registry = shared.registry.lock();
            let mut epoch = shared.epoch.lock();
            *epoch = Epoch::new();
            epoch.instances_len_at_start = registry.instances.len();
            epoch.children_at_start = registry.children.clone();
            epoch.reads_at_start = registry
                .instances
                .iter()
                .map(|instance| {
                    (
                        instance.id,
                        instance.reads.clone(),
                        instance.lifetime_writes.clone(),
                    )
                })
                .collect();
        }
        let view_names: Vec<&'static str> = {
            let views = shared.views.lock();
            views.iter().map(|view| view.name).collect()
        };
        for view in shared.views.lock().iter() {
            view.store.begin_epoch();
        }
        // Normalize: apply the external patches (round-0 delta).
        {
            let mut epoch = shared.epoch.lock();
            for op in &ops {
                let view = view_id_of(&shared, op.view)?;
                if !shared.external_views.lock().contains(&view) {
                    return Err(Error::ExternalPatchToNonExternal {
                        view: view_names[view as usize].to_string(),
                    });
                }
                let changes = shared.views.lock()[view as usize]
                    .store
                    .apply(Producer::External, u32::MAX, &op.kind)?;
                for change in changes {
                    accumulate(&mut epoch, view, view_names[view as usize], change);
                }
            }
        }
        // Build the round-0 worklist.
        let delta0 = {
            let mut epoch = shared.epoch.lock();
            let delta = epoch.round_delta.clone();
            epoch.round_delta.clear();
            epoch.round_seen.clear();
            delta
        };
        let mut extra: Vec<InstanceId> = Vec::new();
        if first {
            // Cold start: every component root evaluates once (T1).
            let ranks: Vec<u32> = shared.views.lock().iter().map(|view| view.rank).collect();
            let components = shared.components.lock();
            let mut registry = shared.registry.lock();
            for (component_id, entry) in components.iter().enumerate() {
                let id = registry.instances.len() as InstanceId;
                let rank = entry
                    .observed
                    .iter()
                    .find(|(_, previous)| !*previous)
                    .map(|(view, _)| ranks[*view as usize])
                    .unwrap_or(0);
                registry.instances.push(Instance {
                    id,
                    component: component_id as u32,
                    path: vec![PathStep {
                        kind: "root",
                        elem: String::new(),
                    }],
                    rank,
                    kind: InstanceKind::Root,
                    parent: None,
                    reads: Vec::new(),
                    epoch_writes: Vec::new(),
                    lifetime_writes: Vec::new(),
                    retired: false,
                });
                extra.push(id);
            }
        }
        // Previous readers of the previous epoch's delta.
        {
            let last = shared.counters.lock().last_epoch_changes.clone();
            let registry = shared.registry.lock();
            for (view, key) in last {
                let fact = FactRef {
                    view,
                    key,
                    temporal: true,
                };
                extra.extend(registry.readers_of(&fact, true).iter().copied());
            }
        }
        let worklist = shared.build_worklist(&delta0, &extra)?;
        let mut worklist = worklist;
        let mut rounds = 0u32;
        if worklist.is_empty() {
            // No readers: if the normalized delta is also empty there is
            // no epoch work at all; otherwise the external changes still
            // commit (they are facts, even with no consumers).
            if shared.epoch.lock().epoch_delta.is_empty() {
                return Ok(CommandReport {
                    epoch: shared.counters.lock().epoch,
                    rounds: 0,
                    runs: 0,
                    changed: Vec::new(),
                });
            }
        }
        // Rounds to quiescence.
        while !worklist.is_empty() {
            if rounds >= shared.max_rounds {
                shared.rollback_epoch();
                return Err(Error::Internal(
                    "round limit exceeded (possible engine bug)".into(),
                ));
            }
            if shared.epoch.lock().runs >= shared.max_runs {
                shared.rollback_epoch();
                return Err(Error::Internal(
                    "run limit exceeded (possible engine bug)".into(),
                ));
            }
            shared.run_round(&worklist)?;
            rounds += 1;
            let delta = {
                let mut epoch = shared.epoch.lock();
                let delta = epoch.round_delta.clone();
                epoch.round_delta.clear();
                epoch.round_seen.clear();
                delta
            };
            if delta.is_empty() {
                break; // quiescence
            }
            worklist = shared.build_worklist(&delta, &[])?;
        }
        let runs = shared.epoch.lock().runs;
        // Commit: one snapshot, dynamic graph, ownership index, counters,
        // and subscriptions, atomically.
        {
            let mut registry = shared.registry.lock();
            let retired: Vec<InstanceId> = registry
                .instances
                .iter()
                .filter(|instance| instance.retired)
                .map(|instance| instance.id)
                .collect();
            for id in retired {
                registry.closures.remove(&id);
            }
            registry.compact();
        }
        let changed = shared.epoch.lock().epoch_delta.clone();
        {
            let mut counters = shared.counters.lock();
            counters.epoch += 1;
            counters.last_epoch_changes = changed
                .iter()
                .map(|change| (change.view, change.key.clone()))
                .collect();
        }
        for view in shared.views.lock().iter() {
            view.store.commit();
        }
        // Deliver subscriptions in deterministic order.
        {
            let subscriptions = shared.subscriptions.lock();
            for (view, subscriber) in subscriptions.iter() {
                let view_changes: Vec<RawChange> = changed
                    .iter()
                    .filter(|change| change.view == *view)
                    .cloned()
                    .collect();
                if !view_changes.is_empty() {
                    subscriber(&view_changes);
                }
            }
        }
        Ok(CommandReport {
            epoch: shared.counters.lock().epoch,
            rounds,
            runs,
            changed,
        })
    }

    /// Reads the committed state (test/observation surface).
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Test hook: the committed revision of one fact.
    #[allow(dead_code)]
    pub(crate) fn debug_revision_of<V: ViewSpec>(&self, fact: &dyn KeyValue) -> Option<u64> {
        let (store, _, _) = self.shared.view_store::<V>().ok()?;
        store.debug_revision(fact)
    }

}

impl Shared {
    /// Test hook: an `Arc` to the shared state (for in-crate tests).
    #[allow(dead_code)]
    pub(crate) fn from_engine_for_tests(engine: &Engine) -> Arc<Shared> {
        Arc::clone(&engine.shared)
    }
}

impl Shared {
    /// Validation + static ranks, once, before the first command (§5.4).
    fn prepare(&self) -> Result<()> {
        if *self.prepared.lock() {
            return Ok(());
        }
        // Authority: every observed view has a producer.
        let observed: Vec<Vec<ViewId>> = {
            let components = self.components.lock();
            components
                .iter()
                .map(|entry| entry.observed.iter().map(|(view, _)| *view).collect())
                .collect()
        };
        let view_names: Vec<&'static str> = {
            let views = self.views.lock();
            views.iter().map(|view| view.name).collect()
        };
        {
            let views = self.views.lock();
            for entry in &observed {
                for view in entry {
                    if views[*view as usize].producers.is_empty() {
                        return Err(Error::NoProducerForView {
                            view: view_names[*view as usize].to_string(),
                        });
                    }
                }
            }
        }
        // Deterministic ranks: Kahn layering over observed → emitted edges
        // (temporal edges excluded), tie-broken by view registration
        // order; cycle leftovers ranked after in registration order.
        let edges: Vec<(ViewId, ViewId)> = {
            let components = self.components.lock();
            let mut edges = Vec::new();
            for entry in components.iter() {
                for (u, previous) in &entry.observed {
                    if *previous {
                        continue;
                    }
                    for v in &entry.emitted {
                        if u != v {
                            edges.push((*u, *v));
                        }
                    }
                }
            }
            edges
        };
        let n = view_names.len();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut indeg = vec![0u32; n];
        for (u, v) in edges {
            adj[u as usize].push(v);
            indeg[v as usize] += 1;
        }
        let mut ranks = vec![0u32; n];
        let mut queue: Vec<u32> = (0..n as u32).filter(|v| indeg[*v as usize] == 0).collect();
        let mut next_rank = 0u32;
        while !queue.is_empty() {
            queue.sort_unstable();
            let layer = std::mem::take(&mut queue);
            for u in &layer {
                ranks[*u as usize] = next_rank;
            }
            for u in &layer {
                for v in &adj[*u as usize] {
                    indeg[*v as usize] -= 1;
                    if indeg[*v as usize] == 0 {
                        queue.push(*v);
                    }
                }
            }
            next_rank += 1;
        }
        for (v, degree) in indeg.iter().enumerate() {
            if *degree > 0 {
                ranks[v] = next_rank + v as u32; // cycle leftover, registration order
            }
        }
        let mut views = self.views.lock();
        for (view, rank) in ranks.iter().enumerate() {
            views[view].rank = *rank;
        }
        *self.prepared.lock() = true;
        Ok(())
    }

    /// Runs one round's worklist, possibly in parallel, then applies the
    /// results deterministically.
    fn run_round(self: &Arc<Self>, worklist: &[InstanceId]) -> Result<()> {
        {
            let mut epoch = self.epoch.lock();
            epoch.worklist = worklist.to_vec();
            epoch.ran_this_round.clear();
            epoch.round_children_snapshot.clear();
        }
        // Snapshot the worklist parents' children for retirement diffs.
        {
            let worklist: HashSet<InstanceId> = worklist.iter().copied().collect();
            let registry = self.registry.lock();
            let mut by_parent: HashMap<InstanceId, Vec<ChildKey>> = HashMap::new();
            for ((parent, _), bucket) in registry.children.iter() {
                if worklist.contains(parent) {
                    let keys = by_parent.entry(*parent).or_default();
                    for (key, id) in bucket {
                        if !registry.instances[*id as usize].retired {
                            keys.push(key.clone());
                        }
                    }
                }
            }
            self.epoch.lock().round_children_snapshot = by_parent;
        }
        let n = worklist.len();
        let workers = self.workers.min(n).max(1);
        let next = AtomicUsize::new(0);
        if workers == 1 {
            for i in 0..n {
                self.run_instance(worklist[i]);
            }
        } else {
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        self.run_instance(worklist[i]);
                    });
                }
            });
        }
        self.apply_round()
    }

    /// Runs one instance (at most once per round). Children created by a
    /// running visitor run immediately on the same thread.
    pub(crate) fn run_instance(self: &Arc<Self>, id: InstanceId) {
        let mut epoch = self.epoch.lock();
        if !epoch.ran_this_round.insert(id) {
            return;
        }
        epoch.runs += 1;
        drop(epoch);
        let buffer: RunBufferHandle = Arc::new(Mutex::new(RunBuffer::new(id)));
        let (component, mut closure, root_component) = {
            let mut registry = self.registry.lock();
            let instance = &registry.instances[id as usize];
            let component = instance.component;
            let closure = match instance.kind {
                InstanceKind::Root => None,
                InstanceKind::Child { .. } => registry.closures.remove(&id),
            };
            let root_component = if closure.is_none() {
                Some(Arc::clone(
                    &self.components.lock()[component as usize].component,
                ))
            } else {
                None
            };
            (component, closure, root_component)
        };
        ACTIVE.with(|active| {
            active.borrow_mut().push(Frame {
                instance: id,
                component,
                buffer: Arc::clone(&buffer),
                shared: Arc::clone(self),
            });
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cx = RunContext {
                shared: self,
                component,
                instance: id,
            };
            match &mut closure {
                Some(closure) => closure(),
                None => root_component.as_ref().expect("root component").run(&cx),
            }
        }));
        ACTIVE.with(|active| {
            active.borrow_mut().pop();
        });
        {
            let mut buffer = buffer.lock();
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => buffer.error = Some(error),
                Err(panic) => buffer.error = Some(Error::Panic(panic_message(&panic))),
            }
        }
        if let Some(closure) = closure {
            self.registry.lock().closures.insert(id, closure);
        }
        self.epoch.lock().results.push((id, buffer));
    }

    /// Applies one round's results in deterministic order: reads refresh
    /// the reverse index, writes validate ownership and accumulate the
    /// round delta, and retired children leave the index. Retirement
    /// (with retraction of the child's lifetime writes) runs as a second
    /// phase after every result of the round has applied, so a retired
    /// child's own same-round write cannot resurrect its facts.
    fn apply_round(&self) -> Result<()> {
        let results = std::mem::take(&mut self.epoch.lock().results);
        // Deterministic order: (rank, path, id).
        let registry = self.registry.lock();
        let mut sorted = results;
        // Deterministic: parallel spawn order must never leak into the
        // application order, so instances with equal rank and path (e.g.
        // two components' visitors over the same syntax node) tie-break
        // on the component ordinal before the instance id.
        sorted.sort_by(|(a, _), (b, _)| {
            let ia = &registry.instances[*a as usize];
            let ib = &registry.instances[*b as usize];
            ia.rank
                .cmp(&ib.rank)
                .then_with(|| ia.path.cmp(&ib.path))
                .then_with(|| ia.component.cmp(&ib.component))
                .then_with(|| a.cmp(b))
        });
        drop(registry);
        // The first error in deterministic order aborts the epoch.
        for (_, buffer) in &sorted {
            let mut buffer = buffer.lock();
            if let Some(error) = buffer.error.take() {
                return Err(error);
            }
        }
        let worklist = self.epoch.lock().worklist.clone();
        let stores: Vec<(Arc<dyn DynStore>, &'static str)> = {
            let views = self.views.lock();
            views
                .iter()
                .map(|view| (Arc::clone(&view.store), view.name))
                .collect()
        };
        // Phase 1: apply every result in deterministic order.
        let mut retirements: Vec<(InstanceId, ChildKey)> = Vec::new();
        for (id, buffer) in sorted {
            let (reads, writes, children) = {
                let mut buffer = buffer.lock();
                (
                    std::mem::take(&mut buffer.reads),
                    std::mem::take(&mut buffer.writes),
                    std::mem::take(&mut buffer.children),
                )
            };
            let old_children: Option<Vec<ChildKey>> = self
                .epoch
                .lock()
                .round_children_snapshot
                .get(&id)
                .cloned();
            let is_worklist_parent = worklist.contains(&id);
            let (component, is_worklist_parent) = {
                let mut registry = self.registry.lock();
                let old_reads = registry.instances[id as usize].reads.clone();
                for fact in &old_reads {
                    registry.reverse_remove(id, fact);
                }
                registry.instances[id as usize].reads = reads;
                let new_reads = registry.instances[id as usize].reads.clone();
                for fact in &new_reads {
                    registry.reverse_add(id, fact);
                }
                (
                    registry.instances[id as usize].component,
                    is_worklist_parent,
                )
            };
            // Apply writes: ownership validation + deltas.
            let mut applied: Vec<(ViewId, &'static str, Change)> = Vec::new();
            for (view, kind) in &writes {
                let (store, name) = &stores[*view as usize];
                let changes = store
                    .apply(Producer::Component(component), id, kind)
                    .map_err(|error| {
                        self.rollback_epoch();
                        error
                    })?;
                for change in changes {
                    applied.push((*view, name, change));
                }
            }
            {
                let mut registry = self.registry.lock();
                let instance = &mut registry.instances[id as usize];
                for (view, _, change) in &applied {
                    let key = change.key.clone();
                    if !instance
                        .epoch_writes
                        .iter()
                        .any(|(v, k)| *v == *view && k.eq_value(key.as_ref()))
                    {
                        instance.epoch_writes.push((*view, key.clone()));
                    }
                    if !instance
                        .lifetime_writes
                        .iter()
                        .any(|(v, k)| *v == *view && k.eq_value(key.as_ref()))
                    {
                        instance.lifetime_writes.push((*view, key.clone()));
                    }
                }
            }
            {
                let mut epoch = self.epoch.lock();
                for (view, name, change) in applied {
                    accumulate(&mut epoch, view, name, change);
                }
            }
            // Collect the retirement decisions (executed in phase 2).
            if is_worklist_parent {
                let old = old_children.unwrap_or_default();
                for child_key in old {
                    if children
                        .iter()
                        .any(|registered| registered.matches(&child_key))
                    {
                        continue;
                    }
                    retirements.push((id, child_key));
                }
            }
        }
        // Phase 2: retire the round's dead children: their reads leave
        // the reverse index and their lifetime writes retract. Retirement
        // cascades to the child's own children, so a removed subtree
        // retracts entirely (a removed declaration's whole derived
        // subtree dies with it, not just its root node).
        let mut pending_retirements: VecDeque<(InstanceId, ChildKey)> = retirements.into();
        while let Some((parent, child_key)) = pending_retirements.pop_front() {
            let (retired_id, component, lifetime) = {
                let mut registry = self.registry.lock();
                let hash = child_key.hash();
                let Some(bucket) = registry.children.get(&(parent, hash)) else {
                    continue;
                };
                let Some((_, child)) = bucket
                    .iter()
                    .find(|(candidate, _)| candidate.matches(&child_key))
                else {
                    continue;
                };
                let child = *child;
                {
                    let instance = &mut registry.instances[child as usize];
                    if instance.retired {
                        continue;
                    }
                    instance.retired = true;
                }
                let reads = registry.instances[child as usize].reads.clone();
                for fact in &reads {
                    registry.reverse_remove(child, fact);
                }
                // The retired child's own children retire too.
                for ((retired_parent, _), bucket) in registry.children.iter() {
                    if *retired_parent == child {
                        for (key, id) in bucket {
                            if !registry.instances[*id as usize].retired {
                                pending_retirements.push_back((child, key.clone()));
                            }
                        }
                    }
                }
                (
                    child,
                    registry.instances[child as usize].component,
                    std::mem::take(&mut registry.instances[child as usize].lifetime_writes),
                )
            };
            let _ = retired_id;
            for (view, key) in lifetime {
                let (store, name) = &stores[view as usize];
                let changes = store
                    .retract(Producer::Component(component), key.as_ref())
                    .map_err(|error| {
                        self.rollback_epoch();
                        error
                    })?;
                let mut epoch = self.epoch.lock();
                for change in changes {
                    accumulate(&mut epoch, view, name, change);
                }
            }
        }
        // Deferred (same-round topology) ops apply against the round's
        // final candidate state; their changes join the round delta.
        for (view, (store, name)) in stores.iter().enumerate() {
            let changes = store.end_round().map_err(|error| {
                self.rollback_epoch();
                error
            })?;
            let mut epoch = self.epoch.lock();
            for (instance, change) in changes {
                let mut registry = self.registry.lock();
                if let Some(instance) = registry.instances.get_mut(instance as usize) {
                    let key = change.key.clone();
                    if !instance
                        .epoch_writes
                        .iter()
                        .any(|(v, k)| *v == view as u32 && k.eq_value(key.as_ref()))
                    {
                        instance.epoch_writes.push((view as u32, key.clone()));
                    }
                    if !instance
                        .lifetime_writes
                        .iter()
                        .any(|(v, k)| *v == view as u32 && k.eq_value(key.as_ref()))
                    {
                        instance.lifetime_writes.push((view as u32, key.clone()));
                    }
                }
                accumulate(&mut epoch, view as u32, name, change);
            }
        }
        self.epoch.lock().round += 1;
        Ok(())
    }

    /// Builds the next worklist: readers of the delta, deduped, cycle
    /// checked, sorted by (rank, path, id).
    fn build_worklist(&self, delta: &[RawChange], extra: &[InstanceId]) -> Result<Vec<InstanceId>> {
        let registry = self.registry.lock();
        let mut seen: HashSet<InstanceId> = HashSet::new();
        let mut candidates: Vec<InstanceId> = Vec::new();
        for change in delta {
            let fact = FactRef {
                view: change.view,
                key: change.key.clone(),
                temporal: false,
            };
            for reader in registry.readers_of(&fact, false) {
                if seen.insert(*reader) {
                    candidates.push(*reader);
                }
            }
        }
        for id in extra {
            if seen.insert(*id) {
                candidates.push(*id);
            }
        }
        // Cycle rejection (T6): scheduling a visitor whose read set
        // transitively includes a fact it (transitively) wrote.
        let mut checked: Vec<InstanceId> = Vec::with_capacity(candidates.len());
        for id in candidates {
            if let Some(listing) = find_cycle(&registry, id) {
                return Err(Error::FactCycle { listing });
            }
            checked.push(id);
        }
        checked.sort_by(|a, b| {
            let ia = &registry.instances[*a as usize];
            let ib = &registry.instances[*b as usize];
            ia.rank
                .cmp(&ib.rank)
                .then_with(|| ia.path.cmp(&ib.path))
                .then_with(|| ia.component.cmp(&ib.component))
                .then_with(|| a.cmp(b))
        });
        Ok(checked)
    }

    /// Restores every store, instance, index, and counter to its
    /// pre-epoch state (T6 rollback: nothing partial escapes).
    fn rollback_epoch(&self) {
        for view in self.views.lock().iter() {
            view.store.rollback();
        }
        let mut registry = self.registry.lock();
        let epoch = self.epoch.lock();
        let created: HashSet<InstanceId> = epoch.created_instances.iter().copied().collect();
        registry.closures.retain(|id, _| !created.contains(id));
        registry.instances.truncate(epoch.instances_len_at_start);
        for instance in &mut registry.instances {
            instance.retired = false;
            instance.epoch_writes.clear();
            instance.lifetime_writes.clear();
        }
        for (id, reads, lifetime_writes) in &epoch.reads_at_start {
            registry.instances[*id as usize].reads = reads.clone();
            registry.instances[*id as usize].lifetime_writes = lifetime_writes.clone();
        }
        registry.children = epoch.children_at_start.clone();
        registry.rebuild_reverse();
    }

    /// Registers (or reuses) a child visitor instance. Runs from the api
    /// layer while the parent is executing.
    pub(crate) fn register_child(
        &self,
        parent: InstanceId,
        key: ChildKey,
        rank: u32,
        closure: Box<dyn FnMut() -> Result<()> + Send + Sync>,
    ) -> Result<InstanceId> {
        let new_id: Option<InstanceId>;
        {
            let mut registry = self.registry.lock();
            let hash = key.hash();
            let existing: Option<InstanceId> = registry
                .children
                .get(&(parent, hash))
                .and_then(|bucket| {
                    bucket
                        .iter()
                        .find(|(candidate, _)| candidate.matches(&key))
                        .map(|(_, id)| *id)
                });
            match existing {
                Some(id) if !registry.instances[id as usize].retired => {
                    registry.closures.insert(id, closure);
                    return Ok(id);
                }
                Some(id) => {
                    // Retired: a re-creation starts a new lineage; replace the slot.
                    let new_len = registry.instances.len() as InstanceId;
                    if let Some((_, slot)) = registry
                        .children
                        .get_mut(&(parent, hash))
                        .and_then(|bucket| {
                            bucket
                                .iter_mut()
                                .find(|(candidate, _)| candidate.matches(&key))
                        })
                    {
                        *slot = new_len;
                    }
                    let _ = id;
                }
                None => {
                    let new_len = registry.instances.len() as InstanceId;
                    registry
                        .children
                        .entry((parent, hash))
                        .or_default()
                        .push((key.clone(), new_len));
                }
            }
            let (component, parent_path) = {
                let parent_instance = &registry.instances[parent as usize];
                (parent_instance.component, parent_instance.path.clone())
            };
            let id = registry.instances.len() as InstanceId;
            let path = {
                let mut path = parent_path;
                path.push(key.path_step());
                path
            };
            registry.instances.push(Instance {
                id,
                component,
                path,
                rank,
                kind: InstanceKind::Child,
                parent: Some(parent),
                reads: Vec::new(),
                epoch_writes: Vec::new(),
                lifetime_writes: Vec::new(),
                retired: false,
            });
            registry.closures.insert(id, closure);
            new_id = Some(id);
        }
        if let Some(id) = new_id {
            self.epoch.lock().created_instances.push(id);
        }
        Ok(new_id.expect("child registered"))
    }

    /// The rank of one view (for child instance ranks).
    pub(crate) fn view_rank(&self, view: ViewId) -> u32 {
        self.views.lock()[view as usize].rank
    }

    /// The active instance's path (for fresh-id derivation).
    pub(crate) fn active_path(&self) -> Result<Vec<PathStep>> {
        ACTIVE.with(|active| {
            let active = active.borrow();
            let Some(frame) = active.last() else {
                return Err(Error::Internal(
                    "identity allocation outside a visitor".into(),
                ));
            };
            let registry = self.registry.lock();
            Ok(registry.instances[frame.instance as usize].path.clone())
        })
    }

    /// The active instance's component (for fresh-id derivation).
    pub(crate) fn active_component(&self) -> Result<u32> {
        ACTIVE.with(|active| {
            let active = active.borrow();
            let Some(frame) = active.last() else {
                return Err(Error::Internal(
                    "identity allocation outside a visitor".into(),
                ));
            };
            Ok(frame.component)
        })
    }

    /// The view id of one registered view type.

    /// The store + name of one registered view.
    pub(crate) fn view_store<V: ViewSpec>(&self) -> Result<(Arc<dyn DynStore>, ViewId, &'static str)> {
        let view = view_id_of(&self, TypeId::of::<V>())?;
        let views = self.views.lock();
        Ok((Arc::clone(&views[view as usize].store), view, views[view as usize].name))
    }
}

/// The deterministic identity derivation: component ordinal, view, the
/// allocation site's visitor path, and the allocation lane (§5.6).
pub(crate) fn fresh_identity(
    component: u32,
    view: TypeId,
    path: &[PathStep],
    lane: u64,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    component.hash(&mut hasher);
    view.hash(&mut hasher);
    for step in path {
        step.kind.hash(&mut hasher);
        step.elem.hash(&mut hasher);
    }
    lane.hash(&mut hasher);
    hasher.finish()
}

fn view_id_of(shared: &Shared, ty: TypeId) -> Result<ViewId> {
    shared
        .view_by_type
        .lock()
        .get(&ty)
        .copied()
        .ok_or_else(|| Error::ViewNotRegistered {
            view: format!("{ty:?}"),
        })
}

fn accumulate(epoch: &mut Epoch, view: ViewId, view_name: &'static str, change: Change) {
    let hash = change.key.hash_value();
    if !epoch.round_seen.contains(&(view, hash)) {
        epoch.round_seen.insert((view, hash));
        epoch.round_delta.push(RawChange {
            view,
            view_name,
            key: change.key.clone(),
            prev: change.prev.clone(),
            next: change.next.clone(),
        });
    } else if let Some(entry) = epoch
        .round_delta
        .iter_mut()
        .find(|entry| entry.view == view && entry.key.eq_value(change.key.as_ref()))
    {
        entry.next = change.next.clone();
    }
    if !epoch.epoch_seen.contains(&(view, hash)) {
        epoch.epoch_seen.insert((view, hash));
        epoch.epoch_delta.push(RawChange {
            view,
            view_name,
            key: change.key,
            prev: change.prev,
            next: change.next,
        });
    } else if let Some(entry) = epoch
        .epoch_delta
        .iter_mut()
        .find(|entry| entry.view == view && entry.key.eq_value(change.key.as_ref()))
    {
        entry.next = change.next;
    }
}

/// Deterministic DFS from the instance's epoch writes to its read set,
/// returning a cycle listing if one exists (T6).
fn find_cycle(registry: &Registry, id: InstanceId) -> Option<Vec<String>> {
    let instance = &registry.instances[id as usize];
    if instance.epoch_writes.is_empty() {
        return None;
    }
    let read_keys: Vec<FactRef> = instance
        .reads
        .iter()
        .filter(|fact| !fact.temporal)
        .cloned()
        .collect();
    if read_keys.is_empty() {
        return None;
    }
    let mut writes = instance.epoch_writes.clone();
    writes.sort_by(|(a_view, a_key), (b_view, b_key)| {
        a_view
            .cmp(b_view)
            .then_with(|| format!("{:?}", a_key).cmp(&format!("{:?}", b_key)))
    });
    for (view, key) in writes {
        let start = FactRef {
            view,
            key: key.clone(),
            temporal: false,
        };
        let mut visited: HashSet<(ViewId, u64)> = HashSet::new();
        let mut stack: Vec<(FactRef, Vec<String>)> = vec![(
            start.clone(),
            vec![format!("fact {key:?} in view[{view}]")],
        )];
        while let Some((fact, path)) = stack.pop() {
            if read_keys.iter().any(|read| {
                read.view == fact.view && read.key.eq_value(fact.key.as_ref())
            }) {
                let mut listing = path.clone();
                listing.push(format!(
                    "fact {fact:?} read by visitor <{}>",
                    path_of(registry, id)
                ));
                return Some(listing);
            }
            if !visited.insert((fact.view, fact.key.hash_value())) {
                continue;
            }
            let readers: Vec<InstanceId> = registry
                .readers_of(&fact, false)
                .iter()
                .copied()
                .collect();
            let mut next: Vec<(FactRef, Vec<String>)> = Vec::new();
            for reader in readers {
                let reader_path = path_of(registry, reader);
                let mut writer_writes: Vec<(ViewId, Arc<dyn KeyValue>)> =
                    registry.instances[reader as usize].epoch_writes.clone();
                writer_writes.sort_by(|(a_view, a_key), (b_view, b_key)| {
                    a_view
                        .cmp(b_view)
                        .then_with(|| format!("{:?}", a_key).cmp(&format!("{:?}", b_key)))
                });
                for (w_view, w_key) in writer_writes {
                    let mut next_path = path.clone();
                    next_path.push(format!("fact {fact:?} read by visitor <{reader_path}>"));
                    next_path.push(format!("writes fact {w_key:?} in view[{w_view}]"));
                    next.push((
                        FactRef {
                            view: w_view,
                            key: w_key,
                            temporal: false,
                        },
                        next_path,
                    ));
                }
            }
            stack.extend(next);
        }
    }
    None
}

fn path_of(registry: &Registry, id: InstanceId) -> String {
    let instance = &registry.instances[id as usize];
    let mut path = String::new();
    for step in &instance.path {
        path.push_str(step.kind);
        if !step.elem.is_empty() {
            path.push('[');
            path.push_str(&step.elem);
            path.push(']');
        }
        path.push('/');
    }
    path
}

fn panic_message(panic: &Box<dyn Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------

/// Registers views and signature edges for one component (or for
/// `external`/`subscribe` declarations).
pub struct EngineBuilder<'a> {
    pub(crate) shared: &'a Arc<Shared>,
    pub(crate) component: Option<u32>,
}

impl EngineBuilder<'_> {
    /// Registers (idempotently) and observes a view.
    pub fn observe<V: ViewSpec>(&mut self) -> Result<()> {
        let view = self.register_view::<V>()?;
        if let Some(component) = self.component {
            let mut components = self.shared.components.lock();
            let entry = &mut components[component as usize];
            if !entry.observed.iter().any(|(v, _)| *v == view) {
                entry.observed.push((view, false));
            }
        }
        Ok(())
    }

    /// Registers (idempotently) and observes a view temporally (`Previous`).
    pub fn previous<V: ViewSpec>(&mut self) -> Result<()> {
        let view = self.register_view::<V>()?;
        if let Some(component) = self.component {
            let mut components = self.shared.components.lock();
            let entry = &mut components[component as usize];
            if !entry.observed.iter().any(|(v, _)| *v == view) {
                entry.observed.push((view, true));
            }
        }
        Ok(())
    }

    /// Registers (idempotently) and emits into a view (joins its producer
    /// set — multi-producer views are ordinary for every shape).
    pub fn emit<V: ViewSpec>(&mut self) -> Result<()> {
        let view = self.register_view::<V>()?;
        if let Some(component) = self.component {
            let mut components = self.shared.components.lock();
            let entry = &mut components[component as usize];
            if !entry.emitted.contains(&view) {
                entry.emitted.push(view);
            }
            let label = format!("component[{}] ({})", component, entry.name);
            let mut views = self.shared.views.lock();
            let producers = &mut views[view as usize].producers;
            if !producers.contains(&label) {
                producers.push(label);
            }
        }
        Ok(())
    }

    pub(crate) fn register_view<V: ViewSpec>(&mut self) -> Result<ViewId> {
        let ty = TypeId::of::<V>();
        {
            let by_type = self.shared.view_by_type.lock();
            if let Some(&view) = by_type.get(&ty) {
                return Ok(view);
            }
        }
        let store = V::new_store();
        let view = {
            let mut views = self.shared.views.lock();
            let id = views.len() as ViewId;
            views.push(ViewEntry {
                name: V::view_name(),
                store: Arc::from(store),
                rank: 0,
                producers: Vec::new(),
                external: false,
            });
            id
        };
        self.shared.view_by_type.lock().insert(ty, view);
        Ok(view)
    }
}

// ---------------------------------------------------------------------------
// Snapshot (committed-state observation)
// ---------------------------------------------------------------------------

/// A read-only view of the committed state.
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) shared: Arc<Shared>,
}

impl Snapshot {
    pub fn box_view<V: BoxView>(&self) -> SnapshotBox<V> {
        SnapshotBox {
            store: self.store_of::<V>(),
            _marker: std::marker::PhantomData,
        }
    }
    pub fn map_view<V: MapView>(&self) -> SnapshotMap<V> {
        SnapshotMap {
            store: self.store_of::<V>(),
            _marker: std::marker::PhantomData,
        }
    }
    pub fn tree_view<V: TreeView>(&self) -> SnapshotTree<V> {
        SnapshotTree {
            store: self.store_of::<V>(),
            _marker: std::marker::PhantomData,
        }
    }
    pub fn graph_view<V: GraphView>(&self) -> SnapshotGraph<V> {
        SnapshotGraph {
            store: self.store_of::<V>(),
            _marker: std::marker::PhantomData,
        }
    }

    fn store_of<V: ViewSpec>(&self) -> Arc<dyn DynStore> {
        let ty = TypeId::of::<V>();
        let view = *self
            .shared
            .view_by_type
            .lock()
            .get(&ty)
            .expect("snapshot of an unregistered view");
        Arc::clone(&self.shared.views.lock()[view as usize].store)
    }
}

pub struct SnapshotBox<V: BoxView> {
    store: Arc<dyn DynStore>,
    _marker: std::marker::PhantomData<V>,
}

impl<V: BoxView> SnapshotBox<V> {
    pub fn get(&self) -> Option<Arc<V::Value>> {
        self.store
            .read_committed(&crate::reactive::view::BoxFactKey::Value)
            .and_then(|value| downcast_value(value))
    }
}

pub struct SnapshotMap<V: MapView> {
    store: Arc<dyn DynStore>,
    _marker: std::marker::PhantomData<V>,
}

impl<V: MapView> SnapshotMap<V> {
    pub fn get(&self, key: &V::Key) -> Option<Arc<V::Value>> {
        let fact: Arc<dyn KeyValue> =
            Arc::new(crate::reactive::view::MapFactKey::Entry(key.clone()));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
    }
    pub fn contains(&self, key: &V::Key) -> bool {
        self.get(key).is_some()
    }
    pub fn keys(&self) -> Vec<V::Key> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::MapFactKey::<V::Key>::Keys);
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
            .map(|keys: Arc<Vec<V::Key>>| (*keys).clone())
            .unwrap_or_default()
    }
}

pub struct SnapshotTree<V: TreeView> {
    store: Arc<dyn DynStore>,
    _marker: std::marker::PhantomData<V>,
}

impl<V: TreeView> SnapshotTree<V> {
    pub fn node(&self, id: NodeId) -> Option<Arc<V::Value>> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::TreeFactKey::Node(id));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
    }
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::TreeFactKey::Children(id));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
            .map(|kids: Arc<Vec<NodeId>>| (*kids).clone())
            .unwrap_or_default()
    }
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::TreeFactKey::Parent(id));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
            .and_then(|parent: Arc<Option<NodeId>>| *parent)
    }
    pub fn roots(&self) -> Vec<NodeId> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::TreeFactKey::Roots);
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
            .map(|roots: Arc<Vec<NodeId>>| (*roots).clone())
            .unwrap_or_default()
    }
}

pub struct SnapshotGraph<V: GraphView> {
    store: Arc<dyn DynStore>,
    _marker: std::marker::PhantomData<V>,
}

impl<V: GraphView> SnapshotGraph<V> {
    pub fn node(&self, id: NodeId) -> Option<Arc<V::Value>> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::GraphFactKey::<V::Label>::Node(id));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
    }
    pub fn edge(&self, source: NodeId, label: &V::Label, target: NodeId) -> Option<Arc<V::Edge>> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::GraphFactKey::Edge(
            crate::reactive::view::GraphEdgeKey {
                source,
                label: label.clone(),
                target,
            },
        ));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
    }
    pub fn outgoing(
        &self,
        source: NodeId,
        label: &V::Label,
    ) -> Vec<crate::reactive::view::GraphEdgeKey<V::Label>> {
        let fact: Arc<dyn KeyValue> =
            Arc::new(crate::reactive::view::GraphFactKey::Bucket(source, label.clone()));
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| {
                downcast_value(value).map(
                    |edges: Arc<Vec<crate::reactive::view::GraphEdgeKey<V::Label>>>| {
                        (*edges).clone()
                    },
                )
            })
            .unwrap_or_default()
    }
    pub fn nodes(&self) -> Vec<NodeId> {
        let fact: Arc<dyn KeyValue> = Arc::new(crate::reactive::view::GraphFactKey::<V::Label>::Nodes);
        self.store
            .read_committed(fact.as_ref())
            .and_then(|value| downcast_value(value))
            .map(|nodes: Arc<Vec<NodeId>>| (*nodes).clone())
            .unwrap_or_default()
    }
}

/// Downcasts an erased value (infallible for the view's own type).
pub(crate) fn downcast_value<V: Value>(value: Arc<dyn Value>) -> Option<Arc<V>> {
    let any: Arc<dyn Any + Send + Sync> = value;
    any.downcast::<V>().ok()
}
