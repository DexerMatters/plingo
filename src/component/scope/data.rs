use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use fluent_uri::Uri;
use thiserror::Error;

use crate::{
    component::{lex::LexerRoot, parse::AstKey},
    scheme::node::NodeKey,
};

/// Declares the types that belong to one independent scope-graph domain.
///
/// Domain typing prevents facts from unrelated languages or analyses from
/// sharing opaque scope identities merely because their label/data types match.
pub trait ScopeDomain: Clone + Eq + Hash + Send + Sync + 'static {
    type Root: LexerRoot + Clone + 'static;
    type Ast: Clone + Send + Sync + 'static;
    type Anchor: Clone + Eq + Hash + Send + Sync + 'static;
    type Label: Clone + Eq + Hash + Send + Sync + 'static;
    type Datum: Clone + Eq + Hash + Send + Sync + 'static;
    type Reference: Clone + Eq + Hash + Send + Sync + 'static;
    type Request: Clone + Eq + Hash + Send + Sync + 'static;
}

/// Opaque identity of one scope in domain `D`.
pub struct Scope<D: ScopeDomain>(u64, PhantomData<fn() -> D>);

impl<D: ScopeDomain> Scope<D> {
    pub(crate) const fn allocated(id: u64) -> Self {
        Self(id, PhantomData)
    }
}

impl<D: ScopeDomain> Copy for Scope<D> {}
impl<D: ScopeDomain> Clone for Scope<D> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<D: ScopeDomain> PartialEq for Scope<D> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<D: ScopeDomain> Eq for Scope<D> {}
impl<D: ScopeDomain> Hash for Scope<D> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl<D: ScopeDomain> PartialOrd for Scope<D> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<D: ScopeDomain> Ord for Scope<D> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}
impl<D: ScopeDomain> fmt::Debug for Scope<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Scope").field(&self.0).finish()
    }
}

/// Stable owner of a scope allocation.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ScopeOwner<D: ScopeDomain> {
    Document(Uri<&'static str>),
    Ast(AstKey),
    External(D::Anchor),
}

impl<D: ScopeDomain> fmt::Debug for ScopeOwner<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Document(uri) => formatter.debug_tuple("Document").field(uri).finish(),
            Self::Ast(ast) => formatter.debug_tuple("Ast").field(ast).finish(),
            Self::External(_) => formatter.write_str("External(..)"),
        }
    }
}

impl<D: ScopeDomain> ScopeOwner<D> {
    pub const fn document(uri: Uri<&'static str>) -> Self {
        Self::Document(uri)
    }

    pub const fn ast(ast: AstKey) -> Self {
        Self::Ast(ast)
    }

    pub fn external(anchor: D::Anchor) -> Self {
        Self::External(anchor)
    }
}

/// One visible scope allocation. This relation exposes allocation/reclamation as
/// ordinary graph additions/removals without a broad scope-delta wrapper.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeAllocation<D: ScopeDomain> {
    pub owner: ScopeOwner<D>,
    pub scope: Scope<D>,
}

/// Cycle policy declared by one scope relationship.
///
/// Scope facts are multi-owner graph relations. The property travels with the
/// fact so a domain-specific validator can enforce acyclic subsets without
/// coupling semantic passes to a private scope-state ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScopeProperty {
    #[default]
    Cyclic,
    Acyclic,
}

/// A datum attached to a scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeDatum<D: ScopeDomain> {
    pub scope: Scope<D>,
    pub datum: D::Datum,
}

/// A labelled relationship in a scope graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeEdge<D: ScopeDomain> {
    pub source: Scope<D>,
    pub label: D::Label,
    pub target: Scope<D>,
    pub property: ScopeProperty,
}

/// An application-defined reference attached to a scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeReference<D: ScopeDomain> {
    pub scope: Scope<D>,
    pub reference: D::Reference,
}

#[derive(Debug, Error)]
pub enum ScopeError<D: ScopeDomain> {
    #[error("parsed AST artifact {0:?} is unavailable")]
    MissingAst(AstKey),
    #[error("scope {0:?} is unavailable")]
    MissingScope(Scope<D>),
    #[error("scope rule failed: {0}")]
    Rule(String),
}

/// A selector used by a materialized scope-resolution query.
pub trait DatumSelector<D: ScopeDomain>: NodeKey {
    fn accepts(&self, datum: &D::Datum) -> bool;
}
