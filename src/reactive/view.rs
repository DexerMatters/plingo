//! Uniform typed reactive views and opaque node identities.
//!
//! A view is a typed collection `Input -> Option<Output>`. The runtime stores
//! owned outputs behind `Arc` and captures dependencies from effect calls.
//! Composite tree and scope façades use the same contract.

use std::marker::PhantomData;
use std::sync::Arc;

/// The uniform typed view contract used by plain reactive functions.
///
/// The procedural macro supplies the hidden codec methods. Authored code
pub trait View: Sized + Send + Sync + 'static {
    type Input: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static;
    type Output: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static;

    fn name() -> &'static str;
    #[doc(hidden)]
    fn __shared_writes() -> bool {
        false
    }

    #[doc(hidden)]
    fn __register(
        effect: &crate::reactive::__macro_private::EffectContext,
    ) -> crate::reactive::Result<()>;
    #[doc(hidden)]
    fn __observe(
        effect: &crate::reactive::__macro_private::EffectContext,
        input: Self::Input,
        temporal: crate::reactive::__macro_private::Temporal,
    ) -> crate::reactive::Result<Option<Arc<Self::Output>>>;
    #[doc(hidden)]
    fn __inputs(
        effect: &crate::reactive::__macro_private::EffectContext,
        temporal: crate::reactive::__macro_private::Temporal,
    ) -> crate::reactive::Result<Vec<Self::Input>>;
    #[doc(hidden)]
    fn __emit(
        effect: &crate::reactive::__macro_private::EffectContext,
        input: Self::Input,
        output: Option<Self::Output>,
    ) -> crate::reactive::Result<()>;
    #[doc(hidden)]
    fn __snapshot(
        snapshot: &crate::reactive::Snapshot,
        input: Self::Input,
    ) -> Option<Arc<Self::Output>>;
    #[doc(hidden)]
    fn __snapshot_inputs(snapshot: &crate::reactive::Snapshot) -> Vec<Self::Input>;
}

/// An opaque typed node identity used by generated tree and scope façades.
///
/// The raw ordinal is crate-private and its debug representation intentionally
/// does not reveal it.
pub struct Node<V: 'static> {
    raw: u64,
    marker: PhantomData<fn() -> V>,
}

impl<V: 'static> Copy for Node<V> {}

impl<V: 'static> Clone for Node<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> PartialEq for Node<V> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<V: 'static> Eq for Node<V> {}

impl<V: 'static> std::hash::Hash for Node<V> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<V: 'static> Node<V> {
    #[doc(hidden)]
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self {
            raw,
            marker: PhantomData,
        }
    }

    /// The stable raw identity, used as a link id in tree order/root facts
    /// and by generated façades. Public only for macro-generated code.
    #[doc(hidden)]
    pub fn raw_id(self) -> u64 {
        self.raw
    }
}

impl<V: 'static> std::fmt::Debug for Node<V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Node")
    }
}
