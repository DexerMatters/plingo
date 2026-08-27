//! Incremental semantics over typed reactive views.
//!
//! The [`reactive`] module is the engine kernel: components are plain
//! functions that observe and emit typed views through kind-specific
//! handles, and the engine re-executes only the computations whose read
//! facts changed, in deterministic rounds, with epochs that commit or roll
//! back atomically. The authoring surface is `reactive`'s [`prelude`] plus
//! the `#[view]` macro from `reactive-macros` (re-exported as [`view`]).
//!
//! A view declares one **kind witness** as its single tuple field —
//! `Map<K, V>`, `List<K, I>`, `Tree<K, N>`, `Graph<P, L>`, or `Box<V>` —
//! and the macro generates that kind's fact codec plus an emit/observe
//! handle pair (`emit_view::<V>()` / `observe_view::<V>()`). Reactive and
//! incremental granularity is the smallest unit of each structure: map
//! entry, list slot + length, tree node + root list, graph node + edge
//! bucket, box cell (plan §5).
//!
//! ```ignore
//! #[view]
//! struct Diagnoses(List<String, Diagnostic>);
//!
//! fn parse_document(uri: String) -> Result<()> {
//!     let tokens = observe_view::<Tokens>()?;
//!     let diags = emit_view::<Diagnoses>()?;
//!     // ... pure computation between handle calls; every call records an
//!     // exact per-fact read or write ...
//! #   Ok(())
//! }
//! ```
//!
//! The [`framework`] module layers source, lexer, parser, scope-graph, and
//! workspace pipelines on the engine. The scope graph is a `GraphView`;
//! resolution reads exactly one edge-bucket fact per hop.

#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(private_interfaces)]

extern crate self as plingo;

pub use fluent_uri::Uri;
pub use plingo_macros::{
    NonTerminal, PrettyNonTerminal, PrettyTerminal, ScopeDomain, Terminal, generate, lregex,
    scope_path,
};
pub use reactive_macros::{abstract_tree, component, view};

pub use framework::Workspace;
pub use reactive::prelude;
pub use reactive::{Error, Result};

pub mod compile_fixtures;
#[doc(hidden)]
pub use compile_fixtures::CompileFixtures;
pub mod framework;
pub mod reactive;
pub mod utils;
pub mod visual;
