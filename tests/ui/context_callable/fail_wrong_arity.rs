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

struct Layer;

impl FallibleLayer for Layer {
    type __Error = DummyError;
}

impl NonTopLayer for Layer {
    type _Error = DummyError;
    type Change = ();
}

impl Layer {
    #[context_callable]
    pub async fn invalid<'a>(
        &'a mut self,
        _ctx: &'a Context,
        left: &'a usize,
        right: &'a usize,
    ) -> CallOutcome<Self, usize> {
        CallOutcome::ok(*left + *right)
    }
}

fn main() {}
