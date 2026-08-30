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
/// The identity is either a generated syntax key or an automatic component
/// output key. The cached raw hash is only an index hint; equality always
/// checks the complete logical key.
pub struct Node<V: 'static> {
    raw: u64,
    identity: Option<Arc<dyn crate::reactive::value::KeyValue>>,
    marker: PhantomData<fn() -> V>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SyntaxNodeIdentity {
    pub(crate) view: std::any::TypeId,
    pub(crate) uri: Arc<str>,
    pub(crate) lineage: u64,
    pub(crate) member: &'static str,
    pub(crate) root: bool,
}

impl<V: 'static> Clone for Node<V> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw,
            identity: self.identity.as_ref().map(Arc::clone),
            marker: PhantomData,
        }
    }
}

impl<V: 'static> PartialEq for Node<V> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.identity, &other.identity) {
            (Some(left), Some(right)) => left.eq_value(right.as_ref()),
            _ => self.raw == other.raw,
        }
    }
}

impl<V: 'static> Eq for Node<V> {}

impl<V: 'static> std::hash::Hash for Node<V> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<V: 'static> Node<V> {
    /// Constructs a generated syntax identity from its complete logical key.
    ///
    /// Only the scope facade still uses this legacy constructor; parser
    /// publication mints identities through `__published_syntax_box`, which
    /// carries the collision-safe member descriptor (Cut B item 6).
    #[doc(hidden)]
    pub(crate) fn from_syntax(raw: u64, uri: &str, lineage: u64, root: bool) -> Self {
        Self {
            raw,
            identity: Some(Arc::new(SyntaxNodeIdentity {
                view: std::any::TypeId::of::<V>(),
                uri: Arc::from(uri),
                lineage,
                member: "",
                root,
            })),
            marker: PhantomData,
        }
    }

    /// Returns the complete generated syntax identity when this node carries
    /// one. Publication snapshots use this instead of retaining only the
    /// cached raw hash, because a later command may need to rehydrate the
    /// exact node after its originating arena has been replaced.
    pub(crate) fn syntax_identity(&self) -> Option<SyntaxNodeIdentity> {
        self.identity
            .as_ref()?
            .as_any()
            .downcast_ref::<SyntaxNodeIdentity>()
            .cloned()
    }

    /// Rehydrates an opaque identity from a committed adjacency fact.
    ///
    /// This is used only while applying an already-published delta whose
    /// complete syntax identity is stored by the surrounding publication
    /// record. New identities must use [`Self::from_syntax`] or
    /// [`Self::from_automatic`].
    #[doc(hidden)]
    pub(crate) fn from_cached_raw(raw: u64) -> Self {
        Self {
            raw,
            identity: None,
            marker: PhantomData,
        }
    }

    /// Constructs an automatic identity with its complete erased logical key.
    #[doc(hidden)]
    pub(crate) fn from_automatic(
        raw: u64,
        identity: Arc<dyn crate::reactive::value::KeyValue>,
    ) -> Self {
        Self {
            raw,
            identity: Some(identity),
            marker: PhantomData,
        }
    }

    /// Converts an automatically allocated node into the public abstract-tree
    /// identity without copying its complete erased key.
    pub(crate) fn into_ast_box<T>(self) -> crate::reactive::abstract_tree::AstBox<T> {
        let identity = self
            .identity
            .expect("automatic node identity must retain its complete key");
        crate::reactive::abstract_tree::AstBox::from_parts(self.raw, identity)
    }

    /// The stable cached hash, used only by framework-internal fact
    /// encodings and the crate's own test fixtures. Never part of the
    /// authoring surface (plan §7).
    #[doc(hidden)]
    pub(crate) fn raw_id(&self) -> u64 {
        self.raw
    }
}

impl<V: 'static> std::fmt::Debug for Node<V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Node({})", self.raw)
    }
}
