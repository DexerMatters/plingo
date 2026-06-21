use std::{fmt, future::Future};

use plingo::{
    context_callable,
    scheme::{
        call::CallOutcome,
        change::EmittedChanges,
        context::Context,
        layer::{FallibleLayer, NonTopLayer, TopLayer},
    },
};

#[derive(Debug, Clone)]
struct DummyError;

impl fmt::Display for DummyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dummy error")
    }
}

struct Lower;

impl FallibleLayer for Lower {
    type __Error = DummyError;
}

impl NonTopLayer for Lower {
    type _Error = DummyError;
    type Change = ();
}

struct Top;

impl FallibleLayer for Top {
    type __Error = DummyError;
}

impl TopLayer for Top {
    type Error = DummyError;
    type Lower = Lower;

    fn emit<'a>(
        &'a mut self,
        _ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<EmittedChanges<Self::Lower>>, Self::Error>> + Send + 'a
    {
        async { Ok(None) }
    }
}

impl Top {
    #[context_callable]
    pub async fn propagate<'a>(
        &'a mut self,
        _ctx: &'a Context,
        _value: &'a usize,
    ) -> CallOutcome<Self, ()> {
        CallOutcome::emit(vec![])
    }
}

fn main() {}
