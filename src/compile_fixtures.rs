//! Compile-time rejection fixtures for the transparent reactive API.
//!
//! Each doctest is a `compile_fail` gate. The module is hidden from the
//! public API; it exists only so rustdoc collects the gates.
//!
//! `#[view]` takes no arguments; the kind witness is the tuple field.
//!
//! ```compile_fail
//! use plingo::view;
//!
//! #[view(input = u64, output = u64)]
//! pub struct BadView;
//! ```
//!
//! The removed component macro is no longer exported.
//!
//! ```compile_fail
//! use plingo::component;
//!
//! #[component]
//! fn removed(_: ()) -> plingo::reactive::Result<()> {
//!     Ok(())
//! }
//! ```
//!
//! A view declares exactly one kind-witness tuple field.
//!
//! ```compile_fail
//! use plingo::view;
//!
//! #[view]
//! pub struct NoWitness;
//! ```
//!
//! Witness kinds must be known (`Map`, `List`, `Tree`, `Graph`, or `Box`).
//!
//! ```compile_fail
//! use plingo::view;
//!
//! #[view]
//! pub struct BadWitness(std::collections::HashMap<u64, u64>);
//! ```
//!
//! View inputs must satisfy the cache-key bounds.
//!
//! ```compile_fail
//! use plingo::reactive::kind::Map;
//! use plingo::view;
//!
//! #[view]
//! struct BadInput(Map<std::sync::Mutex<u64>, u64>);
//! ```
//!
//! View outputs must be shared, comparable payloads.
//!
//! ```compile_fail
//! use plingo::reactive::kind::Map;
//! use plingo::view;
//!
//! #[view]
//! struct BadOutput(Map<u64, std::cell::Cell<u64>>);
//! ```
//!
//! Planned results must be cacheable across the engine boundary.
//!
//! ```compile_fail
//! use plingo::reactive::{Engine, Result};
//!
//! fn bad(_: ()) -> Result<std::rc::Rc<u64>> {
//!     Ok(std::rc::Rc::new(1))
//! }
//!
//! let mut engine = Engine::new();
//! let _ = engine.plan(bad, ());
//! ```
//!
//! `#[view]` rejects duplicate kind witnesses.
//!
//! ```compile_fail
//! use plingo::reactive::kind::Map;
//! use plingo::view;
//!
//! #[view]
//! pub struct TwoFields(Map<u64, u64>, Map<u32, u32>);
//! ```
//!
//! `#[view]` accepts only structs.
//!
//! ```compile_fail
//! use plingo::reactive::kind::Map;
//! use plingo::view;
//!
//! #[view]
//! pub enum BadView {
//!     Item,
//! }
//! ```
//!
//! `#[abstract_tree]` rejects non-enum items.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//!
//! #[abstract_tree]
//! pub struct NotAnEnum {
//!     x: u8,
//! }
//! ```
//!
//! `#[abstract_tree]` rejects `#[tree(child)]` on a non-family field.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//!
//! #[abstract_tree]
//! pub enum Family {
//!     Item {
//!         #[tree(child)]
//!         bad: String,
//!     },
//! }
//! ```
//! `#[abstract_tree]` rejects an unsupported wrapper around `AstBox`.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//! use plingo::reactive::abstract_tree::AstBox;
//! use std::rc::Rc;
//!
//! #[abstract_tree]
//! pub enum Bad {
//!     Item {
//!         child: Rc<AstBox<Bad>>,
//!     },
//! }
//! ```
//!
//! `#[abstract_tree]` rejects a duplicate family member.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//! use plingo::reactive::abstract_tree::AstBox;
//!
//! #[abstract_tree(members(Left, Left))]
//! pub enum Left {
//!     Leaf,
//! }
//! ```
//!
//! `#[abstract_tree]` rejects `member_of` together with `tree`.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//! use plingo::reactive::abstract_tree::AstBox;
//!
//! #[abstract_tree(tree = MyTree, member_of = MyTree)]
//! pub enum Left {
//!     Leaf,
//! }
//! ```
//!
//! `#[abstract_tree]` rejects a cross-family child.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//! use plingo::reactive::abstract_tree::AstBox;
//!
//! #[abstract_tree(tree = AlphaTree, members(Alpha, Beta))]
//! pub enum Alpha {
//!     Wrap { child: AstBox<Beta> },
//! }
//!
//! #[abstract_tree(tree = GammaTree)]
//! pub enum Beta {
//!     Leaf,
//! }
//! ```
//!
//! `AstBox<T>` render output is owned by exactly one component evaluation:
//! `T::render` requires an active component frame and rejects a second
//! render of the same output slot ("abstract-tree output slot rendered
//! more than once"). Calling it outside any component run fails at the
//! effect boundary with `EffectOutsideRun { effect: "tree_render", .. }`.
//!
//! ```compile_fail
//! use plingo::abstract_tree;
//! use plingo::reactive::abstract_tree::{AstBox, TreeRender};
//!
//! #[abstract_tree]
//! pub enum Leaf {
//!     Bare,
//! }
//!
//! impl TreeRender for Leaf {
//!     fn __materialize(node: AstBox<Self>) -> plingo::reactive::Result<Self> {
//!         unreachable!()
//!     }
//!     fn __snapshot_materialize(
//!         snapshot: &plingo::reactive::Snapshot,
//!         node: AstBox<Self>,
//!     ) -> plingo::reactive::Result<Self> {
//!         unreachable!()
//!     }
//!     fn __render(value: Self) -> plingo::reactive::Result<AstBox<Self>> {
//!         unreachable!()
//!     }
//! }
//! ```
/// The compile-time rejection gates live in this module's documentation.
#[doc(hidden)]
pub struct CompileFixtures;
