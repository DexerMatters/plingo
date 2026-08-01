use std::collections::HashMap;

use indexmap::IndexSet;

use crate::component::parse::{build::LRStateId, data::product::ProductId};

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

#[derive(Clone)]
pub(crate) struct GssArena {
    nodes: IndexSet<GssNode>,
    edges: IndexSet<GssEdge>,
    edges_out: Vec<Vec<GssEdgeId>>,
}

impl GssArena {
    pub fn new() -> GssArena {
        GssArena {
            nodes: IndexSet::new(),
            edges: IndexSet::new(),
            edges_out: Vec::new(),
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
        }

        inserted
    }

    pub fn get_node(&self, id: GssNodeId) -> Option<&GssNode> {
        self.nodes.get_index(id)
    }

    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
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

    pub(crate) fn match_frontiers(
        &self,
        old_sets: (&[GssNodeId], &[GssNodeId]),
        new_sets: (&[GssNodeId], &[GssNodeId]),
    ) -> Option<(
        HashMap<GssNodeId, GssNodeId>,
        HashMap<ProductId, ProductId>,
        bool,
    )> {
        #[derive(Default)]
        struct Mapping {
            nodes: HashMap<GssNodeId, GssNodeId>,
            nodes_rev: HashMap<GssNodeId, GssNodeId>,
            products: HashMap<ProductId, ProductId>,
            products_rev: HashMap<ProductId, ProductId>,
            node_log: Vec<(GssNodeId, GssNodeId)>,
            product_log: Vec<(ProductId, ProductId)>,
            shared_prefixes: usize,
        }

        impl Mapping {
            fn mark(&self) -> (usize, usize, usize) {
                (
                    self.node_log.len(),
                    self.product_log.len(),
                    self.shared_prefixes,
                )
            }

            fn rollback(&mut self, mark: (usize, usize, usize)) {
                while self.node_log.len() > mark.0 {
                    let (old, new) = self.node_log.pop().expect("node mapping log");
                    self.nodes.remove(&old);
                    self.nodes_rev.remove(&new);
                }
                while self.product_log.len() > mark.1 {
                    let (old, new) = self.product_log.pop().expect("product mapping log");
                    self.products.remove(&old);
                    self.products_rev.remove(&new);
                }
                self.shared_prefixes = mark.2;
            }

            fn bind_node(&mut self, old: GssNodeId, new: GssNodeId) -> bool {
                if self.nodes.get(&old).is_some_and(|&mapped| mapped != new)
                    || self
                        .nodes_rev
                        .get(&new)
                        .is_some_and(|&mapped| mapped != old)
                {
                    return false;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = self.nodes.entry(old) {
                    e.insert(new);
                    self.nodes_rev.insert(new, old);
                    self.node_log.push((old, new));
                }
                true
            }

            fn bind_product(&mut self, old: ProductId, new: ProductId) -> bool {
                if self.products.get(&old).is_some_and(|&mapped| mapped != new)
                    || self
                        .products_rev
                        .get(&new)
                        .is_some_and(|&mapped| mapped != old)
                {
                    return false;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = self.products.entry(old) {
                    e.insert(new);
                    self.products_rev.insert(new, old);
                    self.product_log.push((old, new));
                }
                true
            }
        }

        fn match_node(
            arena: &GssArena,
            old: GssNodeId,
            new: GssNodeId,
            mapping: &mut Mapping,
        ) -> bool {
            if let Some(&mapped) = mapping.nodes.get(&old) {
                return mapped == new;
            }
            if old == new {
                mapping.shared_prefixes += 1;
                return mapping.bind_node(old, new);
            }
            let Some((old_node, new_node)) = arena.get_node(old).zip(arena.get_node(new)) else {
                return false;
            };
            if old_node.state != new_node.state
                || mapping
                    .nodes_rev
                    .get(&new)
                    .is_some_and(|&mapped| mapped != old)
            {
                return false;
            }

            let old_edges = arena.outgoing_edges(old).copied().collect::<Vec<_>>();
            let new_edges = arena.outgoing_edges(new).copied().collect::<Vec<_>>();
            if old_edges.len() != new_edges.len() {
                return false;
            }

            let mark = mapping.mark();
            mapping.bind_node(old, new);

            fn match_edges(
                arena: &GssArena,
                old: &[GssEdge],
                new: &[GssEdge],
                index: usize,
                used: &mut [bool],
                mapping: &mut Mapping,
            ) -> bool {
                if index == old.len() {
                    return true;
                }
                let shared = new.iter().position(|edge| {
                    edge.to == old[index].to && edge.product == old[index].product
                });
                for (order, candidate) in shared.into_iter().chain(0..new.len()).enumerate() {
                    if order > 0 && shared == Some(candidate) {
                        continue;
                    }
                    if used[candidate] {
                        continue;
                    }
                    let mark = mapping.mark();
                    if !mapping.bind_product(old[index].product, new[candidate].product)
                        || !match_node(arena, old[index].to, new[candidate].to, mapping)
                    {
                        mapping.rollback(mark);
                        continue;
                    }
                    used[candidate] = true;
                    if match_edges(arena, old, new, index + 1, used, mapping) {
                        return true;
                    }
                    used[candidate] = false;
                    mapping.rollback(mark);
                }
                false
            }

            if match_edges(
                arena,
                &old_edges,
                &new_edges,
                0,
                &mut vec![false; new_edges.len()],
                mapping,
            ) {
                true
            } else {
                mapping.rollback(mark);
                false
            }
        }

        fn match_set(
            arena: &GssArena,
            old: &[GssNodeId],
            new: &[GssNodeId],
            index: usize,
            used: &mut [bool],
            mapping: &mut Mapping,
        ) -> bool {
            if old.len() != new.len() {
                return false;
            }
            if index == old.len() {
                return true;
            }
            for candidate in 0..new.len() {
                if used[candidate] {
                    continue;
                }
                let mark = mapping.mark();
                if !match_node(arena, old[index], new[candidate], mapping) {
                    mapping.rollback(mark);
                    continue;
                }
                used[candidate] = true;
                if match_set(arena, old, new, index + 1, used, mapping) {
                    return true;
                }
                used[candidate] = false;
                mapping.rollback(mark);
            }
            false
        }

        let mut mapping = Mapping::default();
        if !match_set(
            self,
            old_sets.0,
            new_sets.0,
            0,
            &mut vec![false; new_sets.0.len()],
            &mut mapping,
        ) || !match_set(
            self,
            old_sets.1,
            new_sets.1,
            0,
            &mut vec![false; new_sets.1.len()],
            &mut mapping,
        ) {
            return None;
        }
        Some((mapping.nodes, mapping.products, mapping.shared_prefixes > 0))
    }

    fn resize_edge_grid(&mut self, rows: usize) {
        if self.edges_out.len() < rows {
            self.edges_out.resize_with(rows, Vec::new);
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/component_parse_gss.rs"]
mod tests;
