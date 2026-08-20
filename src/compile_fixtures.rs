//! Compile-time rejection fixtures (verification matrix row 11).
//!
//! Each doctest is a `compile_fail` gate: the authoring macros must reject
//! these programs at compile time with a macro error. The module is hidden
//! from the public API; it exists only so rustdoc collects the gates.

//! `#[component]` rejects duplicate views in one signature, except one
//! `Observed<V>`/`Previous<V>` pair.
//!
//! ```compile_fail
//! use plingo::reactive::prelude::*;
//! use plingo::{reactive_component as component, reactive_view as view};
//!
//! #[view(box, value = u64)] pub struct A;
//! #[view(box, value = u64)] pub struct B;
//!
//! #[component]
//! fn bad_duplicate(a: A, b: A) -> () { Ok(()) }
//! ```

//! `#[component]` rejects `self` receivers.
//!
//! ```compile_fail
//! use plingo::{reactive_component as component, reactive_view as view};
//!
//! #[view(box, value = u64)] pub struct A;
//!
//! #[component]
//! fn bad_self(self: u32) -> () { Ok(()) }
//! ```

//! `#[component]` rejects `Emitted<V>` in argument position.
//!
//! ```compile_fail
//! use plingo::reactive::prelude::*;
//! use plingo::{reactive_component as component, reactive_view as view};
//!
//! #[view(box, value = u64)] pub struct A;
//!
//! #[component]
//! fn bad_emitted(out: Emitted<A>) -> () { Ok(()) }
//! ```

//! `#[component]` rejects non-tuple, non-unit returns.
//!
//! ```compile_fail
//! use plingo::{reactive_component as component, reactive_view as view};
//!
//! #[view(box, value = u64)] pub struct A;
//! #[view(box, value = u64)] pub struct B;
//!
//! #[component]
//! fn bad_return(a: A) -> u64 { 1 }
//! ```

//! `#[abstract_tree]` rejects non-enum items.
//!
//! ```compile_fail
//! use plingo::reactive_abstract_tree as abstract_tree;
//!
//! #[abstract_tree]
//! pub struct NotAnEnum { x: u8 }
//! ```

//! `#[abstract_tree]` rejects `#[tree(child)]` on a non-family field.
//!
//! ```compile_fail
//! use plingo::reactive_abstract_tree as abstract_tree;
//!
//! #[abstract_tree]
//! pub enum Family {
//!     Item {
//!         #[tree(child)]
//!         bad: String,
//!     },
//! }
//! ```

//! `#[view]` rejects a missing value type.
//!
//! ```compile_fail
//! use plingo::reactive_view as view;
//!
//! #[view(box)]
//! pub struct BadView;
//! ```

/// The compile-time rejection gates live in this module's documentation.
#[doc(hidden)]
pub struct CompileFixtures;