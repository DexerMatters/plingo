//! Indexed fact-store primitives (plan §5.1).
//!
//! Three layers live here:
//!
//! 1. [`RadixMap`] — an eight-level, 256-way bitmap trie over `u64` keys.
//!    Insertion and removal path-copy at most eight nodes and prune empty
//!    branches; iteration yields ascending keys without ever touching a
//!    hash-table seed.
//! 2. [`Hamt`] — a 32-way bitmap trie over five-bit hash fragments with a
//!    terminal exact-equality collision bucket. Point lookup and removal
//!    are `O(log32 n)`; no dense child array ever allocates.
//! 3. [`ErasedFactKey`] / [`SnapshotKey`] / [`OwnerSet`] — the erased-key
//!    wrappers the committed snapshot index and owner bookkeeping use.
//!
//! Hashes are prefilters only: every equality decision resolves through
//! [`KeyValue::eq_value`] on the stored key.

use std::any::TypeId;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::reactive::engine::EngineWork;
use crate::reactive::error::{Error, Result};
use crate::reactive::plain::record_command_metric;
use crate::reactive::value::{KeyValue, Value};

// ---------------------------------------------------------------------------
// Trie key contract
// ---------------------------------------------------------------------------

/// A key addressable by the persistent tries through its cached hash.
pub(crate) trait TrieKey: Clone {
    /// The cached full hash of this key.
    fn trie_hash(&self) -> u64;

    /// Exact structural equality; hashes only narrow candidates.
    fn trie_eq(&self, other: &Self) -> bool;
}

// ---------------------------------------------------------------------------
// RadixMap
// ---------------------------------------------------------------------------

/// A persistent map from `u64` to `V`; writes path-copy, readers share.
#[derive(Debug, Clone)]
pub(crate) struct RadixMap<V> {
    root: Option<Arc<RadixNode<V>>>,
    len: usize,
}

#[derive(Debug)]
enum RadixNode<V> {
    /// Interior node: one bit per byte slot across `[u64; 4]`.
    Branch {
        bitmap: [u64; 4],
        children: Arc<[Arc<RadixNode<V>>]>,
    },
    Leaf { key: u64, value: V },
}

const RADIX_DEPTH: u32 = 8;

impl<V> Default for RadixMap<V> {
    fn default() -> Self {
        Self {
            root: None,
            len: 0,
        }
    }
}

impl<V> RadixMap<V> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn nibble(key: u64, depth: u32) -> usize {
        ((key >> (56 - 8 * depth)) & 0xFF) as usize
    }

    fn bitmap_index(nibble: usize) -> (usize, u64) {
        (nibble / 64, 1u64 << (nibble % 64))
    }

    fn rank(bitmap: &[u64; 4], word: usize, bit: u64) -> usize {
        bitmap[..word]
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum::<usize>()
            + (bitmap[word] & (bit - 1)).count_ones() as usize
    }

    pub(crate) fn get(&self, key: u64) -> Option<&V> {
        let mut node = self.root.as_deref()?;
        let mut depth = 0;
        loop {
            match node {
                RadixNode::Leaf { key: leaf_key, value } => {
                    return (*leaf_key == key).then_some(value);
                }
                RadixNode::Branch { bitmap, children } => {
                    if depth == RADIX_DEPTH {
                        return None;
                    }
                    let nibble = Self::nibble(key, depth);
                    let (word, bit) = Self::bitmap_index(nibble);
                    if bitmap[word] & bit == 0 {
                        return None;
                    }
                    node = children[Self::rank(bitmap, word, bit)].as_ref();
                    depth += 1;
                }
            }
        }
    }

    /// Inserts or replaces one entry. Replacement keeps the stored key and
    /// swaps the payload.
    pub(crate) fn insert(&mut self, key: u64, value: V) {
        let replacing = self.get(key).is_some();
        let leaf = Arc::new(RadixNode::Leaf { key, value });
        match self.root.take() {
            None => self.root = Some(leaf),
            Some(root) => self.root = Some(Self::insert_at(root, leaf, key, 0)),
        }
        if !replacing {
            self.len += 1;
        }
    }

    fn insert_at(node: Arc<RadixNode<V>>, leaf: Arc<RadixNode<V>>, key: u64, depth: u32) -> Arc<RadixNode<V>> {
        match node.as_ref() {
            RadixNode::Leaf { .. } => {
                if depth == RADIX_DEPTH {
                    // Full key consumed: identical keys replace payloads.
                    return leaf;
                }
                let same_key = matches!(
                    node.as_ref(),
                    RadixNode::Leaf { key: existing, .. } if *existing == key
                );
                if same_key {
                    return leaf;
                }
                Self::split_leaf(node, leaf, key, depth)
            }
            RadixNode::Branch { bitmap, children } => {
                let nibble = Self::nibble(key, depth);
                let (word, bit) = Self::bitmap_index(nibble);
                let present = bitmap[word] & bit != 0;
                let rank = Self::rank(bitmap, word, bit);
                let mut next_children: Vec<Arc<RadixNode<V>>> = children.to_vec();
                if present {
                    let child = Arc::clone(&next_children[rank]);
                    next_children[rank] = Self::insert_at(child, leaf, key, depth + 1);
                } else {
                    next_children.insert(rank, leaf);
                }
                let mut next_bitmap = *bitmap;
                next_bitmap[word] |= bit;
                Arc::new(RadixNode::Branch {
                    bitmap: next_bitmap,
                    children: next_children.into(),
                })
            }
        }
    }

    fn split_leaf(existing: Arc<RadixNode<V>>, new_leaf: Arc<RadixNode<V>>, new_key: u64, depth: u32) -> Arc<RadixNode<V>> {
        let existing_key = match existing.as_ref() {
            RadixNode::Leaf { key, .. } => *key,
            RadixNode::Branch { .. } => unreachable!("split on branch"),
        };
        let existing_nibble = Self::nibble(existing_key, depth);
        let new_nibble = Self::nibble(new_key, depth);
        if existing_nibble == new_nibble {
            debug_assert_ne!(existing_key, new_key);
            let deeper = if depth + 1 >= RADIX_DEPTH {
                // Impossible for distinct 64-bit keys to agree on all bytes.
                unreachable!("distinct keys collide across all nibbles")
            } else {
                Self::split_leaf(existing, new_leaf, new_key, depth + 1)
            };
            let (word, bit) = Self::bitmap_index(new_nibble);
            let mut bitmap = [0u64; 4];
            bitmap[word] |= bit;
            return Arc::new(RadixNode::Branch {
                bitmap,
                children: vec![deeper].into(),
            });
        }
        let mut bitmap = [0u64; 4];
        let mut children: Vec<Arc<RadixNode<V>>> = Vec::with_capacity(2);
        for (key, node) in [(existing_key, existing), (new_key, new_leaf)] {
            let nibble = Self::nibble(key, depth);
            let (word, bit) = Self::bitmap_index(nibble);
            let rank = Self::rank(&bitmap, word, bit);
            if bitmap[word] & bit == 0 {
                bitmap[word] |= bit;
                children.insert(rank, node);
            }
        }
        Arc::new(RadixNode::Branch {
            bitmap,
            children: children.into(),
        })
    }

    pub(crate) fn remove(&mut self, key: u64) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        match Self::remove_at(Arc::clone(&root), key, 0) {
            RemoveResult::Missing => {
                self.root = Some(root);
                false
            }
            RemoveResult::Removed(replacement) => {
                self.len -= 1;
                self.root = replacement;
                true
            }
        }
    }

    fn remove_at(node: Arc<RadixNode<V>>, key: u64, depth: u32) -> RemoveResult<V> {
        match node.as_ref() {
            RadixNode::Leaf { key: leaf_key, .. } => {
                if *leaf_key == key {
                    RemoveResult::Removed(None)
                } else {
                    RemoveResult::Missing
                }
            }
            RadixNode::Branch { bitmap, children } => {
                if depth == RADIX_DEPTH {
                    return RemoveResult::Missing;
                }
                let nibble = Self::nibble(key, depth);
                let (word, bit) = Self::bitmap_index(nibble);
                if bitmap[word] & bit == 0 {
                    return RemoveResult::Missing;
                }
                let rank = Self::rank(bitmap, word, bit);
                match Self::remove_at(Arc::clone(&children[rank]), key, depth + 1) {
                    RemoveResult::Missing => RemoveResult::Missing,
                    RemoveResult::Removed(replacement) => {
                        let mut next_children: Vec<Arc<RadixNode<V>>> = children.to_vec();
                        let mut next_bitmap = *bitmap;
                        match replacement {
                            Some(child) => next_children[rank] = child,
                            None => {
                                next_children.remove(rank);
                                next_bitmap[word] &= !bit;
                            }
                        }
                        if next_children.is_empty() {
                            RemoveResult::Removed(None)
                        } else if next_children.len() == 1 && matches!(next_children[0].as_ref(), RadixNode::Leaf { .. }) && depth > 0 {
                            // Collapse single-leaf branches back into their parent's slot.
                            let only = Arc::clone(&next_children[0]);
                            RemoveResult::Removed(Some(only))
                        } else {
                            RemoveResult::Removed(Some(Arc::new(RadixNode::Branch {
                                bitmap: next_bitmap,
                                children: next_children.into(),
                            })))
                        }
                    }
                }
            }
        }
    }

    /// Ascending-key iteration over the whole map.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, &V)> {
        let mut stack: Vec<&RadixNode<V>> = Vec::new();
        if let Some(root) = &self.root {
            stack.push(root.as_ref());
        }
        std::iter::from_fn(move || loop {
            let node = stack.pop()?;
            match node {
                RadixNode::Leaf { key, value } => return Some((*key, value)),
                RadixNode::Branch { children, .. } => {
                    for child in children.iter().rev() {
                        stack.push(child.as_ref());
                    }
                }
            }
        })
    }
}

enum RemoveResult<V> {
    Removed(Option<Arc<RadixNode<V>>>),
    Missing,
}

// ---------------------------------------------------------------------------
// Hamt
// ---------------------------------------------------------------------------

/// A persistent hash-array-mapped trie over [`TrieKey`]s.
///
/// Thirty-two-way branching on five-bit fragments; at most thirteen levels
/// cover a `u64`. Keys sharing every fragment land in one terminal bucket
/// that resolves by exact [`TrieKey::trie_eq`].
#[derive(Debug, Clone)]
pub(crate) struct Hamt<K: TrieKey, V: Clone> {
    root: Option<Arc<HamtNode<K, V>>>,
    len: usize,
}

const HAMT_FRAGMENT_BITS: u32 = 5;
const HAMT_MAX_DEPTH: u32 = 13; // ceil(64 / 5)

#[derive(Debug)]
enum HamtNode<K: TrieKey, V: Clone> {
    Bucket(Arc<[HamtEntry<K, V>]>),
    Branch {
        bitmap: u32,
        children: Arc<[Arc<HamtNode<K, V>>]>,
    },
}

#[derive(Debug, Clone)]
struct HamtEntry<K: TrieKey, V>
where
    K: TrieKey,
    V: Clone,
{
    hash: u64,
    key: K,
    value: V,
}

impl<K: TrieKey, V: Clone> Default for Hamt<K, V> {
    fn default() -> Self {
        Self {
            root: None,
            len: 0,
        }
    }
}

impl<K: TrieKey, V: Clone> Hamt<K, V> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn fragment(hash: u64, depth: u32) -> u32 {
        ((hash >> (depth * HAMT_FRAGMENT_BITS)) & 0x1F) as u32
    }

    fn rank(bitmap: u32, fragment: u32) -> usize {
        (bitmap & ((1u32 << fragment) - 1)).count_ones() as usize
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        let mut node = self.root.as_deref()?;
        let hash = key.trie_hash();
        for depth in 0..HAMT_MAX_DEPTH {
            match node {
                HamtNode::Bucket(entries) => {
                    return entries
                        .iter()
                        .find(|entry| entry.hash == hash && entry.key.trie_eq(key))
                        .map(|entry| &entry.value);
                }
                HamtNode::Branch { bitmap, children } => {
                    let fragment = Self::fragment(hash, depth);
                    if bitmap & (1 << fragment) == 0 {
                        return None;
                    }
                    node = children[Self::rank(*bitmap, fragment)].as_ref();
                }
            }
        }
        None
    }

    /// Inserts or replaces one entry keyed by exact [`TrieKey::trie_eq`].
    pub(crate) fn insert(&mut self, key: K, value: V) {
        let replacing = self.get(&key).is_some();
        let hash = key.trie_hash();
        let entry = HamtEntry { hash, key, value };
        match self.root.take() {
            None => {
                self.root = Some(Arc::new(HamtNode::Bucket(vec![entry].into())));
            }
            Some(root) => self.root = Some(Self::insert_at(root, entry, 0)),
        }
        if !replacing {
            self.len += 1;
        }
    }

    fn replace_in_bucket(entries: &Arc<[HamtEntry<K, V>]>, entry: HamtEntry<K, V>) -> Option<Arc<HamtNode<K, V>>> {
        let mut next: Vec<HamtEntry<K, V>> = entries.to_vec();
        for slot in next.iter_mut() {
            if slot.hash == entry.hash && slot.key.trie_eq(&entry.key) {
                *slot = entry;
                return Some(Arc::new(HamtNode::Bucket(next.into())));
            }
        }
        None
    }

    fn insert_at(node: Arc<HamtNode<K, V>>, entry: HamtEntry<K, V>, depth: u32) -> Arc<HamtNode<K, V>> {
        match node.as_ref() {
            HamtNode::Bucket(entries) => {
                // Exact-key replacement takes precedence over growth.
                if let Some(replaced) = Self::replace_in_bucket(entries, entry.clone()) {
                    return replaced;
                }
                let same_hash = entries.iter().all(|existing| existing.hash == entry.hash);
                let mut next: Vec<HamtEntry<K, V>> = entries.to_vec();
                if same_hash || depth + 1 >= HAMT_MAX_DEPTH {
                    next.push(entry);
                    return Arc::new(HamtNode::Bucket(next.into()));
                }
                // Split the bucket across the differing fragment.
                let mut bitmap = 0u32;
                let mut groups: Vec<(u32, Vec<HamtEntry<K, V>>)> = Vec::new();
                for existing in next.drain(..) {
                    let fragment = Self::fragment(existing.hash, depth);
                    bitmap |= 1 << fragment;
                    match groups.iter_mut().find(|(f, _)| *f == fragment) {
                        Some((_, group)) => group.push(existing),
                        None => groups.push((fragment, vec![existing])),
                    }
                }
                let entry_fragment = Self::fragment(entry.hash, depth);
                bitmap |= 1 << entry_fragment;
                match groups.iter_mut().find(|(f, _)| *f == entry_fragment) {
                    Some((_, group)) => group.push(entry),
                    None => groups.push((entry_fragment, vec![entry])),
                }
                // Children must sit in ascending fragment order so bitmap
                // rank indexing stays valid.
                groups.sort_by_key(|(fragment, _)| *fragment);
                let mut children: Vec<Arc<HamtNode<K, V>>> = Vec::with_capacity(groups.len());
                for (_, group) in groups {
                    children.push(Arc::new(HamtNode::Bucket(group.into())));
                }
                Arc::new(HamtNode::Branch {
                    bitmap,
                    children: children.into(),
                })
            }
            HamtNode::Branch { bitmap, children } => {
                let fragment = Self::fragment(entry.hash, depth);
                let present = bitmap & (1 << fragment) != 0;
                let rank = Self::rank(*bitmap, fragment);
                let mut next_children: Vec<Arc<HamtNode<K, V>>> = children.to_vec();
                if present {
                    let child = Arc::clone(&next_children[rank]);
                    next_children[rank] = Self::insert_at(child, entry, depth + 1);
                } else {
                    next_children.insert(rank, Arc::new(HamtNode::Bucket(vec![entry].into())));
                }
                let next_bitmap = *bitmap | (1 << fragment);
                Arc::new(HamtNode::Branch {
                    bitmap: next_bitmap,
                    children: next_children.into(),
                })
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &K) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        match Self::remove_at(Arc::clone(&root), key, 0) {
            RemoveOutcome::Missing => {
                self.root = Some(root);
                false
            }
            RemoveOutcome::Removed(replacement) => {
                self.len -= 1;
                self.root = replacement;
                true
            }
        }
    }

    fn remove_at(node: Arc<HamtNode<K, V>>, key: &K, depth: u32) -> RemoveOutcome<K, V> {
        let hash = key.trie_hash();
        match node.as_ref() {
            HamtNode::Bucket(entries) => {
                let position = entries
                    .iter()
                    .position(|entry| entry.hash == hash && entry.key.trie_eq(key));
                match position {
                    None => RemoveOutcome::Missing,
                    Some(index) => {
                        let mut next: Vec<HamtEntry<K, V>> = entries.to_vec();
                        next.remove(index);
                        RemoveOutcome::Removed(
                            (!next.is_empty()).then(|| Arc::new(HamtNode::Bucket(next.into()))),
                        )
                    }
                }
            }
            HamtNode::Branch { bitmap, children } => {
                let fragment = Self::fragment(hash, depth);
                if bitmap & (1 << fragment) == 0 {
                    return RemoveOutcome::Missing;
                }
                let rank = Self::rank(*bitmap, fragment);
                match Self::remove_at(Arc::clone(&children[rank]), key, depth + 1) {
                    RemoveOutcome::Missing => RemoveOutcome::Missing,
                    RemoveOutcome::Removed(replacement) => {
                        let mut next_bitmap = *bitmap;
                        let mut next_children: Vec<Arc<HamtNode<K, V>>> = children.to_vec();
                        match replacement {
                            Some(child) => next_children[rank] = child,
                            None => {
                                next_children.remove(rank);
                                next_bitmap &= !(1 << fragment);
                            }
                        }
                        if next_children.is_empty() {
                            RemoveOutcome::Removed(None)
                        } else {
                            // Collapse singleton buckets upward so chains do
                            // not linger after pruning.
                            if next_children.len() == 1
                                && let HamtNode::Bucket(_) = next_children[0].as_ref()
                                && !matches!(node.as_ref(), HamtNode::Bucket(_))
                            {
                                let only = Arc::clone(&next_children[0]);
                                return RemoveOutcome::Removed(Some(only));
                            }
                            RemoveOutcome::Removed(Some(Arc::new(HamtNode::Branch {
                                bitmap: next_bitmap,
                                children: next_children.into(),
                            })))
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        enum Frame<'a, K: TrieKey, V: Clone> {
            Node(&'a HamtNode<K, V>),
            Bucket { entries: &'a [HamtEntry<K, V>], next: usize },
        }
        let mut stack: Vec<Frame<K, V>> = Vec::new();
        // Frames borrow shared nodes; no cloning occurs during iteration.
        if let Some(root) = &self.root {
            stack.push(Frame::Node(root.as_ref()));
        }
        std::iter::from_fn(move || loop {
            let mut frame = stack.pop()?;
            loop {
                match frame {
                    Frame::Node(HamtNode::Branch { children, .. }) => {
                        for child in children.iter().rev() {
                            stack.push(Frame::Node(child.as_ref()));
                        }
                        break;
                    }
                    Frame::Node(HamtNode::Bucket(entries)) => {
                        frame = Frame::Bucket { entries, next: 0 };
                    }
                    Frame::Bucket { entries, next } => {
                        if next >= entries.len() {
                            break;
                        }
                        stack.push(Frame::Bucket { entries, next: next + 1 });
                        return Some((&entries[next].key, &entries[next].value));
                    }
                }
            }
        })
    }
}

enum RemoveOutcome<K: TrieKey, V: Clone> {
    Removed(Option<Arc<HamtNode<K, V>>>),
    Missing,
}

// ---------------------------------------------------------------------------
// Erased keys
// ---------------------------------------------------------------------------

/// The erased identity of one fact: view plus key plus its cached hash.
///
/// `Hash` covers `(view, cached hash)` only; `Eq` additionally calls
/// [`KeyValue::eq_value`], so collisions cannot alias facts.
#[derive(Clone)]
pub(crate) struct ErasedFactKey {
    pub(crate) view: TypeId,
    pub(crate) hash: u64,
    pub(crate) key: Arc<dyn KeyValue>,
}

impl ErasedFactKey {
    pub(crate) fn new(view: TypeId, key: Arc<dyn KeyValue>) -> Self {
        Self {
            view,
            hash: key.hash_value(),
            key,
        }
    }
}

impl PartialEq for ErasedFactKey {
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view
            && self.hash == other.hash
            && self.key.eq_value(other.key.as_ref())
    }
}
impl Eq for ErasedFactKey {}

impl std::hash::Hash for ErasedFactKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.view.hash(state);
        self.hash.hash(state);
    }
}

impl std::fmt::Debug for ErasedFactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedFactKey")
            .field("view", &self.view)
            .field("hash", &self.hash)
            .finish()
    }
}

impl TrieKey for ErasedFactKey {
    fn trie_hash(&self) -> u64 {
        mix_view_hash(self.view, self.hash)
    }

    fn trie_eq(&self, other: &Self) -> bool {
        self.view == other.view && self.key.eq_value(other.key.as_ref())
    }
}

fn mix_view_hash(view: TypeId, hash: u64) -> u64 {
    // TypeIds are stable within a process; fold their hashed bits in.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&view, &mut hasher);
    let view_bits = std::hash::Hasher::finish(&mut hasher);
    splitmix(view_bits ^ hash.rotate_left(17))
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

// ---------------------------------------------------------------------------
// Owner sets
// ---------------------------------------------------------------------------

/// A persistent set of owner ids; membership checks are point queries.
#[derive(Debug, Clone, Default)]
pub(crate) struct OwnerSet {
    members: Hamt<OwnerIdKey, ()>,
}

#[derive(Clone, Debug)]
struct OwnerIdKey(u64);

impl TrieKey for OwnerIdKey {
    fn trie_hash(&self) -> u64 {
        splitmix(self.0)
    }

    fn trie_eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl OwnerSet {
    pub(crate) fn insert(&mut self, owner: u64) {
        self.members.insert(OwnerIdKey(owner), ());
    }

    pub(crate) fn remove(&mut self, owner: u64) {
        self.members.remove(&OwnerIdKey(owner));
    }

    pub(crate) fn contains(&self, owner: u64) -> bool {
        self.members.get(&OwnerIdKey(owner)).is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.members.iter().map(|(key, ())| key.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_map_insert_get_iter_remove_roundtrip() {
        let mut map = RadixMap::default();
        for ordinal in 0..2_000u64 {
            map.insert(ordinal * 7 + 3, format!("v{ordinal}"));
        }
        assert_eq!(map.len(), 2_000);
        for ordinal in 0..2_000u64 {
            assert_eq!(
                map.get(ordinal * 7 + 3).map(String::as_str),
                Some(format!("v{ordinal}").as_str())
            );
        }
        assert_eq!(map.get(5), None);

        // Ascending iteration despite sequential insertion order.
        let keys: Vec<u64> = map.iter().map(|(k, _)| k).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);

        // Persistent snapshots observe their own revision.
        let snapshot_keys: Vec<u64> = map.iter().map(|(k, _)| k).collect();
        for ordinal in 0..1_000u64 {
            map.remove(ordinal * 7 + 3);
        }
        assert_eq!(map.len(), 1_000);
        for key in snapshot_keys.iter().take(500) {
            assert!(map.get(*key).is_none(), "{key} should be removed");
        }
        for ordinal in 1_000..2_000u64 {
            assert!(map.get(ordinal * 7 + 3).is_some());
        }
    }

    #[test]
    fn radix_map_shuffled_keys_stay_ordered_and_exact() {
        let mut map = RadixMap::default();
        let keys: Vec<u64> = (0..5_000u64)
            .map(|index| splitmix(index.wrapping_mul(0x51ED270B + index)))
            .collect();
        for (index, key) in keys.iter().enumerate() {
            map.insert(*key, index);
        }
        assert_eq!(map.len(), keys.len());
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(map.get(*key), Some(&index));
        }
        let walked: Vec<u64> = map.iter().map(|(k, _)| k).collect();
        let mut sorted = walked.clone();
        sorted.sort_unstable();
        assert_eq!(walked, sorted);
        for key in &keys {
            assert!(map.remove(*key));
        }
        assert!(map.is_empty());
    }

    fn probe_key(index: u64, hash: u64) -> SnapshotKeyForTest {
        SnapshotKeyForTest { index, hash }
    }

    #[derive(Debug, Clone)]
    struct SnapshotKeyForTest {
        index: u64,
        hash: u64,
    }

    impl TrieKey for SnapshotKeyForTest {
        fn trie_hash(&self) -> u64 {
            self.hash
        }

        fn trie_eq(&self, other: &Self) -> bool {
            self.index == other.index
        }
    }

    #[test]
    fn hamt_handles_collisions_removal_and_depth() {
        let mut map = Hamt::default();
        // Identical hashes force terminal-bucket resolution.
        for index in 0..50u64 {
            map.insert(probe_key(index, 42), index as i64);
        }
        assert_eq!(map.len(), 50);
        for index in 0..50u64 {
            assert_eq!(map.get(&probe_key(index, 42)), Some(&(index as i64)));
        }
        assert!(map.get(&probe_key(999, 42)).is_none());
        for index in 0..50u64 {
            assert!(map.remove(&probe_key(index, 42)));
        }
        assert!(map.is_empty());

        // Distinct hashes exercise deep paths.
        for index in 0..4_000u64 {
            map.insert(probe_key(index, splitmix(index)), -(index as i64));
        }
        assert_eq!(map.len(), 4_000);
        for index in 0..4_000u64 {
            assert_eq!(map.get(&probe_key(index, splitmix(index))), Some(&-(index as i64)));
        }
        // A different hash makes a different key even when trie_eq would
        // compare equal: canonical cached hashes are part of the address.
        map.insert(probe_key(60_000, splitmix(3_999)), -7);
        assert_eq!(map.len(), 4_001);
        assert_eq!(map.get(&probe_key(60_000, splitmix(3_999))), Some(&-7));
        assert!(map.remove(&probe_key(0, splitmix(0))));
        assert!(map.get(&probe_key(0, splitmix(0))).is_none());
    }

    #[test]
    fn owner_set_membership_roundtrip() {
        const EXTERNAL: u64 = u64::MAX;
        let mut set = OwnerSet::default();
        set.insert(1);
        set.insert(EXTERNAL);
        set.insert(7);
        assert!(set.contains(1));
        assert!(set.contains(EXTERNAL));
        assert!(set.contains(7));
        assert!(!set.contains(2));
        set.remove(1);
        assert!(!set.contains(1));
        assert_eq!(set.len(), 2);
        set.remove(EXTERNAL);
        set.remove(7);
        assert!(set.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Indexed fact state (plan §5.1)
// ---------------------------------------------------------------------------

/// One writer of a fact: invocation id or the external writer sentinel.
#[derive(Clone, Debug)]
pub(crate) struct FactOwner {
    pub(crate) id: u64,
    pub(crate) name: String,
}

/// One fact value with its exact owner set.
#[derive(Clone)]
pub(crate) struct PlainFact {
    pub(crate) view: TypeId,
    pub(crate) name: &'static str,
    pub(crate) key: Arc<dyn KeyValue>,
    pub(crate) value: Arc<dyn Value>,
    pub(crate) writers: OwnerSet,
    pub(crate) owner_names: Vec<(u64, String)>,
    pub(crate) shared: bool,
}

impl std::fmt::Debug for PlainFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainFact")
            .field("view", &self.view)
            .field("name", &self.name)
            .field("shared", &self.shared)
            .field("writers", &self.writers.len())
            .finish()
    }
}

/// One occupied slot: an immutable ordinal plus the fact.
#[derive(Clone, Debug)]
pub(crate) struct FactSlot {
    pub(crate) ordinal: u64,
    pub(crate) fact: PlainFact,
}

/// The indexed mutable fact store.
///
/// Slots are an arena with a free list; ordinals are never reused. All three
/// secondary indexes are plain maps maintained alongside the slots; the
/// command journal restores them from recorded pre-state on rollback.
/// Enumeration always walks ordinal-ordered structures, never hash order.
#[derive(Clone, Default)]
pub(crate) struct PlainState {
    pub(crate) slots: Vec<Option<FactSlot>>,
    pub(crate) free: Vec<usize>,
    /// Hash-bucketed slot indexes; buckets resolve by exact key equality,
    /// so lookups never clone user keys.
    pub(crate) by_key: HashMap<u64, Vec<usize>>,
    pub(crate) by_view: HashMap<TypeId, BTreeMap<u64, usize>>,
    pub(crate) by_owner: HashMap<u64, BTreeMap<u64, usize>>,
    pub(crate) next_ordinal: u64,
}

/// Mixed lookup hash for one fact identity.
fn fact_hash(view: TypeId, key: &dyn KeyValue) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&view, &mut hasher);
    let view_bits = std::hash::Hasher::finish(&mut hasher);
    splitmix(view_bits ^ key.hash_value().rotate_left(17))
}

pub(crate) type FactChange = crate::reactive::plain::FactChange;

impl PlainState {
    fn index_slot(&mut self, index: usize) {
        let slot = self.slots[index]
            .as_ref()
            .expect("indexed slot is occupied");
        let hash = fact_hash(slot.fact.view, slot.fact.key.as_ref());
        self.by_key.entry(hash).or_default().push(index);
        self.by_view
            .entry(slot.fact.view)
            .or_default()
            .insert(slot.ordinal, index);
        for owner in slot.fact.writers.iter() {
            self.by_owner
                .entry(owner)
                .or_default()
                .insert(slot.ordinal, index);
        }
    }

    fn unindex_slot(&mut self, index: usize) {
        let Some(Some(slot)) = self.slots.get(index) else {
            return;
        };
        let hash = fact_hash(slot.fact.view, slot.fact.key.as_ref());
        if let Some(bucket) = self.by_key.get_mut(&hash) {
            bucket.retain(|candidate| *candidate != index);
            if bucket.is_empty() {
                self.by_key.remove(&hash);
            }
        }
        if let Some(view) = self.by_view.get_mut(&slot.fact.view) {
            view.remove(&slot.ordinal);
        }
        for owner in slot.fact.writers.iter() {
            if let Some(owners) = self.by_owner.get_mut(&owner) {
                owners.remove(&slot.ordinal);
            }
        }
    }

    /// Resolves the occupied slot index for one exact fact identity.
    pub(crate) fn slot_index(&self, view: TypeId, key: &dyn KeyValue) -> Option<usize> {
        let hash = fact_hash(view, key);
        let bucket = self.by_key.get(&hash)?;
        bucket
            .iter()
            .copied()
            .find(|index| match self.slots[*index].as_ref() {
                Some(slot) => slot.fact.view == view && slot.fact.key.eq_value(key),
                None => false,
            })
    }

    pub(crate) fn slot(&self, view: TypeId, key: &dyn KeyValue) -> Option<&FactSlot> {
        self.slot_index(view, key).and_then(|index| self.slots[index].as_ref())
    }

    pub(crate) fn read(&self, view: TypeId, key: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        record_command_metric::<EngineWork>(|work| {
            work.fact_reads += 1;
        });
        self.slot(view, key)
            .map(|slot| Arc::clone(&slot.fact.value))
    }

    pub(crate) fn inputs<V: crate::reactive::view::View>(&self) -> Vec<V::Input> {
        record_command_metric::<EngineWork>(|work| {
            work.view_enumerations += 1;
        });
        let Some(view) = self.by_view.get(&TypeId::of::<V>()) else {
            return Vec::new();
        };
        view.values()
            .filter_map(|index| {
                let slot = self.slots[*index].as_ref()?;
                slot.fact.key.as_any().downcast_ref::<V::Input>().cloned()
            })
            .collect()
    }

    /// Every occupied slot index of one view in ordinal order.
    pub(crate) fn view_slots(&self, view: TypeId) -> Vec<usize> {
        self.by_view
            .get(&view)
            .map(|ordinals| ordinals.values().copied().collect())
            .unwrap_or_default()
    }

    pub(crate) fn slot_at(&self, index: usize) -> Option<&FactSlot> {
        self.slots.get(index).and_then(|slot| slot.as_ref())
    }

    pub(crate) fn len(&self) -> usize {
        self.by_key.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Reserves (or reuses) a slot for a new fact and indexes it.
    pub(crate) fn insert_fact(&mut self, fact: PlainFact) -> (usize, u64) {
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let index = match self.free.pop() {
            Some(index) => {
                self.slots[index] = Some(FactSlot { ordinal, fact });
                index
            }
            None => {
                self.slots.push(Some(FactSlot { ordinal, fact }));
                self.slots.len() - 1
            }
        };
        self.index_slot(index);
        (index, ordinal)
    }

    /// Replaces the value of one occupied slot, reindexing nothing (identity
    /// and owners unchanged).
    pub(crate) fn set_value(&mut self, index: usize, value: Arc<dyn Value>) {
        if let Some(slot) = self.slots[index].as_mut() {
            slot.fact.value = value;
        }
    }

    /// Replaces one slot wholesale (used by the journal to stage finals).
    pub(crate) fn put_slot(&mut self, index: usize, slot: Option<FactSlot>) {
        match slot {
            Some(slot) => {
                self.unindex_slot(index);
                self.slots[index] = Some(slot);
                self.index_slot(index);
            }
            None => {
                self.unindex_slot(index);
                let was_some = self.slots[index].is_some();
                self.slots[index] = None;
                if was_some {
                    self.free.push(index);
                }
            }
        }
    }

    /// Adds one writer to a slot without touching value or indexes beyond
    /// the owner map.
    pub(crate) fn add_writer(&mut self, index: usize, owner: FactOwner) {
        let Some(slot) = self.slots[index].as_mut() else {
            return;
        };
        let id = owner.id;
        slot.fact.writers.insert(id);
        slot.fact.owner_names.push((id, owner.name));
        self.by_owner
            .entry(id)
            .or_default()
            .insert(slot.ordinal, index);
    }

    /// Removes one writer from a slot; drops the slot when it was the last.
    pub(crate) fn remove_writer(&mut self, index: usize, owner: u64) -> bool {
        let Some(slot) = self.slots[index].as_mut() else {
            return false;
        };
        slot.fact.writers.remove(owner);
        slot.fact.owner_names.retain(|(id, _)| *id != owner);
        if let Some(owners) = self.by_owner.get_mut(&owner) {
            owners.remove(&slot.ordinal);
        }
        if slot.fact.writers.is_empty() {
            self.unindex_slot(index);
            self.slots[index] = None;
            self.free.push(index);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Fact journal (plan §5.1)
// ---------------------------------------------------------------------------

/// One touched key's command-local record: the shared pre-command slot, the
/// staged final slot, and whether the command reserved the slot.
pub(crate) struct JournalEntry {
    pub(crate) key: ErasedFactKey,
    pub(crate) first: Option<FactSlot>,
    pub(crate) staged: Option<FactSlot>,
    pub(crate) reserved_slot: bool,
}

/// The coalescing command journal.
///
/// First touch clone-shares the pre-command slot; later writes only restage
/// the final state. `A -> B -> A` therefore commits nothing: the final
/// value equals the first value. Rollback replays entries in reverse
/// first-touch order, restoring slots and every secondary index from the
/// recorded pre-state alone.
#[derive(Default)]
pub(crate) struct FactJournal {
    entries: Vec<JournalEntry>,
    index: HashMap<ErasedFactKey, usize>,
}

impl FactJournal {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn touched(&self) -> usize {
        self.entries.len()
    }

    fn position(&self, key: &ErasedFactKey) -> Option<usize> {
        self.index.get(key).copied()
    }

    /// The staged final slot if the command already touched this key,
    /// otherwise `None` (which also covers "touched and now absent").
    fn staged_slot<'a>(&'a self, state: &'a PlainState, key: &ErasedFactKey) -> Option<&'a Option<FactSlot>> {
        match self.position(key) {
            Some(position) => Some(&self.entries[position].staged),
            None => state
                .slot_index(key.view, key.key.as_ref())
                .map(|index| &state.slots[index]),
        }
    }

    fn first_touch(&mut self, state: &PlainState, key: ErasedFactKey) -> usize {
        if let Some(position) = self.position(&key) {
            return position;
        }
        let first = state.slot(key.view, key.key.as_ref()).cloned();
        let position = self.entries.len();
        let staged = first.clone();
        self.entries.push(JournalEntry {
            key: key.clone(),
            first,
            staged,
            reserved_slot: false,
        });
        self.index.insert(key, position);
        position
    }

    /// Applies one candidate write through the journal, returning a round
    /// delta when the staged value actually changed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write(
        &mut self,
        state: &mut PlainState,
        view: TypeId,
        name: &'static str,
        key: Arc<dyn KeyValue>,
        value: Option<Arc<dyn Value>>,
        writer: u64,
        writer_name: &str,
        shareable: bool,
    ) -> Result<Option<FactChange>> {
        let erased = ErasedFactKey::new(view, Arc::clone(&key));
        record_command_metric::<EngineWork>(|work| {
            work.fact_writes += 1;
        });

        // Resolve the current effective slot: staged if touched, else live.
        let current: Option<FactSlot> = match self.position(&erased) {
            Some(position) => self.entries[position].staged.clone(),
            None => state.slot(view, key.as_ref()).cloned(),
        };

        let next: Option<FactSlot> = match current {
            None => {
                let Some(value) = value else {
                    return Ok(None);
                };
                let position = self.first_touch(state, erased.clone());
                self.entries[position].reserved_slot = true;
                Some(FactSlot {
                    ordinal: 0, // assigned on apply
                    fact: PlainFact {
                        view,
                        name,
                        key,
                        value,
                        writers: OwnerSet::default(),
                        owner_names: Vec::new(),
                        shared: shareable,
                    },
                })
                .map(|mut slot| {
                    slot.fact.writers.insert(writer);
                    slot.fact.owner_names.push((writer, writer_name.to_string()));
                    slot
                })
            }
            Some(mut slot) => {
                let owner_present = slot.fact.writers.contains(writer);
                                if !owner_present && !(slot.fact.shared && shareable) {
                    return Err(Error::conflicting_write(
                        name,
                        key.as_ref(),
                        writer_name,
                        &slot.fact.owner_names,
                    ));
                }
                if !owner_present {
                    let Some(next) = value.as_ref() else {
                        return Ok(None);
                    };
                    if !slot.fact.value.value_eq(next.as_ref()) {
                                                return Err(Error::conflicting_write(
                            name,
                            key.as_ref(),
                            writer_name,
                            &slot.fact.owner_names,
                        ));
                    }
                    slot.fact.writers.insert(writer);
                    slot.fact.owner_names.push((writer, writer_name.to_string()));
                    Some(slot)
                } else {
                    let changed = match &value {
                        Some(next) => !slot.fact.value.value_eq(next.as_ref()),
                        None => true,
                    };
                    if !changed {
                        return Ok(None);
                    }
                    if slot.fact.shared && slot.fact.writers.len() > 1
                        && let Some(next) = value.as_ref()
                        && !slot.fact.value.value_eq(next.as_ref())
                    {
                        return Err(Error::conflicting_write(
                            name,
                            key.as_ref(),
                            writer_name,
                            &slot.fact.owner_names,
                        ));
                    }
                    match value {
                        Some(next) => slot.fact.value = next,
                        None => {
                            if slot.fact.shared && slot.fact.writers.len() > 1 {
                                slot.fact.writers.remove(writer);
                                slot.fact.owner_names.retain(|(id, _)| *id != writer);
                            } else {
                                // Full removal; slot freed at apply time.
                                return self.stage_absent(state, erased);
                            }
                        }
                    }
                    Some(slot)
                }
            }
        };

        let change = self.stage(state, erased, next)?;
        Ok(change)
    }

    /// Installs one pre-built slot (root promotion): value conflicts fail
    /// with both owner lists; equal values union writers.
    pub(crate) fn install(&mut self, state: &mut PlainState, slot: FactSlot) -> Result<Option<FactChange>> {
        let erased = ErasedFactKey::new(slot.fact.view, Arc::clone(&slot.fact.key));
        record_command_metric::<EngineWork>(|work| {
            work.fact_writes += 1;
        });
        let current: Option<FactSlot> = match self.position(&erased) {
            Some(position) => self.entries[position].staged.clone(),
            None => state.slot(slot.fact.view, slot.fact.key.as_ref()).cloned(),
        };
        match current {
            None => self.stage(state, erased, Some(slot)),
            Some(existing) => {
                if !existing.fact.value.value_eq(slot.fact.value.as_ref())
                    || existing.fact.shared != slot.fact.shared
                {
                                        return Err(Error::ConflictingWrites {
                        view: slot.fact.name.to_string(),
                        input: format!("{:?}", slot.fact.key),
                        functions: slot
                            .fact
                            .owner_names
                            .iter()
                            .map(|(_, name)| name.clone())
                            .chain(existing.fact.owner_names.iter().map(|(_, name)| name.clone()))
                            .collect(),
                    });
                }
                let mut merged = existing.clone();
                for owner in slot.fact.writers.iter() {
                    if !merged.fact.writers.contains(owner) {
                        merged.fact.writers.insert(owner);
                        if let Some((_, name)) = slot
                            .fact
                            .owner_names
                            .iter()
                            .find(|(id, _)| *id == owner)
                        {
                            merged.fact.owner_names.push((owner, name.clone()));
                        }
                    }
                }
                self.stage(state, erased, Some(merged))
            }
        }
    }

    /// Applies one candidate retraction through the journal.
    pub(crate) fn retract(
        &mut self,
        state: &mut PlainState,
        view: TypeId,
        name: &'static str,
        key: &dyn KeyValue,
        writer: u64,
        writer_name: &str,
    ) -> Result<Option<FactChange>> {
        let erased = ErasedFactKey::new(view, key.clone_key());
        record_command_metric::<EngineWork>(|work| {
            work.fact_retractions += 1;
        });
        let current: Option<FactSlot> = match self.position(&erased) {
            Some(position) => self.entries[position].staged.clone(),
            None => state.slot(view, key).cloned(),
        };
        let Some(mut slot) = current else {
            return Ok(None);
        };
        if !slot.fact.writers.contains(writer) {
                        if slot.fact.shared {
                return Ok(None);
            }
            return Err(Error::conflicting_write(
                name,
                erased.key.as_ref(),
                writer_name,
                &slot.fact.owner_names,
            ));
        }
        if slot.fact.shared && slot.fact.writers.len() > 1 {
            slot.fact.writers.remove(writer);
            slot.fact.owner_names.retain(|(id, _)| *id != writer);
            return self.stage(state, erased, Some(slot));
        }
        self.stage_absent(state, erased)
    }

    fn stage_absent(&mut self, state: &mut PlainState, erased: ErasedFactKey) -> Result<Option<FactChange>> {
        self.stage(state, erased, None)
    }

    fn stage(
        &mut self,
        state: &mut PlainState,
        erased: ErasedFactKey,
        next: Option<FactSlot>,
    ) -> Result<Option<FactChange>> {
        let position = self.first_touch(state, erased.clone());
        let previous_staged = self.entries[position].staged.clone();

        // Round delta: did the effective value change versus the prior
        // staged (or, on first touch, live) state?
        let round_change = match (&previous_staged, &next) {
            (None, None) => false,
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (Some(previous), Some(next)) => {
                previous.fact.view != next.fact.view
                    || !previous.fact.value.value_eq(next.fact.value.as_ref())
            }
        };

        let presence_changed = matches!(
            (&previous_staged, &next),
            (None, Some(_)) | (Some(_), None)
        );
        let next = match (previous_staged.is_none(), next) {
            (true, Some(mut slot)) if slot.ordinal == 0 => {
                slot.ordinal = state.next_ordinal;
                state.next_ordinal += 1;
                Some(slot)
            }
            (_, slot) => slot,
        };

        // Apply the staged state to the mutable store immediately: the
        // journal's recorded `first` is the rollback authority.
        match (&previous_staged, &next) {
            (_, None) => {
                if let Some(index) = state.slot_index(erased.view, erased.key.as_ref()) {
                    state.put_slot(index, None);
                }
            }
            (None, Some(slot)) => {
                state.insert_fact(slot.fact.clone());
            }
            (Some(previous), Some(slot)) => {
                if let Some(index) = state.slot_index(erased.view, erased.key.as_ref()) {
                    if previous.ordinal == slot.ordinal {
                        // Owner-list changes ride along via full slot put.
                        state.put_slot(index, Some(slot.clone()));
                    } else {
                        // The live slot was freed and re-created with a new
                        // ordinal; replace it wholesale.
                        state.put_slot(index, None);
                        state.insert_fact(slot.fact.clone());
                    }
                } else {
                    state.insert_fact(slot.fact.clone());
                }
            }
        }

        self.entries[position].staged = next;

        Ok(round_change.then(|| FactChange {
            view: erased.view,
            key: erased.key,
            presence_changed,
        }))
    }

    /// The committed change list: one entry per touched key whose first and
    /// final values differ, in first-touch order.
    pub(crate) fn commit_changes(&self) -> Vec<FactChange> {
        self.entries
            .iter()
            .filter(|entry| match (&entry.first, &entry.staged) {
                (None, None) => false,
                (None, Some(_)) | (Some(_), None) => true,
                (Some(first), Some(final_slot)) => {
                    first.fact.view != final_slot.fact.view
                        || !first.fact.value.value_eq(final_slot.fact.value.as_ref())
                }
            })
            .map(|entry| {
                let presence_changed = matches!(
                    (&entry.first, &entry.staged),
                    (None, Some(_)) | (Some(_), None)
                );
                FactChange {
                    view: entry.key.view,
                    key: Arc::clone(&entry.key.key),
                    presence_changed,
                }
            })
            .collect()
    }

    /// Restores the pre-command state in reverse first-touch order.
    pub(crate) fn rollback(self, state: &mut PlainState) {
        for entry in self.entries.into_iter().rev() {
            match entry.first {
                Some(first) => {
                    // Restore the exact pre-command slot content. The slot
                    // may have been freed meanwhile; put_slot reindexes and
                    // never allocates a new ordinal.
                    if let Some(index) = state.slot_index(first.fact.view, first.fact.key.as_ref()) {
                        state.put_slot(index, Some(first));
                    } else {
                        // Slot index gone (freed and possibly reused by a
                        // LATER untouched fact is impossible: reuse only
                        // happens through this journal's own staging, which
                        // rollback rewinds first). Reinsert preserving the
                        // original ordinal.
                        let mut restored = first;
                        let index = state
                            .free
                            .pop()
                            .unwrap_or_else(|| {
                                state.slots.push(None);
                                state.slots.len() - 1
                            });
                        let ordinal = restored.ordinal;
                        restored.ordinal = ordinal;
                        state.slots[index] = Some(restored);
                        state.index_slot(index);
                    }
                }
                None => {
                    // The fact did not exist before the command; remove it.
                    if let Some(index) = state.slot_index(entry.key.view, entry.key.key.as_ref()) {
                        state.put_slot(index, None);
                    }
                }
            }
        }
    }

    /// The pre-command value for one key (Temporal::Previous resolution).
    pub(crate) fn first_value(&self, view: TypeId, key: &dyn KeyValue) -> Option<Arc<dyn Value>> {
        let erased = ErasedFactKey::new(view, key.clone_key());
        let position = self.position(&erased)?;
        self.entries[position]
            .first
            .as_ref()
            .map(|slot| Arc::clone(&slot.fact.value))
    }

    /// Whether the key was present before the command.
    pub(crate) fn first_present(&self, view: TypeId, key: &dyn KeyValue) -> Option<bool> {
        let erased = ErasedFactKey::new(view, key.clone_key());
        let position = self.position(&erased)?;
        Some(self.entries[position].first.is_some())
    }

    /// Previous-epoch inputs of one view: live keys adjusted by the journal.
    pub(crate) fn previous_inputs<V: crate::reactive::view::View>(&self, state: &PlainState) -> Vec<V::Input> {
        let mut keys = state.inputs::<V>();
        for entry in &self.entries {
            if entry.key.view != TypeId::of::<V>() {
                continue;
            }
            let downcast = entry.key.key.as_any().downcast_ref::<V::Input>().cloned();
            let Some(input) = downcast else { continue };
            match (&entry.first, &entry.staged) {
                (Some(_), Some(_)) => {}
                (Some(_), None) => keys.retain(|key| *key != input),
                (None, Some(_)) => keys.push(input),
                (None, None) => {}
            }
        }
        keys.sort_by_key(|key| std::format!("{key:?}"));
        keys.dedup();
        keys
    }
}

// ---------------------------------------------------------------------------
// Committed snapshot root (plan §5.1)
// ---------------------------------------------------------------------------

/// One committed snapshot fact payload.
#[derive(Clone)]
pub(crate) struct SnapshotEntry {
    pub(crate) key: Arc<dyn KeyValue>,
    pub(crate) value: Arc<dyn Value>,
}

impl std::fmt::Debug for SnapshotEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotEntry").finish()
    }
}

/// One view's committed fact index: exact-key lookups through the HAMT,
/// ordinal-ordered enumeration through the radix map. Values are frozen
/// `Arc` clones; later commits path-copy new roots.
#[derive(Clone, Default)]
pub(crate) struct SnapshotView {
    by_key: Hamt<SnapshotKey, u64>,
    by_ordinal: RadixMap<SnapshotEntry>,
}

/// A key inside one committed view (the view is the containing map).
#[derive(Clone)]
pub(crate) struct SnapshotKey {
    hash: u64,
    key: Arc<dyn KeyValue>,
}

impl SnapshotKey {
    pub(crate) fn new(key: &dyn KeyValue) -> Self {
        Self {
            hash: key.hash_value(),
            key: key.clone_key(),
        }
    }
}

impl TrieKey for SnapshotKey {
    fn trie_hash(&self) -> u64 {
        self.hash
    }

    fn trie_eq(&self, other: &Self) -> bool {
        self.key.eq_value(other.key.as_ref())
    }
}

impl std::fmt::Debug for SnapshotView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotView").field("len", &self.by_ordinal.len()).finish()
    }
}

impl SnapshotView {
    pub(crate) fn lookup(&self, key: &dyn KeyValue) -> Option<&SnapshotEntry> {
        let ordinal = *self.by_key.get(&SnapshotKey::new(key))?;
        self.by_ordinal.get(ordinal)
    }

    pub(crate) fn insert(&mut self, ordinal: u64, entry: SnapshotEntry) {
        // A key may migrate ordinals inside one commit when a single
        // journal entry coalesces retract + reinsert (freed slot
        // re-created). Drop the stale ordinal so enumeration never lists
        // the same key twice while lookups still resolve through by_key.
        if let Some(old) = self
            .by_key
            .get(&SnapshotKey::new(entry.key.as_ref()))
            .copied()
            && old != ordinal
        {
            self.by_ordinal.remove(old);
        }
        self.by_key
            .insert(SnapshotKey::new(entry.key.as_ref()), ordinal);
        self.by_ordinal.insert(ordinal, entry);
    }

    pub(crate) fn remove(&mut self, key: &dyn KeyValue) {
        let probe = SnapshotKey::new(key);
        if let Some(ordinal) = self.by_key.get(&probe).copied() {
            self.by_key.remove(&probe);
            self.by_ordinal.remove(ordinal);
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &SnapshotEntry> {
        self.by_ordinal.iter().map(|(_, entry)| entry)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_ordinal.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_ordinal.is_empty()
    }
}

/// The committed read index: one outer map clone per snapshot, path-copied
/// view roots per commit.
#[derive(Clone, Default)]
pub(crate) struct SnapshotRoot {
    views: Arc<HashMap<TypeId, Arc<SnapshotView>>>,
}

impl SnapshotRoot {
    /// All committed view roots (for live-fact sizing, plan §20.6).
    pub(crate) fn views(&self) -> &HashMap<TypeId, Arc<SnapshotView>> {
        &self.views
    }

    pub(crate) fn view(&self, view: TypeId) -> Option<&SnapshotView> {
        self.views.get(&view).map(|view| view.as_ref())
    }

    /// Applies one journal's committed deltas, path-copying only touched
    /// view roots and cloning the small outer map once.
    pub(crate) fn apply(&mut self, entries: &[JournalDelta]) {
        if entries.is_empty() {
            return;
        }
        let mut views = (*self.views).clone();
        for delta in entries {
            let view_arc = views
                .entry(delta.view)
                .or_default()
                .clone();
            let mut next = (*view_arc).clone();
            match &delta.final_entry {
                Some((ordinal, key, value)) => {
                    next.insert(*ordinal, SnapshotEntry {
                        key: Arc::clone(key),
                        value: Arc::clone(value),
                    });
                }
                None => next.remove(delta.key.as_ref()),
            }
            views.insert(delta.view, Arc::new(next));
        }
        self.views = Arc::new(views);
    }
}

/// One key's committed delta extracted from the journal at commit.
pub(crate) struct JournalDelta {
    pub view: TypeId,
    pub key: Arc<dyn KeyValue>,
    /// `Some((ordinal, key, value))` when present after commit.
    pub final_entry: Option<(u64, Arc<dyn KeyValue>, Arc<dyn Value>)>,
}

impl FactJournal {
    /// Extracts the committed deltas for snapshot application, skipping
    /// keys whose first and final values are equal (A -> B -> A stays cold).
    pub(crate) fn commit_deltas(&self) -> Vec<JournalDelta> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let changed = match (&entry.first, &entry.staged) {
                    (None, None) => return None,
                    (None, Some(_)) | (Some(_), None) => true,
                    (Some(first), Some(final_slot)) => {
                        first.fact.view != final_slot.fact.view
                            || !first.fact.value.value_eq(final_slot.fact.value.as_ref())
                    }
                };
                if !changed {
                    return None;
                }
                let final_entry = entry.staged.as_ref().map(|slot| {
                    (
                        slot.ordinal,
                        Arc::clone(&slot.fact.key),
                        Arc::clone(&slot.fact.value),
                    )
                });
                Some(JournalDelta {
                    view: entry.key.view,
                    key: Arc::clone(&entry.key.key),
                    final_entry,
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Dependency index and dirty queue (plan §5.2)
// ---------------------------------------------------------------------------

/// One dependency row: which invocation reads which fact coordinate.
#[derive(Clone, Debug)]
pub(crate) struct DepRow {
    pub invocation: u64,
    pub key_hash: Option<(u64, TypeId)>,
    /// Exact rows carry the full erased identity for collision checks;
    /// wildcard rows are view-scoped.
    pub exact_key: Option<Arc<dyn KeyValue>>,
}

/// Per-root reverse-dependency index keyed by erased coordinates rather
/// than fact ids, so reads of absent keys wake when those keys appear.
#[derive(Default)]
pub(crate) struct DependencyIndex {
    current_exact: HashMap<(u64, TypeId), Vec<DepRow>>,
    previous_exact: HashMap<(u64, TypeId), Vec<DepRow>>,
    current_wildcard: HashMap<TypeId, Vec<u64>>,
    previous_wildcard: HashMap<TypeId, Vec<u64>>,
    current_keyset: HashMap<TypeId, Vec<u64>>,
    previous_keyset: HashMap<TypeId, Vec<u64>>,
}

fn dep_hash(view: TypeId, key: &dyn KeyValue) -> (u64, TypeId) {
    (key.hash_value(), view)
}

impl DependencyIndex {
    fn remove_invocation(rows: &mut Vec<DepRow>, invocation: u64) {
        rows.retain(|row| row.invocation != invocation);
    }

    fn insert_row(rows: &mut Vec<DepRow>, row: DepRow) {
        if rows.iter().any(|existing| existing.invocation == row.invocation && existing.key_hash == row.key_hash) {
            return;
        }
        rows.push(row);
    }

    /// Replaces one invocation's dependency rows wholesale.
    pub(crate) fn replace(
        &mut self,
        view_reads: &[(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)],
        old: &[(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)],
        invocation: u64,
    ) {
        // Remove old rows first.
        for (view, key, temporal, keyset) in old {
            match key {
                Some(key) => {
                    let map = if *temporal {
                        &mut self.previous_exact
                    } else {
                        &mut self.current_exact
                    };
                    if let Some(rows) = map.get_mut(&dep_hash(*view, key.as_ref())) {
                        Self::remove_invocation(rows, invocation);
                    }
                }
                None if *keyset => {
                    let map = if *temporal {
                        &mut self.previous_keyset
                    } else {
                        &mut self.current_keyset
                    };
                    if let Some(rows) = map.get_mut(view) {
                        rows.retain(|row| *row != invocation);
                    }
                }
                None => {
                    let map = if *temporal {
                        &mut self.previous_wildcard
                    } else {
                        &mut self.current_wildcard
                    };
                    if let Some(rows) = map.get_mut(view) {
                        remove_invocation_rows(rows, invocation);
                    }
                }
            }
        }
        // Insert new rows.
        for (view, key, temporal, keyset) in view_reads {
            match key {
                Some(key) => {
                    let map = if *temporal {
                        &mut self.previous_exact
                    } else {
                        &mut self.current_exact
                    };
                    let rows = map.entry(dep_hash(*view, key.as_ref())).or_default();
                    Self::insert_row(
                        rows,
                        DepRow {
                            invocation,
                            key_hash: Some(dep_hash(*view, key.as_ref())),
                            exact_key: Some(Arc::clone(key)),
                        },
                    );
                }
                None if *keyset => {
                    let map = if *temporal {
                        &mut self.previous_keyset
                    } else {
                        &mut self.current_keyset
                    };
                    let rows = map.entry(*view).or_default();
                    if !rows.contains(&invocation) {
                        rows.push(invocation);
                    }
                }
                None => {
                    let map = if *temporal {
                        &mut self.previous_wildcard
                    } else {
                        &mut self.current_wildcard
                    };
                    let rows = map.entry(*view).or_default();
                    if !rows.contains(&invocation) {
                        rows.push(invocation);
                    }
                }
            }
        }
    }

    /// Removes every row belonging to one invocation.
    pub(crate) fn remove_all(
        &mut self,
        reads: &[(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)],
        invocation: u64,
    ) {
        Self::replace(self, &[], reads, invocation);
    }

    /// Exact, keyset, and wildcard readers of CURRENT changes.
    pub(crate) fn mark_current(
        &self,
        changes: &[(TypeId, Option<Arc<dyn KeyValue>>, bool)],
        mut visit: impl FnMut(u64),
    ) -> usize {
        let mut marked = 0;
        let mut seen = std::collections::HashSet::new();
        for (view, key, presence_changed) in changes {
            if let Some(key) = key {
                if let Some(rows) = self.current_exact.get(&dep_hash(*view, key.as_ref())) {
                    for row in rows {
                        if row.exact_key.as_ref().is_some_and(|stored| stored.eq_value(key.as_ref()))
                            && seen.insert(row.invocation)
                        {
                            visit(row.invocation);
                            marked += 1;
                        }
                    }
                }
            }
            if *presence_changed {
                if let Some(rows) = self.current_keyset.get(view) {
                    for invocation in rows {
                        if seen.insert(*invocation) {
                            visit(*invocation);
                            marked += 1;
                        }
                    }
                }
            }
            if let Some(rows) = self.current_wildcard.get(view) {
                for invocation in rows {
                    if seen.insert(*invocation) {
                        visit(*invocation);
                        marked += 1;
                    }
                }
            }
        }
        marked
    }

    /// Readers whose PREVIOUS-epoch reads matched the committed journal.
    pub(crate) fn mark_previous(
        &self,
        changes: &[(TypeId, Option<Arc<dyn KeyValue>>, bool)],
        mut visit: impl FnMut(u64),
    ) -> usize {
        let mut marked = 0;
        let mut seen = std::collections::HashSet::new();
        for (view, key, presence_changed) in changes {
            if let Some(key) = key {
                if let Some(rows) = self.previous_exact.get(&dep_hash(*view, key.as_ref())) {
                    for row in rows {
                        if row.exact_key.as_ref().is_some_and(|stored| stored.eq_value(key.as_ref()))
                            && seen.insert(row.invocation)
                        {
                            visit(row.invocation);
                            marked += 1;
                        }
                    }
                }
            }
            if *presence_changed {
                if let Some(rows) = self.previous_keyset.get(view) {
                    for invocation in rows {
                        if seen.insert(*invocation) {
                            visit(*invocation);
                            marked += 1;
                        }
                    }
                }
            }
            if let Some(rows) = self.previous_wildcard.get(view) {
                for invocation in rows {
                    if seen.insert(*invocation) {
                        visit(*invocation);
                        marked += 1;
                    }
                }
            }
        }
        marked
    }
}

fn remove_invocation_rows(rows: &mut Vec<u64>, invocation: u64) {
    rows.retain(|candidate| *candidate != invocation);
}

fn current_wild<'a>(map: &'a mut HashMap<TypeId, Vec<u64>>) -> &'a mut HashMap<TypeId, Vec<u64>> { map }
fn previous_wild<'a>(map: &'a mut HashMap<TypeId, Vec<u64>>) -> &'a mut HashMap<TypeId, Vec<u64>> { map }

/// Deterministic dirty ordering: root installation ordinal then invocation
/// ordinal, both monotonic and never hash-derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DirtyKey {
    pub(crate) root_install_ordinal: u64,
    pub(crate) invocation_ordinal: u64,
    pub(crate) root: u64,
    pub(crate) invocation: u64,
}

/// The ordered dirty set with membership deduplication.
#[derive(Default)]
pub(crate) struct DirtyQueue {
    ordered: std::collections::BTreeSet<DirtyKey>,
    present: std::collections::HashSet<u64>,
}

impl DirtyQueue {
    pub(crate) fn insert(&mut self, key: DirtyKey) {
        if self.present.insert(key.invocation) {
            self.ordered.insert(key);
        }
    }

    pub(crate) fn pop(&mut self) -> Option<DirtyKey> {
        let key = self.ordered.pop_first()?;
        self.present.remove(&key.invocation);
        Some(key)
    }

    pub(crate) fn clear(&mut self) {
        self.ordered.clear();
        self.present.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.ordered.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Persistent sequence (plan §5.5)
// ---------------------------------------------------------------------------

/// A clone-cheap persistent sequence: inline storage up to eight values,
/// then a path-copying 32-way vector. Indexed lookup, iteration, and an
/// explicitly allocating `to_vec`; appends cost `O(log n)` past the inline
/// window (plan §5.5).
#[derive(Debug, Clone)]
pub(crate) enum PersistentSeq<T: Clone> {
    Inline(ArrayVec8<T>),
    Tree { len: usize, root: Arc<SeqBranch<T>> },
}

#[derive(Debug)]
pub(crate) struct ArrayVec8<T> {
    values: [Option<T>; 8],
    len: u8,
}

impl<T: Clone> Default for ArrayVec8<T> {
    fn default() -> Self {
        Self { values: std::array::from_fn(|_| None), len: 0 }
    }
}

impl<T: Clone> Clone for ArrayVec8<T> {
    fn clone(&self) -> Self {
        Self { values: std::array::from_fn(|index| self.values[index].clone()), len: self.len }
    }
}

const SEQ_LEAF_CAP: usize = 32;
const SEQ_BRANCH: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct SeqBranch<T> {
    children: Vec<Arc<SeqNode<T>>>,
    /// 1 = children are leaves; each extra level multiplies capacity.
    height: u8,
}

#[derive(Debug, Clone)]
enum SeqNode<T> {
    Leaf(Arc<[T]>),
    Branch(Arc<SeqBranch<T>>),
}

impl<T: Clone> Default for PersistentSeq<T> {
    fn default() -> Self {
        PersistentSeq::Inline(ArrayVec8::default())
    }
}

impl<T: Clone> PersistentSeq<T> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            PersistentSeq::Inline(inline) => inline.len as usize,
            PersistentSeq::Tree { len, .. } => *len,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn push(&mut self, value: T) {
        match self {
            PersistentSeq::Inline(inline) => {
                if (inline.len as usize) < 8 {
                    inline.values[inline.len as usize] = Some(value);
                    inline.len += 1;
                    return;
                }
                let spilled: Vec<T> = inline
                    .values
                    .iter()
                    .take(inline.len as usize)
                    .filter_map(|slot| slot.clone())
                    .chain(std::iter::once(value))
                    .collect();
                let len = spilled.len();
                let leaf: Arc<SeqNode<T>> = Arc::new(SeqNode::Leaf(spilled.into()));
                *self = PersistentSeq::Tree {
                    len,
                    root: Arc::new(SeqBranch { children: vec![leaf], height: 1 }),
                };
            }
            PersistentSeq::Tree { len, root } => {
                let index = *len;
                let new_root = SeqBranch::push(Arc::clone(root), value, index);
                *len += 1;
                *root = new_root;
            }
        }
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        match self {
            PersistentSeq::Inline(inline) => inline.values.get(index).and_then(|slot| slot.as_ref()),
            PersistentSeq::Tree { len, root } => {
                if index >= *len {
                    return None;
                }
                SeqBranch::get(root, index)
            }
        }
    }

    pub(crate) fn iter(&self) -> IterSeq<'_, T> {
        let mut frames: Vec<IterFrame<'_, T>> = Vec::new();
        match self {
            PersistentSeq::Inline(inline) => {
                let values: &[Option<T>] = &inline.values;
                frames.push(IterFrame::Inline { values, next: 0, len: inline.len as usize });
            }
            PersistentSeq::Tree { root, .. } => {
                frames.push(IterFrame::Root(root.as_ref()));
            }
        }
        IterSeq { frames }
    }

    pub(crate) fn to_vec(&self) -> Vec<T> {
        match self {
            PersistentSeq::Inline(inline) => inline
                .values
                .iter()
                .take(inline.len as usize)
                .filter_map(|slot| slot.clone())
                .collect(),
            PersistentSeq::Tree { .. } => self.iter().cloned().collect(),
        }
    }
}

impl<T: Clone> SeqBranch<T> {
    fn capacity(height: u8) -> usize {
        let mut cap = SEQ_LEAF_CAP;
        for _ in 1..height {
            cap *= SEQ_BRANCH;
        }
        cap
    }

    fn push(node: Arc<Self>, value: T, index: usize) -> Arc<Self> {
        let mut node = (*node).clone();
        let child_cap = Self::capacity(node.height);
        let child_index = index / child_cap;
        if child_index < node.children.len() {
            let existing = Arc::clone(&node.children[child_index]);
            let replaced: SeqNode<T> = match existing.as_ref() {
                SeqNode::Leaf(values) => {
                    debug_assert_eq!(node.height, 1, "leaves sit at height one");
                    let mut next: Vec<T> = values.to_vec();
                    next.push(value);
                    SeqNode::Leaf(next.into())
                }
                SeqNode::Branch(branch) => {
                    debug_assert!(node.height > 1, "nested levels hold branches");
                    SeqNode::Branch(Self::push(Arc::clone(branch), value, index % child_cap))
                }
            };
            node.children[child_index] = Arc::new(replaced);
        } else {
            debug_assert_eq!(child_index, node.children.len(), "pushes are dense");
            let fresh: SeqNode<T> = if node.height == 1 {
                SeqNode::Leaf(vec![value].into())
            } else {
                SeqNode::Branch(Self::push(
                    Arc::new(SeqBranch { children: Vec::new(), height: node.height - 1 }),
                    value,
                    index % child_cap,
                ))
            };
            node.children.push(Arc::new(fresh));
        }
        Arc::new(node)
    }

    fn get<'a>(node: &'a SeqBranch<T>, mut index: usize) -> Option<&'a T> {
        let child_cap = Self::capacity(node.height);
        let child_index = index / child_cap;
        let child = node.children.get(child_index)?;
        index %= child_cap;
        match child.as_ref() {
            SeqNode::Leaf(values) => values.get(index),
            SeqNode::Branch(branch) => Self::get(branch, index),
        }
    }
}

enum IterFrame<'a, T> {
    Inline { values: &'a [Option<T>], next: usize, len: usize },
    Node(&'a SeqNode<T>),
    Root(&'a SeqBranch<T>),
    LeafPos { values: &'a [T], next: usize },
}

/// Borrowing iterator over a persistent sequence.
pub(crate) struct IterSeq<'a, T> {
    frames: Vec<IterFrame<'a, T>>,
}

impl<'a, T> Iterator for IterSeq<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        loop {
            let mut frame = self.frames.pop()?;
            match &mut frame {
                IterFrame::Inline { values, next, len } => {
                    if *next >= *len {
                        continue;
                    }
                    let index = *next;
                    let out = (*values)[index].as_ref();
                    let advanced = *next + 1;
                    let slice: &'a [Option<T>] = *values;
                    let total = *len;
                    self.frames
                        .push(IterFrame::Inline { values: slice, next: advanced, len: total });
                    return out;
                }
                IterFrame::Root(branch) => {
                    for child in branch.children.iter().rev() {
                        self.frames.push(IterFrame::Node(child.as_ref()));
                    }
                }
                IterFrame::Node(SeqNode::Branch(branch)) => {
                    for child in branch.children.iter().rev() {
                        self.frames.push(IterFrame::Node(child.as_ref()));
                    }
                }
                IterFrame::Node(SeqNode::Leaf(values)) => {
                    self.frames.push(IterFrame::LeafPos { values, next: 0 });
                }
                IterFrame::LeafPos { values, next } => {
                    if *next >= values.len() {
                        continue;
                    }
                    let index = *next;
                    let out = &(*values)[index];
                    let advanced = *next + 1;
                    let slice: &'a [T] = *values;
                    self.frames.push(IterFrame::LeafPos { values: slice, next: advanced });
                    return Some(out);
                }
            }
        }
    }
}


#[cfg(test)]
mod seq_tests {
    use super::*;

    #[test]
    fn persistent_seq_inline_spill_and_iteration() {
        let mut seq: PersistentSeq<usize> = PersistentSeq::new();
        for value in 0..100usize {
            seq.push(value);
        }
        assert_eq!(seq.len(), 100);
        for index in 0..100usize {
            assert_eq!(seq.get(index), Some(&index));
        }
        assert_eq!(seq.get(100), None);
        let walked: Vec<usize> = seq.iter().copied().collect();
        assert_eq!(walked, (0..100).collect::<Vec<_>>());
        // Clone-cheap: snapshots observe their own revision.
        let snapshot = seq.clone();
        seq.push(100);
        assert_eq!(snapshot.len(), 100);
        assert_eq!(seq.len(), 101);
    }

    #[test]
    fn persistent_seq_deep_levels() {
        let mut seq: PersistentSeq<u8> = PersistentSeq::new();
        for index in 0..5_000usize {
            seq.push((index % 256) as u8);
        }
        assert_eq!(seq.len(), 5_000);
        for index in [0usize, 1, 31, 32, 33, 1_023, 1_024, 4_999] {
            let expected = (index % 256) as u8;
            assert_eq!(seq.get(index), Some(&expected));
        }
        assert_eq!(seq.iter().count(), 5_000);
    }
}
