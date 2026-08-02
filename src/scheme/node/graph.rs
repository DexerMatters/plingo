use arc_swap::ArcSwap;

use super::{
    api::{
        Command, DefinitionEdge, EdgeKind, IndexedRelation, InputNode, NodeError, NodeInspection,
        NodeProvider, NodeSchema, PortKind, ReadGraph, Relation, SnapshotId, View,
    },
    engine::{
        CommandCx, DeferredChildFactory, ErasedProvider, ProviderEntry, StagedProviderState,
        Transaction, commit_provider_states,
    },
    identity::{ErasedValue, FactId, RelationFactId, TaskId, typed_value},
    state::GraphState,
};
use std::{
    any::{TypeId, type_name},
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex, mpsc},
};

use crate::component::api::{Component, ComponentProvider, component_task};

#[derive(Clone)]
pub struct Snapshot {
    id: SnapshotId,
    state: Arc<GraphState>,
}

impl Snapshot {
    pub fn id(&self) -> SnapshotId {
        self.id
    }

    /// Returns the revision in which a view key last changed.
    pub fn changed_at<V: View>(&self, key: V::Key) -> Option<SnapshotId> {
        self.state
            .facts
            .get(&FactId::new::<V>(key))
            .map(|fact| fact.changed_at)
    }
}

impl ReadGraph for Snapshot {
    fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.state, key)
    }

    fn contains<R: Relation>(&self, fact: R::Fact) -> bool {
        self.state
            .relation_supports
            .contains_key(&RelationFactId::new::<R>(fact))
    }

    fn scan<R: IndexedRelation>(&self, index: R::Index) -> Vec<R::Fact> {
        self.scan_all::<R>()
            .into_iter()
            .filter(|fact| R::index(fact) == index)
            .collect()
    }

    fn scan_all<R: Relation>(&self) -> Vec<R::Fact> {
        self.state
            .relation_supports
            .keys()
            .filter_map(RelationFactId::get::<R>)
            .collect()
    }
}

/// Cloneable, read-only access to the latest successfully committed state.
/// Writes remain serialized through [`Graph`] or [`super::GraphHandle`].
#[derive(Clone)]
pub struct GraphReader {
    current: Arc<ArcSwap<GraphState>>,
}

impl GraphReader {
    /// Captures the current immutable snapshot. The returned value remains
    /// stable even when later transactions commit.
    pub fn snapshot(&self) -> Snapshot {
        let state = self.current.load_full();
        Snapshot {
            id: state.revision,
            state,
        }
    }

    pub fn changed_at<V: View>(&self, key: V::Key) -> Option<SnapshotId> {
        self.snapshot().changed_at::<V>(key)
    }
}

impl ReadGraph for GraphReader {
    fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        self.snapshot().get::<V>(key)
    }
    fn contains<R: Relation>(&self, fact: R::Fact) -> bool {
        self.snapshot().contains::<R>(fact)
    }
    fn scan<R: IndexedRelation>(&self, index: R::Index) -> Vec<R::Fact> {
        self.snapshot().scan::<R>(index)
    }
    fn scan_all<R: Relation>(&self) -> Vec<R::Fact> {
        self.snapshot().scan_all::<R>()
    }
}

/// A committed change observed through a subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewUpdate<V> {
    Initial { snapshot: SnapshotId, value: V },
    Changed { snapshot: SnapshotId, value: V },
    Removed { snapshot: SnapshotId },
}

/// A subscription to one materialized map fact.
pub struct Subscription<V: View> {
    receiver: mpsc::Receiver<ViewUpdate<V::Value>>,

    _cleanup: SubscriberLease,
    _view: PhantomData<fn() -> V>,
}

impl<V: View> Subscription<V> {
    /// Blocks until the next committed update or until every graph handle is
    /// dropped.  Consumers that need async integration can bridge this receiver
    /// in their own executor without granting mutation access to the graph.
    pub fn recv(&self) -> Result<ViewUpdate<V::Value>, mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn try_recv(&self) -> Result<ViewUpdate<V::Value>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

/// A committed presence transition for one relation fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelationUpdate<F> {
    Initial { snapshot: SnapshotId, present: bool },
    Added { snapshot: SnapshotId, fact: F },
    Removed { snapshot: SnapshotId, fact: F },
}

/// Observation of one support-counted relation fact.
pub struct RelationSubscription<R: Relation> {
    receiver: mpsc::Receiver<RelationUpdate<R::Fact>>,
    _cleanup: SubscriberLease,
    _relation: PhantomData<fn() -> R>,
}

impl<R: Relation> RelationSubscription<R> {
    pub fn recv(&self) -> Result<RelationUpdate<R::Fact>, mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn try_recv(&self) -> Result<RelationUpdate<R::Fact>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

type SubscriberId = u64;

enum SubscriberTarget {
    View(FactId),
    Relation(RelationFactId),
}

struct SubscriberLease {
    id: SubscriberId,
    target: SubscriberTarget,
    releases: Arc<Mutex<Vec<(SubscriberId, SubscriberTarget)>>>,
}

impl Drop for SubscriberLease {
    fn drop(&mut self) {
        if let Ok(mut releases) = self.releases.lock() {
            let target = match &self.target {
                SubscriberTarget::View(fact) => SubscriberTarget::View(fact.clone()),
                SubscriberTarget::Relation(fact) => SubscriberTarget::Relation(fact.clone()),
            };
            releases.push((self.id, target));
        }
    }
}

/// A demand lease keeps one provider instance and its descendants materialized;
/// dropping it queues their release.
pub struct DemandLease {
    task: TaskId,
    releases: Arc<Mutex<Vec<TaskId>>>,
}

impl Drop for DemandLease {
    fn drop(&mut self) {
        if let Ok(mut releases) = self.releases.lock() {
            releases.push(self.task.clone());
        }
    }
}

/// Result returned by a synchronous relation-presence effect handler.
pub type RelationEffectResult = Result<(), String>;

/// A relation effect handler failure recorded after its transaction committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectFailure {
    pub snapshot: SnapshotId,
    pub relation: TypeId,
    pub relation_name: &'static str,
    pub fact: String,
    pub message: String,
}

trait ErasedSubscriber: Send {
    fn id(&self) -> SubscriberId;
    /// Returns false when the receiver has been dropped.
    fn send(&self, snapshot: SnapshotId, value: Option<&Arc<dyn ErasedValue>>) -> bool;
}

struct TypedSubscriber<V: View> {
    id: SubscriberId,
    sender: mpsc::Sender<ViewUpdate<V::Value>>,
    _view: PhantomData<fn() -> V>,
}

impl<V: View> ErasedSubscriber for TypedSubscriber<V> {
    fn id(&self) -> SubscriberId {
        self.id
    }

    fn send(&self, snapshot: SnapshotId, value: Option<&Arc<dyn ErasedValue>>) -> bool {
        let update = match value.and_then(typed_value::<V>) {
            Some(value) => ViewUpdate::Changed { snapshot, value },
            None => ViewUpdate::Removed { snapshot },
        };
        self.sender.send(update).is_ok()
    }
}

trait ErasedRelationSubscriber: Send {
    fn id(&self) -> SubscriberId;
    fn send(&self, snapshot: SnapshotId, present: bool) -> bool;
}

struct TypedRelationSubscriber<R: Relation> {
    id: SubscriberId,
    fact: R::Fact,
    sender: mpsc::Sender<RelationUpdate<R::Fact>>,
}

impl<R: Relation> ErasedRelationSubscriber for TypedRelationSubscriber<R> {
    fn id(&self) -> SubscriberId {
        self.id
    }

    fn send(&self, snapshot: SnapshotId, present: bool) -> bool {
        let update = if present {
            RelationUpdate::Added {
                snapshot,
                fact: self.fact.clone(),
            }
        } else {
            RelationUpdate::Removed {
                snapshot,
                fact: self.fact.clone(),
            }
        };
        self.sender.send(update).is_ok()
    }
}

trait ErasedRelationEffect: Send + Sync {
    fn work(&self, snapshot: SnapshotId, relation: &RelationFactId) -> Option<EffectWork>;
}

struct TypedRelationEffect<R: Relation, F> {
    handler: Arc<F>,
    _relation: PhantomData<fn() -> R>,
}

impl<R, F> ErasedRelationEffect for TypedRelationEffect<R, F>
where
    R: Relation,
    R::Fact: fmt::Debug,
    F: Fn(SnapshotId, R::Fact) -> RelationEffectResult + Send + Sync + 'static,
{
    fn work(&self, snapshot: SnapshotId, relation: &RelationFactId) -> Option<EffectWork> {
        let fact = relation.get::<R>()?;
        let fact_debug = format!("{fact:?}");
        let handler = Arc::clone(&self.handler);
        Some(EffectWork {
            snapshot,
            relation: TypeId::of::<R>(),
            relation_name: type_name::<R>(),
            fact: fact_debug,
            run: Box::new(move |_| handler(snapshot, fact)),
        })
    }
}

struct TypedRelationCommandEffect<R: Relation, C: Command, F> {
    handler: Arc<F>,
    _relation: PhantomData<fn() -> R>,
    _command: PhantomData<fn() -> C>,
}

impl<R, C, F> ErasedRelationEffect for TypedRelationCommandEffect<R, C, F>
where
    R: Relation,
    R::Fact: fmt::Debug,
    C: Command + 'static,
    F: Fn(SnapshotId, R::Fact) -> Result<C, String> + Send + Sync + 'static,
{
    fn work(&self, snapshot: SnapshotId, relation: &RelationFactId) -> Option<EffectWork> {
        let fact = relation.get::<R>()?;
        let fact_debug = format!("{fact:?}");
        let handler = Arc::clone(&self.handler);
        Some(EffectWork {
            snapshot,
            relation: TypeId::of::<R>(),
            relation_name: type_name::<R>(),
            fact: fact_debug,
            run: Box::new(move |graph| {
                let command = handler(snapshot, fact)?;
                graph
                    .command(command)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }),
        })
    }
}

struct EffectWork {
    snapshot: SnapshotId,
    relation: TypeId,
    relation_name: &'static str,
    fact: String,
    run: Box<dyn FnOnce(&mut Graph) -> RelationEffectResult + Send>,
}

/// Transactional runtime of providers, typed ports, facts, demands, and subscriptions.
pub struct Graph {
    state: Arc<GraphState>,
    current: Arc<ArcSwap<GraphState>>,
    providers: HashMap<TypeId, Arc<dyn ErasedProvider>>,
    input_schemas: HashMap<TypeId, NodeSchema>,
    definition_edges: Vec<DefinitionEdge>,
    deferred_children: Arc<HashMap<TypeId, Vec<DeferredChildFactory>>>,
    subscribers: HashMap<FactId, Vec<Box<dyn ErasedSubscriber>>>,
    relation_subscribers: HashMap<RelationFactId, Vec<Box<dyn ErasedRelationSubscriber>>>,
    relation_added_effects: HashMap<TypeId, Vec<Box<dyn ErasedRelationEffect>>>,
    relation_removed_effects: HashMap<TypeId, Vec<Box<dyn ErasedRelationEffect>>>,
    effect_failures: Vec<EffectFailure>,
    pending_effects: VecDeque<EffectWork>,
    draining_effects: bool,
    deferred_releases: Arc<Mutex<Vec<TaskId>>>,
    deferred_subscriber_removals: Arc<Mutex<Vec<(SubscriberId, SubscriberTarget)>>>,
    next_subscriber: SubscriberId,
    workers: usize,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    /// Creates a graph that automatically uses the machine's available
    /// parallelism for isolated ready-task waves.
    pub fn new() -> Self {
        Self::with_workers(
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
        )
    }

    /// Creates a graph with a bounded worker count. One worker is useful for
    /// deterministic scheduler debugging; normal hosts should use [`Self::new`].
    pub fn with_workers(workers: usize) -> Self {
        let state = Arc::new(GraphState::default());
        Self {
            current: Arc::new(ArcSwap::from(Arc::clone(&state))),
            state,
            providers: HashMap::new(),
            input_schemas: HashMap::new(),
            definition_edges: Vec::new(),
            deferred_children: Arc::new(HashMap::new()),
            subscribers: HashMap::new(),
            relation_subscribers: HashMap::new(),
            relation_added_effects: HashMap::new(),
            relation_removed_effects: HashMap::new(),
            effect_failures: Vec::new(),
            pending_effects: VecDeque::new(),
            draining_effects: false,
            deferred_releases: Arc::new(Mutex::new(Vec::new())),
            deferred_subscriber_removals: Arc::new(Mutex::new(Vec::new())),
            next_subscriber: 0,
            workers: workers.max(1),
        }
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    pub fn revision(&self) -> SnapshotId {
        self.state.revision
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            id: self.state.revision,
            state: Arc::clone(&self.state),
        }
    }

    /// Returns a cloneable, lock-free reader for the latest committed state.
    pub fn reader(&self) -> GraphReader {
        GraphReader {
            current: Arc::clone(&self.current),
        }
    }

    /// Installs one provider. Provider kinds are unique capabilities in a
    /// graph and their complete port schema is enforced at publication time.
    pub fn install<P: NodeProvider>(&mut self, provider: P) -> Result<(), NodeError> {
        if self.providers.contains_key(&TypeId::of::<P>()) {
            return Err(NodeError::DuplicateProvider(type_name::<P>()));
        }
        let schema = P::schema();
        self.record_publication_edges(&schema);
        self.providers
            .insert(TypeId::of::<P>(), Arc::new(ProviderEntry(provider)));
        Ok(())
    }

    /// Registers a first-class input-node schema. Commands remain the only
    /// mutation authority for its map ports.
    pub fn install_input<I: InputNode>(&mut self) -> Result<(), NodeError> {
        let id = TypeId::of::<I>();
        if self.input_schemas.contains_key(&id) {
            return Err(NodeError::DuplicateInput(type_name::<I>()));
        }
        let schema = I::schema();
        self.record_publication_edges(&schema);
        self.input_schemas.insert(id, schema);
        Ok(())
    }

    /// Returns schemas for installed input authorities and derived providers.
    pub fn schemas(&self) -> Vec<NodeSchema> {
        let mut schemas = self.input_schemas.values().cloned().collect::<Vec<_>>();
        schemas.extend(self.providers.values().map(|provider| provider.schema()));
        schemas.sort_by_key(|schema| schema.provider);
        schemas
    }

    pub fn definition_edges(&self) -> &[DefinitionEdge] {
        &self.definition_edges
    }

    fn record_publication_edges(&mut self, schema: &NodeSchema) {
        self.definition_edges
            .extend(schema.ports.iter().map(|port| DefinitionEdge {
                from: schema.provider,
                to: port.name,
                kind: match port.kind {
                    PortKind::Map => EdgeKind::Publishes,
                    PortKind::Set | PortKind::IndexedSet => EdgeKind::Supports,
                },
            }));
    }

    /// Inspects one live provider instance and its typed edges.
    pub fn inspect<P: NodeProvider>(&self, key: P::Key) -> NodeInspection {
        self.inspect_task(TaskId::new::<P>(key))
    }

    /// Inspects one live component instance.
    pub fn inspect_component<C: Component>(&self, value: C) -> NodeInspection {
        self.inspect_task(component_task(value))
    }

    fn inspect_task(&self, task: TaskId) -> NodeInspection {
        NodeInspection {
            materialized: self.state.task_outputs.contains_key(&task),
            root_pins: self.state.task_pins.get(&task).copied().unwrap_or_default(),
            keeping_parents: self.state.child_parents.get(&task).map_or(0, HashSet::len),
            publications: self.state.task_outputs.get(&task).map_or(0, HashSet::len),
            relation_supports: self
                .state
                .task_relation_outputs
                .get(&task)
                .map_or(0, HashSet::len),
            dependencies: self
                .state
                .task_dependencies
                .get(&task)
                .map_or(0, HashSet::len),
            children: self.state.task_children.get(&task).map_or(0, HashSet::len),
        }
    }

    /// Declares a typed keeps-alive edge from one provider kind to another.
    pub fn connect<Parent, Child>(
        &mut self,
        key: impl Fn(Parent::Key) -> Child::Key + Send + Sync + 'static,
    ) -> Result<(), NodeError>
    where
        Parent: NodeProvider,
        Child: NodeProvider,
    {
        if !self.providers.contains_key(&TypeId::of::<Parent>()) {
            return Err(NodeError::MissingProvider(type_name::<Parent>()));
        }
        if !self.providers.contains_key(&TypeId::of::<Child>()) {
            return Err(NodeError::MissingProvider(type_name::<Child>()));
        }
        self.definition_edges.push(DefinitionEdge {
            from: type_name::<Parent>(),
            to: type_name::<Child>(),
            kind: EdgeKind::KeepsAlive,
        });
        Arc::make_mut(&mut self.deferred_children)
            .entry(TypeId::of::<Parent>())
            .or_default()
            .push(Arc::new(move |parent| {
                parent
                    .key
                    .get::<Parent::Key>()
                    .map(|parent_key| TaskId::new::<Child>(key(parent_key)))
            }));
        Ok(())
    }

    /// Declares a typed keeps-alive edge from a kernel provider to a component.
    pub fn connect_component<Parent, Child>(
        &mut self,
        key: impl Fn(Parent::Key) -> Child + Send + Sync + 'static,
    ) -> Result<(), NodeError>
    where
        Parent: NodeProvider,
        Child: Component,
    {
        if !self.providers.contains_key(&TypeId::of::<Parent>()) {
            return Err(NodeError::MissingProvider(type_name::<Parent>()));
        }
        if !self
            .providers
            .contains_key(&TypeId::of::<ComponentProvider<Child>>())
        {
            return Err(NodeError::MissingProvider(type_name::<Child>()));
        }
        self.definition_edges.push(DefinitionEdge {
            from: type_name::<Parent>(),
            to: type_name::<Child>(),
            kind: EdgeKind::KeepsAlive,
        });
        Arc::make_mut(&mut self.deferred_children)
            .entry(TypeId::of::<Parent>())
            .or_default()
            .push(Arc::new(move |parent| {
                parent
                    .key
                    .get::<Parent::Key>()
                    .map(|parent_key| component_task(key(parent_key)))
            }));
        Ok(())
    }

    /// Declares a typed keeps-alive edge from one component kind to another.
    pub fn connect_components<Parent, Child>(
        &mut self,
        key: impl Fn(Parent) -> Child + Send + Sync + 'static,
    ) -> Result<(), NodeError>
    where
        Parent: Component,
        Child: Component,
    {
        if !self
            .providers
            .contains_key(&TypeId::of::<ComponentProvider<Parent>>())
        {
            return Err(NodeError::MissingProvider(type_name::<Parent>()));
        }
        if !self
            .providers
            .contains_key(&TypeId::of::<ComponentProvider<Child>>())
        {
            return Err(NodeError::MissingProvider(type_name::<Child>()));
        }
        self.definition_edges.push(DefinitionEdge {
            from: type_name::<Parent>(),
            to: type_name::<Child>(),
            kind: EdgeKind::KeepsAlive,
        });
        Arc::make_mut(&mut self.deferred_children)
            .entry(TypeId::of::<ComponentProvider<Parent>>())
            .or_default()
            .push(Arc::new(move |parent| {
                parent
                    .key
                    .get::<Parent>()
                    .map(|parent_key| component_task(key(parent_key)))
            }));
        Ok(())
    }

    /// Subscribes to one relation fact. The initial event records its current
    /// presence; later events are delivered only after a transaction commits.
    pub fn subscribe_relation<R: Relation>(&mut self, fact: R::Fact) -> RelationSubscription<R> {
        let relation = RelationFactId::new::<R>(fact.clone());
        let id = self.allocate_subscriber();
        let present = self.state.relation_supports.contains_key(&relation);
        let (sender, receiver) = mpsc::channel();
        sender
            .send(RelationUpdate::Initial {
                snapshot: self.state.revision,
                present,
            })
            .expect("fresh relation subscription receiver is live");
        self.relation_subscribers
            .entry(relation.clone())
            .or_default()
            .push(Box::new(TypedRelationSubscriber::<R> { id, fact, sender }));
        RelationSubscription {
            receiver,
            _cleanup: SubscriberLease {
                id,
                target: SubscriberTarget::Relation(relation),
                releases: Arc::clone(&self.deferred_subscriber_removals),
            },
            _relation: PhantomData,
        }
    }

    /// Registers a synchronous handler for future first-support transitions of
    /// a relation fact. Work is queued from the final committed state and runs
    /// only after the state and all subscriptions have been published.
    pub fn on_relation_added<R: Relation>(
        &mut self,
        handler: impl Fn(SnapshotId, R::Fact) -> RelationEffectResult + Send + Sync + 'static,
    ) where
        R::Fact: fmt::Debug,
    {
        self.relation_added_effects
            .entry(TypeId::of::<R>())
            .or_default()
            .push(Box::new(TypedRelationEffect::<R, _> {
                handler: Arc::new(handler),
                _relation: PhantomData,
            }));
    }

    /// Registers a synchronous handler for future last-support transitions of
    /// a relation fact. See [`Self::on_relation_added`] for execution timing.
    pub fn on_relation_removed<R: Relation>(
        &mut self,
        handler: impl Fn(SnapshotId, R::Fact) -> RelationEffectResult + Send + Sync + 'static,
    ) where
        R::Fact: fmt::Debug,
    {
        self.relation_removed_effects
            .entry(TypeId::of::<R>())
            .or_default()
            .push(Box::new(TypedRelationEffect::<R, _> {
                handler: Arc::new(handler),
                _relation: PhantomData,
            }));
    }

    /// Registers a post-commit relation effect that issues a follow-up graph
    /// command. The command runs in a later transaction, never during the
    /// derivation that produced the relation transition.
    pub fn on_relation_added_command<R, C>(
        &mut self,
        handler: impl Fn(SnapshotId, R::Fact) -> Result<C, String> + Send + Sync + 'static,
    ) where
        R: Relation,
        R::Fact: fmt::Debug,
        C: Command + 'static,
    {
        self.relation_added_effects
            .entry(TypeId::of::<R>())
            .or_default()
            .push(Box::new(TypedRelationCommandEffect::<R, C, _> {
                handler: Arc::new(handler),
                _relation: PhantomData,
                _command: PhantomData,
            }));
    }

    /// Returns failures from post-commit relation effect handlers without
    /// removing them from the graph's failure log.
    pub fn effect_failures(&self) -> &[EffectFailure] {
        &self.effect_failures
    }

    #[cfg(test)]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Returns and clears all recorded post-commit relation effect failures.
    pub fn drain_effect_failures(&mut self) -> Vec<EffectFailure> {
        std::mem::take(&mut self.effect_failures)
    }

    /// Returns the revision in which a view key last changed.
    pub fn changed_at<V: View>(&self, key: V::Key) -> Option<SnapshotId> {
        self.state
            .facts
            .get(&FactId::new::<V>(key))
            .map(|fact| fact.changed_at)
    }

    /// Demands one provider instance and returns an RAII materialization lease.
    /// The lease is the only root liveness handle; published values are read
    /// through the shared [`ReadGraph`] protocol.
    pub fn demand<P: NodeProvider>(&mut self, key: P::Key) -> Result<DemandLease, NodeError> {
        self.collect_garbage()?;
        if !self.providers.contains_key(&TypeId::of::<P>()) {
            return Err(NodeError::MissingProvider(type_name::<P>()));
        }
        let base = self.state.revision;
        let target = base.checked_add(1).ok_or(NodeError::RevisionOverflow)?;
        let mut transaction = Transaction::new(
            Arc::new(self.providers.clone()),
            Arc::clone(&self.deferred_children),
            (*self.state).clone(),
            target,
        );
        let task = TaskId::new::<P>(key);
        transaction.pin(task.clone());
        if !transaction.state.task_outputs.contains_key(&task) {
            transaction.schedule(task.clone());
        }
        transaction.run_pending(self.workers)?;
        let (state, provider_states) = transaction.finish();
        self.commit_transaction(state, provider_states);
        Ok(DemandLease {
            task,
            releases: Arc::clone(&self.deferred_releases),
        })
    }

    /// Requests one component instance and returns an RAII materialization
    /// lease. The component value is both the identity and the input; no
    /// separate key type exists.
    pub fn request<C: Component>(&mut self, value: C) -> Result<DemandLease, NodeError> {
        self.collect_garbage()?;
        if !self
            .providers
            .contains_key(&TypeId::of::<ComponentProvider<C>>())
        {
            return Err(NodeError::MissingProvider(type_name::<C>()));
        }
        let base = self.state.revision;
        let target = base.checked_add(1).ok_or(NodeError::RevisionOverflow)?;
        let mut transaction = Transaction::new(
            Arc::new(self.providers.clone()),
            Arc::clone(&self.deferred_children),
            (*self.state).clone(),
            target,
        );
        let task = component_task(value);
        transaction.pin(task.clone());
        if !transaction.state.task_outputs.contains_key(&task) {
            transaction.schedule(task.clone());
        }
        transaction.run_pending(self.workers)?;
        let (state, provider_states) = transaction.finish();
        self.commit_transaction(state, provider_states);
        Ok(DemandLease {
            task,
            releases: Arc::clone(&self.deferred_releases),
        })
    }

    /// Registers one component kind. The component's declared [`WriteSet`]
    /// (plus its canonical output port) becomes its enforced publication
    /// schema.
    pub fn register<C: Component>(&mut self) -> Result<(), NodeError> {
        let id = TypeId::of::<ComponentProvider<C>>();
        if self.providers.contains_key(&id) {
            return Err(NodeError::DuplicateProvider(type_name::<C>()));
        }
        let provider = ComponentProvider::<C>::new();
        let schema = provider.schema();
        self.record_publication_edges(&schema);
        self.providers.insert(id, Arc::new(provider));
        Ok(())
    }

    /// Subscribes to one materialized map port. Demand is intentionally
    /// separate: a subscription observes facts but does not silently create a
    /// hidden provider lease.
    pub fn subscribe<V: View>(&mut self, key: V::Key) -> Result<Subscription<V>, NodeError> {
        let value = self
            .get::<V>(key.clone())
            .ok_or(NodeError::MissingView(type_name::<V>()))?;
        let fact = FactId::new::<V>(key);
        let id = self.allocate_subscriber();
        let (sender, receiver) = mpsc::channel();
        sender
            .send(ViewUpdate::Initial {
                snapshot: self.state.revision,
                value,
            })
            .map_err(|error| NodeError::message(error.to_string()))?;
        self.subscribers
            .entry(fact.clone())
            .or_default()
            .push(Box::new(TypedSubscriber::<V> {
                id,
                sender,
                _view: PhantomData,
            }));
        Ok(Subscription {
            receiver,
            _cleanup: SubscriberLease {
                id,
                target: SubscriberTarget::View(fact),
                releases: Arc::clone(&self.deferred_subscriber_removals),
            },
            _view: PhantomData,
        })
    }

    /// Explicitly releasing a demand is unnecessary: dropping [`DemandLease`]
    /// queues reclamation and [`Self::collect_garbage`] applies it.
    fn allocate_subscriber(&mut self) -> SubscriberId {
        let id = self.next_subscriber;
        self.next_subscriber = self.next_subscriber.wrapping_add(1);
        id
    }

    /// Applies queued RAII lease drops in one transaction.
    pub fn collect_garbage(&mut self) -> Result<(), NodeError> {
        self.collect_dropped_subscribers()?;
        let releases = {
            let mut queued = self
                .deferred_releases
                .lock()
                .map_err(|_| NodeError::message("deferred release lock poisoned"))?;
            std::mem::take(&mut *queued)
        };
        if releases.is_empty() {
            return Ok(());
        }
        let base = self.state.revision;
        let target = base.checked_add(1).ok_or(NodeError::RevisionOverflow)?;
        let mut transaction = Transaction::new(
            Arc::new(self.providers.clone()),
            Arc::clone(&self.deferred_children),
            (*self.state).clone(),
            target,
        );
        for task in releases {
            transaction.unpin(task);
        }
        transaction.run_pending(self.workers)?;
        let (state, provider_states) = transaction.finish();
        self.commit_transaction(state, provider_states);
        Ok(())
    }

    fn collect_dropped_subscribers(&mut self) -> Result<(), NodeError> {
        let removals = {
            let mut queued = self
                .deferred_subscriber_removals
                .lock()
                .map_err(|_| NodeError::message("deferred subscriber lock poisoned"))?;
            std::mem::take(&mut *queued)
        };
        for (id, target) in removals {
            match target {
                SubscriberTarget::View(fact) => {
                    if let Some(subscribers) = self.subscribers.get_mut(&fact) {
                        subscribers.retain(|subscriber| subscriber.id() != id);
                        if subscribers.is_empty() {
                            self.subscribers.remove(&fact);
                        }
                    }
                }
                SubscriberTarget::Relation(fact) => {
                    if let Some(subscribers) = self.relation_subscribers.get_mut(&fact) {
                        subscribers.retain(|subscriber| subscriber.id() != id);
                        if subscribers.is_empty() {
                            self.relation_subscribers.remove(&fact);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs a root-state command and atomically publishes all resulting node
    /// output changes.  Subscribers are notified only after a successful commit.
    pub fn command<C: Command>(&mut self, command: C) -> Result<C::Output, NodeError> {
        self.collect_garbage()?;
        let base = self.state.revision;
        let target = base.checked_add(1).ok_or(NodeError::RevisionOverflow)?;
        let mut transaction = Transaction::new(
            Arc::new(self.providers.clone()),
            Arc::clone(&self.deferred_children),
            (*self.state).clone(),
            target,
        );
        let output = {
            let mut cx = CommandCx {
                transaction: &mut transaction,
            };
            command.apply(&mut cx)?
        };
        transaction.run_pending(self.workers)?;
        let (state, provider_states) = transaction.finish();
        self.commit_transaction(state, provider_states);
        Ok(output)
    }

    /// Installs every part of a successful transaction before making it
    /// externally observable through subscriptions or effects.
    fn commit_transaction(
        &mut self,
        state: GraphState,
        provider_states: Vec<Box<dyn StagedProviderState>>,
    ) {
        commit_provider_states(provider_states);
        let effects = self.commit(state);
        self.pending_effects.extend(effects);
        self.drain_effects();
    }

    fn commit(&mut self, state: GraphState) -> VecDeque<EffectWork> {
        let previous = Arc::clone(&self.state);
        let state = Arc::new(state);
        let snapshot = state.revision;
        let mut changed = HashSet::new();
        changed.extend(
            previous
                .facts
                .keys()
                .filter(|key| {
                    state.facts.get(*key).is_none_or(|after| {
                        !after.value.equals(previous.facts[*key].value.as_ref())
                    })
                })
                .cloned(),
        );
        changed.extend(
            state
                .facts
                .keys()
                .filter(|key| {
                    previous
                        .facts
                        .get(*key)
                        .is_none_or(|before| !before.value.equals(state.facts[*key].value.as_ref()))
                })
                .cloned(),
        );

        let mut changed_relations = HashSet::new();
        changed_relations.extend(
            previous
                .relation_supports
                .keys()
                .filter(|relation| !state.relation_supports.contains_key(*relation))
                .cloned(),
        );
        changed_relations.extend(
            state
                .relation_supports
                .keys()
                .filter(|relation| !previous.relation_supports.contains_key(*relation))
                .cloned(),
        );

        self.current.store(Arc::clone(&state));
        self.state = state;

        for fact in changed {
            let Some(subscribers) = self.subscribers.get_mut(&fact) else {
                continue;
            };
            let value = self.state.facts.get(&fact).map(|fact| &fact.value);
            subscribers.retain(|subscriber| subscriber.send(snapshot, value));
        }
        let mut effects = VecDeque::new();
        for relation in changed_relations {
            let present = self.state.relation_supports.contains_key(&relation);
            if let Some(subscribers) = self.relation_subscribers.get_mut(&relation) {
                subscribers.retain(|subscriber| subscriber.send(snapshot, present));
            }
            let handlers = if present {
                self.relation_added_effects.get(&relation.relation)
            } else {
                self.relation_removed_effects.get(&relation.relation)
            };
            if let Some(handlers) = handlers {
                effects.extend(
                    handlers
                        .iter()
                        .filter_map(|handler| handler.work(snapshot, &relation)),
                );
            }
        }
        effects
    }

    fn drain_effects(&mut self) {
        if self.draining_effects {
            return;
        }
        self.draining_effects = true;
        while let Some(EffectWork {
            snapshot,
            relation,
            relation_name,
            fact,
            run,
        }) = self.pending_effects.pop_front()
        {
            if let Err(message) = run(self) {
                self.effect_failures.push(EffectFailure {
                    snapshot,
                    relation,
                    relation_name,
                    fact,
                    message,
                });
            }
        }
        self.draining_effects = false;
    }
}

impl ReadGraph for Graph {
    fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.state, key)
    }

    fn contains<R: Relation>(&self, fact: R::Fact) -> bool {
        self.state
            .relation_supports
            .contains_key(&RelationFactId::new::<R>(fact))
    }

    fn scan<R: IndexedRelation>(&self, index: R::Index) -> Vec<R::Fact> {
        self.scan_all::<R>()
            .into_iter()
            .filter(|fact| R::index(fact) == index)
            .collect()
    }

    fn scan_all<R: Relation>(&self) -> Vec<R::Fact> {
        self.state
            .relation_supports
            .keys()
            .filter_map(RelationFactId::get::<R>)
            .collect()
    }
}

pub(crate) fn read_from_state<V: View>(state: &GraphState, key: V::Key) -> Option<V::Value> {
    let fact = FactId::new::<V>(key);
    state
        .facts
        .get(&fact)
        .and_then(|fact| typed_value::<V>(&fact.value))
}
