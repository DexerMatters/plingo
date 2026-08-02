use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use crate::component::structural::{GraphEdges, StructuralEdge, Structure};
use crate::scheme::node::{Graph, NodeError, PortDeclaration};

use super::node::ScopeCatalogNode;

/// Write-set marker declaring the canonical scope-datum map port.
pub struct ScopeDefinitions<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> crate::component::api::WriteSet for ScopeDefinitions<D> {
    fn declarations<Owner: crate::component::api::Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::map::<
            crate::component::structural::StructureNode<ScopeStructure<D>>,
        >()]
    }
}

/// Write-set marker declaring the canonical scope-edge set port.
pub struct ScopeEdges<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> crate::component::api::WriteSet for ScopeEdges<D> {
    fn declarations<Owner: crate::component::api::Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::indexed_set::<
            crate::component::structural::StructureEdges<ScopeStructure<D>>,
        >()]
    }
}

/// Write-set marker declaring canonical scope discovery-entry ports.
pub struct ScopeEntries<D: ScopeDomain, E, M = ()>(PhantomData<fn() -> (D, E, M)>);

impl<D, E, M> crate::component::api::WriteSet for ScopeEntries<D, E, M>
where
    D: ScopeDomain,
    E: crate::scheme::node::NodeKey,
    M: crate::scheme::node::NodeKey,
{
    fn declarations<Owner: crate::component::api::Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::indexed_set::<
            crate::component::structural::StructureEntries<ScopeStructure<D>, E, M>,
        >()]
    }
}

/// Write-set marker declaring the canonical source-requirement port.
impl<D: ScopeDomain> crate::component::api::WriteSet for super::node::SourceRequirements<D> {
    fn declarations<Owner: crate::component::api::Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::indexed_set::<
            super::node::SourceRequirements<D>,
        >()]
    }
}

/// Types owned by one independent scope-graph domain.
pub trait ScopeDomain: Clone + Eq + Hash + Send + Sync + 'static {
    type ScopeKey: Clone + Eq + Hash + Send + Sync + 'static;
    type ScopeData: Clone + Eq + Hash + Send + Sync + 'static;
    type Label: Clone + Eq + Hash + Send + Sync + 'static;
    type Request: Clone + Eq + Hash + Send + Sync + 'static;
}

/// Structural-view descriptor for one domain's semantic scope graph.
///
/// Scope facts remain available through their purpose-specific ports; this
/// descriptor exposes the same identities and edges to generic structural
/// transforms without treating arbitrary intermediate structures as scopes.
pub struct ScopeStructure<D: ScopeDomain>(PhantomData<fn() -> D>);

impl<D: ScopeDomain> Structure for ScopeStructure<D> {
    type NodeKey = ScopeId<D>;
    type NodeMetadata = ();
    type Edge = ScopeEdge<D>;
    type Topology = GraphEdges;
}

impl<D: ScopeDomain> ScopeStructure<D> {
    /// Installs the private catalog provider for this scope domain once.
    pub fn install(graph: &mut Graph) -> Result<(), NodeError> {
        graph.install(ScopeCatalogNode::<D>::new())
    }
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

/// A labelled graph relationship.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeEdge<D: ScopeDomain> {
    pub source: ScopeId<D>,
    pub label: D::Label,
    pub target: ScopeId<D>,
    pub property: ScopeProperty,
}

impl<D: ScopeDomain> StructuralEdge<ScopeStructure<D>> for ScopeEdge<D> {
    fn source(&self) -> ScopeId<D> {
        self.source
    }

    fn target(&self) -> ScopeId<D> {
        self.target
    }
}
