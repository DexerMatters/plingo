//! Borrowed scope-graph operations over a component [`Context`].

use std::{marker::PhantomData, sync::Arc};

use crate::{
    component::{
        api::{Component, Context, ContextView, Error},
        structural::{
            StructuralArtifact, StructureEdges, StructureEntries, StructureEntry, StructureNode,
        },
    },
    scheme::node::{NodeError, NodeKey},
};

use super::{
    ScopeAllocation, ScopeDomain, ScopeEdge, ScopeId, ScopeProperty, ScopeStructure,
    node::{ScopeAllocations, ScopeCatalogNode, ScopeHandle, SourceRequirements},
};

/// Borrowed access to a domain's materialized scope catalog and structural graph.
pub struct ScopeView<'cx, 'tx, C: Component, D: ScopeDomain> {
    pub(crate) cx: &'cx mut Context<'tx, C>,
    _domain: PhantomData<fn() -> D>,
}

impl<'cx, 'tx, C: Component, D: ScopeDomain> ScopeView<'cx, 'tx, C, D> {
    pub(crate) fn open(cx: &'cx mut Context<'tx, C>) -> Self {
        Self {
            cx,
            _domain: PhantomData,
        }
    }

    /// Starts a dependency-tracked resolution query from one scope.
    pub fn query_from<'a>(
        &'a mut self,
        start: ScopeId<D>,
    ) -> super::query::ScopeQuery<'a, 'tx, C, D> {
        super::query::ScopeQuery { cx: self.cx, start }
    }

    /// Materializes and returns the stable scope identity for a domain key.
    /// If the catalog has not published yet, the current component suspends
    /// and reruns after the catalog allocates.
    pub fn scope(&mut self, key: D::ScopeKey) -> Result<ScopeId<D>, Error> {
        self.cx.retain_provider::<ScopeCatalogNode<D>>(key.clone());
        match self.cx.get::<ScopeHandle<D>>(key) {
            Some(scope) => Ok(scope),
            None => {
                self.cx.awaiting = true;
                Err(Error::suspended())
            }
        }
    }

    /// Declares one semantic scope and publishes its immutable datum.
    pub fn declare(&mut self, key: D::ScopeKey, data: D::ScopeData) -> Result<ScopeId<D>, Error> {
        self.define_scope(key, data)
    }

    /// Declares one scope and links it from an existing source scope.
    pub fn declare_linked(
        &mut self,
        key: D::ScopeKey,
        data: D::ScopeData,
        source: ScopeId<D>,
        label: D::Label,
        property: ScopeProperty,
    ) -> Result<ScopeId<D>, Error> {
        let scope = self.declare(key, data)?;
        self.support_edge(source, label, scope, property)?;
        Ok(scope)
    }

    /// Finds an allocated scope without materializing a new catalog entry.
    pub fn find_scope(&mut self, key: D::ScopeKey) -> Option<ScopeId<D>> {
        self.allocations(key)
            .into_iter()
            .next()
            .map(|allocation| allocation.scope)
    }

    /// Supports one indexed scope discovery entry.
    pub fn support_entry<E, M>(
        &mut self,
        entry: StructureEntry<ScopeId<D>, E, M>,
    ) -> Result<(), Error>
    where
        E: NodeKey,
        M: NodeKey,
    {
        self.cx
            .support::<StructureEntries<ScopeStructure<D>, E, M>>(entry)
    }

    /// Publishes the immutable semantic data owned by a scope.
    pub fn define_scope(
        &mut self,
        key: D::ScopeKey,
        data: D::ScopeData,
    ) -> Result<ScopeId<D>, Error> {
        let scope = self.scope(key)?;
        self.cx.define::<StructureNode<ScopeStructure<D>>>(
            scope,
            StructuralArtifact::new(scope, data),
        )?;
        Ok(scope)
    }

    /// Reads a scope's immutable semantic data without cloning the artifact.
    pub fn data(&mut self, scope: ScopeId<D>) -> Option<Arc<D::ScopeData>> {
        self.cx
            .get::<StructureNode<ScopeStructure<D>>>(scope)
            .and_then(|artifact| artifact.deref::<D::ScopeData>())
    }

    /// Supports a labelled scope relationship.
    pub fn support_edge(
        &mut self,
        source: ScopeId<D>,
        label: D::Label,
        target: ScopeId<D>,
        property: ScopeProperty,
    ) -> Result<(), Error> {
        if property == ScopeProperty::Acyclic && source == target {
            return Err(NodeError::message("an acyclic scope edge cannot self-loop").into());
        }
        self.cx
            .support::<StructureEdges<ScopeStructure<D>>>(ScopeEdge {
                source,
                label,
                target,
                property,
            })
    }

    /// Reads all relationships owned by one scope.
    pub fn edges(&mut self, source: ScopeId<D>) -> Vec<ScopeEdge<D>> {
        crate::scheme::node::ReadGraph::scan::<StructureEdges<ScopeStructure<D>>>(
            &self.cx.derive,
            source,
        )
    }

    /// Reads all catalog allocations for one domain key.
    pub fn allocations(&mut self, key: D::ScopeKey) -> Vec<ScopeAllocation<D>> {
        crate::scheme::node::ReadGraph::scan::<ScopeAllocations<D>>(&self.cx.derive, key)
    }

    /// Supports one source-requirement fact for the current component.
    pub fn require_source(&mut self, request: D::Request) -> Result<(), Error> {
        self.cx.support::<SourceRequirements<D>>(request)
    }

    /// Reads source-requirement facts observed by the current component.
    pub fn source_requirements(&mut self) -> Vec<D::Request> {
        crate::scheme::node::ReadGraph::scan_all::<SourceRequirements<D>>(&self.cx.derive)
    }
}

impl<C: Component, D: ScopeDomain> ContextView<C> for crate::component::api::Scope<D> {
    type Access<'cx, 'tx>
        = ScopeView<'cx, 'tx, C, D>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        ScopeView::open(cx)
    }
}
