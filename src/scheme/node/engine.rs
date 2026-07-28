use super::{
    SnapshotId,
    api::{ComponentState, IndexedRelation, Node, NodeError, Relation, View},
    graph::read_from_state,
    identity::{
        DependencyId, ErasedValue, FactId, RelationBucketId, RelationFactId, RelationIndexer,
        TaskId, boxed_value, relation_bucket_for,
    },
    state::{GraphState, StoredFact},
};
use std::{
    any::{Any, TypeId, type_name},
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

pub(crate) trait ErasedNode: Send + Sync {
    fn name(&self) -> &'static str;
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

pub(crate) struct NodeEntry<N>(pub(crate) N);

impl<N: Node> ErasedNode for NodeEntry<N> {
    fn name(&self) -> &'static str {
        type_name::<N>()
    }

    fn run<'nodes>(
        &self,
        transaction: &mut Transaction<'nodes>,
        task: TaskId,
    ) -> Result<(), NodeError> {
        let key = task
            .key
            .get::<N::Key>()
            .ok_or(NodeError::MissingNode(type_name::<N>()))?;
        let (transaction, dependencies, outputs, relations, children) = {
            let mut cx = DeriveCx {
                transaction,
                dependencies: HashSet::new(),
                outputs: HashMap::new(),
                relations: HashSet::new(),
                children: HashSet::new(),
            };
            let output = self.0.derive(&mut cx, key.clone())?;
            cx.emit::<N::Output>(key, output)?;
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
            .get::<N::Key>()
            .ok_or(NodeError::MissingNode(type_name::<N>()))?;
        let mut cx = ReclaimCx { transaction };
        self.0.reclaim(&mut cx, key)
    }
}

pub(crate) struct Transaction<'a> {
    nodes: &'a HashMap<TypeId, Arc<dyn ErasedNode>>,
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
        nodes: &'a HashMap<TypeId, Arc<dyn ErasedNode>>,
        mut state: GraphState,
        target: SnapshotId,
    ) -> Self {
        state.revision = target;
        Self {
            nodes,
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

    pub(crate) fn run_pending(&mut self) -> Result<(), NodeError> {
        while !self.pending.is_empty() || !self.orphaned.is_empty() {
            if let Some(task) = self.pending.pop_front() {
                if self.pending_set.contains(&task) {
                    self.run_node(task)?;
                }
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
                self.nodes
                    .get(&task.node)
                    .map_or("<unknown node>", |node| node.name()),
            ));
        }
        let node = self
            .nodes
            .get(&task.node)
            .cloned()
            .ok_or(NodeError::MissingNode("<unknown node>"))?;
        self.running.push(task.clone());
        let result = node.run(self, task);
        self.running.pop();
        result
    }

    fn read<V: View>(&self, key: V::Key) -> Option<V::Value> {
        read_from_state::<V>(&self.state, key)
    }

    fn set_root<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        let fact = FactId::new::<V>(key);
        if let Some(owner) = self.state.fact_owners.get(&fact) {
            return Err(NodeError::RootOutputConflict(
                self.nodes
                    .get(&owner.node)
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

    fn ensure_relation_index<R: IndexedRelation>(&mut self) {
        let relation = TypeId::of::<R>();
        if self.state.relation_indexers.contains_key(&relation) {
            return;
        }
        let indexer = RelationIndexer {
            bucket_for: relation_bucket_for::<R>,
        };
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
        let node_name = self
            .nodes
            .get(&task.node)
            .map_or("<unknown node>", |node| node.name());
        for fact in outputs.keys() {
            if self.state.root_facts.contains(fact) {
                return Err(NodeError::OutputRootConflict(node_name));
            }
            if let Some(owner) = self.state.fact_owners.get(fact)
                && owner != &task
            {
                return Err(NodeError::OutputConflict {
                    node: node_name,
                    owner: self
                        .nodes
                        .get(&owner.node)
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
            .nodes
            .get(&task.node)
            .cloned()
            .ok_or(NodeError::MissingNode("<unknown node>"))?;
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

/// Context available to a node while deriving one keyed output set.
pub struct DeriveCx<'transaction, 'nodes> {
    transaction: &'transaction mut Transaction<'nodes>,
    dependencies: HashSet<DependencyId>,
    outputs: HashMap<FactId, Arc<dyn ErasedValue>>,
    relations: HashSet<RelationFactId>,
    children: HashSet<TaskId>,
}

impl<'transaction, 'nodes> DeriveCx<'transaction, 'nodes> {
    /// Reads a view and records a dynamic invalidation dependency.
    pub fn observe<V: View>(&mut self, key: V::Key) -> Result<V::Value, NodeError> {
        let fact = FactId::new::<V>(key.clone());
        // Absence is information too: a node that handles a missing view must
        // rerun once the view is later materialized.
        self.dependencies.insert(DependencyId::View(fact));
        self.transaction
            .read::<V>(key)
            .ok_or(NodeError::MissingView(type_name::<V>()))
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

    /// Ensures another node's primary output exists without making this task
    /// depend on that output. Pair this with a narrower secondary view when a
    /// coordinator only needs materialization, not every primary revision.
    pub fn materialize<N: Node>(&mut self, key: N::Key) -> Result<(), NodeError> {
        let task = TaskId::new::<N>(key);
        self.transaction.run_node(task.clone())?;
        self.children.insert(task);
        Ok(())
    }

    /// Ensures another node's primary output exists and observes it.
    pub fn require<N: Node>(
        &mut self,
        key: N::Key,
    ) -> Result<<N::Output as View>::Value, NodeError> {
        let task = TaskId::new::<N>(key.clone());
        self.transaction.run_node(task.clone())?;
        self.children.insert(task);
        self.observe::<N::Output>(key)
    }

    /// Observes whether a relation fact is present and records a dependency on
    /// its support set.
    pub fn observe_relation<R: Relation>(&mut self, fact: R::Fact) -> bool {
        let relation = RelationFactId::new::<R>(fact);
        let present = self
            .transaction
            .state
            .relation_supports
            .contains_key(&relation);
        self.dependencies.insert(DependencyId::Relation(relation));
        present
    }

    /// Reads facts in one indexed relation bucket and records a dependency on
    /// that bucket, including when it is currently empty.
    pub fn relation_facts_at<R: IndexedRelation>(&mut self, index: R::Index) -> Vec<R::Fact> {
        self.transaction.ensure_relation_index::<R>();
        let bucket = RelationBucketId::new::<R>(index);
        self.dependencies
            .insert(DependencyId::RelationBucket(bucket.clone()));
        self.transaction
            .state
            .relation_buckets
            .get(&bucket)
            .into_iter()
            .flatten()
            .filter_map(RelationFactId::get::<R>)
            .collect()
    }

    /// Emits one relation fact.  The fact remains visible while any other node
    /// also emits it, and is retracted after this node's final support is gone.
    pub fn emit_relation<R: Relation>(&mut self, fact: R::Fact) -> Result<(), NodeError> {
        if !self.relations.insert(RelationFactId::new::<R>(fact)) {
            return Err(NodeError::DuplicateOutput(type_name::<R>()));
        }
        Ok(())
    }

    /// Emits an additional owned output.  It is retracted automatically when
    /// the current node run no longer emits it.
    pub fn emit<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        let fact = FactId::new::<V>(key);
        if self.outputs.insert(fact, boxed_value(value)).is_some() {
            return Err(NodeError::DuplicateOutput(type_name::<V>()));
        }
        Ok(())
    }

    fn finish(
        self,
    ) -> (
        &'transaction mut Transaction<'nodes>,
        HashSet<DependencyId>,
        HashMap<FactId, Arc<dyn ErasedValue>>,
        HashSet<RelationFactId>,
        HashSet<TaskId>,
    ) {
        (
            self.transaction,
            self.dependencies,
            self.outputs,
            self.relations,
            self.children,
        )
    }
}

/// Context available while reclaiming a task whose externally visible outputs
/// have already been removed. It deliberately exposes only private staged
/// state, never graph output mutation.
pub struct ReclaimCx<'transaction, 'nodes> {
    transaction: &'transaction mut Transaction<'nodes>,
}

impl<'transaction, 'nodes> ReclaimCx<'transaction, 'nodes> {
    /// Returns this transaction's staged copy of node-private state.
    pub fn state_mut<T: Clone + Send + Sync + 'static>(
        &mut self,
        state: &ComponentState<T>,
    ) -> Result<&mut T, NodeError> {
        self.transaction.component_state_mut(state)
    }

    /// Whether another materialized instance of `N` remains alive after the
    /// current task was removed. This supports bounded private caches without
    /// leaking state after the final demand disappears.
    pub fn has_materialized<N: Node>(&self) -> bool {
        self.transaction
            .state
            .task_outputs
            .keys()
            .any(|task| task.node == TypeId::of::<N>())
    }

    /// Whether a specific task still has a root pin or an owning parent after
    /// the current task's child links were removed.
    pub fn is_live<N: Node>(&self, key: N::Key) -> bool {
        self.transaction.is_live(&TaskId::new::<N>(key))
    }
}

/// Context available while applying a root-state command.
pub struct CommandCx<'transaction, 'nodes> {
    pub(crate) transaction: &'transaction mut Transaction<'nodes>,
}

impl<'transaction, 'nodes> CommandCx<'transaction, 'nodes> {
    pub fn read<V: View>(&self, key: V::Key) -> Option<V::Value> {
        self.transaction.read::<V>(key)
    }

    pub fn set<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<(), NodeError> {
        self.transaction.set_root::<V>(key, value)
    }
}
