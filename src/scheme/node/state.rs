use super::SnapshotId;
use super::identity::{
    DependencyId, ErasedValue, FactId, RelationBucketId, RelationFactId, RelationIndexer, TaskId,
};
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone)]
pub(crate) struct StoredFact {
    pub(crate) value: Arc<dyn ErasedValue>,
    pub(crate) changed_at: SnapshotId,
}

#[derive(Clone, Default)]
pub(crate) struct GraphState {
    pub(crate) revision: SnapshotId,
    pub(crate) facts: HashMap<FactId, StoredFact>,
    /// Facts whose authority is a command rather than a derivation.
    pub(crate) root_facts: HashSet<FactId>,
    pub(crate) fact_owners: HashMap<FactId, TaskId>,
    pub(crate) task_outputs: HashMap<TaskId, HashSet<FactId>>,
    pub(crate) relation_supports: HashMap<RelationFactId, HashSet<TaskId>>,
    pub(crate) relation_buckets: HashMap<RelationBucketId, HashSet<RelationFactId>>,
    pub(crate) relation_indexers: HashMap<TypeId, RelationIndexer>,
    pub(crate) task_relation_outputs: HashMap<TaskId, HashSet<RelationFactId>>,
    pub(crate) task_dependencies: HashMap<TaskId, HashSet<DependencyId>>,
    pub(crate) reverse_dependencies: HashMap<DependencyId, HashSet<TaskId>>,
    pub(crate) task_children: HashMap<TaskId, HashSet<TaskId>>,
    pub(crate) child_parents: HashMap<TaskId, HashSet<TaskId>>,
    pub(crate) task_pins: HashMap<TaskId, usize>,
}
