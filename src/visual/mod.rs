//! Read-only character views of immutable parser and scope-graph facts.
//!
//! Render values through [`PrettyDisplay`](crate::utils::PrettyDisplay):
//!
//! ```ignore
//! println!("{}", ScopeGraph::<Domain>::from_graph(&graph).pretty(&()));
//! println!("{}", AstTree::new(&snapshot, roots).pretty(&()));
//! ```

pub mod ast;
mod graph;

pub use ast::{AstRenderer, AstTree, PrettyAstField, PrettyNonTerminal, PrettyTerminal};
pub use graph::ScopeGraph;
