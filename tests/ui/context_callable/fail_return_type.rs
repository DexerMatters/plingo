use std::fmt;

use plingo::{
    context_callable,
    scheme::{
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
    type Address = ();
    type Unit = ();
}

impl Layer {
    #[context_callable]
    pub async fn invalid<'a>(
        &'a mut self,
        _ctx: &'a Context,
        value: &'a usize,
    ) -> Result<usize, DummyError> {
        Ok(*value)
    }
}

fn main() {}
