use super::{
    SnapshotId,
    api::{
        IndexedRelation, NodeError, NodeProvider, NodeSchema, ProviderState, ReadGraph, Relation,
        View,
    },
    graph::read_from_state,
    identity::{
        DependencyId, ErasedValue, FactId, RelationBucketId, RelationFactId, RelationIndexer,
        TaskId, boxed_value, relation_bucket_for,
    },
    state::{GraphState, StoredFact},
};
use std::{
    any::{Any, TypeId, type_name},
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    thread,
};

pub(crate) trait ErasedProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> NodeSchema;
    /// Whether this provider stages private state through
    /// [`DeriveCx::state_mut`]. State-touching tasks always run on the serial
    /// lane so parallel workers never observe staged mutations.
    fn uses_state(&self) -> bool {
        false
    }
    fn run<'tx>(&self, cx: DeriveCx<'tx>, task: TaskId) -> Result<DeriveCx<'tx>, NodeError>;
    fn reclaim<'tx>(&self, cx: &mut ReclaimCx<'tx>, task: TaskId) -> Result<(), NodeError>;
}

pub(crate) struct ProviderEntry<P>(pub(crate) P);

impl<P: NodeProvider> ErasedProvider for ProviderEntry<P> {
    fn name(&self) -> &'static str {
        type_name::<P>()
    }
    fn schema(&self) -> NodeSchema {
        P::schema()
    }
    fn uses_state(&self) -> bool {
        P::uses_state()
    }
    fn run<'tx>(&self, cx: DeriveCx<'tx>, task: TaskId) -> Result<DeriveCx<'tx>, NodeError> {
        let mut cx = cx;
        let key = task
            .key
            .get::<P::Key>()
            .ok_or(NodeError::MissingProvider(type_name::<P>()))?;
        self.0.derive(&mut cx, key)?;
        Ok(cx)
    }
    fn reclaim<'tx>(&self, cx: &mut ReclaimCx<'tx>, task: TaskId) -> Result<(), NodeError> {
        let key = task
            .key
            .get::<P::Key>()
            .ok_or(NodeError::MissingProvider(type_name::<P>()))?;
        self.0.reclaim(cx, key)
    }
}

pub(crate) type DeferredChildFactory = Arc<dyn Fn(&TaskId) -> Option<TaskId> + Send + Sync>;

/// One wave worker's recorded derivation outcome before coordinator merge.
#[derive(Default)]
pub(crate) struct TaskPatch {
    pub(crate) dependencies: HashSet<DependencyId>,
    pub(crate) outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
    pub(crate) relations: HashSet<RelationFactId>,
    pub(crate) children: HashSet<TaskId>,
    pub(crate) scheduled: HashSet<TaskId>,
    pub(crate) relation_indexers: HashMap<TypeId, RelationIndexer>,
    pub(crate) awaiting: bool,
    pub(crate) awaited: HashSet<TaskId>,
}

pub(crate) trait StagedProviderState {
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn commit(self: Box<Self>);
}

pub(crate) struct StagedState<T: Clone + Send + Sync + 'static> {
    target: ProviderState<T>,
    value: T,
}

impl<T: Clone + Send + Sync + 'static> StagedProviderState for StagedState<T> {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.value
    }

    fn commit(self: Box<Self>) {
        *self
            .target
            .value
            .lock()
            .expect("component state lock cannot be poisoned by staged access") = self.value;
    }
}

/// A single transaction against the graph. Providers and deferred-child
/// factories are shared through `Arc` so derivations can run on worker threads
/// without borrowing the graph.
pub(crate) struct Transaction {
    pub(crate) providers: Arc<HashMap<TypeId, Arc<dyn ErasedProvider>>>,
    deferred_children: Arc<HashMap<TypeId, Vec<DeferredChildFactory>>>,
    pub(crate) state: GraphState,
    target: SnapshotId,
    pending: VecDeque<TaskId>,
    pending_set: HashSet<TaskId>,
    orphaned: VecDeque<TaskId>,
    orphaned_set: HashSet<TaskId>,
    provider_states: HashMap<usize, Box<dyn StagedProviderState>>,
    /// Tasks that suspended awaiting a child in this transaction, mapped to
    /// the child tasks they await. Used for deterministic cycle detection.
    suspended_awaits: HashMap<TaskId, HashSet<TaskId>>,
}

/// The finished derivation of one task, ready for coordinator application.
pub(crate) enum DeriveOutput<'tx> {
    Live {
        transaction: &'tx mut Transaction,
        dependencies: HashSet<DependencyId>,
        outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
        relations: HashSet<RelationFactId>,
        children: HashSet<TaskId>,
        awaiting: bool,
        awaited: HashSet<TaskId>,
    },
    Patch {
        dependencies: HashSet<DependencyId>,
        outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
        relations: HashSet<RelationFactId>,
        children: HashSet<TaskId>,
        scheduled: HashSet<TaskId>,
        relation_indexers: HashMap<TypeId, RelationIndexer>,
        awaiting: bool,
        awaited: HashSet<TaskId>,
    },
}

impl Transaction {
    pub(crate) fn new(
        providers: Arc<HashMap<TypeId, Arc<dyn ErasedProvider>>>,
        deferred_children: Arc<HashMap<TypeId, Vec<DeferredChildFactory>>>,
        mut state: GraphState,
        target: SnapshotId,
    ) -> Self {
        state.revision = target;
        Self {
            providers,
            deferred_children,
            state,
            target,
            pending: VecDeque::new(),
            pending_set: HashSet::new(),
            orphaned: VecDeque::new(),
            orphaned_set: HashSet::new(),
            provider_states: HashMap::new(),
            suspended_awaits: HashMap::new(),
        }
    }

    pub(crate) fn schedule(&mut self, task: TaskId) {
        if self.pending_set.insert(task.clone()) {
            self.pending.push_back(task);
        }
    }

    /// Drains ready work in immutable-read waves. State-touching tasks run on
    /// the serial lane; pure tasks run concurrently on bounded workers.
    pub(crate) fn run_pending(&mut self, workers: usize) -> Result<(), NodeError> {
        while !self.pending.is_empty() || !self.orphaned.is_empty() {
            if self.pending.is_empty() {
                let task = self.orphaned.pop_front().expect("orphan queue is nonempty");
                self.orphaned_set.remove(&task);
                self.reclaim_task(task)?;
                continue;
            }

            if workers > 1 && self.pending.len() > 1 {
                let mut wave = Vec::new();
                while wave.len() < workers {
                    let Some(front) = self.pending.front() else {
                        break;
                    };
                    let uses_state = self
                        .providers
                        .get(&front.provider)
                        .is_some_and(|provider| provider.uses_state());
                    if uses_state {
                        break;
                    }
                    let task = self
                        .pending
                        .pop_front()
                        .expect("front task was inspected above");
                    self.pending_set.remove(&task);
                    wave.push(task);
                }
                if wave.len() > 1 {
                    self.run_parallel_wave(wave)?;
                    continue;
                }
                if wave.len() == 1 {
                    let task = wave.pop().expect("one wave task");
                    self.run_node(task)?;
                    continue;
                }
            }

            let task = self.pending.pop_front().expect("pending queue is nonempty");
            self.run_node(task)?;
        }
        Ok(())
    }

    /// Runs one task serially against the live transaction state.
    fn run_node(&mut self, task: TaskId) -> Result<(), NodeError> {
        let should_run = self.pending_set.remove(&task)
            || !self
                .state
                .task_outputs
                .get(&task)
                .is_some_and(|outputs| !outputs.is_empty());
        if !should_run {
            return Ok(());
        }
        let node = self
            .providers
            .get(&task.provider)
            .cloned()
            .ok_or(NodeError::MissingProvider("<unknown node>"))?;
        let schema = node.schema();
        let cx = DeriveCx::live(self, schema);
        let cx = node.run(cx, task.clone())?;
        let output = cx.finish();
        let DeriveOutput::Live {
            transaction,
            dependencies,
            outputs,
            relations,
            children,
            awaiting,
            awaited,
        } = output
        else {
            unreachable!("serial derivations run in live mode");
        };
        let patch = TaskPatch {
            dependencies,
            outputs,
            relations,
            children,
            scheduled: HashSet::new(),
            relation_indexers: HashMap::new(),
            awaiting,
            awaited,
        };
        let mut applied_changed = HashSet::new();
        transaction.apply_patch(task, patch, &mut applied_changed)?;
        Ok(())
    }

    /// Runs a bounded batch of ready tasks on worker threads. Each worker
    /// reads one shared immutable snapshot and records only local patch data;
    /// the coordinator merges patches in stable ready-queue order and
    /// reschedules any patch invalidated by an earlier write in the same wave.
    fn run_parallel_wave(&mut self, wave: Vec<TaskId>) -> Result<(), NodeError> {
        let snapshot = Arc::new(self.state.clone());
        let awaits = Arc::new(self.suspended_awaits.clone());
        let providers = Arc::clone(&self.providers);
        let deferred_children = Arc::clone(&self.deferred_children);
        let outcomes = thread::scope(|scope| {
            let handles = wave
                .iter()
                .cloned()
                .map(|task| {
                    let snapshot = Arc::clone(&snapshot);
                    let awaits = Arc::clone(&awaits);
                    let providers = Arc::clone(&providers);
                    let deferred_children = Arc::clone(&deferred_children);
                    scope.spawn(move || {
                        evaluate_worker(&providers, &snapshot, &awaits, &deferred_children, task)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| NodeError::message("a graph worker panicked"))?
                })
                .collect::<Result<Vec<_>, NodeError>>()
        })?;

        let mut applied_changed: HashSet<DependencyId> = HashSet::new();
        for (task, patch) in wave.into_iter().zip(outcomes) {
            if patch
                .dependencies
                .iter()
                .any(|dependency| applied_changed.contains(dependency))
            {
                self.schedule(task);
                continue;
            }
            self.apply_patch(task, patch, &mut applied_changed)?;
        }
        Ok(())
    }

    /// Applies one derivation patch to the live state. Returns the set of
    /// dependency identities whose committed value actually changed, so later
    /// same-wave patches can be invalidated precisely.
    fn apply_patch(
        &mut self,
        task: TaskId,
        patch: TaskPatch,
        applied_changed: &mut HashSet<DependencyId>,
    ) -> Result<(), NodeError> {
        for (relation, indexer) in patch.relation_indexers {
            self.register_relation_index(relation, indexer);
        }
        let changed = self.replace_task_outputs(
            task.clone(),
            patch.dependencies,
            patch.outputs,
            patch.relations,
            patch.children,
        )?;
        applied_changed.extend(changed);
        if patch.awaiting {
            self.suspended_awaits.insert(task, patch.awaited);
        } else {
            self.suspended_awaits.remove(&task);
        }
        for child in patch.scheduled {
            self.schedule(child);
        }
        Ok(())
    }

    pub(crate) fn pin(&mut self, task: TaskId) {
        *self.state.task_pins.entry(task).or_default() += 1;
    }

    pub(crate) fn unpin(&mut self, task: TaskId) {
        let remove = self.state.task_pins.get_mut(&task).is_some_and(|pins| {
            *pins -= 1;
            *pins == 0
        });
        if remove {
            self.state.task_pins.remove(&task);
            self.queue_reclaim(task);
        }
    }

    fn queue_reclaim(&mut self, task: TaskId) {
        if self.is_live(&task) || !self.orphaned_set.insert(task.clone()) {
            return;
        }
        self.orphaned.push_back(task);
    }

    fn is_live(&self, task: &TaskId) -> bool {
        self.state.task_pins.contains_key(task)
            || self
                .state
                .child_parents
                .get(task)
                .is_some_and(|parents| !parents.is_empty())
    }

    fn set_root<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        let fact = FactId::new::<V>(key);
        if let Some(owner) = self.state.fact_owners.get(&fact) {
            return Err(NodeError::RootOutputConflict(
                self.providers
                    .get(&owner.provider)
                    .map_or("<unknown node>", |node| node.name()),
            ));
        }
        self.write_fact(fact.clone(), boxed_value(value));
        self.state.root_facts.insert(fact);

        Ok(())
    }

    fn write_fact(&mut self, fact: FactId, value: Arc<dyn ErasedValue>) -> bool {
        let changed = self
            .state
            .facts
            .get(&fact)
            .is_none_or(|previous| !previous.value.equals(value.as_ref()));
        if changed {
            self.state.facts.insert(
                fact.clone(),
                StoredFact {
                    value,
                    changed_at: self.target,
                },
            );
            self.schedule_dependents(DependencyId::View(fact));
        }
        changed
    }

    fn remove_fact(&mut self, fact: &FactId) -> bool {
        if self.state.facts.remove(fact).is_some() {
            self.schedule_dependents(DependencyId::View(fact.clone()));
            true
        } else {
            false
        }
    }

    fn component_state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ProviderState<T>,
    ) -> Result<&mut T, NodeError> {
        let id = Arc::as_ptr(&state.value) as usize;
        if let std::collections::hash_map::Entry::Vacant(e) = self.provider_states.entry(id) {
            e.insert(Box::new(StagedState {
                target: state.clone(),
                value: state.get()?,
            }));
        }
        self.provider_states
            .get_mut(&id)
            .and_then(|state| state.as_any_mut().downcast_mut())
            .ok_or_else(|| NodeError::message("component state type mismatch"))
    }

    fn register_relation_index(&mut self, relation: TypeId, indexer: RelationIndexer) {
        if self.state.relation_indexers.contains_key(&relation) {
            return;
        }
        self.state.relation_indexers.insert(relation, indexer);
        for fact in self
            .state
            .relation_supports
            .keys()
            .filter(|fact| fact.relation == relation)
        {
            let bucket = (indexer.bucket_for)(fact)
                .expect("indexed relation indexer must accept its own relation facts");
            self.state
                .relation_buckets
                .entry(bucket)
                .or_default()
                .insert(fact.clone());
        }
    }

    fn add_relation_to_bucket(&mut self, relation: &RelationFactId) -> Option<RelationBucketId> {
        let bucket = self
            .state
            .relation_indexers
            .get(&relation.relation)
            .and_then(|indexer| (indexer.bucket_for)(relation))?;
        self.state
            .relation_buckets
            .entry(bucket.clone())
            .or_default()
            .insert(relation.clone())
            .then_some(bucket)
    }

    fn remove_relation_from_bucket(
        &mut self,
        relation: &RelationFactId,
    ) -> Option<RelationBucketId> {
        let bucket = self
            .state
            .relation_indexers
            .get(&relation.relation)
            .and_then(|indexer| (indexer.bucket_for)(relation))?;
        let removed = self
            .state
            .relation_buckets
            .get_mut(&bucket)
            .is_some_and(|facts| facts.remove(relation));
        if !removed {
            return None;
        }
        if self
            .state
            .relation_buckets
            .get(&bucket)
            .is_some_and(HashSet::is_empty)
        {
            self.state.relation_buckets.remove(&bucket);
        }
        Some(bucket)
    }

    fn schedule_dependents(&mut self, dependency: DependencyId) {
        let dependents = self
            .state
            .reverse_dependencies
            .get(&dependency)
            .cloned()
            .unwrap_or_default();
        for task in dependents {
            self.schedule(task);
        }
    }

    /// Applies one task's complete replacement contribution and returns the
    /// set of dependency identities whose observable value changed.
    fn replace_task_outputs(
        &mut self,
        task: TaskId,
        dependencies: HashSet<DependencyId>,
        outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
        relations: HashSet<RelationFactId>,
        children: HashSet<TaskId>,
    ) -> Result<HashSet<DependencyId>, NodeError> {
        let mut changed = HashSet::new();
        let provider_name = self
            .providers
            .get(&task.provider)
            .map_or("<unknown provider>", |provider| provider.name());
        for fact in outputs.keys() {
            if self.state.root_facts.contains(fact) {
                return Err(NodeError::OutputRootConflict(provider_name));
            }
            if let Some(owner) = self.state.fact_owners.get(fact)
                && owner != &task
            {
                return Err(NodeError::OutputConflict {
                    provider: provider_name,
                    owner: self
                        .providers
                        .get(&owner.provider)
                        .map_or("<unknown node>", |node| node.name()),
                });
            }
        }

        let output_ids = outputs.keys().cloned().collect::<HashSet<_>>();
        let old_outputs = self.state.task_outputs.remove(&task).unwrap_or_default();
        for fact in old_outputs.difference(&output_ids) {
            self.state.fact_owners.remove(fact);
            if self.remove_fact(fact) {
                changed.insert(DependencyId::View(fact.clone()));
            }
        }
        for (fact, value) in outputs {
            if self.write_fact(fact.clone(), value) {
                changed.insert(DependencyId::View(fact.clone()));
            }
            self.state.fact_owners.insert(fact, task.clone());
        }
        self.state.task_outputs.insert(task.clone(), output_ids);

        let old_relations = self
            .state
            .task_relation_outputs
            .remove(&task)
            .unwrap_or_default();
        changed.extend(self.remove_task_relations(&task, &old_relations, &relations));
        for relation in &relations {
            let supports = self
                .state
                .relation_supports
                .entry(relation.clone())
                .or_default();
            if supports.insert(task.clone()) && supports.len() == 1 {
                if let Some(bucket) = self.add_relation_to_bucket(relation) {
                    self.schedule_dependents(DependencyId::RelationBucket(bucket.clone()));
                    changed.insert(DependencyId::RelationBucket(bucket));
                }
                self.schedule_dependents(DependencyId::Relation(relation.clone()));
                self.schedule_dependents(DependencyId::RelationType(relation.relation));
                changed.insert(DependencyId::Relation(relation.clone()));
                changed.insert(DependencyId::RelationType(relation.relation));
            }
        }
        self.state
            .task_relation_outputs
            .insert(task.clone(), relations);

        self.replace_task_dependencies(task.clone(), dependencies);
        self.replace_task_children(task, children);
        Ok(changed)
    }

    fn remove_task_relations(
        &mut self,
        task: &TaskId,
        old_relations: &HashSet<RelationFactId>,
        retained_relations: &HashSet<RelationFactId>,
    ) -> HashSet<DependencyId> {
        let mut changed = HashSet::new();
        for relation in old_relations.difference(retained_relations) {
            let remove = self
                .state
                .relation_supports
                .get_mut(relation)
                .is_some_and(|supports| {
                    supports.remove(task);
                    supports.is_empty()
                });
            if remove {
                self.state.relation_supports.remove(relation);
                if let Some(bucket) = self.remove_relation_from_bucket(relation) {
                    self.schedule_dependents(DependencyId::RelationBucket(bucket.clone()));
                    changed.insert(DependencyId::RelationBucket(bucket));
                }
                self.schedule_dependents(DependencyId::Relation(relation.clone()));
                self.schedule_dependents(DependencyId::RelationType(relation.relation));
                changed.insert(DependencyId::Relation(relation.clone()));
                changed.insert(DependencyId::RelationType(relation.relation));
            }
        }
        changed
    }

    fn replace_task_dependencies(&mut self, task: TaskId, dependencies: HashSet<DependencyId>) {
        let old_dependencies = self
            .state
            .task_dependencies
            .remove(&task)
            .unwrap_or_default();
        for dependency in old_dependencies.difference(&dependencies) {
            if let Some(reverse) = self.state.reverse_dependencies.get_mut(dependency) {
                reverse.remove(&task);
                if reverse.is_empty() {
                    self.state.reverse_dependencies.remove(dependency);
                }
            }
        }
        for dependency in &dependencies {
            self.state
                .reverse_dependencies
                .entry(dependency.clone())
                .or_default()
                .insert(task.clone());
        }
        if !dependencies.is_empty() {
            self.state.task_dependencies.insert(task, dependencies);
        }
    }

    fn replace_task_children(&mut self, task: TaskId, children: HashSet<TaskId>) {
        let old_children = self.state.task_children.remove(&task).unwrap_or_default();
        for child in &children {
            self.state
                .child_parents
                .entry(child.clone())
                .or_default()
                .insert(task.clone());
        }
        if !children.is_empty() {
            self.state
                .task_children
                .insert(task.clone(), children.clone());
        }
        for child in old_children.difference(&children) {
            let remove = self
                .state
                .child_parents
                .get_mut(child)
                .is_some_and(|parents| {
                    parents.remove(&task);
                    parents.is_empty()
                });
            if remove {
                self.state.child_parents.remove(child);
                self.queue_reclaim(child.clone());
            }
        }
    }

    fn reclaim_task(&mut self, task: TaskId) -> Result<(), NodeError> {
        if self.is_live(&task) {
            return Ok(());
        }
        self.pending_set.remove(&task);

        let dependencies = self
            .state
            .task_dependencies
            .remove(&task)
            .unwrap_or_default();
        self.remove_task_dependencies(&task, &dependencies);

        let outputs = self.state.task_outputs.remove(&task).unwrap_or_default();
        for fact in outputs {
            if self.state.fact_owners.get(&fact) == Some(&task) {
                self.state.fact_owners.remove(&fact);
                self.remove_fact(&fact);
            }
        }

        let relations = self
            .state
            .task_relation_outputs
            .remove(&task)
            .unwrap_or_default();
        self.remove_task_relations(&task, &relations, &HashSet::new());
        self.replace_task_children(task.clone(), HashSet::new());
        self.suspended_awaits.remove(&task);

        let node = self
            .providers
            .get(&task.provider)
            .cloned()
            .ok_or(NodeError::MissingProvider("<unknown node>"))?;
        let mut cx = ReclaimCx { transaction: self };
        node.reclaim(&mut cx, task)
    }

    fn remove_task_dependencies(&mut self, task: &TaskId, dependencies: &HashSet<DependencyId>) {
        for dependency in dependencies {
            if let Some(reverse) = self.state.reverse_dependencies.get_mut(dependency) {
                reverse.remove(task);
                if reverse.is_empty() {
                    self.state.reverse_dependencies.remove(dependency);
                }
            }
        }
    }

    pub(crate) fn finish(self) -> (GraphState, Vec<Box<dyn StagedProviderState>>) {
        (self.state, self.provider_states.into_values().collect())
    }
}

/// Evaluates one ready task against a shared immutable wave snapshot. The
/// derivation records a local [`TaskPatch`]; nothing touches the transaction.
fn evaluate_worker(
    providers: &HashMap<TypeId, Arc<dyn ErasedProvider>>,
    snapshot: &Arc<GraphState>,
    awaits: &Arc<HashMap<TaskId, HashSet<TaskId>>>,
    deferred_children: &Arc<HashMap<TypeId, Vec<DeferredChildFactory>>>,
    task: TaskId,
) -> Result<TaskPatch, NodeError> {
    let node = providers
        .get(&task.provider)
        .cloned()
        .ok_or(NodeError::MissingProvider("<unknown node>"))?;
    let schema = node.schema();
    let mut patch = TaskPatch::default();
    let cx = DeriveCx::patch(snapshot, awaits, deferred_children, schema, &mut patch);
    let cx = node.run(cx, task)?;
    match cx.finish() {
        DeriveOutput::Patch {
            dependencies,
            outputs,
            relations,
            children,
            scheduled,
            relation_indexers,
            awaiting,
            awaited,
        } => {
            patch.dependencies = dependencies;
            patch.outputs = outputs;
            patch.relations = relations;
            patch.children = children;
            patch.scheduled = scheduled;
            patch.relation_indexers = relation_indexers;
            patch.awaiting = awaiting;
            patch.awaited = awaited;
            Ok(patch)
        }
        DeriveOutput::Live { .. } => unreachable!("worker derivations run in patch mode"),
    }
}

pub(crate) fn commit_provider_states(provider_states: Vec<Box<dyn StagedProviderState>>) {
    for state in provider_states {
        state.commit();
    }
}

/// Where a derivation reads facts and records its staged contribution.
pub(crate) enum DeriveMode<'tx> {
    /// Serial execution: reads see the live transaction state and scheduling
    /// touches the transaction directly.
    Live { transaction: &'tx mut Transaction },
    /// Parallel execution: reads see one shared wave snapshot and scheduling
    /// is recorded in a worker-local patch.
    Patch {
        patch: &'tx mut TaskPatch,
        snapshot: Arc<GraphState>,
        awaits: Arc<HashMap<TaskId, HashSet<TaskId>>>,
    },
}

/// Context available to a provider while deriving one keyed publication set.
pub struct DeriveCx<'tx> {
    mode: DeriveMode<'tx>,
    deferred_children: Arc<HashMap<TypeId, Vec<DeferredChildFactory>>>,
    schema: NodeSchema,
    dependencies: RefCell<HashSet<DependencyId>>,
    indexers: RefCell<HashMap<TypeId, RelationIndexer>>,
    outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
    relations: HashSet<RelationFactId>,
    children: HashSet<TaskId>,
    pub(crate) awaiting: bool,
    pub(crate) awaited: HashSet<TaskId>,
}

impl<'tx> DeriveCx<'tx> {
    pub(crate) fn live(transaction: &'tx mut Transaction, schema: NodeSchema) -> Self {
        let deferred_children = Arc::clone(&transaction.deferred_children);
        Self {
            mode: DeriveMode::Live { transaction },
            deferred_children,
            schema,
            dependencies: RefCell::new(HashSet::new()),
            indexers: RefCell::new(HashMap::new()),
            outputs: HashMap::new(),
            relations: HashSet::new(),
            children: HashSet::new(),
            awaiting: false,
            awaited: HashSet::new(),
        }
    }

    pub(crate) fn patch(
        snapshot: &Arc<GraphState>,
        awaits: &Arc<HashMap<TaskId, HashSet<TaskId>>>,
        deferred_children: &Arc<HashMap<TypeId, Vec<DeferredChildFactory>>>,
        schema: NodeSchema,
        patch: &'tx mut TaskPatch,
    ) -> Self {
        Self {
            mode: DeriveMode::Patch {
                patch,
                snapshot: Arc::clone(snapshot),
                awaits: Arc::clone(awaits),
            },
            deferred_children: Arc::clone(deferred_children),
            schema,
            dependencies: RefCell::new(HashSet::new()),
            indexers: RefCell::new(HashMap::new()),
            outputs: HashMap::new(),
            relations: HashSet::new(),
            children: HashSet::new(),
            awaiting: false,
            awaited: HashSet::new(),
        }
    }

    /// Reads a current value without adding an invalidation dependency. This is
    /// for coordinators deciding whether to schedule work for a newly emitted
    /// semantic publication.
    pub(crate) fn peek<V: View>(&self, key: V::Key) -> Option<V::Value> {
        match &self.mode {
            DeriveMode::Live { transaction } => read_from_state::<V>(&transaction.state, key),
            DeriveMode::Patch { snapshot, .. } => read_from_state::<V>(snapshot, key),
        }
    }

    /// Returns this transaction's mutable staged copy of component state.
    ///
    /// The handle is cloned on first use in a transaction. Mutations are
    /// discarded if the command or any later derivation fails. State-touching
    /// providers run on the serial lane, so this is always live mode.
    pub fn state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ProviderState<T>,
    ) -> Result<&mut T, NodeError> {
        match &mut self.mode {
            DeriveMode::Live { transaction } => transaction.component_state_mut(state),
            DeriveMode::Patch { .. } => Err(NodeError::message(
                "provider state is unavailable inside a parallel wave",
            )),
        }
    }

    /// Materializes another provider inline. Kernel providers (parser, lexer,
    /// scope catalog) use this to run their dependencies within one serial
    /// derivation; component authors use schedule-and-read suspension instead.
    pub fn materialize<P: NodeProvider>(&mut self, key: P::Key) -> Result<(), NodeError> {
        match &mut self.mode {
            DeriveMode::Live { transaction } => {
                let task = TaskId::new::<P>(key);
                transaction.run_node(task.clone())?;
                self.children.insert(task);
                Ok(())
            }
            DeriveMode::Patch { .. } => Err(NodeError::message(
                "a provider cannot be materialized inline inside a parallel wave",
            )),
        }
    }

    /// Schedules a provider child after this task's publication replacement.
    pub fn defer<P: NodeProvider>(&mut self, key: P::Key) {
        let task = TaskId::new::<P>(key);
        self.retain_task(task);
    }

    /// Retains one task and schedules it only when it is not yet materialized.
    /// Materialized targets stay alive as children; invalidation reschedules
    /// them through recorded dependencies, so re-calling an available child
    /// never spins the scheduler.
    pub(crate) fn retain_task(&mut self, task: TaskId) {
        let materialized = match &self.mode {
            DeriveMode::Live { transaction } => transaction
                .state
                .task_outputs
                .get(&task)
                .is_some_and(|outputs| !outputs.is_empty()),
            DeriveMode::Patch { snapshot, .. } => snapshot
                .task_outputs
                .get(&task)
                .is_some_and(|outputs| !outputs.is_empty()),
        };
        if !materialized {
            match &mut self.mode {
                DeriveMode::Live { transaction } => transaction.schedule(task.clone()),
                DeriveMode::Patch { patch, .. } => {
                    patch.scheduled.insert(task.clone());
                }
            }
        }
        self.children.insert(task);
    }

    /// Schedules every provider child connected to the given parent task.
    /// Children run only after the parent's facts commit, so they observe a
    /// complete parser publication rather than partially emitted candidates.
    pub(crate) fn defer_connected(&mut self, task: &TaskId) {
        let children = self
            .deferred_children
            .get(&task.provider)
            .into_iter()
            .flatten()
            .filter_map(|factory| factory(task))
            .collect::<Vec<_>>();
        for child in children {
            self.retain_task(child);
        }
    }

    /// Emits one relation fact. The fact remains visible while any other
    /// provider also supports it, and is retracted after final support is gone.
    pub fn emit_relation<R: Relation>(&mut self, fact: R::Fact) -> Result<(), NodeError> {
        if !self.schema.declares_relation::<R>() {
            return Err(NodeError::UndeclaredPort {
                provider: self.schema.provider,
                port: type_name::<R>(),
                kind: "relation",
            });
        }
        if !self.relations.insert(RelationFactId::new::<R>(fact)) {
            return Err(NodeError::DuplicateOutput(type_name::<R>()));
        }
        Ok(())
    }

    /// Emits an additional owned port fact, retracted when this provider run
    /// no longer publishes it.
    pub fn emit<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        if !self.schema.declares_map::<V>() {
            return Err(NodeError::UndeclaredPort {
                provider: self.schema.provider,
                port: type_name::<V>(),
                kind: "map",
            });
        }
        let fact = FactId::new::<V>(key);
        if self.outputs.insert(fact, boxed_value(value)).is_some() {
            return Err(NodeError::DuplicateOutput(type_name::<V>()));
        }
        Ok(())
    }

    /// Records that this derivation suspended awaiting a child. Staged
    /// definitions and supports are discarded by [`Self::finish`].
    pub(crate) fn mark_awaiting(&mut self) {
        self.awaiting = true;
    }

    /// Detects a component call cycle: the caller awaits the target, and the
    /// target transitively awaits the caller through earlier suspensions in
    /// this transaction.
    pub(crate) fn check_cycle(&self, caller: &TaskId, target: &TaskId) -> Result<(), NodeError> {
        let awaits = match &self.mode {
            DeriveMode::Live { transaction } => &transaction.suspended_awaits,
            DeriveMode::Patch { awaits, .. } => awaits.as_ref(),
        };
        let mut pending = vec![target.clone()];
        let mut seen: HashSet<TaskId> = HashSet::new();
        while let Some(task) = pending.pop() {
            if &task == caller {
                return Err(NodeError::DependencyCycle(self.schema.provider));
            }
            if !seen.insert(task.clone()) {
                continue;
            }
            if let Some(children) = awaits.get(&task) {
                pending.extend(children.iter().cloned());
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> DeriveOutput<'tx> {
        let dependencies = self.dependencies.into_inner();
        let indexers = self.indexers.into_inner();
        let mut outputs = self.outputs;
        let mut relations = self.relations;
        if self.awaiting {
            outputs.clear();
            relations.clear();
        }
        let awaiting = self.awaiting;
        let awaited = self.awaited;
        let children = self.children;
        match self.mode {
            DeriveMode::Live { transaction } => {
                for (relation, indexer) in indexers {
                    transaction.register_relation_index(relation, indexer);
                }
                DeriveOutput::Live {
                    transaction,
                    dependencies,
                    outputs,
                    relations,
                    children,
                    awaiting,
                    awaited,
                }
            }
            DeriveMode::Patch { patch, .. } => DeriveOutput::Patch {
                dependencies,
                outputs,
                relations,
                children,
                scheduled: std::mem::take(&mut patch.scheduled),
                relation_indexers: indexers,
                awaiting,
                awaited,
            },
        }
    }
}

impl ReadGraph for DeriveCx<'_> {
    fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        let fact = FactId::new::<V>(key.clone());
        self.dependencies
            .borrow_mut()
            .insert(DependencyId::View(fact));
        match &self.mode {
            DeriveMode::Live { transaction } => read_from_state::<V>(&transaction.state, key),
            DeriveMode::Patch { snapshot, .. } => read_from_state::<V>(snapshot, key),
        }
    }

    fn contains<R: Relation>(&self, fact: R::Fact) -> bool {
        let relation = RelationFactId::new::<R>(fact);
        self.dependencies
            .borrow_mut()
            .insert(DependencyId::Relation(relation.clone()));
        let present = match &self.mode {
            DeriveMode::Live { transaction } => {
                transaction.state.relation_supports.contains_key(&relation)
            }
            DeriveMode::Patch { snapshot, .. } => {
                snapshot.relation_supports.contains_key(&relation)
            }
        };
        present
    }

    fn scan<R: IndexedRelation>(&self, index: R::Index) -> Vec<R::Fact> {
        self.indexers
            .borrow_mut()
            .entry(TypeId::of::<R>())
            .or_insert(RelationIndexer {
                bucket_for: relation_bucket_for::<R>,
            });
        let bucket = RelationBucketId::new::<R>(index.clone());
        self.dependencies
            .borrow_mut()
            .insert(DependencyId::RelationBucket(bucket));
        let facts = match &self.mode {
            DeriveMode::Live { transaction } => &transaction.state,
            DeriveMode::Patch { snapshot, .. } => snapshot.as_ref(),
        };
        facts
            .relation_supports
            .keys()
            .filter_map(RelationFactId::get::<R>)
            .filter(|fact| R::index(fact) == index)
            .collect()
    }

    fn scan_all<R: Relation>(&self) -> Vec<R::Fact> {
        self.dependencies
            .borrow_mut()
            .insert(DependencyId::RelationType(TypeId::of::<R>()));
        let facts = match &self.mode {
            DeriveMode::Live { transaction } => &transaction.state,
            DeriveMode::Patch { snapshot, .. } => snapshot.as_ref(),
        };
        facts
            .relation_supports
            .keys()
            .filter_map(RelationFactId::get::<R>)
            .collect()
    }
}

/// Context available while reclaiming a task whose externally visible outputs
/// have already been removed. It deliberately exposes only private staged
/// state, never graph output mutation.
pub struct ReclaimCx<'tx> {
    transaction: &'tx mut Transaction,
}

impl<'tx> ReclaimCx<'tx> {
    /// Returns this transaction's staged copy of provider-private state.
    pub fn state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ProviderState<T>,
    ) -> Result<&mut T, NodeError> {
        self.transaction.component_state_mut(state)
    }

    /// Whether another materialized instance of `N` remains alive after the
    /// current task was removed. This supports bounded private caches without
    /// leaking state after the final demand disappears.
    pub fn has_materialized<P: NodeProvider>(&self) -> bool {
        self.transaction
            .state
            .task_outputs
            .keys()
            .any(|task| task.provider == TypeId::of::<P>())
    }

    /// Whether a specific provider task still has a root pin or parent.
    pub fn is_live<P: NodeProvider>(&self, key: P::Key) -> bool {
        self.transaction.is_live(&TaskId::new::<P>(key))
    }
}

/// Context available while applying a root-state command.
pub struct CommandCx<'tx> {
    pub(crate) transaction: &'tx mut Transaction,
}

impl<'tx> CommandCx<'tx> {
    pub fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.transaction.state, key)
    }

    pub fn set<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        self.transaction.set_root::<V>(key, value)
    }
}
