//! Typed, keyed structural materializations.
//!
//! A structure is a graph-shaped family of immutable facts. Roots, node values,
//! and topology are published independently so consumers observe only the
//! structural facts their rules read.

use std::{
    any::{Any, TypeId},
    fmt,
    marker::PhantomData,
    sync::Arc,
};

use crate::scheme::node::{NodeKey, NodeValue};

/// Type-erased immutable payload used by a [`StructuralArtifact`].
///
/// Erasure is only the graph boundary. Typed rule cases recover the original
/// `Arc<T>` without cloning the payload.
pub trait ErasedStructuralValue: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn clone_any(&self) -> Arc<dyn Any + Send + Sync>;
    fn equals(&self, other: &dyn ErasedStructuralValue) -> bool;
}

struct StructuralValue<T: NodeValue>(Arc<T>);

impl<T: NodeValue> ErasedStructuralValue for StructuralValue<T> {
    fn as_any(&self) -> &dyn Any {
        self.0.as_ref()
    }

    fn clone_any(&self) -> Arc<dyn Any + Send + Sync> {
        Arc::clone(&self.0) as Arc<dyn Any + Send + Sync>
    }

    fn equals(&self, other: &dyn ErasedStructuralValue) -> bool {
        other
            .as_any()
            .downcast_ref::<T>()
            .is_some_and(|value| value == self.0.as_ref())
    }
}

struct OpaqueValue(Arc<dyn Any + Send + Sync>);

impl ErasedStructuralValue for OpaqueValue {
    fn as_any(&self) -> &dyn Any {
        self.0.as_ref()
    }

    fn clone_any(&self) -> Arc<dyn Any + Send + Sync> {
        Arc::clone(&self.0)
    }

    fn equals(&self, _: &dyn ErasedStructuralValue) -> bool {
        // Parser products provide the stable change identity. Their erased
        // payload does not promise `PartialEq`.
        true
    }
}

/// One typed, independently observable structural node.
///
/// `M` is structure-owned metadata that participates in value equality. Parsed
/// syntax uses its `ProductId`; ordinary derived structures normally use `()`.
pub struct StructuralArtifact<K: NodeKey, M: NodeValue = ()> {
    pub key: K,
    pub metadata: M,
    type_id: TypeId,
    value: Arc<dyn ErasedStructuralValue>,
}

impl<K: NodeKey> StructuralArtifact<K> {
    pub fn new<T: NodeValue>(key: K, value: T) -> Self {
        Self::with_metadata(key, (), value)
    }
}

impl<K: NodeKey, M: NodeValue> StructuralArtifact<K, M> {
    pub fn with_metadata<T: NodeValue>(key: K, metadata: M, value: T) -> Self {
        Self {
            key,
            metadata,
            type_id: TypeId::of::<T>(),
            value: Arc::new(StructuralValue(Arc::new(value))),
        }
    }

    /// Returns the original immutable payload when this artifact has type `T`.
    pub fn deref<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        (self.type_id == TypeId::of::<T>())
            .then(|| self.value.clone_any().downcast::<T>().ok())
            .flatten()
    }

    /// Constructs an artifact from a parser or foreign immutable erased value.
    ///
    /// `metadata` is its stable change identity when the payload itself cannot
    /// provide structural equality.
    pub fn from_erased(
        key: K,
        metadata: M,
        type_id: TypeId,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Self {
        Self {
            key,
            metadata,
            type_id,
            value: Arc::new(OpaqueValue(value)),
        }
    }

    pub fn is<T: NodeValue>(&self) -> bool {
        self.type_id == TypeId::of::<T>()
    }
}

impl<K: NodeKey, M: NodeValue> Clone for StructuralArtifact<K, M> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            metadata: self.metadata.clone(),
            type_id: self.type_id,
            value: Arc::clone(&self.value),
        }
    }
}

impl<K: NodeKey, M: NodeValue> PartialEq for StructuralArtifact<K, M> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.metadata == other.metadata
            && self.type_id == other.type_id
            && self.value.equals(other.value.as_ref())
    }
}

impl<K: NodeKey, M: NodeValue> Eq for StructuralArtifact<K, M> {}

impl<K: NodeKey + fmt::Debug, M: NodeValue + fmt::Debug> fmt::Debug for StructuralArtifact<K, M> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StructuralArtifact")
            .field("key", &self.key)
            .field("metadata", &self.metadata)
            .field("type_id", &self.type_id)
            .finish_non_exhaustive()
    }
}

/// One optional discovery entry for a structure.
///
/// Entries are support-counted facts rather than an exclusive root manifest.
/// The entry handle is domain-owned (for example a document or module key),
/// while the referenced node and metadata belong to the structure.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StructureEntry<K: NodeKey, E: NodeKey, M: NodeKey = ()> {
    pub entry: E,
    pub node: K,
    pub metadata: M,
}

impl<K: NodeKey, E: NodeKey, M: NodeKey> StructureEntry<K, E, M> {
    pub fn new(entry: E, node: K, metadata: M) -> Self {
        Self {
            entry,
            node,
            metadata,
        }
    }
}

/// The explicit topology representation of a structure.
pub trait Topology: Send + Sync + 'static {}

/// Ordered containment edges, suitable for ASTs and tree-shaped IRs.
pub struct OrderedChildren;
impl Topology for OrderedChildren {}

/// Arbitrary labelled, cyclic, or multi-owner graph edges.
pub struct GraphEdges;
impl Topology for GraphEdges {}

/// A graph edge owned by a structure's topology relation.
pub trait StructuralEdge<S: Structure + ?Sized>: NodeKey {
    fn source(&self) -> S::NodeKey;
    fn target(&self) -> S::NodeKey;
}

/// Static description of a keyed structural materialization.
///
/// It names node identity, artifact metadata, and topology. Discovery entries
/// are optional relation facts and are not part of the structure contract.
pub trait Structure: Send + Sync + 'static + Sized {
    type NodeKey: NodeKey + fmt::Debug;
    type NodeMetadata: NodeValue + fmt::Debug + Default;
    type Edge: StructuralEdge<Self>;
    type Topology: Topology;
}

/// An ordered child link. Structures using [`OrderedChildren`] publish these in
/// `StructureChildren` rather than inventing graph edge labels.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChildRef<K: NodeKey> {
    pub slot: usize,
    pub target: K,
}

/// Marker used when a structure has no semantic graph edges.
pub struct NoEdge<S: Structure>(PhantomData<fn() -> S>);

impl<S: Structure> NoEdge<S> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<S: Structure> Clone for NoEdge<S> {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl<S: Structure> Default for NoEdge<S> {
    fn default() -> Self {
        Self::new()
    }
}
impl<S: Structure> PartialEq for NoEdge<S> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl<S: Structure> Eq for NoEdge<S> {}
impl<S: Structure> std::hash::Hash for NoEdge<S> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<S>().hash(state);
    }
}
impl<S: Structure> fmt::Debug for NoEdge<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("NoEdge").finish()
    }
}

impl<S: Structure> StructuralEdge<S> for NoEdge<S> {
    fn source(&self) -> S::NodeKey {
        unreachable!("a structure without graph edges cannot expose an edge")
    }

    fn target(&self) -> S::NodeKey {
        unreachable!("a structure without graph edges cannot expose an edge")
    }
}
