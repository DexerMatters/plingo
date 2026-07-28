//! Incremental semantic passes over parser artifacts and scope-graph facts.
//!
//! Each [`ElaboratorNode`] is independently keyed, demand-driven, and owns the
//! scope facts it emits. Scope allocation itself remains built-in under
//! `component::scope`.

mod node;

pub use node::{
    ElaborationFrameKey, ElaborationKey, ElaborationStamp, ElaboratorNode, Here, ResolveError,
};
