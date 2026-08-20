//! View shapes and their fact algebras (§5.2).
//!
//! A view is a singleton fact space; each shape defines its exact facts
//! and the write footprint of each operation. `List` is deliberately not a
//! v1 shape: ordered sequences are ordered trees.
//!
//! The shape is a *concrete* associated type ([`BoxShape`], [`MapShape`],
//! [`TreeShape`], [`GraphShape`]), so the authoring surface can implement
//! each shape's handle methods on provably disjoint bounds and still use
//! the same method names across shapes.

use crate::reactive::store::{BoxStore, DynStore, GraphStore, MapStore, TreeStore};
use std::sync::Arc;
#[allow(unused_imports)] // used by the in-crate view tests
use crate::reactive::value::{KeySpec, KeyValue, Value};

/// The structural shape of a view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Box,
    Map,
    Tree,
    /// An abstract tree lowers onto the exact Tree facts (§5.2).
    AbstractTree,
    Graph,
}

/// The marker implemented by the concrete shape types.
pub trait ShapeSpec: 'static + Send + Sync {
    const KIND: ShapeKind;
}

/// The Box shape: exactly one fact, the value itself.
pub struct BoxShape;
impl ShapeSpec for BoxShape {
    const KIND: ShapeKind = ShapeKind::Box;
}

/// The Map shape: an ordered key registry plus one entry fact per key.
pub struct MapShape;
impl ShapeSpec for MapShape {
    const KIND: ShapeKind = ShapeKind::Map;
}

/// The Tree shape: ordered roots, node facts, ordered-children facts, and
/// parent facts.
pub struct TreeShape;
impl ShapeSpec for TreeShape {
    const KIND: ShapeKind = ShapeKind::Tree;
}

/// The AbstractTree shape: the exact Tree facts, with generated
/// case-visitor sugar (not generated in v1). Its shape type is the Tree
/// shape: the facts and api are identical; the `ShapeKind::AbstractTree`
/// variant is reported by the schema for views that opt into it.
pub type AbstractTreeShape = TreeShape;

/// The Graph shape: an ordered node registry, node facts, edge facts, and
/// outgoing buckets.
pub struct GraphShape;
impl ShapeSpec for GraphShape {
    const KIND: ShapeKind = ShapeKind::Graph;
}

/// A stable node identity inside a Tree, AbstractTree, or Graph view.
///
/// Node ids are minted by
/// [`EmittedHandle::fresh_node_id`](crate::reactive::api::EmittedHandle::fresh_node_id)
/// (deterministic causal identities: same allocation site, same path, same
/// lane ⇒ same id) or supplied by the author (e.g. syntax node ids). They
/// are never recycled: removing a node and re-creating it starts a new
/// causal lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// The fact space of a Box view: exactly one fact, the value itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BoxFactKey {
    Value,
}

/// The fact space of a Map view: the ordered key registry plus one entry
/// fact per key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MapFactKey<K> {
    Keys,
    Entry(K),
}

/// The fact space of a Tree view: the ordered roots, one node fact per
/// node, one ordered-children fact per parent, one parent fact per node
/// (materialized lazily, see `store.rs`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TreeFactKey {
    Roots,
    Node(NodeId),
    Children(NodeId),
    Parent(NodeId),
}

/// The identity of one graph edge: its (source, label, target) triple.
/// Re-inserting the same triple addresses the same fact (an update).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GraphEdgeKey<L> {
    pub source: NodeId,
    pub label: L,
    pub target: NodeId,
}

/// The fact space of a Graph view: the ordered node registry, one node
/// fact per node, one edge fact per (source, label, target) triple, and
/// one outgoing-bucket fact per (source, label) pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GraphFactKey<L> {
    Nodes,
    Node(NodeId),
    Edge(GraphEdgeKey<L>),
    Bucket(NodeId, L),
}

/// The specification of one view type: its concrete shape and the types
/// of its facts. This is the reactive counterpart of the legacy
/// `#[derive(View)]` shape schema; the reactive derive generates it from
/// `#[view(...)]` attributes.
pub trait ViewSpec: 'static + Send + Sync {
    /// The concrete shape (which api surface applies).
    type Shape: ShapeSpec;

    /// Map keys / graph labels. `()` for non-keyed shapes.
    type Key: KeySpec;

    /// Payload values: Box value, Map value, Tree node data, Graph node data.
    type Value: Value;

    /// Graph edge data. `()` for non-graph shapes.
    type Edge: Value;

    /// Graph labels are [`ViewSpec::Key`]; this alias is kept for
    /// symmetry and the derive's attribute syntax.
    type Label: KeySpec;

    /// Human-readable view name (for diagnostics and cycle listings).
    fn view_name() -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Constructs the per-shape store for this view.
    fn new_store() -> Arc<dyn DynStore> {
        match Self::Shape::KIND {
            ShapeKind::Box => Arc::new(BoxStore::new(Self::view_name())),
            ShapeKind::Map => Arc::new(MapStore::<Self::Key>::new(Self::view_name())),
            ShapeKind::Tree | ShapeKind::AbstractTree => {
                Arc::new(TreeStore::new(Self::view_name()))
            }
            ShapeKind::Graph => Arc::new(GraphStore::<Self::Label>::new(Self::view_name())),
        }
    }
}

/// Shape marker for Box views (convenience bound).
pub trait BoxView: ViewSpec<Shape = BoxShape> {}
impl<T: ViewSpec<Shape = BoxShape>> BoxView for T {}

/// Shape marker for Map views.
pub trait MapView: ViewSpec<Shape = MapShape> {}
impl<T: ViewSpec<Shape = MapShape>> MapView for T {}

/// Shape marker for Tree views.
pub trait TreeView: ViewSpec<Shape = TreeShape> {}
impl<T: ViewSpec<Shape = TreeShape>> TreeView for T {}

/// Shape marker for AbstractTree views (Tree facts, generated case sugar).
pub trait AbstractTreeView: ViewSpec<Shape = AbstractTreeShape> {}
impl<T: ViewSpec<Shape = AbstractTreeShape>> AbstractTreeView for T {}

/// Shape marker for Graph views.
pub trait GraphView: ViewSpec<Shape = GraphShape> {}
impl<T: ViewSpec<Shape = GraphShape>> GraphView for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn erased_equality_and_hash_are_type_safe() {
        let a: Arc<dyn KeyValue> = Arc::new(42u64);
        let b: Arc<dyn KeyValue> = Arc::new(42u64);
        let c: Arc<dyn KeyValue> = Arc::new(43u64);
        let d: Arc<dyn KeyValue> = Arc::new("42".to_string());
        assert!(a.eq_value(b.as_ref()));
        assert!(!a.eq_value(c.as_ref()));
        assert!(!a.eq_value(d.as_ref()));
        assert_eq!(a.hash_value(), b.hash_value());
        assert_ne!(a.hash_value(), c.hash_value());

        let v1: Arc<dyn Value> = Arc::new(vec![1u32, 2]);
        let v2: Arc<dyn Value> = Arc::new(vec![1u32, 2]);
        let v3: Arc<dyn Value> = Arc::new(vec![1u32, 3]);
        assert!(v1.value_eq(v2.as_ref()));
        assert!(!v1.value_eq(v3.as_ref()));
    }

    #[test]
    fn concrete_shapes_are_distinct() {
        assert_ne!(ShapeKind::Box, ShapeKind::Map);
        assert_ne!(ShapeKind::Tree, ShapeKind::Graph);
        assert_eq!(TreeShape::KIND, ShapeKind::Tree);
        // AbstractTree lowers onto the Tree shape in v1.
        assert_eq!(AbstractTreeShape::KIND, ShapeKind::Tree);
    }
}
