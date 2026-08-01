use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use crate::component::lex::LexerRoot;

/// Types owned by one independent scope-graph domain.
pub trait ScopeDomain: Clone + Eq + Hash + Send + Sync + 'static {
    type Root: LexerRoot + Clone + 'static;
    type Ast: Clone + Send + Sync + 'static;
    type ScopeKey: Clone + Eq + Hash + Send + Sync + 'static;
    type ScopeData: Clone + Eq + Hash + Send + Sync + 'static;
    type Label: Clone + Eq + Hash + Send + Sync + 'static;
    type Request: Clone + Eq + Hash + Send + Sync + 'static;
}

/// Stable graph-local identity for one domain-defined semantic scope.
pub struct ScopeId<D: ScopeDomain>(u64, PhantomData<fn() -> D>);

impl<D: ScopeDomain> ScopeId<D> {
    pub(crate) const fn logical(id: u64) -> Self {
        Self(id, PhantomData)
    }

    pub(crate) const fn id(self) -> u64 {
        self.0
    }
}

impl<D: ScopeDomain> Copy for ScopeId<D> {}
impl<D: ScopeDomain> Clone for ScopeId<D> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<D: ScopeDomain> PartialEq for ScopeId<D> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<D: ScopeDomain> Eq for ScopeId<D> {}
impl<D: ScopeDomain> Hash for ScopeId<D> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl<D: ScopeDomain> PartialOrd for ScopeId<D> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<D: ScopeDomain> Ord for ScopeId<D> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}
impl<D: ScopeDomain> fmt::Debug for ScopeId<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ScopeId").field(&self.0).finish()
    }
}

/// Public catalog allocation for one semantic scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeAllocation<D: ScopeDomain> {
    pub key: D::ScopeKey,
    pub scope: ScopeId<D>,
}

/// Cycle policy declared by one relationship.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScopeProperty {
    #[default]
    Cyclic,
    Acyclic,
}

/// A complete scope is safe to use as a resolution frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopeLifecycle<D: ScopeDomain> {
    pub scope: ScopeId<D>,
}

impl<D: ScopeDomain> ScopeLifecycle<D> {
    pub const fn closed(scope: ScopeId<D>) -> Self {
        Self { scope }
    }

    pub const fn is_closed(&self) -> bool {
        true
    }
}

/// A labelled graph relationship.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeEdge<D: ScopeDomain> {
    pub source: ScopeId<D>,
    pub label: D::Label,
    pub target: ScopeId<D>,
    pub property: ScopeProperty,
}
