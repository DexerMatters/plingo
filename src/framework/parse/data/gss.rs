use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::framework::parse::{
    build::LRStateId,
    data::product::{CanonicalProductKey, ProductArena, ProductId, StateProductKey},
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

/// Canonical, identity-free shape of one GSS node and its predecessor edges.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CanonicalGssNodeKey {
    pub(crate) state: LRStateId,
    pub(crate) edges: Arc<[CanonicalGssEdgeKey]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CanonicalGssEdgeKey {
    pub(crate) to: Arc<CanonicalGssNodeKey>,
    pub(crate) product: CanonicalProductKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CanonicalFrontierKey {
    pub(crate) base: Arc<[CanonicalGssNodeKey]>,
    pub(crate) active: Arc<[CanonicalGssNodeKey]>,
}
#[derive(Clone, PartialEq, Eq)]
struct FingerprintedKey<K> {
    fingerprint: u64,
    key: K,
}

impl<K> Hash for FingerprintedKey<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

type FingerprintedNodeKey = FingerprintedKey<CanonicalGssNodeKey>;
type FingerprintedProductKey = FingerprintedKey<CanonicalProductKey>;

#[derive(Default)]
pub(crate) struct CanonicalFrontierCache {
    nodes: HashMap<GssNodeId, CanonicalGssNodeKey>,
    node_fingerprints: HashMap<GssNodeId, u64>,
    products: HashMap<ProductId, CanonicalProductKey>,
    product_fingerprints: HashMap<ProductId, u64>,
    /// State-level abstractions for suffix convergence proofs. Many products
    /// share one key by design, so no ambiguity is tracked here.
    state_nodes: HashMap<GssNodeId, Arc<StateNodeKey>>,
    state_product_keys: HashMap<ProductId, StateProductKey>,
}

/// Edge payload of a [`StateNodeKey`]: the predecessor's state key plus the
/// structurally abstracted reduction product.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct StateEdgeKey {
    pub(crate) to: Arc<StateNodeKey>,
    pub(crate) product: Arc<StateProductKey>,
}

/// LR configuration of one GSS node: its automaton state plus the abstract
/// structure of every reduction path that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct StateNodeKey {
    pub(crate) state: LRStateId,
    pub(crate) edges: Arc<[StateEdgeKey]>,
}

impl GssArena {
    /// Matches two frontiers at the LR-state level and returns the product
    /// correspondence the seam binding needs (plan §5.6).
    ///
    /// Nodes pair when their automaton states are equal and their outgoing
    /// edges pair one-to-one under [`StateProductKey`] abstraction; edge
    /// products then map old to new by position. Token products pair across
    /// terminal kinds — the shifted node's state already encodes the token
    /// class — which is exactly what lets a replaced value converge.
    /// Structural divergence anywhere rejects the match; the caller then
    /// replays conservatively.
    pub(crate) fn match_state_frontiers(
        &self,
        old_active: &[GssNodeId],
        new_active: &[GssNodeId],
        products: &ProductArena,
        trees: &crate::framework::parse::data::green::TreeArena,
        cache: &mut CanonicalFrontierCache,
    ) -> Option<HashMap<ProductId, ProductId>> {
        if old_active.len() != new_active.len() {
            return None;
        }
        let mut map: HashMap<ProductId, ProductId> = HashMap::new();
        let mut seen: HashSet<(GssNodeId, GssNodeId)> = HashSet::new();
        let mut stack: Vec<(GssNodeId, GssNodeId)> = Vec::new();
        // Frontier node sets are insertion-ordered; pair roots by their
        // sorted structural keys so parse-order differences never reject an
        // otherwise equal configuration.
        let mut pair_roots = |old_roots: &[GssNodeId],
                              new_roots: &[GssNodeId]|
         -> Option<Vec<(GssNodeId, GssNodeId)>> {
            let mut old_keys: Vec<(Arc<StateNodeKey>, GssNodeId)> = old_roots
                .iter()
                .map(|&id| {
                    Some((
                        cache.state_nodes.get(&id).cloned().unwrap_or_else(|| {
                            self.state_node_key(id, products, trees, cache, &mut HashSet::new())
                                .expect("state key for paired root")
                        }),
                        id,
                    ))
                })
                .collect::<Option<Vec<_>>>()?;
            let mut new_keys: Vec<(Arc<StateNodeKey>, GssNodeId)> = new_roots
                .iter()
                .map(|&id| {
                    Some((
                        cache.state_nodes.get(&id).cloned().unwrap_or_else(|| {
                            self.state_node_key(id, products, trees, cache, &mut HashSet::new())
                                .expect("state key for paired root")
                        }),
                        id,
                    ))
                })
                .collect::<Option<Vec<_>>>()?;
            if old_keys.len() != new_keys.len() {
                return None;
            }
            old_keys.sort_unstable();
            new_keys.sort_unstable();
            Some(
                old_keys
                    .into_iter()
                    .zip(new_keys)
                    .map(|((_, old_id), (_, new_id))| (old_id, new_id))
                    .collect(),
            )
        };
        for pair in pair_roots(old_active, new_active)?.into_iter() {
            stack.push(pair);
        }
        while let Some((old_id, new_id)) = stack.pop() {
            if !seen.insert((old_id, new_id)) {
                continue;
            }
            let old_node = self.get_node(old_id)?;
            let new_node = self.get_node(new_id)?;
            if old_node.state != new_node.state {
                return None;
            }
            let mut old_edges: Vec<(StateEdgeKey, GssNodeId, ProductId)> = Vec::new();
            for edge in self.outgoing_edges(old_id) {
                let product_key = match cache.state_product_keys.get(&edge.product) {
                    Some(key) => key.clone(),
                    None => products.structural_key(
                        trees,
                        edge.product,
                        &mut cache.state_product_keys,
                        &mut HashSet::new(),
                    )?,
                };
                let to_key = match cache.state_nodes.get(&edge.to) {
                    Some(key) => Arc::clone(key),
                    None => {
                        self.state_node_key(edge.to, products, trees, cache, &mut HashSet::new())?
                    }
                };
                old_edges.push((
                    StateEdgeKey {
                        to: to_key,
                        product: Arc::new(product_key),
                    },
                    edge.to,
                    edge.product,
                ));
            }
            let mut new_edges: Vec<(StateEdgeKey, GssNodeId, ProductId)> = Vec::new();
            for edge in self.outgoing_edges(new_id) {
                let product_key = match cache.state_product_keys.get(&edge.product) {
                    Some(key) => key.clone(),
                    None => products.structural_key(
                        trees,
                        edge.product,
                        &mut cache.state_product_keys,
                        &mut HashSet::new(),
                    )?,
                };
                let to_key = match cache.state_nodes.get(&edge.to) {
                    Some(key) => Arc::clone(key),
                    None => {
                        self.state_node_key(edge.to, products, trees, cache, &mut HashSet::new())?
                    }
                };
                new_edges.push((
                    StateEdgeKey {
                        to: to_key,
                        product: Arc::new(product_key),
                    },
                    edge.to,
                    edge.product,
                ));
            }
            old_edges.sort_unstable();
            new_edges.sort_unstable();
            if old_edges.len() != new_edges.len() {
                return None;
            }
            for (old_edge, new_edge) in old_edges.iter().zip(new_edges.iter()) {
                if old_edge.0 != new_edge.0 {
                    return None;
                }
                if old_edge.2 != new_edge.2 {
                    map.insert(old_edge.2, new_edge.2);
                }
                stack.push((old_edge.1, new_edge.1));
            }
        }
        Some(map)
    }

    /// Computes one node's [`StateNodeKey`], memoized in `cache`.
    fn state_node_key(
        &self,
        id: GssNodeId,
        products: &ProductArena,
        trees: &crate::framework::parse::data::green::TreeArena,
        cache: &mut CanonicalFrontierCache,
        active: &mut HashSet<GssNodeId>,
    ) -> Option<Arc<StateNodeKey>> {
        if let Some(key) = cache.state_nodes.get(&id) {
            return Some(Arc::clone(key));
        }
        if !active.insert(id) {
            return None;
        }
        let state = self.get_node(id)?.state;
        let mut edges = Vec::new();
        for edge in self.outgoing_edges(id) {
            let product_key = match cache.state_product_keys.get(&edge.product) {
                Some(key) => key.clone(),
                None => products.structural_key(
                    trees,
                    edge.product,
                    &mut cache.state_product_keys,
                    &mut HashSet::new(),
                )?,
            };
            // Epsilon reductions create self-edges; representing the target
            // with a state sentinel keeps the key computable without
            // recursing into the cycle.
            let to_key = if edge.to == id {
                Arc::new(StateNodeKey {
                    state: usize::MAX,
                    edges: Arc::from([]),
                })
            } else if let Some(key) = cache.state_nodes.get(&edge.to) {
                Arc::clone(key)
            } else {
                self.state_node_key(edge.to, products, trees, cache, active)?
            };
            edges.push(StateEdgeKey {
                to: to_key,
                product: Arc::new(product_key),
            });
        }
        edges.sort_unstable();
        let key = Arc::new(StateNodeKey {
            state,
            edges: edges.into(),
        });
        cache.state_nodes.insert(id, Arc::clone(&key));
        Some(key)
    }
}

pub(crate) struct CanonicalFrontier {
    pub(crate) key: CanonicalFrontierKey,
    pub(crate) fingerprint: u64,
    pub(crate) node_ids: HashMap<FingerprintedNodeKey, GssNodeId>,
    pub(crate) product_ids: HashMap<FingerprintedProductKey, ProductId>,
    pub(crate) ambiguous: bool,
}

#[derive(Clone)]
pub(crate) struct GssArena {
    /// Frozen GSS node and edge generations. IDs remain offsets in these
    /// append-only sequences, while each mutable tail belongs to one parser
    /// transaction.
    node_chunks: Arc<Vec<Arc<[GssNode]>>>,
    node_starts: Arc<Vec<usize>>,
    node_indexes: Arc<Vec<Arc<HashMap<GssNode, GssNodeId>>>>,
    node_tail: Vec<GssNode>,
    node_tail_index: HashMap<GssNode, GssNodeId>,
    node_len: usize,
    edge_chunks: Arc<Vec<Arc<[GssEdge]>>>,
    edge_starts: Arc<Vec<usize>>,
    edge_indexes: Arc<Vec<Arc<HashMap<GssEdge, GssEdgeId>>>>,
    edge_tail: Vec<GssEdge>,
    edge_tail_index: HashMap<GssEdge, GssEdgeId>,
    edge_len: usize,
    /// The latest outgoing-edge list for each changed node. Frozen maps are
    /// searched newest-first, so adding an edge never rewrites old lists.
    outgoing_layers: Arc<Vec<Arc<HashMap<GssNodeId, Arc<[GssEdgeId]>>>>>,
    outgoing_tail: HashMap<GssNodeId, Vec<GssEdgeId>>,
}

impl GssArena {
    pub fn new() -> GssArena {
        GssArena {
            node_chunks: Arc::new(Vec::new()),
            node_starts: Arc::new(Vec::new()),
            node_indexes: Arc::new(Vec::new()),
            node_tail: Vec::new(),
            node_tail_index: HashMap::new(),
            node_len: 0,
            edge_chunks: Arc::new(Vec::new()),
            edge_starts: Arc::new(Vec::new()),
            edge_indexes: Arc::new(Vec::new()),
            edge_tail: Vec::new(),
            edge_tail_index: HashMap::new(),
            edge_len: 0,
            outgoing_layers: Arc::new(Vec::new()),
            outgoing_tail: HashMap::new(),
        }
    }

    pub fn node(&mut self, state: LRStateId, column: usize, generation: u32) -> GssNodeId {
        let node = GssNode::new(state, column, generation);
        if let Some(&id) = self.node_tail_index.get(&node) {
            return id;
        }
        for index in self.node_indexes.iter().rev() {
            if let Some(&id) = index.get(&node) {
                return id;
            }
        }
        let id = self.node_len;
        self.node_tail_index.insert(node.clone(), id);
        self.node_tail.push(node);
        self.node_len = self.node_len.saturating_add(1);
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
        if self.edge_tail_index.contains_key(&edge)
            || self
                .edge_indexes
                .iter()
                .rev()
                .any(|index| index.contains_key(&edge))
        {
            return false;
        }
        let edge_id = self.edge_len;
        self.edge_tail_index.insert(edge, edge_id);
        self.edge_tail.push(edge);
        self.edge_len = self.edge_len.saturating_add(1);
        let mut outgoing = self
            .outgoing_tail
            .remove(&from)
            .or_else(|| self.outgoing_edge_ids(from).map(<[GssEdgeId]>::to_vec))
            .unwrap_or_default();
        outgoing.push(edge_id);
        self.outgoing_tail.insert(from, outgoing);
        true
    }

    pub fn get_node(&self, id: GssNodeId) -> Option<&GssNode> {
        let sealed_len = self.node_len.saturating_sub(self.node_tail.len());
        if id >= sealed_len {
            return self.node_tail.get(id - sealed_len);
        }
        let index = self
            .node_starts
            .partition_point(|&start| start <= id)
            .checked_sub(1)?;
        let start = self.node_starts[index];
        self.node_chunks[index].get(id - start)
    }

    pub(crate) fn node_count(&self) -> usize {
        self.node_len
    }

    pub(crate) fn edge_count(&self) -> usize {
        self.edge_len
    }

    pub fn get_edge(&self, id: GssEdgeId) -> Option<&GssEdge> {
        let sealed_len = self.edge_len.saturating_sub(self.edge_tail.len());
        if id >= sealed_len {
            return self.edge_tail.get(id - sealed_len);
        }
        let index = self
            .edge_starts
            .partition_point(|&start| start <= id)
            .checked_sub(1)?;
        let start = self.edge_starts[index];
        self.edge_chunks[index].get(id - start)
    }

    pub fn outgoing_edge_ids(&self, id: GssNodeId) -> Option<&[GssEdgeId]> {
        if let Some(outgoing) = self.outgoing_tail.get(&id) {
            return Some(outgoing.as_slice());
        }
        self.outgoing_layers
            .iter()
            .rev()
            .find_map(|layer| layer.get(&id).map(AsRef::as_ref))
    }

    pub fn outgoing_edges(&self, id: GssNodeId) -> impl Iterator<Item = &GssEdge> {
        self.outgoing_edge_ids(id)
            .into_iter()
            .flatten()
            .filter_map(|&edge_id| self.get_edge(edge_id))
    }

    /// Publishes the current append-only GSS generation and changed outgoing
    /// lists. The old records and directories stay shared by prior roots.
    pub(crate) fn seal_generation(&mut self) {
        if !self.node_tail.is_empty() {
            let start = self.node_len - self.node_tail.len();
            let nodes: Arc<[GssNode]> = std::mem::take(&mut self.node_tail).into();
            let index = std::mem::take(&mut self.node_tail_index);
            let mut chunks = self.node_chunks.as_ref().clone();
            chunks.push(nodes);
            self.node_chunks = Arc::new(chunks);
            let mut starts = self.node_starts.as_ref().clone();
            starts.push(start);
            self.node_starts = Arc::new(starts);
            let mut indexes = self.node_indexes.as_ref().clone();
            indexes.push(Arc::new(index));
            self.node_indexes = Arc::new(indexes);
        }
        if !self.edge_tail.is_empty() {
            let start = self.edge_len - self.edge_tail.len();
            let edges: Arc<[GssEdge]> = std::mem::take(&mut self.edge_tail).into();
            let index = std::mem::take(&mut self.edge_tail_index);
            let mut chunks = self.edge_chunks.as_ref().clone();
            chunks.push(edges);
            self.edge_chunks = Arc::new(chunks);
            let mut starts = self.edge_starts.as_ref().clone();
            starts.push(start);
            self.edge_starts = Arc::new(starts);
            let mut indexes = self.edge_indexes.as_ref().clone();
            indexes.push(Arc::new(index));
            self.edge_indexes = Arc::new(indexes);
        }
        if !self.outgoing_tail.is_empty() {
            let pending = std::mem::take(&mut self.outgoing_tail)
                .into_iter()
                .map(|(id, ids)| (id, Arc::<[GssEdgeId]>::from(ids)))
                .collect::<HashMap<_, _>>();
            let mut layers = self.outgoing_layers.as_ref().clone();
            layers.push(Arc::new(pending));
            self.outgoing_layers = Arc::new(layers);
        }
    }
}

impl GssArena {
    /// Builds an identity-free canonical key for one frontier pair. Kept for
    /// tests and one-off callers; replay passes a transaction-local cache.
    pub(crate) fn canonical_frontier(
        &self,
        sets: (&[GssNodeId], &[GssNodeId]),
        products: &ProductArena,
    ) -> Option<CanonicalFrontier> {
        let mut cache = CanonicalFrontierCache::default();
        self.canonical_frontier_cached(sets, products, &mut cache)
    }

    pub(crate) fn canonical_frontier_cached(
        &self,
        sets: (&[GssNodeId], &[GssNodeId]),
        products: &ProductArena,
        cache: &mut CanonicalFrontierCache,
    ) -> Option<CanonicalFrontier> {
        struct Canonicalizer<'a> {
            arena: &'a GssArena,
            products: &'a ProductArena,
            cache: &'a mut CanonicalFrontierCache,
            active_nodes: HashSet<GssNodeId>,
            node_ids: HashMap<FingerprintedNodeKey, GssNodeId>,
            product_ids: HashMap<FingerprintedProductKey, ProductId>,
            ambiguous: bool,
        }

        impl Canonicalizer<'_> {
            fn product_info(&mut self, id: ProductId) -> Option<(CanonicalProductKey, u64)> {
                let key = if let Some(key) = self.cache.products.get(&id) {
                    key.clone()
                } else {
                    self.products.canonical_key(
                        id,
                        &mut self.cache.products,
                        &mut HashSet::new(),
                    )?
                };
                let fingerprint =
                    if let Some(fingerprint) = self.cache.product_fingerprints.get(&id) {
                        *fingerprint
                    } else {
                        self.products.canonical_fingerprint(
                            id,
                            &mut self.cache.product_fingerprints,
                            &mut HashSet::new(),
                        )?
                    };
                if let Some(previous) = self.product_ids.insert(
                    FingerprintedKey {
                        fingerprint,
                        key: key.clone(),
                    },
                    id,
                ) && previous != id
                {
                    self.ambiguous = true;
                }
                Some((key, fingerprint))
            }

            fn node_info(&mut self, id: GssNodeId) -> Option<(CanonicalGssNodeKey, u64)> {
                let mut work = vec![(id, false)];
                while let Some((current, expanded)) = work.pop() {
                    if let Some(key) = self.cache.nodes.get(&current).cloned() {
                        let fingerprint = *self.cache.node_fingerprints.get(&current)?;
                        if let Some(previous) = self.node_ids.insert(
                            FingerprintedKey {
                                fingerprint,
                                key: key.clone(),
                            },
                            current,
                        ) && previous != current
                        {
                            self.ambiguous = true;
                        }
                        continue;
                    }
                    if expanded {
                        let state = self.arena.get_node(current)?.state;
                        let outgoing: Vec<_> = self
                            .arena
                            .outgoing_edges(current)
                            .map(|edge| (edge.to, edge.product))
                            .collect();
                        let mut edges = Vec::with_capacity(outgoing.len());
                        let mut edge_fingerprints = Vec::with_capacity(outgoing.len());
                        for (to, product) in outgoing {
                            let to_key = self.cache.nodes.get(&to)?.clone();
                            let to_fingerprint = *self.cache.node_fingerprints.get(&to)?;
                            let product_key = self.cache.products.get(&product)?.clone();
                            let product_fingerprint =
                                *self.cache.product_fingerprints.get(&product)?;
                            edges.push(CanonicalGssEdgeKey {
                                to: Arc::new(to_key),
                                product: product_key,
                            });
                            edge_fingerprints.push((to_fingerprint, product_fingerprint));
                        }
                        edges.sort_unstable();
                        edge_fingerprints.sort_unstable();
                        let key = CanonicalGssNodeKey {
                            state,
                            edges: edges.into(),
                        };
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        state.hash(&mut hasher);
                        edge_fingerprints.hash(&mut hasher);
                        let fingerprint = hasher.finish();
                        self.active_nodes.remove(&current);
                        self.cache.nodes.insert(current, key.clone());
                        self.cache.node_fingerprints.insert(current, fingerprint);
                        if let Some(previous) = self.node_ids.insert(
                            FingerprintedKey {
                                fingerprint,
                                key: key.clone(),
                            },
                            current,
                        ) && previous != current
                        {
                            self.ambiguous = true;
                        }
                        continue;
                    }
                    if !self.active_nodes.insert(current) {
                        return None;
                    }
                    let outgoing: Vec<_> = self
                        .arena
                        .outgoing_edges(current)
                        .map(|edge| (edge.to, edge.product))
                        .collect();
                    for &(_, product) in &outgoing {
                        self.product_info(product)?;
                    }
                    work.push((current, true));
                    for &(to, _) in outgoing.iter().rev() {
                        if !self.cache.nodes.contains_key(&to) {
                            if self.active_nodes.contains(&to) {
                                return None;
                            }
                            work.push((to, false));
                        }
                    }
                }
                let key = self.cache.nodes.get(&id)?.clone();
                let fingerprint = *self.cache.node_fingerprints.get(&id)?;
                Some((key, fingerprint))
            }

            fn root_set(
                &mut self,
                roots: &[GssNodeId],
            ) -> Option<(Arc<[CanonicalGssNodeKey]>, Vec<u64>)> {
                let mut entries = roots
                    .iter()
                    .map(|&id| {
                        self.node_info(id)
                            .map(|(key, fingerprint)| (key, fingerprint))
                    })
                    .collect::<Option<Vec<_>>>()?;
                entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
                if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                    self.ambiguous = true;
                }
                let keys = entries
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                let fingerprints = entries
                    .iter()
                    .map(|(_, fingerprint)| *fingerprint)
                    .collect();
                Some((keys.into(), fingerprints))
            }
        }

        let mut canonicalizer = Canonicalizer {
            arena: self,
            products,
            cache,
            active_nodes: HashSet::new(),
            node_ids: HashMap::new(),
            product_ids: HashMap::new(),
            ambiguous: false,
        };
        let (base, base_fingerprints) = canonicalizer.root_set(sets.0)?;
        let (active, active_fingerprints) = canonicalizer.root_set(sets.1)?;
        let mut fingerprint_hasher = std::collections::hash_map::DefaultHasher::new();
        base_fingerprints.hash(&mut fingerprint_hasher);
        active_fingerprints.hash(&mut fingerprint_hasher);
        Some(CanonicalFrontier {
            key: CanonicalFrontierKey { base, active },
            fingerprint: fingerprint_hasher.finish(),
            node_ids: canonicalizer.node_ids,
            product_ids: canonicalizer.product_ids,
            ambiguous: canonicalizer.ambiguous,
        })
    }

    pub(crate) fn match_canonical_frontiers(
        &self,
        old_sets: (&[GssNodeId], &[GssNodeId]),
        new_sets: (&[GssNodeId], &[GssNodeId]),
        products: &ProductArena,
    ) -> Option<(
        HashMap<GssNodeId, GssNodeId>,
        HashMap<ProductId, ProductId>,
        bool,
    )> {
        let mut cache = CanonicalFrontierCache::default();
        self.match_canonical_frontiers_cached(old_sets, new_sets, products, &mut cache)
    }

    pub(crate) fn match_canonical_frontiers_cached(
        &self,
        old_sets: (&[GssNodeId], &[GssNodeId]),
        new_sets: (&[GssNodeId], &[GssNodeId]),
        products: &ProductArena,
        cache: &mut CanonicalFrontierCache,
    ) -> Option<(
        HashMap<GssNodeId, GssNodeId>,
        HashMap<ProductId, ProductId>,
        bool,
    )> {
        let old = self.canonical_frontier_cached(old_sets, products, cache)?;
        let new = self.canonical_frontier_cached(new_sets, products, cache)?;
        if old.ambiguous || new.ambiguous || old.key != new.key {
            return None;
        }
        let mut nodes = HashMap::with_capacity(old.node_ids.len());
        for (key, old_id) in old.node_ids {
            nodes.insert(old_id, *new.node_ids.get(&key)?);
        }
        let mut products_map = HashMap::with_capacity(old.product_ids.len());
        for (key, old_id) in old.product_ids {
            products_map.insert(old_id, *new.product_ids.get(&key)?);
        }
        let shared = nodes.iter().any(|(old, new)| old == new)
            || products_map.iter().any(|(old, new)| old == new);
        Some((nodes, products_map, shared))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::parse::data::product::{Product, ProductArena};

    #[test]
    fn canonical_frontier_ignores_node_ids_and_edge_order() {
        let mut products = ProductArena::new();
        let first = products.insert(Product::token(1, 7));
        let second = products.insert(Product::token(2, 8));
        let mut gss = GssArena::new();

        let old_root = gss.node(1, 10, 0);
        let old_first = gss.node(2, 0, 0);
        let old_second = gss.node(3, 0, 0);
        gss.add_edge(old_root, old_first, first, 0);
        gss.add_edge(old_root, old_second, second, 0);

        let new_root = gss.node(1, 11, 0);
        let new_first = gss.node(2, 1, 0);
        let new_second = gss.node(3, 1, 0);
        gss.add_edge(new_root, new_second, second, 0);
        gss.add_edge(new_root, new_first, first, 0);

        let old = gss
            .canonical_frontier((&[old_root], &[]), &products)
            .expect("old frontier");
        let new = gss
            .canonical_frontier((&[new_root], &[]), &products)
            .expect("new frontier");
        assert_eq!(old.key, new.key);
        assert!(!old.ambiguous);
        assert!(!new.ambiguous);
        assert_eq!(
            gss.match_canonical_frontiers((&[old_root], &[]), (&[new_root], &[]), &products)
                .expect("canonical match")
                .0
                .get(&old_root),
            Some(&new_root)
        );
    }

    #[test]
    fn equivalent_frontier_roots_are_conservatively_ambiguous() {
        let mut products = ProductArena::new();
        let token = products.insert(Product::token(1, 7));
        let mut gss = GssArena::new();
        let child = gss.node(2, 0, 0);
        let first_root = gss.node(1, 10, 0);
        let second_root = gss.node(1, 11, 0);
        gss.add_edge(first_root, child, token, 0);
        gss.add_edge(second_root, child, token, 0);

        let frontier = gss
            .canonical_frontier((&[first_root, second_root], &[]), &products)
            .expect("frontier");
        assert!(frontier.ambiguous);
        assert!(
            gss.match_canonical_frontiers(
                (&[first_root, second_root], &[]),
                (&[first_root, second_root], &[]),
                &products
            )
            .is_none()
        );
    }
}
