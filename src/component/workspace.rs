//! High-level document-oriented compiler access.
//!
//! A [`Workspace`] owns one graph configuration. [`Document`] owns the demand
//! leases for the artifacts it requests, so edit clients never manipulate
//! commands or leases directly.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use fluent_uri::Uri;

use crate::{
    component::source::{SourceEdit, SourceInput},
    scheme::node::{
        DemandLease, Graph, NodeError, NodeProvider, ReadGraph, SnapshotId, Subscription, View,
    },
};

/// A configured incremental compiler graph.
#[derive(Clone)]
pub struct Workspace {
    graph: Rc<RefCell<Graph>>,
}

impl Workspace {
    /// Builds a graph through direct graph configuration.
    pub fn build<F>(configure: F) -> Result<Self, NodeError>
    where
        F: FnOnce(&mut Graph) -> Result<(), NodeError>,
    {
        let mut graph = Graph::new();
        configure(&mut graph)?;
        Ok(Self {
            graph: Rc::new(RefCell::new(graph)),
        })
    }

    /// Opens a document with one requested root artifact. The artifact's
    /// provider stays materialized until the document is closed or dropped.
    pub fn open<P>(
        &self,
        uri: Uri<&'static str>,
        text: impl Into<Arc<str>>,
    ) -> Result<Document, NodeError>
    where
        P: NodeProvider<Key = Uri<&'static str>>,
    {
        let mut graph = self.graph.borrow_mut();
        graph.command(SourceInput::load_text(uri, text))?;
        let demand = graph.demand::<P>(uri)?;
        Ok(Document {
            workspace: self.clone(),
            uri,
            demands: vec![demand],
        })
    }

    /// Returns the latest committed revision without exposing graph mutation.
    pub fn revision(&self) -> SnapshotId {
        self.graph.borrow().revision()
    }
}

/// An open source document and the artifact demands it owns.
pub struct Document {
    workspace: Workspace,
    uri: Uri<&'static str>,
    demands: Vec<DemandLease>,
}

impl Document {
    pub fn uri(&self) -> Uri<&'static str> {
        self.uri
    }

    /// Applies one UTF-8-safe source edit. The graph determines incrementality
    /// from the exact dependency set; callers select no rebuild mode.
    pub fn apply(&self, edit: SourceEdit) -> Result<(), NodeError> {
        if edit.span().uri != self.uri {
            return Err(NodeError::message(
                "source edit targets a different document",
            ));
        }
        self.workspace
            .graph
            .borrow_mut()
            .command(SourceInput::apply(edit))
    }

    /// Requests another root artifact for this document. It remains available
    /// for the document lifetime without a caller-managed [`DemandLease`].
    pub fn demand<P>(&mut self) -> Result<(), NodeError>
    where
        P: NodeProvider<Key = Uri<&'static str>>,
    {
        let demand = self.workspace.graph.borrow_mut().demand::<P>(self.uri)?;
        self.demands.push(demand);
        Ok(())
    }

    /// Reads a requested document-keyed artifact from the latest committed
    /// revision.
    pub fn artifact<V>(&self) -> Result<V::Value, NodeError>
    where
        V: View<Key = Uri<&'static str>>,
    {
        self.workspace
            .graph
            .borrow()
            .get::<V>(self.uri)
            .ok_or_else(NodeError::missing_view::<V>)
    }

    /// Subscribes to one already materialized artifact. Streaming is explicit;
    /// normal reads use [`Self::artifact`].
    pub fn watch<V>(&self) -> Result<Subscription<V>, NodeError>
    where
        V: View<Key = Uri<&'static str>>,
    {
        self.workspace.graph.borrow_mut().subscribe::<V>(self.uri)
    }
}
