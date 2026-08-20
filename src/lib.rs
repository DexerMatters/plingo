#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(private_interfaces)]
#![allow(clippy::result_large_err)]

extern crate self as plingo;

pub use fluent_uri::Uri;
pub use reactive_macros::abstract_tree as reactive_abstract_tree;
pub use reactive_macros::component as reactive_component;
pub use reactive_macros::view as reactive_view;

pub use plingo_macros::{
    NonTerminal, PrettyNonTerminal, PrettyTerminal, ScopeDomain, Terminal, generate, lregex,
    scope_path,
};

pub use framework::*;
pub mod framework;
pub mod reactive;
pub mod utils;
pub mod visual;

/// Compile-time rejection gates (doctests); hidden from the public API.
#[doc(hidden)]
pub mod compile_fixtures;
