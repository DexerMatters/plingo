use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::framework::parse::data::{
    ast::{AnchoredSpan, AstBox, AstId, TokenEntryId},
    green::GreenId,
};

pub type ProductId = usize;

/// Parser-computed metadata shared by every projection of a committed parse.
/// Both fields are assembled at shift/reduction time; snapshot publication does
/// not need to walk child products to rediscover AST reachability or spans.
#[derive(Clone)]
pub struct Product {
    pub green: GreenId,
    pub data: ProductData,
    pub extent: AnchoredSpan,
    pub ast_ids: Arc<[AstId]>,
}

impl Product {
    pub fn new(green: GreenId, data: ProductData) -> Self {
        Self {
            green,
            data,
            extent: AnchoredSpan::point(0),
            ast_ids: Arc::from([]),
        }
    }

    pub fn with_metadata(mut self, extent: AnchoredSpan, ast_ids: impl Into<Arc<[AstId]>>) -> Self {
        self.extent = extent;
        self.ast_ids = ast_ids.into();
        self
    }

    pub fn error(green: GreenId) -> Self {
        Self::new(
            green,
            ProductData::Error {
                children: Vec::new(),
            },
        )
    }

    pub fn error_with_children(green: GreenId, children: Vec<ProductId>) -> Self {
        Self::new(green, ProductData::Error { children })
    }

    pub fn token(green: GreenId, entry: TokenEntryId) -> Self {
        Self::new(
            green,
            ProductData::Token {
                entry,
                ast: None,
                ty: TypeId::of::<()>(),
            },
        )
    }

    pub fn typed_token<T: 'static>(green: GreenId, entry: TokenEntryId, ast: AstBox<T>) -> Self {
        Self::new(
            green,
            ProductData::Token {
                entry,
                ast: Some(ast.raw_id()),
                ty: TypeId::of::<T>(),
            },
        )
    }

    pub fn node<T: 'static>(green: GreenId, ast: AstBox<T>, children: Vec<ProductId>) -> Self {
        Self::new(
            green,
            ProductData::Node {
                ast: ast.raw_id(),
                ty: TypeId::of::<T>(),
                children,
            },
        )
    }
}
/// Content-addressed product shape used for exact frontier comparison.
///
/// AST record ids and byte extents are deliberately absent: they belong to
/// publication/layout domains, while this key proves parser equivalence.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CanonicalProductKey {
    Error {
        green: GreenId,
        children: Arc<[CanonicalProductKey]>,
    },
    Token {
        green: GreenId,
        entry: TokenEntryId,
        typed: bool,
    },
    Node {
        green: GreenId,
        children: Arc<[CanonicalProductKey]>,
    },
}

#[derive(Debug, Clone)]
pub enum ProductData {
    Error {
        children: Vec<ProductId>,
    },
    Token {
        entry: TokenEntryId,
        ast: Option<AstId>,
        ty: TypeId,
    },
    Node {
        ast: AstId,
        ty: TypeId,
        children: Vec<ProductId>,
    },
}

#[derive(Clone)]
pub struct ProductArena {
    /// Frozen product generations. A generation is immutable once published;
    /// roots and snapshots therefore share these chunks without cloning the
    /// accumulated product vector.
    chunks: Arc<Vec<Arc<[Product]>>>,
    chunk_starts: Arc<Vec<usize>>,
    /// Products allocated by the current transaction only.
    tail: Vec<Product>,
    total_len: usize,
}

impl Default for ProductArena {
    fn default() -> Self {
        Self::new()
    }
}

impl ProductArena {
    pub fn new() -> Self {
        Self {
            chunks: Arc::new(Vec::new()),
            chunk_starts: Arc::new(Vec::new()),
            tail: Vec::new(),
            total_len: 0,
        }
    }

    pub fn insert(&mut self, product: Product) -> ProductId {
        let id = self.total_len;
        self.tail.push(product);
        self.total_len = self.total_len.saturating_add(1);
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.total_len
    }

    pub fn get(&self, id: ProductId) -> Option<&Product> {
        let sealed_len = self.total_len.saturating_sub(self.tail.len());
        if id >= sealed_len {
            return self.tail.get(id - sealed_len);
        }
        let index = self
            .chunk_starts
            .partition_point(|&start| start <= id)
            .checked_sub(1)?;
        let start = self.chunk_starts[index];
        self.chunks[index].get(id - start)
    }

    /// Freezes the current append-only generation. Only the small generation
    /// directory is copied; product records remain in an immutable `Arc` slice.
    pub(crate) fn seal_generation(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        let start = self.total_len - self.tail.len();
        let chunk: Arc<[Product]> = std::mem::take(&mut self.tail).into();
        let mut chunks = self.chunks.as_ref().clone();
        chunks.push(chunk);
        self.chunks = Arc::new(chunks);
        let mut starts = self.chunk_starts.as_ref().clone();
        starts.push(start);
        self.chunk_starts = Arc::new(starts);
    }
}

/// LR-state-level product abstraction used for suffix convergence proofs.
///
/// Two products share a [`StateProductKey`] when they reduce to the same
/// nonterminal head over structurally equal children. Concrete green
/// identity — notably which terminal kind filled a value position — is
/// deliberately absent: the LR automaton's future behavior depends only on
/// GSS states, edge topology, and reduction heads, so equal state keys with
/// an equal frontier prove the retained suffix parses identically. The seam
/// binding resolves the differing products at attachment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum StateProductKey {
    /// A reduced error product. The green identity is kept whole: recovery
    /// regions are only reused across unchanged tokens, so equal error
    /// greens are required, and abstracting them could equate different
    /// recovery outcomes.
    Error {
        green: crate::framework::parse::data::green::GreenId,
        children: Arc<[StateProductKey]>,
    },
    /// A shifted token product. The terminal is abstract: the shifted GSS
    /// node's LR state already encodes the terminal class.
    Token,
    /// A reduction product headed by `head` (the production's LHS).
    Node {
        head: u32,
        children: Arc<[StateProductKey]>,
    },
}

impl ProductArena {
    /// Produces the [`StateProductKey`] for one product graph.
    ///
    /// Mirrors [`Self::canonical_key`]'s iterative traversal; the memoization
    /// and cycle guards are caller-owned for the same reasons.
    pub(crate) fn structural_key(
        &self,
        trees: &crate::framework::parse::data::green::TreeArena,
        id: ProductId,
        memo: &mut HashMap<ProductId, StateProductKey>,
        active: &mut HashSet<ProductId>,
    ) -> Option<StateProductKey> {
        if let Some(key) = memo.get(&id) {
            return Some(key.clone());
        }
        let head_of = |green: crate::framework::parse::data::green::GreenId| -> Option<u32> {
            match &trees.get(green)?.data {
                crate::framework::parse::data::green::TreeData::Node { id, .. } => Some(*id),
                crate::framework::parse::data::green::TreeData::Error { node, .. } => Some(*node),
                crate::framework::parse::data::green::TreeData::Leaf { .. } => None,
            }
        };
        let mut work = vec![(id, false)];
        while let Some((current, expanded)) = work.pop() {
            if memo.contains_key(&current) {
                active.remove(&current);
                continue;
            }
            let product = self.get(current)?;
            if expanded {
                let key = match &product.data {
                    ProductData::Error { children } => {
                        let children = children
                            .iter()
                            .map(|&child| memo.get(&child).cloned())
                            .collect::<Option<Vec<_>>>()?;
                        StateProductKey::Error {
                            green: product.green,
                            children: children.into(),
                        }
                    }
                    ProductData::Token { .. } => StateProductKey::Token,
                    ProductData::Node { children, .. } => {
                        let children = children
                            .iter()
                            .map(|&child| memo.get(&child).cloned())
                            .collect::<Option<Vec<_>>>()?;
                        StateProductKey::Node {
                            head: head_of(product.green)?,
                            children: children.into(),
                        }
                    }
                };
                active.remove(&current);
                memo.insert(current, key);
                continue;
            }
            if !active.insert(current) {
                return None;
            }
            match &product.data {
                ProductData::Token { .. } => {
                    active.remove(&current);
                    memo.insert(current, StateProductKey::Token);
                }
                ProductData::Error { children } | ProductData::Node { children, .. } => {
                    work.push((current, true));
                    for &child in children.iter().rev() {
                        if !memo.contains_key(&child) {
                            if active.contains(&child) {
                                return None;
                            }
                            work.push((child, false));
                        }
                    }
                }
            }
        }
        memo.get(&id).cloned()
    }
}

impl ProductArena {
    /// Produces an exact structural key for a product graph.
    ///
    /// The temporary memoization map is caller-owned so one frontier proof
    /// does not repeatedly traverse shared reductions. The explicit work
    /// stack keeps deeply nested documents off the thread stack.
    pub(crate) fn canonical_key(
        &self,
        id: ProductId,
        memo: &mut HashMap<ProductId, CanonicalProductKey>,
        active: &mut HashSet<ProductId>,
    ) -> Option<CanonicalProductKey> {
        if let Some(key) = memo.get(&id) {
            return Some(key.clone());
        }
        let mut work = vec![(id, false)];
        while let Some((current, expanded)) = work.pop() {
            if memo.contains_key(&current) {
                active.remove(&current);
                continue;
            }
            let product = self.get(current)?;
            if expanded {
                let key = match &product.data {
                    ProductData::Error { children } => {
                        let children = children
                            .iter()
                            .map(|&child| memo.get(&child).cloned())
                            .collect::<Option<Vec<_>>>()?;
                        CanonicalProductKey::Error {
                            green: product.green,
                            children: children.into(),
                        }
                    }
                    ProductData::Token { entry, ast, .. } => CanonicalProductKey::Token {
                        green: product.green,
                        entry: *entry,
                        typed: ast.is_some(),
                    },
                    ProductData::Node { children, .. } => {
                        let children = children
                            .iter()
                            .map(|&child| memo.get(&child).cloned())
                            .collect::<Option<Vec<_>>>()?;
                        CanonicalProductKey::Node {
                            green: product.green,
                            children: children.into(),
                        }
                    }
                };
                active.remove(&current);
                memo.insert(current, key);
                continue;
            }
            if !active.insert(current) {
                // A cyclic reduction cannot be assigned a finite structural
                // key. Reject reuse rather than using an identity-dependent
                // cycle marker.
                return None;
            }
            match &product.data {
                ProductData::Token { entry, ast, .. } => {
                    let key = CanonicalProductKey::Token {
                        green: product.green,
                        entry: *entry,
                        typed: ast.is_some(),
                    };
                    active.remove(&current);
                    memo.insert(current, key);
                }
                ProductData::Error { children } | ProductData::Node { children, .. } => {
                    work.push((current, true));
                    for &child in children.iter().rev() {
                        if !memo.contains_key(&child) {
                            if active.contains(&child) {
                                return None;
                            }
                            work.push((child, false));
                        }
                    }
                }
            }
        }
        memo.get(&id).cloned()
    }

    /// Computes a compact identity-free fingerprint for one product graph.
    /// Child fingerprints are memoized so callers can fingerprint a growing
    /// reduction chain in linear work across parser checkpoints. The explicit
    /// work stack mirrors [`Self::canonical_key`] for deep inputs.
    pub(crate) fn canonical_fingerprint(
        &self,
        id: ProductId,
        memo: &mut HashMap<ProductId, u64>,
        active: &mut HashSet<ProductId>,
    ) -> Option<u64> {
        if let Some(fingerprint) = memo.get(&id) {
            return Some(*fingerprint);
        }
        let mut work = vec![(id, false)];
        while let Some((current, expanded)) = work.pop() {
            if memo.contains_key(&current) {
                active.remove(&current);
                continue;
            }
            let product = self.get(current)?;
            if expanded {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                match &product.data {
                    ProductData::Error { children } => {
                        0u8.hash(&mut hasher);
                        product.green.hash(&mut hasher);
                        for &child in children {
                            memo.get(&child)?.hash(&mut hasher);
                        }
                    }
                    ProductData::Token { entry, ast, .. } => {
                        1u8.hash(&mut hasher);
                        product.green.hash(&mut hasher);
                        entry.hash(&mut hasher);
                        ast.is_some().hash(&mut hasher);
                    }
                    ProductData::Node { children, .. } => {
                        2u8.hash(&mut hasher);
                        product.green.hash(&mut hasher);
                        for &child in children {
                            memo.get(&child)?.hash(&mut hasher);
                        }
                    }
                }
                active.remove(&current);
                memo.insert(current, hasher.finish());
                continue;
            }
            if !active.insert(current) {
                return None;
            }
            match &product.data {
                ProductData::Token { entry, ast, .. } => {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    1u8.hash(&mut hasher);
                    product.green.hash(&mut hasher);
                    entry.hash(&mut hasher);
                    ast.is_some().hash(&mut hasher);
                    active.remove(&current);
                    memo.insert(current, hasher.finish());
                }
                ProductData::Error { children } | ProductData::Node { children, .. } => {
                    work.push((current, true));
                    for &child in children.iter().rev() {
                        if !memo.contains_key(&child) {
                            if active.contains(&child) {
                                return None;
                            }
                            work.push((child, false));
                        }
                    }
                }
            }
        }
        memo.get(&id).copied()
    }
}
