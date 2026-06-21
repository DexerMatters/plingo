use std::fmt;

use plingo::{
    context_callable,
    scheme::{
        call::CallOutcome,
        context::Context,
        layer::{FallibleLayer, NonTopLayer},
    },
};

#[derive(Debug, Clone)]
struct DummyError;

impl fmt::Display for DummyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dummy error")
    }
}

struct Target;

impl FallibleLayer for Target {
    type __Error = DummyError;
}

impl NonTopLayer for Target {
    type _Error = DummyError;
    type Change = ();
}

struct Caller;

impl FallibleLayer for Caller {
    type __Error = DummyError;
}

impl NonTopLayer for Caller {
    type _Error = DummyError;
    type Change = ();
}

impl Target {
    #[context_callable]
    pub async fn echo<'a, T>(
        &'a mut self,
        _ctx: &'a Context,
        value: &'a T,
    ) -> CallOutcome<Self, T>
    where
        T: Clone + Send + Sync + 'static,
    {
        CallOutcome::ok(value.clone())
    }
}

impl Caller {
    #[context_callable]
    pub async fn forward<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        value: &'a T,
    ) -> CallOutcome<Self, T>
    where
        T: Clone + Send + Sync + 'static,
    {
        match ctx.call(Target::echo::<T>, value.clone()).await {
            Ok(next) => CallOutcome::ok(next),
            Err(_) => CallOutcome::fail(DummyError),
        }
    }
}

fn main() {}
