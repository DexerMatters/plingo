use std::hash::{DefaultHasher, Hash, Hasher};

use super::{
    data::{
        gss::{GssArena, GssNodeId},
        product::{ProductArena, ProductId},
    },
    parsing::ParseColumn,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FrontierCheckpoint {
    pub frontier_key: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BoundaryCheckpoint {
    pub frontier_key: u64,
    pub semantic_key: u64,
    pub accepted_key: u64,
    pub diagnostics_key: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ColumnCheckpointCache {
    dirty: bool,
    frontier: Option<FrontierCheckpoint>,
    semantic: Option<BoundaryCheckpoint>,
}

impl ColumnCheckpointCache {
    pub(crate) fn invalidate(&mut self) {
        self.dirty = true;
        self.frontier = None;
        self.semantic = None;
    }

    pub(crate) fn frontier(&self) -> Option<&FrontierCheckpoint> {
        if self.dirty {
            None
        } else {
            self.frontier.as_ref()
        }
    }

    pub(crate) fn store_frontier(&mut self, checkpoint: FrontierCheckpoint) {
        self.frontier = Some(checkpoint);
        self.dirty = false;
    }

    pub(crate) fn boundary(&self) -> Option<&BoundaryCheckpoint> {
        if self.dirty {
            None
        } else {
            self.semantic.as_ref()
        }
    }

    pub(crate) fn store_boundary(&mut self, checkpoint: BoundaryCheckpoint) {
        self.frontier = Some(FrontierCheckpoint {
            frontier_key: checkpoint.frontier_key,
        });
        self.semantic = Some(checkpoint);
        self.dirty = false;
    }
}

pub(crate) fn frontier_checkpoint_for_column<'a>(
    column: &'a mut ParseColumn,
    gss: &mut GssArena,
) -> &'a FrontierCheckpoint {
    if column.cached_frontier_checkpoint().is_none() {
        let frontier_key = frontier_hash(column, gss);
        column.cache_frontier_checkpoint(FrontierCheckpoint { frontier_key });
    }
    column
        .cached_frontier_checkpoint()
        .expect("frontier checkpoint cached")
}

pub(crate) fn checkpoint_for_column<'a>(
    column: &'a mut ParseColumn,
    gss: &mut GssArena,
    products: &ProductArena,
) -> &'a BoundaryCheckpoint {
    if column.cached_boundary_checkpoint().is_some() {
        return column
            .cached_boundary_checkpoint()
            .expect("boundary checkpoint cached");
    }

    let frontier_key = frontier_checkpoint_for_column(column, gss).frontier_key;
    let accepted_key = product_list_hash(column.accepted(), products);
    let diagnostics_key = hash_value(&column.diagnostics);
    let products_key = product_list_hash(&column.products, products);
    let frontier_semantic_key = frontier_semantic_hash(column, gss, products);

    column.cache_boundary_checkpoint(BoundaryCheckpoint {
        frontier_key,
        semantic_key: hash_value(&(
            frontier_key,
            frontier_semantic_key,
            products_key,
            accepted_key,
            diagnostics_key,
            column.error_derived,
        )),
        accepted_key,
        diagnostics_key,
    });

    column
        .cached_boundary_checkpoint()
        .expect("boundary checkpoint cached")
}

fn frontier_hash(column: &ParseColumn, gss: &mut GssArena) -> u64 {
    let base = frontier_set_hash(column.base_active_nodes(), gss);
    let active = frontier_set_hash(column.active_nodes(), gss);
    hash_value(&(base, active, column.error_derived))
}

fn frontier_semantic_hash(
    column: &ParseColumn,
    gss: &mut GssArena,
    products: &ProductArena,
) -> u64 {
    let base = frontier_semantic_set_hash(column.base_active_nodes(), gss, products);
    let active = frontier_semantic_set_hash(column.active_nodes(), gss, products);
    hash_value(&(base, active, column.error_derived))
}

fn frontier_set_hash(nodes: impl Iterator<Item = GssNodeId>, gss: &mut GssArena) -> u64 {
    let mut hashes = nodes
        .map(|node_id| gss.frontier_hash(node_id))
        .collect::<Vec<_>>();
    hashes.sort_unstable();
    hash_value(&hashes)
}

fn frontier_semantic_set_hash(
    nodes: impl Iterator<Item = GssNodeId>,
    gss: &mut GssArena,
    products: &ProductArena,
) -> u64 {
    let mut hashes = nodes
        .map(|node_id| gss.frontier_semantic_hash(node_id, products))
        .collect::<Vec<_>>();
    hashes.sort_unstable();
    hash_value(&hashes)
}

fn product_list_hash(products_list: &[ProductId], products: &ProductArena) -> u64 {
    let hashes = products_list
        .iter()
        .copied()
        .map(|product_id| {
            products.get(product_id).map_or_else(
                || hash_value(&("missing-product", product_id)),
                |product| product.semantic_hash(),
            )
        })
        .collect::<Vec<_>>();
    hash_value(&hashes)
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use indexmap::IndexSet;

    use super::checkpoint_for_column;
    use crate::component::parse::{
        data::{
            gss::GssArena,
            product::{Product, ProductArena},
        },
        parsing::ParseColumn,
    };

    #[test]
    fn product_hash_is_precomputed_and_column_checkpoint_is_cached() {
        let mut products = ProductArena::new();
        let product_id = products.insert(Product::token(7, 11, 13));
        let product = products.get(product_id).expect("token product inserted");
        assert_ne!(product.semantic_hash(), 0);

        let mut gss = GssArena::new();
        let start = gss.node(0, 0, 0);
        let shifted = gss.node(1, 0, 0);
        assert!(gss.add_edge(shifted, start, product_id, 0));

        let mut column = ParseColumn::new(Some(0), IndexSet::from([shifted]));
        assert!(column.push_product(product_id));

        let first = checkpoint_for_column(&mut column, &mut gss, &products).clone();
        let second = checkpoint_for_column(&mut column, &mut gss, &products).clone();
        assert_eq!(first, second);

        column.set_error_derived();
        let changed = checkpoint_for_column(&mut column, &mut gss, &products).clone();
        assert_ne!(first, changed);
    }

    #[test]
    fn gss_hash_cache_invalidates_touched_node_only() {
        let mut products = ProductArena::new();
        let product_id = products.insert(Product::token(3, 5, 8));

        let mut gss = GssArena::new();
        let root = gss.node(0, 0, 0);
        let branch = gss.node(1, 0, 0);
        let sibling = gss.node(2, 0, 0);
        assert!(gss.add_edge(branch, root, product_id, 0));

        let branch_before = gss.frontier_semantic_hash(branch, &products);
        let sibling_before = gss.frontier_semantic_hash(sibling, &products);

        let leaf_product = products.insert(Product::token(9, 21, 34));
        assert!(gss.add_edge(branch, sibling, leaf_product, 0));

        let branch_after = gss.frontier_semantic_hash(branch, &products);
        let sibling_after = gss.frontier_semantic_hash(sibling, &products);

        assert_ne!(branch_before, branch_after);
        assert_eq!(sibling_before, sibling_after);
    }
}
