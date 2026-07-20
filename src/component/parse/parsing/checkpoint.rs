//! Cached local frontier shape used to reject impossible reuse candidates cheaply.

use super::ParseColumn;
use crate::component::parse::data::gss::{GssArena, GssNodeId};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FrontierCheckpoint {
    base: Vec<(usize, usize)>,
    active: Vec<(usize, usize)>,
    error_derived: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ColumnCheckpointCache {
    frontier: Option<FrontierCheckpoint>,
}

impl ColumnCheckpointCache {
    pub(crate) fn invalidate(&mut self) {
        self.frontier = None;
    }

    pub(crate) fn frontier(&self) -> Option<&FrontierCheckpoint> {
        self.frontier.as_ref()
    }

    pub(crate) fn store(&mut self, checkpoint: FrontierCheckpoint) {
        self.frontier = Some(checkpoint);
    }
}

pub(crate) fn frontier_checkpoint_for_column<'a>(
    column: &'a mut ParseColumn,
    gss: &GssArena,
) -> &'a FrontierCheckpoint {
    if column.cached_frontier_checkpoint().is_none() {
        let base = frontier_shape(column.base_active_nodes(), gss);
        let active = frontier_shape(column.active_nodes(), gss);
        column.cache_frontier_checkpoint(FrontierCheckpoint {
            base,
            active,
            error_derived: column.error_derived,
        });
    }
    column
        .cached_frontier_checkpoint()
        .expect("frontier checkpoint cached")
}

fn frontier_shape(nodes: impl Iterator<Item = GssNodeId>, gss: &GssArena) -> Vec<(usize, usize)> {
    // This collision-free local shape is only a necessary-condition filter;
    // suffix reuse still requires the exact graph correspondence.
    let mut shape = nodes
        .filter_map(|node| {
            Some((
                gss.get_node(node)?.state,
                gss.outgoing_edge_ids(node).map_or(0, <[_]>::len),
            ))
        })
        .collect::<Vec<_>>();
    shape.sort_unstable();
    shape
}

#[cfg(test)]
mod tests {
    use indexmap::IndexSet;

    use super::frontier_checkpoint_for_column;
    use crate::component::parse::{
        data::{
            gss::GssArena,
            product::{Product, ProductArena},
        },
        parsing::ParseColumn,
    };

    #[test]
    fn frontier_checkpoint_is_cached_and_invalidated() {
        let mut products = ProductArena::new();
        let product = products.insert(Product::token(7, 11, 13));
        let mut gss = GssArena::new();
        let start = gss.node(0, 0, 0);
        let shifted = gss.node(1, 0, 0);
        assert!(gss.add_edge(shifted, start, product, 0));

        let mut column = ParseColumn::new(Some(0), IndexSet::from([shifted]));
        let first = frontier_checkpoint_for_column(&mut column, &mut gss).clone();
        let second = frontier_checkpoint_for_column(&mut column, &mut gss).clone();
        assert_eq!(first, second);

        column.set_error_derived();
        let changed = frontier_checkpoint_for_column(&mut column, &mut gss).clone();
        assert_ne!(first, changed);
    }
}
