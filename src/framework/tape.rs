//! The framework-private persistent sequence substrate.
//!
//! `StableTape` is the only persistent ordered-sequence implementation used by
//! the lexer, parser, and ordered syntax facts.  It is an immutable,
//! path-copied, AVL-balanced rope of bounded leaves.  The binary shape is an
//! implementation detail: callers see rank/metric operations, cursors, and
//! stable occurrence lookup.  Unchanged nodes are shared by `Arc`; every node
//! created on a changed path receives a fresh checked ID from the owning
//! document allocator.

use std::{
    cmp::Ordering,
    ops::Range,
    sync::Arc,
};

use crate::reactive::store::RadixMap;

const LEAF_MAX: usize = 64;
const LEAF_MIN: usize = 32;

/// A checked monotonic immutable-node identity.  IDs are document-local and
/// never reused by a committed revision lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TapeNodeId(pub u64);

/// The hash component of a [`SequenceMetric`].  It is a rejection prefilter
/// only; every reuse/convergence decision still performs pointer or exact
/// entry equality.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct ExactHashPrefilter(pub u64);

impl ExactHashPrefilter {
    fn combine(self, right: Self, left_len: u64) -> Self {
        // SplitMix-style mixing is deterministic across processes.  This is
        // intentionally not an identity and is never accepted as proof.
        let mut value = self.0 ^ right.0.rotate_left(23) ^ left_len.rotate_left(41);
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
        Self(value ^ (value >> 31))
    }
}

/// Additive metric carried by every tape subtree.
pub(crate) trait TapeMetric: Default + Clone + std::fmt::Debug + PartialEq + Eq {
    /// Checked concatenation in source/order direction.
    fn add(&self, right: &Self) -> Self;
}

/// Composite lexer/parser metric.  `source_bytes` enables byte-to-rank lookup;
/// `lexical_count` and `semantic_count` enable the corresponding rank spaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SequenceMetric {
    pub(crate) lexical_count: u64,
    pub(crate) semantic_count: u64,
    pub(crate) source_bytes: u64,
    pub(crate) structural_hash: ExactHashPrefilter,
}

impl SequenceMetric {
    pub(crate) fn lexical(bytes: u64, hash: ExactHashPrefilter) -> Self {
        Self {
            lexical_count: 1,
            semantic_count: 0,
            source_bytes: bytes,
            structural_hash: hash,
        }
    }

    pub(crate) fn semantic(bytes: u64, hash: ExactHashPrefilter) -> Self {
        Self {
            lexical_count: 1,
            semantic_count: 1,
            source_bytes: bytes,
            structural_hash: hash,
        }
    }
}

impl TapeMetric for SequenceMetric {
    fn add(&self, right: &Self) -> Self {
        Self {
            lexical_count: self
                .lexical_count
                .checked_add(right.lexical_count)
                .expect("lexical tape metric overflow"),
            semantic_count: self
                .semantic_count
                .checked_add(right.semantic_count)
                .expect("semantic tape metric overflow"),
            source_bytes: self
                .source_bytes
                .checked_add(right.source_bytes)
                .expect("source-byte tape metric overflow"),
            structural_hash: self
                .structural_hash
                .combine(right.structural_hash, self.lexical_count),
        }
    }
}

/// An entry suitable for a stable tape.  Entry IDs are stable occurrence IDs;
/// values are immutable and exact equality remains the final reuse proof.
pub(crate) trait TapeEntry: Clone + PartialEq {
    fn stable_id(&self) -> u64;
    fn metric(&self) -> SequenceMetric;
}

impl TapeEntry for usize {
    fn stable_id(&self) -> u64 {
        *self as u64
    }

    fn metric(&self) -> SequenceMetric {
        SequenceMetric::lexical(1, ExactHashPrefilter(*self as u64))
    }
}

/// Document-owned checked allocator for immutable tape nodes.  A document
/// revision copies this value into its transactional working state; rollback
/// restores it together with the root, so discarded work cannot leak IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TapeIdAllocator {
    next: u64,
    created: u64,
}

impl Default for TapeIdAllocator {
    fn default() -> Self {
        Self { next: 1, created: 0 }
    }
}

impl TapeIdAllocator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn next_serial(&self) -> u64 {
        self.next
    }

    pub(crate) fn created_since(&self, checkpoint: u64) -> u64 {
        self.created.saturating_sub(checkpoint)
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.created
    }

    fn allocate(&mut self) -> TapeNodeId {
        let id = TapeNodeId(self.next);
        self.next = self.next.checked_add(1).expect("tape node identity overflow");
        self.created = self
            .created
            .checked_add(1)
            .expect("tape node creation counter overflow");
        id
    }
}

/// Dense fractional ordering label for a leaf.  Labels have unbounded binary
/// precision, so inserting repeatedly into one gap never forces a document
/// relabel/rebuild.  A label is a finite binary fraction; omitted bits are 0.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LeafOrderKey(Arc<[u8]>);

impl LeafOrderKey {
    fn half() -> Self {
        Self(Arc::from([1u8]))
    }

    fn initial(ordinal: usize, total: usize) -> Self {
        debug_assert!(ordinal < total);
        let mut width = 0usize;
        let mut capacity = 1usize;
        while capacity <= total {
            width += 1;
            capacity = capacity.saturating_mul(2);
        }
        let numerator = ordinal + 1;
        let mut bits = Vec::with_capacity(width);
        for shift in (0..width).rev() {
            bits.push(((numerator >> shift) & 1) as u8);
        }
        Self::canonical(bits)
    }

    fn canonical(mut bits: Vec<u8>) -> Self {
        debug_assert!(bits.iter().all(|bit| *bit <= 1));
        while bits.last() == Some(&0) {
            bits.pop();
        }
        Self(bits.into())
    }

    fn with_increment_at(&self, precision: usize) -> Self {
        debug_assert!(precision > self.0.len());
        let mut bits = self.0.to_vec();
        bits.resize(precision - 1, 0);
        bits.push(1);
        Self::canonical(bits)
    }

    fn before(&self) -> Self {
        let mut bits = Vec::with_capacity(self.0.len() + 1);
        bits.push(0);
        bits.extend(self.0.iter().copied());
        Self::canonical(bits)
    }

    fn after(&self) -> Self {
        self.with_increment_at(self.0.len() + 1)
    }

    fn between(left: Option<&Self>, right: Option<&Self>) -> Self {
        match (left, right) {
            (None, None) => Self::half(),
            (None, Some(right)) => right.before(),
            (Some(left), None) => left.after(),
            (Some(left), Some(right)) => {
                debug_assert!(left < right, "leaf labels must be ordered");
                let mut precision = left.0.len() + 1;
                loop {
                    let candidate = left.with_increment_at(precision);
                    if candidate < *right {
                        return candidate;
                    }
                    precision = precision
                        .checked_add(1)
                        .expect("leaf order label precision overflow");
                }
            }
        }
    }
}

impl Ord for LeafOrderKey {
    fn cmp(&self, other: &Self) -> Ordering {
        let width = self.0.len().max(other.0.len());
        for index in 0..width {
            let left = self.0.get(index).copied().unwrap_or(0);
            let right = other.0.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                order => return order,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for LeafOrderKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One immutable bounded leaf.
#[derive(Debug)]
pub(crate) struct TapeLeaf<T: TapeEntry> {
    pub(crate) id: TapeNodeId,
    pub(crate) order: LeafOrderKey,
    pub(crate) entries: Arc<[T]>,
    pub(crate) metric: SequenceMetric,
}

impl<T: TapeEntry> TapeLeaf<T> {
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// One immutable AVL branch.  `left`/`right` metrics make every rank operation
/// logarithmic without visiting an unchanged suffix.
#[derive(Debug)]
pub(crate) struct TapeBranch<T: TapeEntry> {
    pub(crate) id: TapeNodeId,
    pub(crate) left: Arc<TapeNode<T>>,
    pub(crate) right: Arc<TapeNode<T>>,
    pub(crate) len: usize,
    pub(crate) height: u16,
    pub(crate) metric: SequenceMetric,
    pub(crate) first_order: LeafOrderKey,
    pub(crate) last_order: LeafOrderKey,
}

/// Immutable tape node.
#[derive(Debug)]
pub(crate) enum TapeNode<T: TapeEntry> {
    Leaf(TapeLeaf<T>),
    Branch(TapeBranch<T>),
}

impl<T: TapeEntry> TapeNode<T> {
    pub(crate) fn id(&self) -> TapeNodeId {
        match self {
            Self::Leaf(leaf) => leaf.id,
            Self::Branch(branch) => branch.id,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Leaf(leaf) => leaf.len(),
            Self::Branch(branch) => branch.len,
        }
    }

    fn height(&self) -> u16 {
        match self {
            Self::Leaf(_) => 1,
            Self::Branch(branch) => branch.height,
        }
    }

    fn metric(&self) -> &SequenceMetric {
        match self {
            Self::Leaf(leaf) => &leaf.metric,
            Self::Branch(branch) => &branch.metric,
        }
    }

    fn first_order(&self) -> &LeafOrderKey {
        match self {
            Self::Leaf(leaf) => &leaf.order,
            Self::Branch(branch) => &branch.first_order,
        }
    }

    fn last_order(&self) -> &LeafOrderKey {
        match self {
            Self::Leaf(leaf) => &leaf.order,
            Self::Branch(branch) => &branch.last_order,
        }
    }
}

/// One stable occurrence location.  Leaf order labels are immutable positional
/// addresses, so a prefix insertion does not rewrite every suffix rank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OccurrenceLocation {
    pub(crate) leaf: TapeNodeId,
    pub(crate) slot: u8,
    order: LeafOrderKey,
}

/// Persistent `occurrence ID -> (leaf, slot)` index.  `RadixMap` path-copies
/// only modified keys, while all untouched suffix locations share their old
/// trie nodes by `Arc`.
#[derive(Clone, Default)]
pub(crate) struct PersistentOccurrenceIndex {
    entries: RadixMap<OccurrenceLocation>,
}

impl PersistentOccurrenceIndex {
    pub(crate) fn get(&self, id: u64) -> Option<&OccurrenceLocation> {
        self.entries.get(id)
    }

    pub(crate) fn insert(&mut self, id: u64, location: OccurrenceLocation) {
        self.entries.insert(id, location);
    }

    pub(crate) fn remove(&mut self, id: u64) -> bool {
        self.entries.remove(id)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Persistent ordered sequence root.  The document owns the accompanying
/// [`TapeIdAllocator`] and occurrence index; snapshots retain only `Arc`
/// subtrees reachable from their roots.
#[derive(Clone, Debug)]
pub(crate) struct StableTape<T: TapeEntry> {
    root: Option<Arc<TapeNode<T>>>,
}

impl<T: TapeEntry> Default for StableTape<T> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<T: TapeEntry> StableTape<T> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn from_entries(
        entries: impl IntoIterator<Item = T>,
        allocator: &mut TapeIdAllocator,
    ) -> Self {
        let entries: Vec<T> = entries.into_iter().collect();
        if entries.is_empty() {
            return Self::new();
        }
        let leaf_count = entries.len().div_ceil(LEAF_MAX);
        let mut leaves = Vec::with_capacity(leaf_count);
        for (ordinal, chunk) in entries.chunks(LEAF_MAX).enumerate() {
            leaves.push(Self::make_leaf(
                chunk.to_vec(),
                LeafOrderKey::initial(ordinal, leaf_count),
                allocator,
            ));
        }
        let root = Self::build_balanced(leaves, allocator);
        Self { root: Some(root) }
    }

    pub(crate) fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |root| root.len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub(crate) fn metric(&self) -> SequenceMetric {
        self.root
            .as_ref()
            .map(|root| *root.metric())
            .unwrap_or_default()
    }

    /// Aggregates entries preceding `rank` without traversing any unrelated
    /// branch. This is the source-coordinate primitive used by lexer and
    /// parser cursors.
    pub(crate) fn metric_before(&self, rank: usize) -> SequenceMetric {
        fn prefix<T: TapeEntry>(node: &TapeNode<T>, rank: usize) -> SequenceMetric {
            match node {
                TapeNode::Leaf(leaf) => leaf
                    .entries
                    .iter()
                    .take(rank.min(leaf.entries.len()))
                    .fold(SequenceMetric::default(), |metric, entry| {
                        metric.add(&entry.metric())
                    }),
                TapeNode::Branch(branch) => {
                    let left_len = branch.left.len();
                    if rank <= left_len {
                        prefix(branch.left.as_ref(), rank)
                    } else {
                        branch.left.metric().add(&prefix(
                            branch.right.as_ref(),
                            rank - left_len,
                        ))
                    }
                }
            }
        }

        self.root
            .as_deref()
            .map(|root| prefix(root, rank.min(root.len())))
            .unwrap_or_default()
    }

    pub(crate) fn root_id(&self) -> Option<TapeNodeId> {
        self.root.as_ref().map(|root| root.id())
    }

    pub(crate) fn root_ptr_eq(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    /// Debug/test proof that at least one immutable subtree is shared.
    pub(crate) fn shares_subtree(&self, other: &Self) -> bool {
        fn shares<T: TapeEntry>(left: &Arc<TapeNode<T>>, right: &Arc<TapeNode<T>>) -> bool {
            if Arc::ptr_eq(left, right) {
                return true;
            }
            match (left.as_ref(), right.as_ref()) {
                (TapeNode::Branch(left), TapeNode::Branch(right)) => {
                    shares(&left.left, &right.left)
                        || shares(&left.left, &right.right)
                        || shares(&left.right, &right.left)
                        || shares(&left.right, &right.right)
                }
                (TapeNode::Branch(left), _) => {
                    shares(&left.left, right) || shares(&left.right, right)
                }
                (_, TapeNode::Branch(right)) => {
                    shares(left, &right.left) || shares(left, &right.right)
                }
                _ => false,
            }
        }

        match (&self.root, &other.root) {
            (Some(left), Some(right)) => shares(left, right),
            _ => false,
        }
    }

    /// Number of external roots retaining this exact root.  This is a
    /// reachability observation only; memory reclamation is normal `Arc` drop.
    pub(crate) fn root_strong_count(&self) -> usize {
        self.root.as_ref().map_or(0, Arc::strong_count)
    }

    pub(crate) fn get(&self, rank: usize) -> Option<&T> {
        if rank >= self.len() {
            return None;
        }
        let mut node = self.root.as_deref()?;
        let mut rank = rank;
        loop {
            match node {
                TapeNode::Leaf(leaf) => return leaf.entries.get(rank),
                TapeNode::Branch(branch) => {
                    let left_len = branch.left.len();
                    if rank < left_len {
                        node = branch.left.as_ref();
                    } else {
                        rank -= left_len;
                        node = branch.right.as_ref();
                    }
                }
            }
        }
    }

    /// Exact equality with pointer identity as the fast proof.  Matching
    /// metrics/hashes only reject unequal subtrees early.
    pub(crate) fn exact_eq(&self, other: &Self) -> bool {
        fn nodes_equal<T: TapeEntry>(left: &Arc<TapeNode<T>>, right: &Arc<TapeNode<T>>) -> bool {
            if Arc::ptr_eq(left, right) {
                return true;
            }
            if left.len() != right.len() || left.metric() != right.metric() {
                return false;
            }
            match (left.as_ref(), right.as_ref()) {
                (TapeNode::Leaf(left), TapeNode::Leaf(right)) => left.entries == right.entries,
                (TapeNode::Branch(left), TapeNode::Branch(right)) => {
                    nodes_equal(&left.left, &right.left) && nodes_equal(&left.right, &right.right)
                }
                _ => {
                    let left_tape = StableTape {
                        root: Some(Arc::clone(left)),
                    };
                    let right_tape = StableTape {
                        root: Some(Arc::clone(right)),
                    };
                    left_tape.iter().eq(right_tape.iter())
                }
            }
        }

        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(left), Some(right)) => nodes_equal(left, right),
            _ => false,
        }
    }

    /// Returns a lazy in-order iterator.  It never gathers leaves first.
    pub(crate) fn iter(&self) -> TapeIter<'_, T> {
        TapeIter::new(self.root.as_deref())
    }

    /// Returns a lazy range iterator.  Only intersecting branches and leaves
    /// are entered; it never walks a prefix merely to reach `range.start`.
    pub(crate) fn iter_range(&self, range: Range<usize>) -> TapeRangeIter<'_, T> {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        TapeRangeIter::new(self.root.as_deref(), start, end)
    }

    pub(crate) fn cursor_at(&self, rank: usize) -> Option<TapeCursor<T>> {
        let root = Arc::clone(self.root.as_ref()?);
        (rank < root.len()).then(|| TapeCursor::new(root, rank))
    }

    /// Splits at an entry rank, path-copying only the two boundary spines.
    /// Unchanged full subtrees are returned by pointer.
    pub(crate) fn split_at(
        &self,
        rank: usize,
        allocator: &mut TapeIdAllocator,
    ) -> (Self, Self) {
        let rank = rank.min(self.len());
        let Some(root) = &self.root else {
            return (Self::new(), Self::new());
        };
        let (left, right) = Self::split_node(root, rank, None, allocator);
        (Self { root: left }, Self { root: right })
    }

    /// Concatenates two roots.  Attaching a retained suffix copies only the
    /// logarithmic boundary spine; it never iterates suffix entries.
    pub(crate) fn concat(&self, right: &Self, allocator: &mut TapeIdAllocator) -> Self {
        let fitted = right.rekey_between(self.last_order_key(), None, allocator);
        Self {
            root: Self::concat_nodes(self.root.clone(), fitted.root, allocator),
        }
    }

    /// Replaces one rank range by another persistent root.
    pub(crate) fn splice(
        &self,
        range: Range<usize>,
        replacement: &Self,
        allocator: &mut TapeIdAllocator,
    ) -> Self {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        let (prefix, remainder) = self.split_at(start, allocator);
        let (_, suffix) = remainder.split_at(end - start, allocator);
        let fitted = replacement.rekey_between(
            prefix.last_order_key(),
            suffix.first_order_key(),
            allocator,
        );
        prefix.concat(&fitted, allocator).concat(&suffix, allocator)
    }

    pub(crate) fn push(&self, value: T, allocator: &mut TapeIdAllocator) -> Self {
        let one = Self::from_entries([value], allocator);
        self.concat(&one, allocator)
    }

    fn first_order_key(&self) -> Option<&LeafOrderKey> {
        self.root.as_ref().map(|root| root.first_order())
    }

    fn last_order_key(&self) -> Option<&LeafOrderKey> {
        self.root.as_ref().map(|root| root.last_order())
    }

    fn fits_between(
        &self,
        lower: Option<&LeafOrderKey>,
        upper: Option<&LeafOrderKey>,
    ) -> bool {
        self.root.as_ref().is_none_or(|root| {
            lower.is_none_or(|lower| lower < root.first_order())
                && upper.is_none_or(|upper| root.last_order() < upper)
        })
    }

    /// Re-labels only a replacement root whose local labels cannot be placed
    /// between its retained neighbours.  It visits replacement entries, never
    /// a retained prefix or suffix.
    fn rekey_between(
        &self,
        lower: Option<&LeafOrderKey>,
        upper: Option<&LeafOrderKey>,
        allocator: &mut TapeIdAllocator,
    ) -> Self {
        if self.fits_between(lower, upper) {
            return self.clone();
        }
        let entries: Vec<T> = self.iter().cloned().collect();
        if entries.is_empty() {
            return Self::new();
        }
        let mut previous = lower.cloned();
        let mut leaves = Vec::with_capacity(entries.len().div_ceil(LEAF_MAX));
        for chunk in entries.chunks(LEAF_MAX) {
            let order = LeafOrderKey::between(previous.as_ref(), upper);
            previous = Some(order.clone());
            leaves.push(Self::make_leaf(chunk.to_vec(), order, allocator));
        }
        Self {
            root: Some(Self::build_balanced(leaves, allocator)),
        }
    }

    /// Builds a persistent occurrence index during initial construction.
    /// Incremental callers should use [`Self::splice_with_index`] instead.
    pub(crate) fn occurrence_index(&self) -> PersistentOccurrenceIndex {
        let mut index = PersistentOccurrenceIndex::default();
        for (id, location) in self.locations_in_range(0..self.len()) {
            index.insert(id, location);
        }
        index
    }

    /// Splices both tape and occurrence index.  Re-indexing is bounded to the
    /// changed range plus at most one bounded leaf on each side; suffix keys
    /// and radix paths remain pointer-shared.
    pub(crate) fn splice_with_index(
        &self,
        index: &PersistentOccurrenceIndex,
        range: Range<usize>,
        replacement: &Self,
        allocator: &mut TapeIdAllocator,
    ) -> (Self, PersistentOccurrenceIndex) {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        let old_window = start.saturating_sub(LEAF_MAX)..(end + LEAF_MAX).min(self.len());
        let old_locations = self.locations_in_range(old_window);
        let next = self.splice(start..end, replacement, allocator);
        let replacement_end = start
            .checked_add(replacement.len())
            .expect("tape splice length overflow");
        let new_window = start.saturating_sub(LEAF_MAX)
            ..(replacement_end + LEAF_MAX).min(next.len());
        let new_locations = next.locations_in_range(new_window);
        let mut next_index = index.clone();
        for (id, _) in old_locations {
            next_index.remove(id);
        }
        for (id, location) in new_locations {
            next_index.insert(id, location);
        }
        (next, next_index)
    }

    /// Rank lookup by stable occurrence ID through the persistent occurrence
    /// index plus immutable leaf order labels.
    pub(crate) fn rank_of_id(
        &self,
        id: u64,
        index: &PersistentOccurrenceIndex,
    ) -> Option<usize> {
        let location = index.get(id)?;
        let root = self.root.as_ref()?;
        let rank = Self::rank_of_order(root, &location.order, 0)?;
        let leaf = Self::leaf_at_order(root, &location.order)?;
        (leaf.id == location.leaf && usize::from(location.slot) < leaf.entries.len())
            .then_some(rank + usize::from(location.slot))
    }

    pub(crate) fn cursor_at_id(
        &self,
        id: u64,
        index: &PersistentOccurrenceIndex,
    ) -> Option<TapeCursor<T>> {
        self.cursor_at(self.rank_of_id(id, index)?)
    }

    pub(crate) fn predecessor_id(
        &self,
        id: u64,
        index: &PersistentOccurrenceIndex,
    ) -> Option<u64> {
        self.rank_of_id(id, index)
            .and_then(|rank| rank.checked_sub(1))
            .and_then(|rank| self.get(rank))
            .map(TapeEntry::stable_id)
    }

    pub(crate) fn successor_id(
        &self,
        id: u64,
        index: &PersistentOccurrenceIndex,
    ) -> Option<u64> {
        self.rank_of_id(id, index)
            .and_then(|rank| rank.checked_add(1))
            .filter(|rank| *rank < self.len())
            .and_then(|rank| self.get(rank))
            .map(TapeEntry::stable_id)
    }

    /// O(log T) lexical-rank lookup from a source byte prefix.  The returned
    /// rank is the first entry ending after `byte_offset`.
    pub(crate) fn lexical_rank_at_byte(&self, byte_offset: u64) -> usize {
        self.lexical_rank_at_byte_detailed(byte_offset).0
    }

    /// O(log T) byte->rank lookup returning the B-tree descent depth too, so
    /// checkpoint-lookup counters measure real structure work (plan §19
    /// gates) rather than a constant.
    pub(crate) fn lexical_rank_at_byte_detailed(&self, byte_offset: u64) -> (usize, usize) {
        let mut rank = 0usize;
        let mut depth = 0usize;
        let mut remaining = byte_offset;
        let Some(mut node) = self.root.as_deref() else {
            return (0, 0);
        };
        loop {
            match node {
                TapeNode::Leaf(leaf) => {
                    depth += 1;
                    for entry in leaf.entries.iter() {
                        let width = entry.metric().source_bytes;
                        if remaining < width {
                            return (rank, depth);
                        }
                        remaining = remaining.saturating_sub(width);
                        rank += 1;
                    }
                    return (rank, depth);
                }
                TapeNode::Branch(branch) => {
                    depth += 1;
                    let left_bytes = branch.left.metric().source_bytes;
                    if remaining < left_bytes {
                        node = branch.left.as_ref();
                    } else {
                        remaining = remaining.saturating_sub(left_bytes);
                        rank += branch.left.len();
                        node = branch.right.as_ref();
                    }
                }
            }
        }
    }

    fn make_leaf(
        entries: Vec<T>,
        order: LeafOrderKey,
        allocator: &mut TapeIdAllocator,
    ) -> Arc<TapeNode<T>> {
        debug_assert!(!entries.is_empty());
        debug_assert!(entries.len() <= LEAF_MAX);
        let metric = entries
            .iter()
            .fold(SequenceMetric::default(), |metric, entry| metric.add(&entry.metric()));
        Arc::new(TapeNode::Leaf(TapeLeaf {
            id: allocator.allocate(),
            order,
            entries: entries.into(),
            metric,
        }))
    }

    fn make_branch(
        left: Arc<TapeNode<T>>,
        right: Arc<TapeNode<T>>,
        allocator: &mut TapeIdAllocator,
    ) -> Arc<TapeNode<T>> {
        debug_assert!(left.last_order() < right.first_order());
        Arc::new(TapeNode::Branch(TapeBranch {
            id: allocator.allocate(),
            len: left
                .len()
                .checked_add(right.len())
                .expect("tape entry count overflow"),
            height: left.height().max(right.height()).saturating_add(1),
            metric: left.metric().add(right.metric()),
            first_order: left.first_order().clone(),
            last_order: right.last_order().clone(),
            left,
            right,
        }))
    }

    fn build_balanced(
        mut nodes: Vec<Arc<TapeNode<T>>>,
        allocator: &mut TapeIdAllocator,
    ) -> Arc<TapeNode<T>> {
        debug_assert!(!nodes.is_empty());
        while nodes.len() > 1 {
            let mut next = Vec::with_capacity(nodes.len().div_ceil(2));
            let mut iterator = nodes.into_iter();
            while let Some(left) = iterator.next() {
                match iterator.next() {
                    Some(right) => next.push(Self::make_branch(left, right, allocator)),
                    None => next.push(left),
                }
            }
            nodes = next;
        }
        nodes.pop().expect("non-empty node level")
    }

    fn concat_nodes(
        left: Option<Arc<TapeNode<T>>>,
        right: Option<Arc<TapeNode<T>>>,
        allocator: &mut TapeIdAllocator,
    ) -> Option<Arc<TapeNode<T>>> {
        match (left, right) {
            (None, right) => right,
            (left, None) => left,
            (Some(left), Some(right)) => {
                if let (TapeNode::Leaf(left_leaf), TapeNode::Leaf(right_leaf)) =
                    (left.as_ref(), right.as_ref())
                {
                    if left_leaf.len() + right_leaf.len() <= LEAF_MAX {
                        let mut entries = Vec::with_capacity(left_leaf.len() + right_leaf.len());
                        entries.extend(left_leaf.entries.iter().cloned());
                        entries.extend(right_leaf.entries.iter().cloned());
                        return Some(Self::make_leaf(entries, left_leaf.order.clone(), allocator));
                    }
                }

                let left_height = left.height();
                let right_height = right.height();
                if left_height > right_height.saturating_add(1) {
                    let TapeNode::Branch(branch) = left.as_ref() else {
                        unreachable!("leaf cannot have height greater than one")
                    };
                    let joined = Self::concat_nodes(
                        Some(Arc::clone(&branch.right)),
                        Some(right),
                        allocator,
                    )
                    .expect("joining two roots yields a root");
                    return Some(Self::rebalance(Arc::clone(&branch.left), joined, allocator));
                }
                if right_height > left_height.saturating_add(1) {
                    let TapeNode::Branch(branch) = right.as_ref() else {
                        unreachable!("leaf cannot have height greater than one")
                    };
                    let joined = Self::concat_nodes(
                        Some(left),
                        Some(Arc::clone(&branch.left)),
                        allocator,
                    )
                    .expect("joining two roots yields a root");
                    return Some(Self::rebalance(joined, Arc::clone(&branch.right), allocator));
                }
                Some(Self::make_branch(left, right, allocator))
            }
        }
    }

    fn rebalance(
        left: Arc<TapeNode<T>>,
        right: Arc<TapeNode<T>>,
        allocator: &mut TapeIdAllocator,
    ) -> Arc<TapeNode<T>> {
        let left_height = left.height();
        let right_height = right.height();
        if left_height > right_height.saturating_add(1) {
            let TapeNode::Branch(left_branch) = left.as_ref() else {
                unreachable!("unbalanced leaf")
            };
            if left_branch.left.height() >= left_branch.right.height() {
                let branch = Self::make_branch(Arc::clone(&left_branch.right), right, allocator);
                return Self::make_branch(Arc::clone(&left_branch.left), branch, allocator);
            }
            let TapeNode::Branch(pivot) = left_branch.right.as_ref() else {
                unreachable!("right-heavy child must be a branch")
            };
            let left_branch = Self::make_branch(
                Arc::clone(&left_branch.left),
                Arc::clone(&pivot.left),
                allocator,
            );
            let right_branch = Self::make_branch(Arc::clone(&pivot.right), right, allocator);
            return Self::make_branch(left_branch, right_branch, allocator);
        }
        if right_height > left_height.saturating_add(1) {
            let TapeNode::Branch(right_branch) = right.as_ref() else {
                unreachable!("unbalanced leaf")
            };
            if right_branch.right.height() >= right_branch.left.height() {
                let branch = Self::make_branch(left, Arc::clone(&right_branch.left), allocator);
                return Self::make_branch(branch, Arc::clone(&right_branch.right), allocator);
            }
            let TapeNode::Branch(pivot) = right_branch.left.as_ref() else {
                unreachable!("left-heavy child must be a branch")
            };
            let left_branch = Self::make_branch(left, Arc::clone(&pivot.left), allocator);
            let right_branch = Self::make_branch(
                Arc::clone(&pivot.right),
                Arc::clone(&right_branch.right),
                allocator,
            );
            return Self::make_branch(left_branch, right_branch, allocator);
        }
        Self::make_branch(left, right, allocator)
    }

    fn split_node(
        node: &Arc<TapeNode<T>>,
        rank: usize,
        upper: Option<&LeafOrderKey>,
        allocator: &mut TapeIdAllocator,
    ) -> (Option<Arc<TapeNode<T>>>, Option<Arc<TapeNode<T>>>) {
        if rank == 0 {
            return (None, Some(Arc::clone(node)));
        }
        if rank >= node.len() {
            return (Some(Arc::clone(node)), None);
        }
        match node.as_ref() {
            TapeNode::Leaf(leaf) => {
                let left = Self::make_leaf(
                    leaf.entries[..rank].to_vec(),
                    leaf.order.clone(),
                    allocator,
                );
                let right_order = LeafOrderKey::between(Some(&leaf.order), upper);
                let right = Self::make_leaf(
                    leaf.entries[rank..].to_vec(),
                    right_order,
                    allocator,
                );
                (Some(left), Some(right))
            }
            TapeNode::Branch(branch) => {
                let left_len = branch.left.len();
                if rank < left_len {
                    let (prefix, middle) = Self::split_node(
                        &branch.left,
                        rank,
                        Some(branch.right.first_order()),
                        allocator,
                    );
                    let suffix = Self::concat_nodes(middle, Some(Arc::clone(&branch.right)), allocator);
                    (prefix, suffix)
                } else {
                    let (middle, suffix) = Self::split_node(
                        &branch.right,
                        rank - left_len,
                        upper,
                        allocator,
                    );
                    let prefix = Self::concat_nodes(Some(Arc::clone(&branch.left)), middle, allocator);
                    (prefix, suffix)
                }
            }
        }
    }

    fn rank_of_order(
        node: &Arc<TapeNode<T>>,
        order: &LeafOrderKey,
        base: usize,
    ) -> Option<usize> {
        match node.as_ref() {
            TapeNode::Leaf(leaf) => (leaf.order == *order).then_some(base),
            TapeNode::Branch(branch) => {
                if order <= branch.left.last_order() {
                    Self::rank_of_order(&branch.left, order, base)
                } else {
                    Self::rank_of_order(&branch.right, order, base + branch.left.len())
                }
            }
        }
    }

    fn leaf_at_order<'a>(
        node: &'a Arc<TapeNode<T>>,
        order: &LeafOrderKey,
    ) -> Option<&'a TapeLeaf<T>> {
        match node.as_ref() {
            TapeNode::Leaf(leaf) => (leaf.order == *order).then_some(leaf),
            TapeNode::Branch(branch) => {
                if order <= branch.left.last_order() {
                    Self::leaf_at_order(&branch.left, order)
                } else {
                    Self::leaf_at_order(&branch.right, order)
                }
            }
        }
    }

    fn locations_in_range(&self, range: Range<usize>) -> Vec<(u64, OccurrenceLocation)> {
        let mut locations = Vec::new();
        let Some(root) = &self.root else {
            return locations;
        };
        Self::collect_locations(root, 0, &range, &mut locations);
        locations
    }

    fn collect_locations(
        node: &Arc<TapeNode<T>>,
        base: usize,
        range: &Range<usize>,
        out: &mut Vec<(u64, OccurrenceLocation)>,
    ) {
        let end = base + node.len();
        if range.start >= end || range.end <= base {
            return;
        }
        match node.as_ref() {
            TapeNode::Leaf(leaf) => {
                let start = range.start.saturating_sub(base);
                let end = (range.end - base).min(leaf.entries.len());
                for (slot, entry) in leaf.entries[start..end].iter().enumerate() {
                    let slot = start + slot;
                    out.push((
                        entry.stable_id(),
                        OccurrenceLocation {
                            leaf: leaf.id,
                            slot: u8::try_from(slot).expect("leaf slot fits u8"),
                            order: leaf.order.clone(),
                        },
                    ));
                }
            }
            TapeNode::Branch(branch) => {
                Self::collect_locations(&branch.left, base, range, out);
                Self::collect_locations(&branch.right, base + branch.left.len(), range, out);
            }
        }
    }
}

/// A lazy in-order reference iterator.
pub(crate) struct TapeIter<'a, T: TapeEntry> {
    stack: Vec<IterFrame<'a, T>>,
}

enum IterFrame<'a, T: TapeEntry> {
    Node(&'a TapeNode<T>),
    Leaf(&'a [T], usize),
}

impl<'a, T: TapeEntry> TapeIter<'a, T> {
    fn new(root: Option<&'a TapeNode<T>>) -> Self {
        let mut stack = Vec::new();
        if let Some(root) = root {
            stack.push(IterFrame::Node(root));
        }
        Self { stack }
    }
}

impl<'a, T: TapeEntry> Iterator for TapeIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stack.pop()? {
                IterFrame::Node(TapeNode::Leaf(leaf)) => {
                    self.stack.push(IterFrame::Leaf(&leaf.entries, 0));
                }
                IterFrame::Node(TapeNode::Branch(branch)) => {
                    self.stack.push(IterFrame::Node(branch.right.as_ref()));
                    self.stack.push(IterFrame::Node(branch.left.as_ref()));
                }
                IterFrame::Leaf(entries, index) => {
                    let value = entries.get(index)?;
                    if index + 1 < entries.len() {
                        self.stack.push(IterFrame::Leaf(entries, index + 1));
                    }
                    return Some(value);
                }
            }
        }
    }
}

/// A lazy reference iterator over one rank interval.
pub(crate) struct TapeRangeIter<'a, T: TapeEntry> {
    stack: Vec<RangeFrame<'a, T>>,
}

enum RangeFrame<'a, T: TapeEntry> {
    Node(&'a TapeNode<T>, usize),
    Leaf(&'a [T], usize, usize),
}

impl<'a, T: TapeEntry> TapeRangeIter<'a, T> {
    fn new(root: Option<&'a TapeNode<T>>, start: usize, end: usize) -> Self {
        let mut stack = Vec::new();
        if start < end {
            if let Some(root) = root {
                stack.push(RangeFrame::Node(root, 0));
            }
        }
        let _ = (start, end);
        // Bounds are stored in the initial node frame through the TLS-free
        // constructor below; avoid a second whole-tree prepass.
        let mut iter = Self { stack: Vec::new() };
        if start < end {
            if let Some(root) = root {
                iter.push_intersection(root, 0, start, end);
            }
        }
        iter
    }

    fn push_intersection(&mut self, node: &'a TapeNode<T>, base: usize, start: usize, end: usize) {
        let node_end = base + node.len();
        if start >= node_end || end <= base {
            return;
        }
        match node {
            TapeNode::Leaf(leaf) => {
                let local_start = start.saturating_sub(base);
                let local_end = (end - base).min(leaf.entries.len());
                if local_start < local_end {
                    self.stack
                        .push(RangeFrame::Leaf(&leaf.entries, local_start, local_end));
                }
            }
            TapeNode::Branch(branch) => {
                // Stack is LIFO: right is pushed first so left yields first.
                self.push_intersection(branch.right.as_ref(), base + branch.left.len(), start, end);
                self.push_intersection(branch.left.as_ref(), base, start, end);
            }
        }
    }
}

impl<'a, T: TapeEntry> Iterator for TapeRangeIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stack.pop()? {
                RangeFrame::Node(_, _) => unreachable!("range nodes are expanded eagerly by path"),
                RangeFrame::Leaf(entries, index, end) => {
                    let value = entries.get(index)?;
                    if index + 1 < end {
                        self.stack.push(RangeFrame::Leaf(entries, index + 1, end));
                    }
                    return Some(value);
                }
            }
        }
    }
}

/// O(log T)-seek persistent cursor.  Advancing across leaf boundaries walks
/// only a bounded path; entries within a leaf advance in O(1).
pub(crate) struct TapeCursor<T: TapeEntry> {
    root: Arc<TapeNode<T>>,
    path: Vec<CursorFrame<T>>,
    leaf: Arc<TapeNode<T>>,
    slot: usize,
    rank: usize,
}

struct CursorFrame<T: TapeEntry> {
    node: Arc<TapeNode<T>>,
    went_right: bool,
}

impl<T: TapeEntry> TapeCursor<T> {
    fn new(root: Arc<TapeNode<T>>, rank: usize) -> Self {
        let mut cursor = Self {
            root: Arc::clone(&root),
            path: Vec::new(),
            leaf: Arc::clone(&root),
            slot: 0,
            rank,
        };
        cursor.descend_to_rank(root, rank);
        cursor
    }

    pub(crate) fn rank(&self) -> usize {
        self.rank
    }

    pub(crate) fn current(&self) -> &T {
        let TapeNode::Leaf(leaf) = self.leaf.as_ref() else {
            unreachable!("cursor always ends at a leaf")
        };
        &leaf.entries[self.slot]
    }

    pub(crate) fn current_id(&self) -> u64 {
        self.current().stable_id()
    }

    /// Advances one entry. Returns false at EOF.
    pub(crate) fn advance(&mut self) -> bool {
        let TapeNode::Leaf(leaf) = self.leaf.as_ref() else {
            unreachable!("cursor always ends at a leaf")
        };
        if self.slot + 1 < leaf.entries.len() {
            self.slot += 1;
            self.rank += 1;
            return true;
        }
        while let Some(mut frame) = self.path.pop() {
            let TapeNode::Branch(branch) = frame.node.as_ref() else {
                unreachable!("cursor path contains branches")
            };
            if !frame.went_right {
                frame.went_right = true;
                let right = Arc::clone(&branch.right);
                self.path.push(frame);
                self.descend_left(right);
                self.rank += 1;
                return true;
            }
        }
        false
    }

    /// Moves one entry backward. Returns false at BOF.
    pub(crate) fn retreat(&mut self) -> bool {
        if self.slot > 0 {
            self.slot -= 1;
            self.rank -= 1;
            return true;
        }
        while let Some(mut frame) = self.path.pop() {
            let TapeNode::Branch(branch) = frame.node.as_ref() else {
                unreachable!("cursor path contains branches")
            };
            if frame.went_right {
                frame.went_right = false;
                let left = Arc::clone(&branch.left);
                self.path.push(frame);
                self.descend_right(left);
                self.rank -= 1;
                return true;
            }
        }
        false
    }

    fn descend_to_rank(&mut self, mut node: Arc<TapeNode<T>>, mut rank: usize) {
        loop {
            match node.as_ref() {
                TapeNode::Leaf(_) => {
                    self.leaf = node;
                    self.slot = rank;
                    return;
                }
                TapeNode::Branch(branch) => {
                    let left_len = branch.left.len();
                    if rank < left_len {
                        self.path.push(CursorFrame {
                            node: Arc::clone(&node),
                            went_right: false,
                        });
                        node = Arc::clone(&branch.left);
                    } else {
                        rank -= left_len;
                        self.path.push(CursorFrame {
                            node: Arc::clone(&node),
                            went_right: true,
                        });
                        node = Arc::clone(&branch.right);
                    }
                }
            }
        }
    }

    fn descend_left(&mut self, mut node: Arc<TapeNode<T>>) {
        loop {
            match node.as_ref() {
                TapeNode::Leaf(_) => {
                    self.leaf = node;
                    self.slot = 0;
                    return;
                }
                TapeNode::Branch(branch) => {
                    self.path.push(CursorFrame {
                        node: Arc::clone(&node),
                        went_right: false,
                    });
                    node = Arc::clone(&branch.left);
                }
            }
        }
    }

    fn descend_right(&mut self, mut node: Arc<TapeNode<T>>) {
        loop {
            match node.as_ref() {
                TapeNode::Leaf(leaf) => {
                    let slot = leaf.entries.len() - 1;
                    self.leaf = node;
                    self.slot = slot;
                    return;
                }
                TapeNode::Branch(branch) => {
                    self.path.push(CursorFrame {
                        node: Arc::clone(&node),
                        went_right: true,
                    });
                    node = Arc::clone(&branch.right);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Entry {
        id: u64,
        bytes: u64,
        semantic: bool,
    }

    impl TapeEntry for Entry {
        fn stable_id(&self) -> u64 {
            self.id
        }

        fn metric(&self) -> SequenceMetric {
            SequenceMetric {
                lexical_count: 1,
                semantic_count: u64::from(self.semantic),
                source_bytes: self.bytes,
                structural_hash: ExactHashPrefilter(self.id),
            }
        }
    }

    fn entries(start: u64, count: u64) -> Vec<Entry> {
        (start..start + count)
            .map(|id| Entry {
                id,
                bytes: 1 + id % 3,
                semantic: id % 2 == 0,
            })
            .collect()
    }

    #[test]
    fn split_concat_splice_preserve_order_and_indexes() {
        let mut allocator = TapeIdAllocator::new();
        let tape = StableTape::from_entries(entries(0, 200), &mut allocator);
        let index = tape.occurrence_index();
        assert_eq!(tape.len(), 200);
        assert_eq!(index.len(), 200);
        assert_eq!(tape.metric().lexical_count, 200);

        let (left, right) = tape.split_at(64, &mut allocator);
        assert_eq!(left.len(), 64);
        assert_eq!(right.len(), 136);
        assert!(right.shares_subtree(&tape));
        let joined = left.concat(&right, &mut allocator);
        assert!(joined.exact_eq(&tape));

        let replacement = StableTape::from_entries(entries(1_000, 3), &mut allocator);
        let (next, next_index) = tape.splice_with_index(&index, 70..73, &replacement, &mut allocator);
        let ids: Vec<u64> = next.iter().map(TapeEntry::stable_id).collect();
        assert_eq!(&ids[..70], &(0..70).collect::<Vec<_>>());
        assert_eq!(&ids[70..73], &[1_000, 1_001, 1_002]);
        assert_eq!(&ids[73..], &(73..200).collect::<Vec<_>>());
        assert!(next.rank_of_id(1_000, &next_index).is_some());
        assert_eq!(next.rank_of_id(199, &next_index), Some(199));
        assert_eq!(next.predecessor_id(1_000, &next_index), Some(69));
        assert_eq!(next.successor_id(1_002, &next_index), Some(73));
    }

    #[test]
    fn cursor_and_range_iteration_do_not_materialize_leaves() {
        let mut allocator = TapeIdAllocator::new();
        let tape = StableTape::from_entries(entries(0, 130), &mut allocator);
        let mut cursor = tape.cursor_at(63).expect("rank exists");
        assert_eq!(cursor.current_id(), 63);
        assert!(cursor.advance());
        assert_eq!(cursor.current_id(), 64);
        assert!(cursor.retreat());
        assert_eq!(cursor.current_id(), 63);
        let range: Vec<u64> = tape
            .iter_range(61..68)
            .map(TapeEntry::stable_id)
            .collect();
        assert_eq!(range, (61..68).collect::<Vec<_>>());
    }

    #[test]
    fn changed_roots_receive_fresh_node_ids_and_keep_suffix_arcs() {
        let mut allocator = TapeIdAllocator::new();
        let tape = StableTape::from_entries(entries(0, 128), &mut allocator);
        let original_root = tape.root_id();
        let next = tape.push(
            Entry {
                id: 999,
                bytes: 1,
                semantic: true,
            },
            &mut allocator,
        );
        assert_ne!(next.root_id(), original_root);
        assert!(next.shares_subtree(&tape));
        assert_eq!(next.get(128).map(TapeEntry::stable_id), Some(999));
    }

    #[test]
    fn byte_rank_uses_subtree_metrics() {
        let mut allocator = TapeIdAllocator::new();
        let tape = StableTape::from_entries(entries(0, 10), &mut allocator);
        let prefix = tape.get(0).unwrap().bytes + tape.get(1).unwrap().bytes;
        assert_eq!(tape.lexical_rank_at_byte(prefix), 2);
    }
}
