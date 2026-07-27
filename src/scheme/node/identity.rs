use super::api::{IndexedRelation, Node, NodeKey, NodeValue, Relation, View};
use std::{
    any::{Any, TypeId},
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

pub(crate) trait ErasedKey: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn equals(&self, other: &dyn ErasedKey) -> bool;
    fn hash_into(&self, state: &mut dyn Hasher);
}

/// Adapts an object-safe hasher to `Hash::hash`, whose generic hasher argument
/// is intentionally `Sized`.
pub(crate) struct HasherAdapter<'a>(&'a mut dyn Hasher);

impl Hasher for HasherAdapter<'_> {
    fn finish(&self) -> u64 {
        self.0.finish()
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }
}

pub(crate) struct KeyValue<K>(K);

impl<K: NodeKey> ErasedKey for KeyValue<K> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn equals(&self, other: &dyn ErasedKey) -> bool {
        other
            .as_any()
            .downcast_ref::<KeyValue<K>>()
            .is_some_and(|other| self.0 == other.0)
    }

    fn hash_into(&self, state: &mut dyn Hasher) {
        self.0.hash(&mut HasherAdapter(state));
    }
}

#[derive(Clone)]
pub(crate) struct KeyId {
    pub(crate) ty: TypeId,
    pub(crate) value: Arc<dyn ErasedKey>,
}

impl KeyId {
    pub(crate) fn new<K: NodeKey>(key: K) -> Self {
        Self {
            ty: TypeId::of::<K>(),
            value: Arc::new(KeyValue(key)),
        }
    }

    pub(crate) fn get<K: NodeKey>(&self) -> Option<K> {
        self.value
            .as_any()
            .downcast_ref::<KeyValue<K>>()
            .map(|value| value.0.clone())
    }
}

impl PartialEq for KeyId {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty && self.value.equals(other.value.as_ref())
    }
}

impl Eq for KeyId {}

impl Hash for KeyId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.ty.hash(state);
        self.value.hash_into(state);
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyId")
            .field("type", &self.ty)
            .finish_non_exhaustive()
    }
}

pub(crate) trait ErasedValue: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn equals(&self, other: &dyn ErasedValue) -> bool;
}

pub(crate) struct ValueBox<V>(V);

impl<V: NodeValue> ErasedValue for ValueBox<V> {
    fn as_any(&self) -> &dyn Any {
        &self.0
    }

    fn equals(&self, other: &dyn ErasedValue) -> bool {
        other
            .as_any()
            .downcast_ref::<V>()
            .is_some_and(|other| self.0 == *other)
    }
}

pub(crate) fn boxed_value<V: NodeValue>(value: V) -> Arc<dyn ErasedValue> {
    Arc::new(ValueBox(value))
}

pub(crate) fn typed_value<V: View>(value: &Arc<dyn ErasedValue>) -> Option<V::Value> {
    value.as_any().downcast_ref::<V::Value>().cloned()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FactId {
    pub(crate) view: TypeId,
    pub(crate) key: KeyId,
}

impl FactId {
    pub(crate) fn new<V: View>(key: V::Key) -> Self {
        Self {
            view: TypeId::of::<V>(),
            key: KeyId::new(key),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RelationFactId {
    pub(crate) relation: TypeId,
    pub(crate) fact: KeyId,
}

impl RelationFactId {
    pub(crate) fn new<R: Relation>(fact: R::Fact) -> Self {
        Self {
            relation: TypeId::of::<R>(),
            fact: KeyId::new(fact),
        }
    }

    pub(crate) fn get<R: Relation>(&self) -> Option<R::Fact> {
        (self.relation == TypeId::of::<R>())
            .then(|| self.fact.get::<R::Fact>())
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RelationBucketId {
    pub(crate) relation: TypeId,
    pub(crate) index: KeyId,
}

impl RelationBucketId {
    pub(crate) fn new<R: IndexedRelation>(index: R::Index) -> Self {
        Self {
            relation: TypeId::of::<R>(),
            index: KeyId::new(index),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RelationIndexer {
    pub(crate) bucket_for: fn(&RelationFactId) -> Option<RelationBucketId>,
}

pub(crate) fn relation_bucket_for<R: IndexedRelation>(
    fact: &RelationFactId,
) -> Option<RelationBucketId> {
    fact.get::<R>()
        .map(|fact| RelationBucketId::new::<R>(R::index(&fact)))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DependencyId {
    View(FactId),
    Relation(RelationFactId),
    /// One indexed relation bucket, including a bucket that was empty when
    /// observed.
    RelationBucket(RelationBucketId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TaskId {
    pub(crate) node: TypeId,
    pub(crate) key: KeyId,
}

impl TaskId {
    pub(crate) fn new<N: Node>(key: N::Key) -> Self {
        Self {
            node: TypeId::of::<N>(),
            key: KeyId::new(key),
        }
    }
}
