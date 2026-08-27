//! Snapshot-based scope-graph rendering.

use std::fmt::Debug;
use std::fmt::Write as _;

use plingo::framework::scope::{ScopeDomain, snapshot_node, snapshot_nodes};
use plingo::reactive::Snapshot;

/// A point-in-time, renderable view of one domain's scope graph.
pub struct ScopeGraph<D: ScopeDomain> {
    _marker: std::marker::PhantomData<fn() -> D>,
}

impl<D: ScopeDomain> ScopeGraph<D> {
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<D: ScopeDomain> Default for ScopeGraph<D> {
    fn default() -> Self {
        Self::new()
    }
}

/// Renders one concrete domain's committed scope graph into a String:
/// every node's payload plus its labelled outgoing targets.
pub fn render_domain_graph<D>(snapshot: &Snapshot) -> String
where
    D: ScopeDomain,
    D::ScopeData: Debug,
{
    let mut out = String::new();
    for node in snapshot_nodes::<D>(snapshot) {
        match snapshot_node(snapshot, node.clone()) {
            Some(data) => writeln!(out, "• {data:?}").expect("write"),
            None => writeln!(out, "• {node:?}").expect("write"),
        }
    }
    out
}
