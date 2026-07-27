use super::engine::{CommandCx, DeriveCx, ReclaimCx};
use std::{
    hash::Hash,
    sync::{Arc, Mutex},
};
use thiserror::Error;

pub type SnapshotId = u64;

/// A stable, typed key used to address a view or a node instance.
pub trait NodeKey: Clone + Eq + Hash + Send + Sync + 'static {}

impl<T> NodeKey for T where T: Clone + Eq + Hash + Send + Sync + 'static {}

/// A value stored in a materialized view.
pub trait NodeValue: Clone + PartialEq + Send + Sync + 'static {}

impl<T> NodeValue for T where T: Clone + PartialEq + Send + Sync + 'static {}

/// A typed, keyed value exposed by the graph.
///
/// A view is a durable, snapshot-readable fact table with at most one value
/// per key.  Relations can be represented by making `Value` a collection whose
/// complete replacement is owned by one node instance.
pub trait View: Send + Sync + 'static {
    type Key: NodeKey;
    type Value: NodeValue;
}

/// A multi-owner set of immutable facts.
///
/// Unlike a [`View`], a relation fact can be emitted by several node instances.
/// It remains visible until its final supporting node retracts it.  This is the
/// primitive used by scope edges, datums, references, and requirements.
pub trait Relation: Send + Sync + 'static {
    type Fact: NodeKey;
}

/// A relation whose facts can be partitioned into independently observable
/// buckets.
pub trait IndexedRelation: Relation {
    type Index: NodeKey;

    fn index(fact: &Self::Fact) -> Self::Index;
}

/// A pure keyed derivation.
///
/// Nodes may observe any registered view and emit any number of views.  The
/// returned value is automatically emitted as this node's primary output.
/// Other emitted values are part of the same owned output set.
pub trait Node: Send + Sync + 'static {
    type Key: NodeKey;
    type Output: View<Key = Self::Key>;

    fn derive(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        key: Self::Key,
    ) -> Result<<Self::Output as View>::Value, NodeError>;

    /// Invoked after the runtime has retracted this task's outputs, relations,
    /// dependencies, and child ownership. Nodes use this only to discard
    /// private caches; published state is always runtime-owned.
    fn reclaim(&self, _cx: &mut ReclaimCx<'_, '_>, _key: Self::Key) -> Result<(), NodeError> {
        Ok(())
    }
}

/// A root-state mutation.
///
/// Commands are the only API that can update a root view.  Derived node output
/// is written exclusively through [`DeriveCx`].
pub trait Command: Send + 'static {
    type Output;

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<Self::Output, NodeError>;
}

/// Errors raised by the node graph.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("node `{0}` is not installed")]
    MissingNode(&'static str),
    #[error("view `{0}` has no value for the requested key")]
    MissingView(&'static str),
    #[error("node `{0}` has already been installed")]
    DuplicateNode(&'static str),
    #[error("node dependency cycle detected while deriving `{0}`")]
    DependencyCycle(&'static str),
    #[error("node `{node}` attempted to overwrite output owned by `{owner}`")]
    OutputConflict {
        node: &'static str,
        owner: &'static str,
    },
    #[error("node `{0}` attempted to overwrite an authoritative root view")]
    OutputRootConflict(&'static str),
    #[error("node `{0}` emitted the same view key more than once")]
    DuplicateOutput(&'static str),
    #[error("root command cannot overwrite output owned by `{0}`")]
    RootOutputConflict(&'static str),
    #[error("node graph revision overflow")]
    RevisionOverflow,
    #[error("{0}")]
    Message(String),
}

impl NodeError {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Mutable node-local data that is staged with the graph transaction.
///
/// Cloning the handle shares the same state. A derivation obtains a mutable
/// staged copy through [`DeriveCx::state_mut`]; that copy replaces the stored
/// value only after the graph transaction commits successfully.
pub struct ComponentState<T: Clone + Send + Sync + 'static> {
    pub(crate) value: Arc<Mutex<T>>,
}

impl<T: Clone + Send + Sync + 'static> ComponentState<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(Mutex::new(value)),
        }
    }

    /// Returns a snapshot of the last successfully committed value.
    pub fn get(&self) -> Result<T, NodeError> {
        self.value
            .lock()
            .map(|value| value.clone())
            .map_err(|_| NodeError::message("component state lock poisoned"))
    }
}

impl<T: Clone + Send + Sync + 'static> Clone for ComponentState<T> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
        }
    }
}
