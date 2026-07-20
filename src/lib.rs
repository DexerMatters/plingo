extern crate self as plingo;

pub use plingo_macros::{NonTerminal, Terminal, context_callable, generate, layer};

#[macro_export]
macro_rules! debug_sink {
    (
        |$ctx:pat_param, $deltas:pat_param| $body:expr $(,)?
    ) => {
        $crate::component::debug::DebugSink::new(move |$ctx, $deltas| {
            ::std::boxed::Box::pin($body)
                as ::std::pin::Pin<
                    ::std::boxed::Box<
                        dyn ::std::future::Future<
                                Output = ::std::result::Result<(), ::std::convert::Infallible>,
                            > + ::std::marker::Send
                            + '_,
                    >,
                >
        })
    };
}

pub mod component;
pub mod marker;
pub mod scheme;
pub mod utils;

#[cfg(test)]
mod tests;
