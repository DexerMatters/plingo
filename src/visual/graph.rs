//! Snapshot-based scope-graph rendering (plan Phase 6).

use std::fmt::Debug;
use std::fmt::Write as _;

use plingo::framework::scope::{ScopeDomain, ScopeGraphSnapshot};

/// A point-in-time, renderable view of one domain's scope graph (shape
/// parity with the legacy type).
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
pub fn render_domain_graph<D>(view: &ScopeGraphSnapshot<'_, D>) -> String
where
    D: ScopeDomain,
    D::ScopeData: Debug,
{
    let mut out = String::new();
    for id in view.node_ids() {
        match view.node_data(id) {
            Some(data) => writeln!(out, "• {data:?}").expect("write"),
            None => writeln!(out, "• {id:?}").expect("write"),
        }
    }
    out
}
