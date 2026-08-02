//! Canonical typed ports for [`Structure`](super::Structure) facts.

use std::{marker::PhantomData, sync::Arc};

use crate::scheme::node::{IndexedRelation, Relation, View};

use super::{ChildRef, StructuralArtifact, StructuralEdge, Structure, StructureEntry};

/// Optional indexed discovery entries for one structure.
pub struct StructureEntries<
    S: Structure,
    E: crate::scheme::node::NodeKey,
    M: crate::scheme::node::NodeKey = (),
>(PhantomData<fn() -> (S, E, M)>);
impl<S, E, M> Relation for StructureEntries<S, E, M>
where
    S: Structure,
    E: crate::scheme::node::NodeKey,
    M: crate::scheme::node::NodeKey,
{
    type Fact = StructureEntry<S::NodeKey, E, M>;
}

impl<S, E, M> IndexedRelation for StructureEntries<S, E, M>
where
    S: Structure,
    E: crate::scheme::node::NodeKey,
    M: crate::scheme::node::NodeKey,
{
    type Index = E;
    fn index(fact: &Self::Fact) -> Self::Index {
        fact.entry.clone()
    }
}

/// One independently observable erased typed structural artifact.
pub struct StructureNode<S: Structure>(PhantomData<fn() -> S>);

impl<S: Structure> View for StructureNode<S> {
    type Key = S::NodeKey;
    type Value = StructuralArtifact<S::NodeKey, S::NodeMetadata>;
}

/// One ordered containment bucket emitted by a source node.
pub struct StructureChildren<S: Structure>(PhantomData<fn() -> S>);

impl<S: Structure> View for StructureChildren<S> {
    type Key = S::NodeKey;
    type Value = Arc<[ChildRef<S::NodeKey>]>;
}

/// Arbitrary structure graph edges, independently observable by source node.
pub struct StructureEdges<S: Structure>(PhantomData<fn() -> S>);

impl<S: Structure> Relation for StructureEdges<S> {
    type Fact = S::Edge;
}

impl<S: Structure> IndexedRelation for StructureEdges<S> {
    type Index = S::NodeKey;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.source()
    }
}
