//! Built-in scope allocation and scope-graph query nodes.
//!
//! User-defined semantic rules live in `component::elaborate`. This module owns
//! only stable scope allocation plus the typed relations and query machinery
//! shared by elaborators.

use std::{collections::HashMap, fmt, hash::Hash, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use crate::{
    component::{parse::ParsedAst, source::LoadSourceText},
    scheme::node::{
        ComponentState, DeriveCx, Graph, IndexedRelation, Node, NodeError, Relation, View,
    },
};

use super::{
    DatumSelector, PathExpr, ResolutionPath, Scope, ScopeAllocation, ScopeDatum, ScopeDomain,
    ScopeEdge, ScopeOwner, ScopeReference,
};

/// Unit completion marker for one scope allocation task.
pub struct ScopeCatalogStamp<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> View for ScopeCatalogStamp<D> {
    type Key = ScopeOwner<D>;
    type Value = ();
}

/// Materialized identity for one scope owner.
pub struct ScopeHandle<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> View for ScopeHandle<D> {
    type Key = ScopeOwner<D>;
    type Value = Scope<D>;
}

/// Allocation facts whose additions/removals are the scope catalog's native
/// delta stream.
pub struct ScopeAllocations<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeAllocations<D> {
    type Fact = ScopeAllocation<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeAllocations<D> {
    type Index = ScopeOwner<D>;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.owner.clone()
    }
}

/// Public relation of URI-free graph edges.
pub struct ScopeEdges<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeEdges<D> {
    type Fact = ScopeEdge<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeEdges<D> {
    type Index = Scope<D>;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.source
    }
}

/// Public relation of URI-free graph data.
pub struct ScopeDatums<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeDatums<D> {
    type Fact = ScopeDatum<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeDatums<D> {
    type Index = Scope<D>;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.scope
    }
}

/// Public relation of URI-free graph references.
pub struct ScopeReferences<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeReferences<D> {
    type Fact = ScopeReference<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeReferences<D> {
    type Index = Scope<D>;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.scope
    }
}

/// Post-commit source requirements emitted by elaborator frames.
pub struct SourceRequirements<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for SourceRequirements<D> {
    type Fact = D::Request;
}

impl<D: ScopeDomain> IndexedRelation for SourceRequirements<D> {
    type Index = D::Request;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.clone()
    }
}

#[derive(Clone)]
struct ScopeCatalogState<D: ScopeDomain> {
    next_scope: u64,
    scopes: HashMap<ScopeOwner<D>, Scope<D>>,
}

impl<D: ScopeDomain> Default for ScopeCatalogState<D> {
    fn default() -> Self {
        Self {
            next_scope: 0,
            scopes: HashMap::new(),
        }
    }
}

/// Built-in owner-to-scope allocator.
///
/// It performs no language analysis and owns no scope graph facts beyond its
/// allocation relation. Elaborator tasks own every edge, datum, reference, and
/// source requirement they emit.
pub struct ScopeCatalogNode<D: ScopeDomain> {
    state: ComponentState<ScopeCatalogState<D>>,
}

impl<D: ScopeDomain> Default for ScopeCatalogNode<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: ScopeDomain> ScopeCatalogNode<D> {
    pub fn new() -> Self {
        Self {
            state: ComponentState::new(ScopeCatalogState::default()),
        }
    }

    /// Installs the built-in catalog required by elaborators in this domain.
    pub fn install(graph: &mut Graph) -> Result<(), NodeError> {
        graph.install(Self::new())
    }

    /// Installs an idempotent post-commit loader for URI requirements emitted
    /// by elaborator frames.
    pub fn install_uri_source_loader(
        graph: &mut Graph,
        loader: impl Fn(Uri<&'static str>) -> Result<Arc<str>, String> + Send + Sync + 'static,
    ) where
        D::Request: Into<Uri<&'static str>> + fmt::Debug,
    {
        graph.on_relation_added_command::<SourceRequirements<D>, LoadSourceText>(
            move |_, request| {
                let uri = request.into();
                loader(uri).map(|text| LoadSourceText { uri, text })
            },
        );
    }

    fn ast_is_live(
        cx: &mut DeriveCx<'_, '_>,
        ast: crate::component::parse::AstKey,
    ) -> Result<bool, NodeError> {
        match cx.observe::<ParsedAst<D::Root, D::Ast>>(ast) {
            Ok(_) => Ok(true),
            Err(NodeError::MissingView(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl<D: ScopeDomain> Node for ScopeCatalogNode<D> {
    type Key = ScopeOwner<D>;
    type Output = ScopeCatalogStamp<D>;

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, owner: Self::Key) -> Result<(), NodeError> {
        if let ScopeOwner::Ast(ast) = &owner
            && !Self::ast_is_live(cx, ast.clone())?
        {
            cx.state_mut(&self.state)?.scopes.remove(&owner);
            return Ok(());
        }

        let scope = {
            let state = cx.state_mut(&self.state)?;
            if let Some(scope) = state.scopes.get(&owner) {
                *scope
            } else {
                let scope = Scope::allocated(state.next_scope);
                state.next_scope = state
                    .next_scope
                    .checked_add(1)
                    .ok_or(NodeError::RevisionOverflow)?;
                state.scopes.insert(owner.clone(), scope);
                scope
            }
        };
        cx.emit::<ScopeHandle<D>>(owner.clone(), scope)?;
        cx.emit_relation::<ScopeAllocations<D>>(ScopeAllocation { owner, scope })?;
        Ok(())
    }

    fn reclaim(
        &self,
        cx: &mut crate::scheme::node::ReclaimCx<'_, '_>,
        owner: Self::Key,
    ) -> Result<(), NodeError> {
        cx.state_mut(&self.state)?.scopes.remove(&owner);
        Ok(())
    }
}

/// Stable key for one materialized scope resolution.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ResolutionKey<D: ScopeDomain, Selector> {
    pub start: Scope<D>,
    pub path: PathExpr<<D as ScopeDomain>::Label>,
    pub selector: Selector,
}

impl<D: ScopeDomain, Selector: fmt::Debug> fmt::Debug for ResolutionKey<D, Selector> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolutionKey")
            .field("start", &self.start)
            .field("path", &"..")
            .field("selector", &self.selector)
            .finish()
    }
}

/// Resolution result view keyed by [`ResolutionKey`].
pub struct ScopeResolution<D: ScopeDomain, Selector>(PhantomData<fn() -> (D, Selector)>);

impl<D, Selector> View for ScopeResolution<D, Selector>
where
    D: ScopeDomain,
    Selector: DatumSelector<D>,
{
    type Key = ResolutionKey<D, Selector>;
    type Value = std::collections::HashSet<ResolutionPath<D>>;
}

/// Generic node that resolves a query from materialized edge and datum facts.
pub struct ResolutionNode<D: ScopeDomain, Selector>(PhantomData<fn() -> (D, Selector)>);

impl<D: ScopeDomain, Selector> Default for ResolutionNode<D, Selector> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<D, Selector> Node for ResolutionNode<D, Selector>
where
    D: ScopeDomain,
    Selector: DatumSelector<D>,
{
    type Key = ResolutionKey<D, Selector>;
    type Output = ScopeResolution<D, Selector>;

    fn derive(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        key: Self::Key,
    ) -> Result<std::collections::HashSet<ResolutionPath<D>>, NodeError> {
        let selector = key.selector.clone();
        Ok(super::query::resolve_indexed(
            key.start,
            key.path,
            move |datum| selector.accepts(datum),
            |scope, needs_datums| {
                let edges = cx.relation_facts_at::<ScopeEdges<D>>(scope);
                let datums = if needs_datums {
                    cx.relation_facts_at::<ScopeDatums<D>>(scope)
                } else {
                    Default::default()
                };
                (edges, datums)
            },
        ))
    }
}
