use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
};

use super::{
    data::{
        ErrorKind, GssArena, GssNodeId, ParseErrorInfo, ProductArena, ProductData, ProductId,
        TreeArena, TreeData,
    },
    grammar::Symbol,
    parsing::ParseColumn,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BoundaryCheckpoint {
    pub column_index: usize,
    pub frontier_key: u64,
    pub semantic_key: u64,
    pub accepted_key: u64,
    pub diagnostics_key: u64,
}

pub(crate) fn checkpoint_for_column(
    column_index: usize,
    column: &ParseColumn,
    gss: &GssArena,
    products: &ProductArena,
    trees: &TreeArena,
) -> BoundaryCheckpoint {
    let frontier_key = frontier_hash(column, gss);
    let accepted_key = product_list_hash(column.accepted(), products, trees);
    let diagnostics_key = hash_value(&column.diagnostics);
    let products_key = product_list_hash(&column.products, products, trees);

    BoundaryCheckpoint {
        column_index,
        frontier_key,
        semantic_key: hash_value(&(
            frontier_key,
            products_key,
            accepted_key,
            diagnostics_key,
            column.error_derived,
        )),
        accepted_key,
        diagnostics_key,
    }
}

fn frontier_hash(column: &ParseColumn, gss: &GssArena) -> u64 {
    let mut memo = HashMap::new();
    let base = frontier_set_hash(column.base_active_nodes(), gss, &mut memo);
    let active = frontier_set_hash(column.active_nodes(), gss, &mut memo);
    hash_value(&(base, active, column.error_derived))
}

fn frontier_set_hash(
    nodes: impl Iterator<Item = GssNodeId>,
    gss: &GssArena,
    memo: &mut HashMap<GssNodeId, u64>,
) -> u64 {
    let mut hashes = nodes
        .map(|node_id| frontier_node_hash(node_id, gss, memo))
        .collect::<Vec<_>>();
    hashes.sort_unstable();
    hash_value(&hashes)
}

fn frontier_node_hash(
    node_id: GssNodeId,
    gss: &GssArena,
    memo: &mut HashMap<GssNodeId, u64>,
) -> u64 {
    if let Some(&hash) = memo.get(&node_id) {
        return hash;
    }

    let hash = match gss.get_node(node_id) {
        Some(node) => {
            let mut parents = gss
                .outgoing_edges(node_id)
                .map(|edge| (edge.product, frontier_node_hash(edge.to, gss, memo)))
                .collect::<Vec<_>>();
            parents.sort_unstable();
            hash_value(&(node.state, node.column, node.generation, parents))
        }
        None => hash_value(&("missing-gss-node", node_id)),
    };

    memo.insert(node_id, hash);
    hash
}

fn product_list_hash(
    products_list: &[ProductId],
    products: &ProductArena,
    trees: &TreeArena,
) -> u64 {
    let mut memo = HashMap::new();
    let hashes = products_list
        .iter()
        .copied()
        .map(|product_id| product_hash(product_id, products, trees, &mut memo))
        .collect::<Vec<_>>();
    hash_value(&hashes)
}

fn product_hash(
    product_id: ProductId,
    products: &ProductArena,
    trees: &TreeArena,
    memo: &mut HashMap<ProductId, u64>,
) -> u64 {
    if let Some(&hash) = memo.get(&product_id) {
        return hash;
    }

    let hash = match products.get(product_id) {
        Some(product) => match (&product.data, trees.get(product.green)) {
            (
                ProductData::Token {
                    fingerprint, ty, ..
                },
                Some(tree),
            ) => match &tree.data {
                TreeData::Leaf { id } => hash_value(&("tok", id, fingerprint, ty, tree.length)),
                _ => hash_value(&("tok-tree-mismatch", product_id)),
            },
            (ProductData::Node { children, ty, .. }, Some(tree)) => match &tree.data {
                TreeData::Node { id, .. } => {
                    let child_hashes = children
                        .iter()
                        .copied()
                        .map(|child| product_hash(child, products, trees, memo))
                        .collect::<Vec<_>>();
                    hash_value(&("node", id, ty, tree.length, child_hashes))
                }
                _ => hash_value(&("node-tree-mismatch", product_id)),
            },
            (ProductData::Error, Some(tree)) => match &tree.data {
                TreeData::Error {
                    kind,
                    node,
                    unexpected,
                    expected,
                    recovered,
                    location,
                    ..
                } => hash_value(&(
                    "err",
                    error_kind_tag(kind),
                    node,
                    symbol_tag(*unexpected),
                    symbol_tag(Some(*expected)),
                    recovered,
                    location,
                    tree.length,
                )),
                _ => hash_value(&("err-tree-mismatch", product_id)),
            },
            _ => hash_value(&("missing-tree", product_id)),
        },
        None => hash_value(&("missing-product", product_id)),
    };

    memo.insert(product_id, hash);
    hash
}

fn error_kind_tag(kind: &ErrorKind) -> u8 {
    match kind {
        ErrorKind::MissingToken => 0,
        ErrorKind::UnexpectedToken => 1,
        ErrorKind::UnexpectedEndOfInput => 2,
        ErrorKind::Recovered => 3,
    }
}

fn symbol_tag(symbol: Option<Symbol>) -> (u8, u32, u32) {
    match symbol {
        Some(Symbol::T(terminal)) => (0, terminal.token_id, 0),
        Some(Symbol::N(non_terminal)) => (1, non_terminal, 0),
        Some(Symbol::Epsilon) => (2, 0, 0),
        None => (3, 0, 0),
    }
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn diagnostics_hash(diagnostics: &[ParseErrorInfo]) -> u64 {
    hash_value(diagnostics)
}
