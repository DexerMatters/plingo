extern crate self as plingo;

pub use plingo_macros::{layer, resolve_action, tokens};

#[macro_export]
macro_rules! debug_sink {
    (
        |$consume_ctx:pat_param, $consume_deltas:pat_param| $consume_body:expr $(,)?
    ) => {
        $crate::component::sink::DebugSink::new(move |$consume_ctx, $consume_deltas| {
            ::std::boxed::Box::pin($consume_body) as $crate::component::sink::BoxFuture<'_, _>
        })
    };
}

pub mod component;
pub mod marker;
pub mod scheme;
pub mod utils;

#[cfg(test)]
mod tests;
