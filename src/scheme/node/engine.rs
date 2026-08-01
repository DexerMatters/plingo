use super::{
    SnapshotId,
    api::{
        ComponentState, IndexedRelation, NodeError, NodeProvider, NodeSchema, ReadGraph, Relation,
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
};

pub(crate) trait ErasedProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> NodeSchema;
    fn run<'nodes>(
        &self,
        transaction: &mut Transaction<'nodes>,
        task: TaskId,
    ) -> Result<(), NodeError>;
    fn reclaim<'nodes>(
        &self,
        transaction: &mut Transaction<'nodes>,
        task: TaskId,
    ) -> Result<(), NodeError>;
}

pub(crate) struct ProviderEntry<P>(pub(crate) P);

impl<P: NodeProvider> ErasedProvider for ProviderEntry<P> {
    fn name(&self) -> &'static str {
        type_name::<P>()
    }
    fn schema(&self) -> NodeSchema {
        P::schema()
    }

    fn run<'nodes>(
        &self,
        transaction: &mut Transaction<'nodes>,
        task: TaskId,
    ) -> Result<(), NodeError> {
        let key = task
            .key
            .get::<P::Key>()
            .ok_or(NodeError::MissingProvider(type_name::<P>()))?;
        let (transaction, dependencies, outputs, relations, children) = {
            let mut cx = DeriveCx {
                transaction,
                schema: self.schema(),
                dependencies: RefCell::new(HashSet::new()),
                indexers: RefCell::new(HashMap::new()),
                outputs: HashMap::new(),
                relations: HashSet::new(),
                children: HashSet::new(),
            };
            self.0.derive(&mut cx, key)?;
            cx.finish()
        };
        transaction.replace_task_outputs(task, dependencies, outputs, relations, children)
    }

    fn reclaim<'nodes>(
        &self,
        transaction: &mut Transaction<'nodes>,
        task: TaskId,
    ) -> Result<(), NodeError> {
        let key = task
            .key
            .get::<P::Key>()
            .ok_or(NodeError::MissingProvider(type_name::<P>()))?;
        self.0.reclaim(&mut ReclaimCx { transaction }, key)
    }
}

pub(crate) type DeferredChildFactory = Box<dyn Fn(&TaskId) -> Option<TaskId> + Send + Sync>;

pub(crate) struct Transaction<'a> {
    providers: &'a HashMap<TypeId, Arc<dyn ErasedProvider>>,
    deferred_children: &'a HashMap<TypeId, Vec<DeferredChildFactory>>,
    pub(crate) state: GraphState,
    target: SnapshotId,
    pending: VecDeque<TaskId>,
    pending_set: HashSet<TaskId>,
    running: Vec<TaskId>,
    orphaned: VecDeque<TaskId>,
    orphaned_set: HashSet<TaskId>,
    component_states: HashMap<usize, Box<dyn StagedComponentState>>,
}

pub(crate) trait StagedComponentState {
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn commit(self: Box<Self>);
}

pub(crate) struct StagedState<T: Clone + Send + Sync + 'static> {
    target: ComponentState<T>,
    value: T,
}

impl<T: Clone + Send + Sync + 'static> StagedComponentState for StagedState<T> {
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

impl<'a> Transaction<'a> {
    pub(crate) fn new(
        providers: &'a HashMap<TypeId, Arc<dyn ErasedProvider>>,
        deferred_children: &'a HashMap<TypeId, Vec<DeferredChildFactory>>,
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
            running: Vec::new(),
            orphaned: VecDeque::new(),
            orphaned_set: HashSet::new(),
            component_states: HashMap::new(),
        }
    }

    pub(crate) fn schedule(&mut self, task: TaskId) {
        if self.pending_set.insert(task.clone()) {
            self.pending.push_back(task);
        }
    }

    fn registered_children(&self, parent: &TaskId) -> Vec<TaskId> {
        self.deferred_children
            .get(&parent.provider)
            .into_iter()
            .flatten()
            .filter_map(|factory| factory(parent))
            .collect()
    }

    pub(crate) fn run_pending(&mut self) -> Result<(), NodeError> {
        while !self.pending.is_empty() || !self.orphaned.is_empty() {
            if let Some(task) = self.pending.pop_front() {
                self.run_node(task)?;
            } else if let Some(task) = self.orphaned.pop_front() {
                self.orphaned_set.remove(&task);
                self.reclaim_task(task)?;
            }
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

    fn run_node(&mut self, task: TaskId) -> Result<(), NodeError> {
        let should_run =
            self.pending_set.remove(&task) || !self.state.task_outputs.contains_key(&task);
        if !should_run {
            return Ok(());
        }
        if self.running.contains(&task) {
            return Err(NodeError::DependencyCycle(
                self.providers
                    .get(&task.provider)
                    .map_or("<unknown node>", |node| node.name()),
            ));
        }
        let node = self
            .providers
            .get(&task.provider)
            .cloned()
            .ok_or(NodeError::MissingProvider("<unknown node>"))?;
        self.running.push(task.clone());
        let result = node.run(self, task);
        self.running.pop();
        result
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

    fn write_fact(&mut self, fact: FactId, value: Arc<dyn ErasedValue>) {
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
    }

    fn remove_fact(&mut self, fact: &FactId) {
        if self.state.facts.remove(fact).is_some() {
            self.schedule_dependents(DependencyId::View(fact.clone()));
        }
    }

    fn component_state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ComponentState<T>,
    ) -> Result<&mut T, NodeError> {
        let id = Arc::as_ptr(&state.value) as usize;
        if let std::collections::hash_map::Entry::Vacant(e) = self.component_states.entry(id) {
            e.insert(Box::new(StagedState {
                target: state.clone(),
                value: state.get()?,
            }));
        }
        self.component_states
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

    fn replace_task_outputs(
        &mut self,
        task: TaskId,
        dependencies: HashSet<DependencyId>,
        outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
        relations: HashSet<RelationFactId>,
        children: HashSet<TaskId>,
    ) -> Result<(), NodeError> {
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
            self.remove_fact(fact);
        }
        for (fact, value) in outputs {
            self.write_fact(fact.clone(), value);
            self.state.fact_owners.insert(fact, task.clone());
        }
        self.state.task_outputs.insert(task.clone(), output_ids);

        let old_relations = self
            .state
            .task_relation_outputs
            .remove(&task)
            .unwrap_or_default();
        self.remove_task_relations(&task, &old_relations, &relations);
        for relation in &relations {
            let supports = self
                .state
                .relation_supports
                .entry(relation.clone())
                .or_default();
            if supports.insert(task.clone()) && supports.len() == 1 {
                if let Some(bucket) = self.add_relation_to_bucket(relation) {
                    self.schedule_dependents(DependencyId::RelationBucket(bucket));
                }
                self.schedule_dependents(DependencyId::Relation(relation.clone()));
                self.schedule_dependents(DependencyId::RelationType(relation.relation));
            }
        }
        self.state
            .task_relation_outputs
            .insert(task.clone(), relations);

        self.replace_task_dependencies(task.clone(), dependencies);
        self.replace_task_children(task, children);
        Ok(())
    }

    fn remove_task_relations(
        &mut self,
        task: &TaskId,
        old_relations: &HashSet<RelationFactId>,
        retained_relations: &HashSet<RelationFactId>,
    ) {
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
                    self.schedule_dependents(DependencyId::RelationBucket(bucket));
                }
                self.schedule_dependents(DependencyId::Relation(relation.clone()));
                self.schedule_dependents(DependencyId::RelationType(relation.relation));
            }
        }
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
        if self.running.contains(&task) {
            self.queue_reclaim(task);
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

        let node = self
            .providers
            .get(&task.provider)
            .cloned()
            .ok_or(NodeError::MissingProvider("<unknown node>"))?;
        node.reclaim(self, task)
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

    pub(crate) fn finish(self) -> (GraphState, Vec<Box<dyn StagedComponentState>>) {
        (self.state, self.component_states.into_values().collect())
    }
}

pub(crate) fn commit_component_states(component_states: Vec<Box<dyn StagedComponentState>>) {
    for state in component_states {
        state.commit();
    }
}

type DerivePublication<'transaction, 'nodes> = (
    &'transaction mut Transaction<'nodes>,
    HashSet<DependencyId>,
    HashMap<FactId, Arc<dyn ErasedValue>>,
    HashSet<RelationFactId>,
    HashSet<TaskId>,
);

/// Context available to a provider while deriving one keyed publication set.
pub struct DeriveCx<'transaction, 'nodes> {
    transaction: &'transaction mut Transaction<'nodes>,
    schema: NodeSchema,
    dependencies: RefCell<HashSet<DependencyId>>,
    indexers: RefCell<HashMap<TypeId, RelationIndexer>>,
    outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
    relations: HashSet<RelationFactId>,
    children: HashSet<TaskId>,
}

impl<'transaction, 'nodes> DeriveCx<'transaction, 'nodes> {
    /// Reads a current value without adding an invalidation dependency. This is
    /// for coordinators deciding whether to schedule work for a newly emitted
    /// semantic publication.
    pub(crate) fn peek<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.transaction.state, key)
    }

    /// Returns this transaction's mutable staged copy of component state.
    ///
    /// The handle is cloned on first use in a transaction. Mutations are
    /// discarded if the command or any later derivation fails.
    pub fn state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ComponentState<T>,
    ) -> Result<&mut T, NodeError> {
        self.transaction.component_state_mut(state)
    }

    /// Materializes another provider and records a typed keeps-alive edge.
    pub fn materialize<P: NodeProvider>(&mut self, key: P::Key) -> Result<(), NodeError> {
        let task = TaskId::new::<P>(key);
        self.transaction.run_node(task.clone())?;
        self.children.insert(task);
        Ok(())
    }

    /// Schedules a provider child after this task's publication replacement.
    pub fn defer<P: NodeProvider>(&mut self, key: P::Key) {
        let task = TaskId::new::<P>(key);
        self.transaction.schedule(task.clone());
        self.children.insert(task);
    }

    /// Defers every provider child connected to the current provider.
    /// Children run only after the parent's facts commit, so they observe a
    /// complete parser publication rather than partially emitted candidates.
    pub(crate) fn defer_connected<P: NodeProvider>(&mut self, key: P::Key) {
        let parent = TaskId::new::<P>(key);
        for child in self.transaction.registered_children(&parent) {
            self.transaction.schedule(child.clone());
            self.children.insert(child);
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

    fn finish(self) -> DerivePublication<'transaction, 'nodes> {
        for (relation, indexer) in self.indexers.into_inner() {
            self.transaction.register_relation_index(relation, indexer);
        }
        (
            self.transaction,
            self.dependencies.into_inner(),
            self.outputs,
            self.relations,
            self.children,
        )
    }
}

impl ReadGraph for DeriveCx<'_, '_> {
    fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        let fact = FactId::new::<V>(key.clone());
        self.dependencies
            .borrow_mut()
            .insert(DependencyId::View(fact));
        read_from_state::<V>(&self.transaction.state, key)
    }

    fn contains<R: Relation>(&self, fact: R::Fact) -> bool {
        let relation = RelationFactId::new::<R>(fact);
        self.dependencies
            .borrow_mut()
            .insert(DependencyId::Relation(relation.clone()));
        self.transaction
            .state
            .relation_supports
            .contains_key(&relation)
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
        self.transaction
            .state
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
        self.transaction
            .state
            .relation_supports
            .keys()
            .filter_map(RelationFactId::get::<R>)
            .collect()
    }
}

/// Context available while reclaiming a task whose externally visible outputs
/// have already been removed. It deliberately exposes only private staged
/// state, never graph output mutation.
pub struct ReclaimCx<'transaction, 'nodes> {
    transaction: &'transaction mut Transaction<'nodes>,
}

impl<'transaction, 'nodes> ReclaimCx<'transaction, 'nodes> {
    /// Returns this transaction's staged copy of provider-private state.
    pub fn state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ComponentState<T>,
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
pub struct CommandCx<'transaction, 'nodes> {
    pub(crate) transaction: &'transaction mut Transaction<'nodes>,
}

impl<'transaction, 'nodes> CommandCx<'transaction, 'nodes> {
    pub fn get<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.transaction.state, key)
    }

    pub fn set<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        self.transaction.set_root::<V>(key, value)
    }
}
