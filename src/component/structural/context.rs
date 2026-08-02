//! Borrowed structural-view operations over a component [`Context`].

use std::{marker::PhantomData, sync::Arc};

use crate::{
    component::api::{Component, Context, ContextView, Error},
    scheme::node::{NodeKey, NodeValue},
};

use super::{
    ChildRef, StructuralArtifact, Structure, StructureChildren, StructureEdges, StructureEntries,
    StructureEntry, StructureNode,
};

/// Borrowed access to one structure's canonical graph ports.
///
/// A structure type is its own view selector: `cx.view::<LoweredAst>()`.
/// This view owns no scheduler, output buffer, or transaction state. Every
/// operation immediately delegates to the [`Context`] that created it.
pub struct StructuralView<'cx, 'tx, C: Component, S: Structure> {
    cx: &'cx mut Context<'tx, C>,
    _structure: PhantomData<fn() -> S>,
}

impl<'cx, 'tx, C: Component, S: Structure> StructuralView<'cx, 'tx, C, S> {
    pub(crate) fn open(cx: &'cx mut Context<'tx, C>) -> Self {
        Self {
            cx,
            _structure: PhantomData,
        }
    }

    /// Reads and type-checks one structural artifact payload without cloning it.
    pub fn artifact<T: NodeValue>(&mut self, key: S::NodeKey) -> Option<Arc<T>> {
        self.cx
            .get::<StructureNode<S>>(key)
            .and_then(|artifact| artifact.deref::<T>())
    }

    /// Publishes one artifact with the structure's default metadata.
    pub fn define_artifact<T: NodeValue>(
        &mut self,
        key: S::NodeKey,
        value: T,
    ) -> Result<(), Error> {
        let artifact =
            StructuralArtifact::with_metadata(key.clone(), S::NodeMetadata::default(), value);
        self.cx.define::<StructureNode<S>>(key, artifact)
    }

    /// Reads an ordered containment bucket.
    pub fn children(&mut self, key: S::NodeKey) -> Option<Arc<[ChildRef<S::NodeKey>]>> {
        self.cx.get::<StructureChildren<S>>(key)
    }

    /// Publishes an ordered containment bucket.
    pub fn define_children(
        &mut self,
        key: S::NodeKey,
        children: Arc<[ChildRef<S::NodeKey>]>,
    ) -> Result<(), Error> {
        self.cx.define::<StructureChildren<S>>(key, children)
    }

    /// Reads all graph edges owned by one source node.
    pub fn edges(&mut self, source: S::NodeKey) -> Vec<S::Edge> {
        crate::scheme::node::ReadGraph::scan::<StructureEdges<S>>(&self.cx.derive, source)
    }

    /// Supports one graph edge through the current component run.
    pub fn support_edge(&mut self, edge: S::Edge) -> Result<(), Error> {
        self.cx.support::<StructureEdges<S>>(edge)
    }

    /// Reads discovery entries indexed by their entry key.
    pub fn entries<E, M>(&mut self, entry: E) -> Vec<StructureEntry<S::NodeKey, E, M>>
    where
        E: NodeKey,
        M: NodeKey,
    {
        crate::scheme::node::ReadGraph::scan::<StructureEntries<S, E, M>>(&self.cx.derive, entry)
    }

    /// Supports one discovery entry through the current component run.
    pub fn support_entry<E, M>(
        &mut self,
        entry: StructureEntry<S::NodeKey, E, M>,
    ) -> Result<(), Error>
    where
        E: NodeKey,
        M: NodeKey,
    {
        self.cx.support::<StructureEntries<S, E, M>>(entry)
    }
}

impl<C: Component, S: Structure> ContextView<C> for S {
    type Access<'cx, 'tx>
        = StructuralView<'cx, 'tx, C, S>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        StructuralView::open(cx)
    }
}
