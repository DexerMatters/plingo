use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use indexmap::IndexSet;

use crate::component::parse::{
    build::LRStateId,
    data::product::{Product, ProductArena, ProductId},
};

pub(crate) type GssNodeId = usize;
pub(crate) type GssEdgeId = usize;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct GssNode {
    pub state: LRStateId,
    pub column: usize,
    pub generation: u32,
}

impl GssNode {
    fn new(state: LRStateId, column: usize, generation: u32) -> GssNode {
        GssNode {
            state,
            column,
            generation,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct GssEdge {
    pub from: GssNodeId,
    pub to: GssNodeId,
    pub product: ProductId,
    pub generation: u32,
}

impl GssEdge {
    pub fn new(from: GssNodeId, to: GssNodeId, product: ProductId, generation: u32) -> GssEdge {
        GssEdge {
            from,
            to,
            product,
            generation,
        }
    }
}

#[derive(Clone, Default)]
struct CachedNodeHash {
    value: u64,
    outgoing_len: usize,
    dependency_key: u64,
}

#[derive(Clone, Default)]
struct GssNodeHashCache {
    frontier: Option<CachedNodeHash>,
    frontier_revision: u64,
    semantic: Option<CachedNodeHash>,
    semantic_revision: u64,
}

impl GssNodeHashCache {
    fn invalidate(&mut self) {
        self.frontier = None;
        self.semantic = None;
        self.frontier_revision = next_revision(self.frontier_revision);
        self.semantic_revision = next_revision(self.semantic_revision);
    }
}

#[derive(Clone)]
pub(crate) struct GssArena {
    nodes: IndexSet<GssNode>,
    edges: IndexSet<GssEdge>,
    edges_out: Vec<Vec<GssEdgeId>>,
    node_hashes: Vec<GssNodeHashCache>,
}

impl GssArena {
    pub fn new() -> GssArena {
        GssArena {
            nodes: IndexSet::new(),
            edges: IndexSet::new(),
            edges_out: Vec::new(),
            node_hashes: Vec::new(),
        }
    }

    pub fn node(&mut self, state: LRStateId, column: usize, generation: u32) -> GssNodeId {
        let node = GssNode::new(state, column, generation);
        let (id, inserted) = self.nodes.insert_full(node);

        if inserted {
            self.resize_edge_grid(self.nodes.len());
        }

        id
    }

    pub fn add_edge(
        &mut self,
        from: GssNodeId,
        to: GssNodeId,
        product: ProductId,
        generation: u32,
    ) -> bool {
        let edge = GssEdge::new(from, to, product, generation);
        let (edge_id, inserted) = self.edges.insert_full(edge);

        if inserted {
            self.edges_out[from].push(edge_id);
            if let Some(cache) = self.node_hashes.get_mut(from) {
                cache.invalidate();
            }
        }

        inserted
    }

    pub fn get_node(&self, id: GssNodeId) -> Option<&GssNode> {
        self.nodes.get_index(id)
    }

    pub fn get_edge(&self, id: GssEdgeId) -> Option<&GssEdge> {
        self.edges.get_index(id)
    }

    pub fn outgoing_edge_ids(&self, id: GssNodeId) -> Option<&[GssEdgeId]> {
        self.edges_out.get(id).map(Vec::as_slice)
    }

    pub fn outgoing_edges(&self, id: GssNodeId) -> impl Iterator<Item = &GssEdge> {
        self.outgoing_edge_ids(id)
            .into_iter()
            .flatten()
            .filter_map(|&edge_id| self.get_edge(edge_id))
    }

    pub(crate) fn frontier_hash(&mut self, id: GssNodeId) -> u64 {
        self.frontier_hash_with_revision(id).0
    }

    pub(crate) fn frontier_semantic_hash(&mut self, id: GssNodeId, products: &ProductArena) -> u64 {
        self.frontier_semantic_hash_with_revision(id, products).0
    }

    fn resize_edge_grid(&mut self, rows: usize) {
        if self.edges_out.len() < rows {
            self.edges_out.resize_with(rows, Vec::new);
        }
        if self.node_hashes.len() < rows {
            self.node_hashes
                .resize_with(rows, GssNodeHashCache::default);
        }
    }

    fn frontier_hash_with_revision(&mut self, id: GssNodeId) -> (u64, u64) {
        let Some(node) = self.get_node(id).cloned() else {
            return (hash_value(&("missing-gss-node", id)), 1);
        };
        let edge_ids = self
            .outgoing_edge_ids(id)
            .map_or_else(Vec::new, |edges| edges.to_vec());
        let mut parents = Vec::with_capacity(edge_ids.len());
        for edge_id in edge_ids {
            let Some(edge) = self.get_edge(edge_id).copied() else {
                continue;
            };
            let (parent_hash, parent_revision) = self.frontier_hash_with_revision(edge.to);
            parents.push((parent_hash, parent_revision));
        }
        parents.sort_unstable();
        let dependency_key = hash_value(
            &parents
                .iter()
                .map(|(_, revision)| *revision)
                .collect::<Vec<_>>(),
        );
        if let Some(cache) = self.node_hashes.get(id) {
            if let Some(cached) = &cache.frontier {
                if cached.outgoing_len == parents.len() && cached.dependency_key == dependency_key {
                    return (cached.value, cache.frontier_revision.max(1));
                }
            }
        }

        let value = hash_value(&(
            node.state,
            parents.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
        ));
        let cache = &mut self.node_hashes[id];
        cache.frontier_revision = next_revision(cache.frontier_revision);
        cache.frontier = Some(CachedNodeHash {
            value,
            outgoing_len: parents.len(),
            dependency_key,
        });
        (value, cache.frontier_revision)
    }

    fn frontier_semantic_hash_with_revision(
        &mut self,
        id: GssNodeId,
        products: &ProductArena,
    ) -> (u64, u64) {
        let Some(node) = self.get_node(id).cloned() else {
            return (hash_value(&("missing-gss-node", id)), 1);
        };
        let edge_ids = self
            .outgoing_edge_ids(id)
            .map_or_else(Vec::new, |edges| edges.to_vec());
        let mut parents = Vec::with_capacity(edge_ids.len());
        for edge_id in edge_ids {
            let Some(edge) = self.get_edge(edge_id).copied() else {
                continue;
            };
            let (parent_hash, parent_revision) =
                self.frontier_semantic_hash_with_revision(edge.to, products);
            let product_hash = products.get(edge.product).map_or_else(
                || hash_value(&("missing-product", edge.product)),
                Product::semantic_hash,
            );
            parents.push((product_hash, parent_hash, parent_revision));
        }
        parents.sort_unstable();
        let dependency_key = hash_value(
            &parents
                .iter()
                .map(|(product_hash, _, revision)| (*product_hash, *revision))
                .collect::<Vec<_>>(),
        );
        if let Some(cache) = self.node_hashes.get(id) {
            if let Some(cached) = &cache.semantic {
                if cached.outgoing_len == parents.len() && cached.dependency_key == dependency_key {
                    return (cached.value, cache.semantic_revision.max(1));
                }
            }
        }

        let value = hash_value(&(
            node.state,
            parents
                .iter()
                .map(|(product_hash, hash, _)| (*product_hash, *hash))
                .collect::<Vec<_>>(),
        ));
        let cache = &mut self.node_hashes[id];
        cache.semantic_revision = next_revision(cache.semantic_revision);
        cache.semantic = Some(CachedNodeHash {
            value,
            outgoing_len: parents.len(),
            dependency_key,
        });
        (value, cache.semantic_revision)
    }
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn next_revision(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}
