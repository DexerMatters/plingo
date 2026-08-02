//! Composable incremental structural views and transforms.

pub(crate) mod context;
mod data;
mod view;

pub use context::StructuralView;
pub use data::{
    ChildRef, ErasedStructuralValue, GraphEdges, NoEdge, OrderedChildren, StructuralArtifact,
    StructuralEdge, Structure, StructureEntry, Topology,
};
pub use view::{StructureChildren, StructureEdges, StructureEntries, StructureNode};

#[cfg(test)]
#[path = "../../../tests/unit/component_structural.rs"]
mod tests;
