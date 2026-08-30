//! Clone-cheap persistent sequences backed by a path-copying balanced rope.
//!
//! The sequence owns no mutable backing buffer.  Leaves contain at most 32
//! values and every edit copies only the spine between the root and the
//! affected leaves; untouched subtrees remain shared by `Arc`.  Sequence
//! equality is value based, so equivalent sequences compare equal regardless
//! of their construction history or tree shape.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use smallvec::SmallVec;

const LEAF_CAP: usize = 32;

/// A foldable measure carried by every sequence node.
///
/// `M` is deliberately independent of the tree shape.  Implementations must
/// return the measure of a leaf from its values and combine measures in order
/// for a concatenation.  The default [`CountMeasure`] is sufficient for the
/// parser column sequence and is useful for most callers.
pub trait SeqMeasure<T>: Clone {
    /// Computes a leaf's measure.
    fn measure_leaf(values: &[T]) -> Self;

    /// Computes the measure of two adjacent sequences.
    fn combine(left: &Self, right: &Self) -> Self;
}

/// A sequence measure whose value also describes the number of logical
/// positions represented by one item.  Parser segment metadata uses this to
/// index columns without walking the retained segment descriptors.
pub(crate) trait SeqMeasureWeight {
    fn weight(&self) -> usize;
}

/// The number of values in a sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CountMeasure(pub usize);

impl<T> SeqMeasure<T> for CountMeasure {
    fn measure_leaf(values: &[T]) -> Self {
        Self(values.len())
    }

    fn combine(left: &Self, right: &Self) -> Self {
        Self(left.0 + right.0)
    }
}
impl SeqMeasureWeight for CountMeasure {
    fn weight(&self) -> usize {
        self.0
    }
}

enum Node<T: Clone, M: SeqMeasure<T>> {
    Leaf {
        values: Arc<[T]>,
        measure: M,
    },
    Branch {
        left: Arc<Node<T, M>>,
        right: Arc<Node<T, M>>,
        len: usize,
        height: u8,
        measure: M,
    },
}

impl<T: Clone, M: SeqMeasure<T>> Node<T, M> {
    #[inline]
    fn leaf(values: Vec<T>) -> Arc<Self> {
        debug_assert!(!values.is_empty());
        debug_assert!(values.len() <= LEAF_CAP);
        let measure = M::measure_leaf(&values);
        Arc::new(Self::Leaf {
            values: values.into(),
            measure,
        })
    }

    #[inline]
    fn branch(left: Arc<Self>, right: Arc<Self>) -> Arc<Self> {
        let len = left.len() + right.len();
        let height = left.height().max(right.height()).saturating_add(1);
        let measure = M::combine(left.measure(), right.measure());
        Arc::new(Self::Branch {
            left,
            right,
            len,
            height,
            measure,
        })
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::Leaf { values, .. } => values.len(),
            Self::Branch { len, .. } => *len,
        }
    }

    #[inline]
    fn height(&self) -> u8 {
        match self {
            Self::Leaf { .. } => 0,
            Self::Branch { height, .. } => *height,
        }
    }

    #[inline]
    fn measure(&self) -> &M {
        match self {
            Self::Leaf { measure, .. } | Self::Branch { measure, .. } => measure,
        }
    }

    fn get(&self, mut index: usize) -> Option<&T> {
        match self {
            Self::Leaf { values, .. } => values.get(index),
            Self::Branch { left, right, .. } => {
                let left_len = left.len();
                if index < left_len {
                    left.get(index)
                } else {
                    index -= left_len;
                    right.get(index)
                }
            }
        }
    }
}

/// A clone-cheap persistent sequence.
///
/// The default measure is [`CountMeasure`].  `push`, `concat`, `split_at`,
/// and `splice` preserve structural sharing and run in `O(log n)` path work,
/// apart from copying at most one 32-value leaf at a split boundary.
#[derive(Clone)]
pub struct PersistentSeq<T: Clone, M: SeqMeasure<T> = CountMeasure> {
    root: Option<Arc<Node<T, M>>>,
}

impl<T: Clone, M: SeqMeasure<T>> Default for PersistentSeq<T, M> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<T: Clone, M: SeqMeasure<T>> PersistentSeq<T, M> {
    /// Creates an empty sequence.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a balanced sequence from values in iteration order.
    pub fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        let values: Vec<T> = values.into_iter().collect();
        if values.is_empty() {
            return Self::new();
        }

        let leaves: Vec<Arc<Node<T, M>>> = values
            .chunks(LEAF_CAP)
            .map(|chunk| Node::leaf(chunk.to_vec()))
            .collect();
        Self {
            root: Some(build_balanced(&leaves)),
        }
    }

    /// Number of values in the sequence.
    #[inline]
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |root| root.len())
    }

    /// Whether the sequence has no values.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Returns the aggregate measure, if the sequence is non-empty.
    #[inline]
    pub fn measure(&self) -> Option<&M> {
        self.root.as_ref().map(|root| root.measure())
    }

    /// Appends one value, copying only the right spine.
    pub fn push(&mut self, value: T) {
        let leaf = Node::leaf(vec![value]);
        self.root = Some(match self.root.take() {
            None => leaf,
            Some(root) => append_node(root, leaf),
        });
    }

    /// Returns a value by index without materializing the sequence.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&T> {
        self.root.as_ref().and_then(|root| {
            if index < root.len() {
                root.get(index)
            } else {
                None
            }
        })
    }
    /// Returns the item containing a logical offset and that item's local
    /// offset.  The weighted descent follows only the metadata spine.
    pub(crate) fn weighted_get(&self, offset: usize) -> Option<(usize, usize, &T)>
    where
        M: SeqMeasureWeight,
    {
        let root = self.root.as_ref()?;
        if offset >= root.measure().weight() {
            return None;
        }
        weighted_get_node(root, offset, 0)
    }

    /// Returns the aggregate measure of all items after `index`.
    ///
    /// The returned value is `None` when the range is empty.  This operation
    /// is used for suffix metadata queries and never iterates untouched
    /// leaves.
    pub(crate) fn measure_after_items(&self, index: usize) -> Option<M>
    where
        M: Clone,
    {
        let root = self.root.as_ref()?;
        if index >= root.len() {
            return None;
        }
        Some(measure_after_node(root, index))
    }

    /// Returns a borrowing iterator in sequence order.
    pub fn iter(&self) -> Iter<'_, T, M> {
        let mut frames = SmallVec::new();
        if let Some(root) = self.root.as_deref() {
            frames.push(Frame::Node(root));
        }
        Iter { frames }
    }

    /// Explicitly materializes the sequence.
    pub fn to_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }

    /// Concatenates two sequences while retaining both input roots.
    pub fn concat(&self, other: &Self) -> Self {
        Self {
            root: join_options(self.root.as_ref(), other.root.as_ref()),
        }
    }

    /// Splits at `index`, returning the prefix and suffix.
    ///
    /// Panics when `index > len`, matching the standard collection slicing
    /// contract.  Splitting exactly at either boundary returns a clone-cheap
    /// empty side.
    pub fn split_at(&self, index: usize) -> (Self, Self) {
        assert!(index <= self.len(), "sequence split index out of bounds");
        match self.root.as_ref() {
            None => (Self::new(), Self::new()),
            Some(_root) if index == 0 => (Self::new(), self.clone()),
            Some(root) if index == root.len() => (self.clone(), Self::new()),
            Some(root) => {
                let (left, right) = split_node(root, index);
                (Self { root: left }, Self { root: right })
            }
        }
    }
    /// Replaces one entry while retaining every untouched subtree.
    ///
    /// This is crate-private because parser reducers use it on sealed
    /// sequences; public callers should use [`Self::splice`].  The operation
    /// copies one bounded leaf and one node per path ancestor.
    pub(crate) fn replace(&self, index: usize, value: T) -> Self {
        assert!(index < self.len(), "sequence replace index out of bounds");
        let root = self.root.as_ref().expect("non-empty sequence has a root");
        Self {
            root: Some(replace_node(root, index, value)),
        }
    }

    /// Replaces `range` with `replacement` and returns a new sequence.
    ///
    /// The untouched prefix and suffix are retained by `Arc`; only the two
    /// splice boundaries and the joining paths are rebuilt.
    pub fn splice<I>(&self, range: Range<usize>, replacement: I) -> Self
    where
        I: IntoIterator<Item = T>,
    {
        assert!(
            range.start <= range.end,
            "sequence splice range is inverted"
        );
        assert!(
            range.end <= self.len(),
            "sequence splice range out of bounds"
        );
        let (prefix, tail) = self.split_at(range.start);
        let (_, suffix) = tail.split_at(range.end - range.start);
        let replacement = Self::from_iter(replacement);
        prefix.concat(&replacement).concat(&suffix)
    }

    /// Returns whether both sequences share their exact root allocation.
    ///
    /// This is primarily useful to assert sharing in primitive tests; value
    /// equality remains the normal API.
    #[cfg(test)]
    fn root_ptr_eq(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }
}

impl<T: Clone + PartialEq, M: SeqMeasure<T>> PartialEq for PersistentSeq<T, M> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other.iter()).all(|(a, b)| a == b)
    }
}

impl<T: Clone + Eq, M: SeqMeasure<T>> Eq for PersistentSeq<T, M> {}

impl<T: Clone + Hash, M: SeqMeasure<T>> Hash for PersistentSeq<T, M> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        for value in self {
            value.hash(state);
        }
    }
}

impl<T: Clone + fmt::Debug, M: SeqMeasure<T>> fmt::Debug for PersistentSeq<T, M> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}
impl<'a, T: Clone, M: SeqMeasure<T>> IntoIterator for &'a PersistentSeq<T, M> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T, M>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Borrowing iterator over a [`PersistentSeq`].
pub struct Iter<'a, T: Clone, M: SeqMeasure<T>> {
    frames: SmallVec<[Frame<'a, T, M>; 16]>,
}

enum Frame<'a, T: Clone, M: SeqMeasure<T>> {
    Node(&'a Node<T, M>),
    Leaf { values: &'a [T], next: usize },
}

impl<'a, T: Clone, M: SeqMeasure<T>> Iterator for Iter<'a, T, M> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.frames.pop()?;
            match frame {
                Frame::Node(Node::Leaf { values, .. }) => {
                    self.frames.push(Frame::Leaf { values, next: 0 });
                }
                Frame::Node(Node::Branch { left, right, .. }) => {
                    self.frames.push(Frame::Node(right));
                    self.frames.push(Frame::Node(left));
                }
                Frame::Leaf { values, next } => {
                    let Some(value) = values.get(next) else {
                        continue;
                    };
                    self.frames.push(Frame::Leaf {
                        values,
                        next: next + 1,
                    });
                    return Some(value);
                }
            }
        }
    }
}

fn weighted_get_node<'a, T: Clone, M: SeqMeasure<T> + SeqMeasureWeight>(
    node: &'a Node<T, M>,
    mut offset: usize,
    base_index: usize,
) -> Option<(usize, usize, &'a T)> {
    match node {
        Node::Leaf { values, .. } => {
            for (index, value) in values.iter().enumerate() {
                let measure = M::measure_leaf(std::slice::from_ref(value));
                let weight = measure.weight();
                if offset < weight {
                    return Some((base_index + index, offset, value));
                }
                offset = offset.saturating_sub(weight);
            }
            None
        }
        Node::Branch { left, right, .. } => {
            let left_weight = left.measure().weight();
            if offset < left_weight {
                weighted_get_node(left, offset, base_index)
            } else {
                weighted_get_node(right, offset - left_weight, base_index + left.len())
            }
        }
    }
}

fn measure_after_node<T: Clone, M: SeqMeasure<T>>(node: &Node<T, M>, index: usize) -> M {
    if index == 0 {
        return node.measure().clone();
    }
    match node {
        Node::Leaf { values, .. } => M::measure_leaf(&values[index..]),
        Node::Branch { left, right, .. } => {
            let left_len = left.len();
            if index < left_len {
                measure_after_node(left, index)
            } else if index == left_len {
                right.measure().clone()
            } else {
                measure_after_node(right, index - left_len)
            }
        }
    }
}

fn replace_node<T: Clone, M: SeqMeasure<T>>(
    node: &Arc<Node<T, M>>,
    index: usize,
    value: T,
) -> Arc<Node<T, M>> {
    match node.as_ref() {
        Node::Leaf { values, .. } => {
            let mut next = values.to_vec();
            next[index] = value;
            Node::leaf(next)
        }
        Node::Branch { left, right, .. } => {
            let left_len = left.len();
            if index < left_len {
                Node::branch(replace_node(left, index, value), Arc::clone(right))
            } else {
                Node::branch(
                    Arc::clone(left),
                    replace_node(right, index - left_len, value),
                )
            }
        }
    }
}

fn build_balanced<T: Clone, M: SeqMeasure<T>>(nodes: &[Arc<Node<T, M>>]) -> Arc<Node<T, M>> {
    debug_assert!(!nodes.is_empty());
    if nodes.len() == 1 {
        return Arc::clone(&nodes[0]);
    }
    let midpoint = nodes.len() / 2;
    let left = build_balanced(&nodes[..midpoint]);
    let right = build_balanced(&nodes[midpoint..]);
    Node::branch(left, right)
}

fn append_node<T: Clone, M: SeqMeasure<T>>(
    root: Arc<Node<T, M>>,
    leaf: Arc<Node<T, M>>,
) -> Arc<Node<T, M>> {
    match root.as_ref() {
        Node::Leaf { values, .. } if values.len() < LEAF_CAP => {
            let mut next = values.to_vec();
            if let Node::Leaf { values: added, .. } = leaf.as_ref() {
                next.extend(added.iter().cloned());
            }
            Node::leaf(next)
        }
        Node::Leaf { .. } => Node::branch(root, leaf),
        Node::Branch { left, right, .. } => {
            let next_right = append_node(Arc::clone(right), leaf);
            join(Arc::clone(left), next_right)
        }
    }
}

fn join_options<T: Clone, M: SeqMeasure<T>>(
    left: Option<&Arc<Node<T, M>>>,
    right: Option<&Arc<Node<T, M>>>,
) -> Option<Arc<Node<T, M>>> {
    match (left, right) {
        (None, None) => None,
        (Some(node), None) | (None, Some(node)) => Some(Arc::clone(node)),
        (Some(left), Some(right)) => Some(join(Arc::clone(left), Arc::clone(right))),
    }
}

/// Joins two AVL-balanced ropes.  The taller side is followed only along its
/// boundary; no untouched subtree is traversed or copied.
fn join<T: Clone, M: SeqMeasure<T>>(
    left: Arc<Node<T, M>>,
    right: Arc<Node<T, M>>,
) -> Arc<Node<T, M>> {
    let left_height = left.height();
    let right_height = right.height();
    if left_height > right_height.saturating_add(1) {
        let Node::Branch {
            left: left_left,
            right: left_right,
            ..
        } = left.as_ref()
        else {
            return Node::branch(left, right);
        };
        let merged = join(Arc::clone(left_right), right);
        Node::branch(Arc::clone(left_left), merged)
    } else if right_height > left_height.saturating_add(1) {
        let Node::Branch {
            left: right_left,
            right: right_right,
            ..
        } = right.as_ref()
        else {
            return Node::branch(left, right);
        };
        let merged = join(left, Arc::clone(right_left));
        Node::branch(merged, Arc::clone(right_right))
    } else {
        Node::branch(left, right)
    }
}

fn split_node<T: Clone, M: SeqMeasure<T>>(
    node: &Arc<Node<T, M>>,
    index: usize,
) -> (Option<Arc<Node<T, M>>>, Option<Arc<Node<T, M>>>) {
    debug_assert!(index > 0 && index < node.len());
    match node.as_ref() {
        Node::Leaf { values, .. } => {
            let left = Node::leaf(values[..index].to_vec());
            let right = Node::leaf(values[index..].to_vec());
            (Some(left), Some(right))
        }
        Node::Branch { left, right, .. } => {
            let left_len = left.len();
            if index < left_len {
                let (prefix, split_left) = split_node(left, index);
                let suffix = join_options(split_left.as_ref(), Some(right));
                (prefix, suffix)
            } else if index == left_len {
                (Some(Arc::clone(left)), Some(Arc::clone(right)))
            } else {
                let (split_right, suffix) = split_node(right, index - left_len);
                let prefix = join_options(Some(left), split_right.as_ref());
                (prefix, suffix)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;

    #[test]
    fn append_spill_and_snapshot_sharing() {
        let mut sequence: PersistentSeq<usize> = PersistentSeq::new();
        for value in 0..100usize {
            sequence.push(value);
        }
        let snapshot = sequence.clone();
        sequence.push(100);
        assert_eq!(snapshot.len(), 100);
        assert_eq!(sequence.len(), 101);
        assert_eq!(snapshot.get(99), Some(&99));
        assert_eq!(sequence.get(100), Some(&100));
        assert!(!snapshot.root_ptr_eq(&sequence));
        assert_eq!(
            snapshot.iter().copied().collect::<Vec<_>>(),
            (0..100).collect::<Vec<_>>()
        );
    }

    #[test]
    fn split_concat_and_splice_preserve_values() {
        let sequence: PersistentSeq<usize> = PersistentSeq::from_iter(0..5_000usize);
        let (prefix, suffix) = sequence.split_at(2_345);
        assert_eq!(prefix.len(), 2_345);
        assert_eq!(suffix.len(), 2_655);
        assert_eq!(prefix.get(2_344), Some(&2_344));
        assert_eq!(suffix.get(0), Some(&2_345));
        assert_eq!(prefix.concat(&suffix), sequence);

        let replaced = sequence.splice(100..4_900, [7usize, 8, 9]);
        let mut expected: Vec<usize> = (0..100).collect();
        expected.extend([7, 8, 9]);
        expected.extend(4_900..5_000);
        assert_eq!(replaced.to_vec(), expected);
    }

    #[test]
    fn shape_independent_equality_and_hash() {
        let balanced: PersistentSeq<usize> = PersistentSeq::from_iter(0..200usize);
        let mut appended: PersistentSeq<usize> = PersistentSeq::new();
        for value in 0..200usize {
            appended.push(value);
        }
        assert_eq!(balanced, appended);
        let mut left_hash = DefaultHasher::new();
        let mut right_hash = DefaultHasher::new();
        balanced.hash(&mut left_hash);
        appended.hash(&mut right_hash);
        assert_eq!(left_hash.finish(), right_hash.finish());
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Sum(usize);

    impl SeqMeasure<usize> for Sum {
        fn measure_leaf(values: &[usize]) -> Self {
            Self(values.iter().sum())
        }

        fn combine(left: &Self, right: &Self) -> Self {
            Self(left.0 + right.0)
        }
    }

    #[test]
    fn custom_measure_follows_edits() {
        let sequence: PersistentSeq<usize, Sum> = PersistentSeq::from_iter(1..=100);
        assert_eq!(sequence.measure(), Some(&Sum(5_050)));
        let replaced = sequence.splice(10..20, [1usize, 2, 3]);
        let expected = (1..=10).sum::<usize>() + 6 + (21..=100).sum::<usize>();
        assert_eq!(replaced.measure(), Some(&Sum(expected)));
    }

    #[test]
    fn replace_preserves_untouched_values_and_measure() {
        let sequence: PersistentSeq<usize, Sum> = PersistentSeq::from_iter(0..100);
        let replaced = sequence.replace(50, 500);
        assert_eq!(replaced.get(50), Some(&500));
        assert_eq!(sequence.get(50), Some(&50));
        assert_eq!(
            replaced.measure(),
            Some(&Sum((0..100).sum::<usize>() - 50 + 500))
        );
    }
}
