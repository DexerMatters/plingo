use super::{
    api::{Command, Node, NodeError, Relation, SnapshotId, View},
    engine::{
        CommandCx, ErasedNode, NodeEntry, StagedComponentState, Transaction,
        commit_component_states,
    },
    identity::{ErasedValue, FactId, RelationFactId, TaskId, typed_value},
    state::GraphState,
};
use std::{
    any::{TypeId, type_name},
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex, mpsc},
};

pub struct Snapshot {
    id: SnapshotId,
    state: Arc<GraphState>,
}

impl Snapshot {
    pub fn id(&self) -> SnapshotId {
        self.id
    }
}

/// A committed change observed through a subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewUpdate<V> {
    Initial { snapshot: SnapshotId, value: V },
    Changed { snapshot: SnapshotId, value: V },
    Removed { snapshot: SnapshotId },
}

/// A durable subscription to one keyed view.
pub struct Subscription<V: View> {
    receiver: mpsc::Receiver<ViewUpdate<V::Value>>,
    /// Derived-view subscriptions retain a task pin until dropped. Root-view
    /// subscriptions have no lease.
    lease: Option<PinLease>,
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

/// Durable observation of one multi-owner relation fact.
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

/// A demand lease released automatically when its owner is dropped.
struct PinLease {
    task: TaskId,
    releases: Arc<Mutex<Vec<TaskId>>>,
}

impl Drop for PinLease {
    fn drop(&mut self) {
        if let Ok(mut releases) = self.releases.lock() {
            releases.push(self.task.clone());
        }
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

/// Result of a demand-driven request. Holding it keeps the requested node and
/// its required descendants materialized; dropping it queues their release.
pub struct RequestHandle<N: Node> {
    value: <N::Output as View>::Value,
    _lease: PinLease,
    _node: PhantomData<fn() -> N>,
}

impl<N: Node> RequestHandle<N> {
    pub fn value(&self) -> &<N::Output as View>::Value {
        &self.value
    }

    pub fn into_value(self) -> <N::Output as View>::Value {
        self.value.clone()
    }
}

impl<N: Node> std::ops::Deref for RequestHandle<N> {
    type Target = <N::Output as View>::Value;

    fn deref(&self) -> &Self::Target {
        &self.value
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

/// A non-stratified runtime of nodes, views, requests, and subscriptions.
pub struct Graph {
    state: Arc<GraphState>,
    history: BTreeMap<SnapshotId, Arc<GraphState>>,
    nodes: HashMap<TypeId, Arc<dyn ErasedNode>>,
    subscribers: HashMap<FactId, Vec<Box<dyn ErasedSubscriber>>>,
    relation_subscribers: HashMap<RelationFactId, Vec<Box<dyn ErasedRelationSubscriber>>>,
    relation_added_effects: HashMap<TypeId, Vec<Box<dyn ErasedRelationEffect>>>,
    relation_removed_effects: HashMap<TypeId, Vec<Box<dyn ErasedRelationEffect>>>,
    effect_failures: Vec<EffectFailure>,
    deferred_releases: Arc<Mutex<Vec<TaskId>>>,
    deferred_subscriber_removals: Arc<Mutex<Vec<(SubscriberId, SubscriberTarget)>>>,
    next_subscriber: SubscriberId,
    retention: usize,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        let state = Arc::new(GraphState::default());
        let mut history = BTreeMap::new();
        history.insert(0, Arc::clone(&state));
        Self {
            state,
            history,
            nodes: HashMap::new(),
            subscribers: HashMap::new(),
            relation_subscribers: HashMap::new(),
            relation_added_effects: HashMap::new(),
            relation_removed_effects: HashMap::new(),
            effect_failures: Vec::new(),
            deferred_releases: Arc::new(Mutex::new(Vec::new())),
            deferred_subscriber_removals: Arc::new(Mutex::new(Vec::new())),
            next_subscriber: 0,
            retention: 64,
        }
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

    pub fn set_snapshot_retention(&mut self, retention: usize) {
        self.retention = retention.max(1);
        self.prune_history();
    }

    /// Installs a node implementation.  Node types are capabilities, so there
    /// is at most one provider per concrete node type in a graph.
    pub fn install<N: Node>(&mut self, node: N) -> Result<(), NodeError> {
        if self.nodes.contains_key(&TypeId::of::<N>()) {
            return Err(NodeError::DuplicateNode(type_name::<N>()));
        }
        self.nodes
            .insert(TypeId::of::<N>(), Arc::new(NodeEntry(node)));
        Ok(())
    }

    /// Reads a materialized value from the latest committed snapshot.
    pub fn read<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.state, key)
    }

    /// Reads a value from an explicitly pinned historical snapshot.
    pub fn read_at<V: View>(&self, snapshot: &Snapshot, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&snapshot.state, key)
    }

    /// Returns whether a relation fact has at least one live supporting node.
    pub fn contains<R: Relation>(&self, fact: R::Fact) -> bool {
        self.state
            .relation_supports
            .contains_key(&RelationFactId::new::<R>(fact))
    }

    /// Returns the materialized facts of one relation.  Fact order is not part
    /// of the relation contract.
    pub fn facts<R: Relation>(&self) -> Vec<R::Fact> {
        self.state
            .relation_supports
            .keys()
            .filter_map(RelationFactId::get::<R>)
            .collect()
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

    /// Requests a node's primary output and returns an RAII demand handle.
    ///
    /// The output stays materialized only while the returned handle is alive.
    /// Dropping it queues reclamation; call [`Self::collect_garbage`] to apply
    /// queued releases immediately when no subsequent graph operation occurs.
    pub fn request<N: Node>(&mut self, key: N::Key) -> Result<RequestHandle<N>, NodeError> {
        self.collect_garbage()?;
        let lease = self.activate::<N>(key.clone())?;
        let value = self
            .read::<N::Output>(key)
            .ok_or(NodeError::MissingView(type_name::<N::Output>()))?;
        Ok(RequestHandle {
            value,
            _lease: lease,
            _node: PhantomData,
        })
    }

    /// Subscribes to a materialized view.  Root views can be subscribed to
    /// directly; derived views are normally activated through [`Self::subscribe`].
    pub fn subscribe_view<V: View>(&mut self, key: V::Key) -> Result<Subscription<V>, NodeError> {
        let value = self
            .read::<V>(key.clone())
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
            lease: None,
            _cleanup: SubscriberLease {
                id,
                target: SubscriberTarget::View(fact),
                releases: Arc::clone(&self.deferred_subscriber_removals),
            },
            _view: PhantomData,
        })
    }

    /// Subscribes to and pins a node's primary output.
    ///
    /// The returned subscription owns an RAII lease. Dropping it queues task
    /// reclamation; the graph drains that queue on its next mutation or when
    /// [`Self::collect_garbage`] is called explicitly.
    pub fn subscribe<N: Node>(
        &mut self,
        key: N::Key,
    ) -> Result<Subscription<N::Output>, NodeError> {
        self.collect_garbage()?;
        let lease = self.activate::<N>(key.clone())?;
        let mut subscription = self.subscribe_view::<N::Output>(key)?;
        subscription.lease = Some(lease);
        Ok(subscription)
    }

    /// Explicitly releases one outstanding root pin.
    ///
    /// Normal callers should drop a [`RequestHandle`] or [`Subscription`].
    /// This escape hatch remains useful for host-managed demand accounting.
    pub fn release<N: Node>(&mut self, key: N::Key) -> Result<(), NodeError> {
        self.collect_garbage()?;
        let task = TaskId::new::<N>(key);
        if !self.state.task_pins.contains_key(&task) {
            return Ok(());
        }
        let base = self.state.revision;
        let target = base.checked_add(1).ok_or(NodeError::RevisionOverflow)?;
        let mut transaction = Transaction::new(&self.nodes, (*self.state).clone(), target);
        transaction.unpin(task);
        transaction.run_pending()?;
        let (state, component_states) = transaction.finish();
        self.commit_transaction(state, component_states);
        Ok(())
    }

    /// Alias for [`Self::release`].
    pub fn unpin<N: Node>(&mut self, key: N::Key) -> Result<(), NodeError> {
        self.release::<N>(key)
    }

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
        let mut transaction = Transaction::new(&self.nodes, (*self.state).clone(), target);
        for task in releases {
            transaction.unpin(task);
        }
        transaction.run_pending()?;
        let (state, component_states) = transaction.finish();
        self.commit_transaction(state, component_states);
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
        let mut transaction = Transaction::new(&self.nodes, (*self.state).clone(), target);
        let output = {
            let mut cx = CommandCx {
                transaction: &mut transaction,
            };
            command.apply(&mut cx)?
        };
        transaction.run_pending()?;
        let (state, component_states) = transaction.finish();
        self.commit_transaction(state, component_states);
        Ok(output)
    }

    fn activate<N: Node>(&mut self, key: N::Key) -> Result<PinLease, NodeError> {
        if !self.nodes.contains_key(&TypeId::of::<N>()) {
            return Err(NodeError::MissingNode(type_name::<N>()));
        }
        let base = self.state.revision;
        let target = base.checked_add(1).ok_or(NodeError::RevisionOverflow)?;
        let mut transaction = Transaction::new(&self.nodes, (*self.state).clone(), target);
        let task = TaskId::new::<N>(key);
        transaction.pin(task.clone());
        if !transaction.state.task_outputs.contains_key(&task) {
            transaction.schedule(task.clone());
        }
        transaction.run_pending()?;
        let (state, component_states) = transaction.finish();
        self.commit_transaction(state, component_states);
        Ok(PinLease {
            task,
            releases: Arc::clone(&self.deferred_releases),
        })
    }

    /// Installs every part of a successful transaction before making it
    /// externally observable through subscriptions or effects.
    fn commit_transaction(
        &mut self,
        state: GraphState,
        component_states: Vec<Box<dyn StagedComponentState>>,
    ) {
        commit_component_states(component_states);
        let effects = self.commit(state);
        self.run_effects(effects);
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

        self.state = Arc::clone(&state);
        self.history.insert(snapshot, state);
        self.prune_history();

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

    fn run_effects(&mut self, mut effects: VecDeque<EffectWork>) {
        while let Some(EffectWork {
            snapshot,
            relation,
            relation_name,
            fact,
            run,
        }) = effects.pop_front()
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
    }

    fn prune_history(&mut self) {
        while self.history.len() > self.retention {
            let Some(oldest) = self.history.first_key_value().map(|(key, _)| *key) else {
                break;
            };
            self.history.remove(&oldest);
        }
    }
}

pub(crate) fn read_from_state<V: View>(state: &GraphState, key: V::Key) -> Option<V::Value> {
    let fact = FactId::new::<V>(key);
    state
        .facts
        .get(&fact)
        .and_then(|fact| typed_value::<V>(&fact.value))
}
