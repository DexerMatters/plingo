//! Type-erased values and keys with deterministic equality and hashing.
//!
//! The engine compares candidate writes against committed writes with
//! [`Value::value_eq`] (T4: an equal candidate publishes nothing) and
//! looks up facts by type-erased [`KeyValue`] identities. No `Ord` bound is
//! imposed on user types; ordering that must be deterministic comes from
//! private stable ordinals and path strings, never from hash iteration
//! (T3).

use std::any::Any;
use std::fmt::Debug;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

/// A materialized fact value: any `Send + Sync + Debug + PartialEq` type.
///
/// The blanket impl compares values by downcasting to the concrete type;
/// a cross-type comparison is `false` (never a panic).
pub trait Value: Any + Send + Sync + Debug {
    /// Structural equality against another erased value.
    fn value_eq(&self, other: &dyn Value) -> bool;

    /// Downcast helper for typed store internals.
    fn as_any(&self) -> &dyn Any;
}

impl<T: Any + Send + Sync + Debug + PartialEq> Value for T {
    fn value_eq(&self, other: &dyn Value) -> bool {
        other
            .as_any()
            .downcast_ref::<T>()
            .is_some_and(|other| self == other)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A fact identity or map key: any `Send + Sync + Debug + Clone + Eq + Hash`
/// type.
///
/// Hashing uses a fixed-seed [`DefaultHasher`], so hash values are
/// deterministic across runs and processes. Hash buckets are only used for
/// lookup; iteration order always comes from stable ordinals.
///
/// This trait is object-safe and is the erased fact-key surface;
/// [`KeySpec`] re-exposes `Clone + Eq + Hash` for typed generics.
pub trait KeyValue: Any + Send + Sync + Debug {
    /// Structural equality against another erased key.
    fn eq_value(&self, other: &dyn KeyValue) -> bool;

    /// Deterministic hash of this key.
    fn hash_value(&self) -> u64;

    /// Clones this key behind its erased type.
    fn clone_key(&self) -> Arc<dyn KeyValue>;

    /// Downcast helper.
    fn as_any(&self) -> &dyn Any;
}

impl<K: Any + Send + Sync + Debug + Clone + Eq + Hash> KeyValue for K {
    fn eq_value(&self, other: &dyn KeyValue) -> bool {
        other
            .as_any()
            .downcast_ref::<K>()
            .is_some_and(|other| self == other)
    }

    fn hash_value(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    fn clone_key(&self) -> Arc<dyn KeyValue> {
        Arc::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
