//! Built-in stable scope catalog and graph fact views.

use std::{collections::HashMap, marker::PhantomData};

use crate::scheme::node::{
    ComponentState, DeriveCx, IndexedRelation, NodeError, NodeProvider, NodeSchema,
    PortDeclaration, Relation, View,
};

use super::{ScopeAllocation, ScopeDomain, ScopeEdge, ScopeId, ScopeLifecycle};

/// Materialized identity for one domain-owned semantic scope key.
pub(crate) struct ScopeHandle<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> View for ScopeHandle<D> {
    type Key = D::ScopeKey;
    type Value = ScopeId<D>;
}

/// Scope catalog allocation facts.
pub struct ScopeAllocations<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeAllocations<D> {
    type Fact = ScopeAllocation<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeAllocations<D> {
    type Index = D::ScopeKey;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.key.clone()
    }
}

/// One domain-defined datum per semantic scope.
pub struct ScopeData<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> View for ScopeData<D> {
    type Key = ScopeId<D>;
    type Value = D::ScopeData;
}

/// Graph edges indexed by source scope.
pub struct ScopeEdges<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeEdges<D> {
    type Fact = ScopeEdge<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeEdges<D> {
    type Index = ScopeId<D>;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.source
    }
}

/// Closed scope frontiers indexed by scope.
pub struct ScopeLifecycles<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Relation for ScopeLifecycles<D> {
    type Fact = ScopeLifecycle<D>;
}

impl<D: ScopeDomain> IndexedRelation for ScopeLifecycles<D> {
    type Index = ScopeId<D>;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.scope
    }
}

/// Source requirements emitted by elaboration tasks.
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
    scopes: HashMap<D::ScopeKey, ScopeId<D>>,
}

impl<D: ScopeDomain> Default for ScopeCatalogState<D> {
    fn default() -> Self {
        Self {
            next_scope: 0,
            scopes: HashMap::new(),
        }
    }
}

/// Stable, demand-reclaimable allocation catalog.
pub(crate) struct ScopeCatalogNode<D: ScopeDomain> {
    state: ComponentState<ScopeCatalogState<D>>,
}

impl<D: ScopeDomain> Default for ScopeCatalogNode<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: ScopeDomain> ScopeCatalogNode<D> {
    pub(crate) fn new() -> Self {
        Self {
            state: ComponentState::new(ScopeCatalogState::default()),
        }
    }
}

impl<D: ScopeDomain> NodeProvider for ScopeCatalogNode<D> {
    type Key = D::ScopeKey;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            std::any::type_name::<Self>(),
            vec![
                PortDeclaration::map::<ScopeHandle<D>>(),
                PortDeclaration::indexed_set::<ScopeAllocations<D>>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let scope = {
            let state = cx.state_mut(&self.state)?;
            if let Some(scope) = state.scopes.get(&key) {
                *scope
            } else {
                let scope = ScopeId::logical(state.next_scope);
                state.next_scope = state
                    .next_scope
                    .checked_add(1)
                    .ok_or(NodeError::RevisionOverflow)?;
                state.scopes.insert(key.clone(), scope);
                scope
            }
        };
        cx.emit::<ScopeHandle<D>>(key.clone(), scope)?;
        cx.emit_relation::<ScopeAllocations<D>>(ScopeAllocation { key, scope })?;
        Ok(())
    }

    fn reclaim(
        &self,
        _cx: &mut crate::scheme::node::ReclaimCx<'_, '_>,
        _key: Self::Key,
    ) -> Result<(), NodeError> {
        Ok(())
    }
}
