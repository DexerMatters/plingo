//! # Plingo — incremental semantics over typed reactive views
//!
//! Plingo renders **typed facts**, not a virtual DOM. A component is an
//! ordinary Rust function that takes a semantic input, reads exact view
//! effects, and returns its desired output. The engine owns identity,
//! scheduling, equality suppression, retraction, fixed-point execution,
//! rollback, and deterministic commit. The author owns schemas and
//! computations only.
//!
//! ## The five concepts
//!
//! 1. **Input** — the exact semantic element that gives one component
//!    instance its lifecycle and identity.
//! 2. **Read effect** — a typed observation of one view fact; it creates a
//!    dependency.
//! 3. **Component call** — a declaration that another component instance is
//!    a child of this render.
//! 4. **Rendered output** — typed effects returned by the component; the
//!    component owns them while it remains rendered.
//! 5. **Commit** — the engine atomically reconciles calls and outputs after
//!    the command reaches a fixed point.
//!
//! ## Walkthrough: a recursive lowering
//!
//! Declare an abstract tree as an ordinary enum. Every semantic child is an
//! `AstBox<T>`; the macro generates the reactive tree view, exact field
//! facts, typed readers, snapshot readers, and render support:
//!
//! ```ignore
//! use plingo::prelude::*;
//!
//! #[abstract_tree(domain = String)]
//! pub enum CoreExpr {
//!     Add { left: AstBox<CoreExpr>, right: AstBox<CoreExpr> },
//!     Number { value: i64 },
//!     Name { text: Arc<str> },
//! }
//! ```
//!
//! Lower it with one recursive component per source node. Reads are exact
//! (`source.view()` reads only membership and the discriminant; each field
//! accessor reads exactly that field), and child calls declare stable child
//! instances instead of recursing through Rust frames:
//!
//! ```ignore
//! #[component]
//! fn lower_expr(source: AstBox<TransformExpr>) -> Result<AstBox<CoreExpr>> {
//!     let output = match source.view()? {
//!         TransformExprView::Add(add) => CoreExpr::Add {
//!             left: lower_expr(add.left()?)?,
//!             right: lower_expr(add.right()?)?,
//!         },
//!         // ... remaining variants
//!     };
//!     CoreExpr::render(output)
//! }
//! ```
//!
//! Mount only externally rooted computations; recursively called components
//! register themselves through their callers, and install order is
//! semantically irrelevant:
//!
//! ```ignore
//! let workspace = Workspace::builder()
//!     .lexer::<Token>()
//!     .parser::<Document>()
//!     .mount::<lower_document::Component>(Document::roots())
//!     .build()?;
//! ```
//!
//! ## Component boundaries
//!
//! A component groups one set of dependencies, one computation, and one set
//! of outputs. Keep outputs together when they are computed from the same
//! reads; split a child component when an output has a smaller dependency
//! set or an independent lifecycle. A parent rerender does **not** rerun an
//! unchanged child. Omitting a previously returned output retracts it — the
//! returned desired output is the ownership contract.
//!
//! ## Exact effects per accessor
//!
//! Every view operation documents the facts it reads, and enumeration
//! methods depend on the smallest keyset/order fact that can change the
//! result:
//!
//! - `source.view()` reads only family membership and the enum discriminant.
//! - A named leaf accessor (`number.value()`) reads exactly that leaf fact.
//! - A child accessor (`add.left()`) reads exactly that child-field fact.
//! - A `ChildList` cursor reads the field's order fact and only the links
//!   it traverses — retaining one link never wakes on a reorder.
//! - `Each::key()` borrows the semantic key without reading the payload;
//!   `Each::value()` records the exact optional payload dependency.
//! - `materialize()` reads every active field: an explicit coarse
//!   dependency for tooling and whole-node computations only.
//!
//! ## Recursive calls and cycles
//!
//! A child call derives the child instance from `(definition, input)`,
//! queues its body instead of entering a Rust frame, and returns the
//! child's stable output identity immediately. Deep trees therefore do not
//! consume proportional stack. A direct or indirect call cycle is rejected
//! with the component path (`Error::ComputationCycle { functions }`);
//! iterative semantics must go through an explicit reducer or state view,
//! never a recursive-call fixed point.
//!
//! ## Debugging
//!
//! Inspect a snapshot (`engine.snapshot()`, effect-free) or the reaction
//! graph (which component read which element and produced which output).
//! Never inspect revisions, dirty bits, or edit categories in semantic
//! code.
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
pub use reactive_macros::{Effects, StateValue, abstract_tree, component, view};

pub use framework::Workspace;
pub use reactive::{Error, Result};

/// The single public authoring import.
///
/// This facade combines typed reactive authoring with the framework's
/// source/lexer/parser workspace builder. Generated macro ABI remains hidden
/// behind these re-exports.
pub mod prelude {
    pub use crate::framework::{
        SourceEdit, SourceEdits, SourceRevisions, SourceSnapshot, Workspace, WorkspaceBuilder,
        WorkspaceReport, source_snapshot,
    };
    pub use crate::reactive::prelude::*;
    pub use crate::{
        Effects, NonTerminal, PrettyNonTerminal, PrettyTerminal, ScopeDomain, StateValue, Terminal,
        abstract_tree, component, generate, lregex, scope_path, view,
    };
}

pub mod compile_fixtures;
#[doc(hidden)]
pub use compile_fixtures::CompileFixtures;
pub mod framework;
pub mod reactive;
pub mod utils;
pub mod visual;
