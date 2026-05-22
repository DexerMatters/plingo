extern crate self as plingo;

pub use plingo_macros::{layer, resolve_action, tokens};

#[macro_export]
macro_rules! debug_sink {
    (
        resolve = |$resolve_ctx:pat_param, $resolve_action:pat_param| $resolve_body:expr,
        consume = |$consume_ctx:pat_param, $consume_deltas:pat_param| $consume_body:expr $(,)?
    ) => {
        $crate::component::sink::DebugSink::new(
            move |$resolve_ctx, $resolve_action| {
                ::std::boxed::Box::pin($resolve_body) as $crate::component::sink::BoxFuture<'_, _>
            },
            move |$consume_ctx, $consume_deltas| {
                ::std::boxed::Box::pin($consume_body) as $crate::component::sink::BoxFuture<'_, _>
            },
        )
    };
}

pub mod component;
pub mod marker;
pub mod scheme;
pub mod utils;
