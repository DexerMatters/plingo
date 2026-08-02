//! High-level authoring and document-oriented access.
//!
//! [`component::Component`] is the ergonomic authoring API: a stable value
//! whose `run` method reads typed views, calls other components, publishes
//! semantic facts, and returns a result. Incrementality, suspension,
//! concurrency, replacement, and reclamation are runtime behavior.

pub mod api;
pub mod lex;
pub mod parse;
pub mod scope;
pub mod source;
pub mod structural;
pub mod workspace;

pub use crate::writes;
pub use api::{
    Component, ComponentDiagnostics, Context, ContextView, DiagnosticSet, Diagnostics, Error,
    Index, IndexView, Output, Parsed, Result, Scope, Set, SetView, Table, TableView, WriteSet,
};
