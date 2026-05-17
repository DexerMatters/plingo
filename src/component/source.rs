use std::{collections::HashMap, future, io, marker::PhantomData};

use fluent_uri::Uri;
use plingo_macros::{layer, resolve_action};
use ropey::Rope;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;

use crate::{
    scheme::{Context, Delta, LayerDeltas, NonTopLayer, Outcome, Resolve, TopLayer},
    utils::Span,
};

pub struct Source<Lower: NonTopLayer> {
    pub sources: HashMap<Uri<&'static str>, Rope>,
    pub receiver: Receiver<Delta<Span, String>>,
    _marker: PhantomData<Lower>,
}

#[derive(Error, Debug)]
pub enum SourceError {
    #[error("Missing source: {0}")]
    MissingSource(Uri<&'static str>),
    #[error("Failed to read source: {0}")]
    ReadError(Uri<&'static str>, io::Error),
}

impl<Lower: NonTopLayer> Source<Lower> {
    pub fn new(receiver: Receiver<Delta<Span, String>>) -> Self {
        Self {
            sources: HashMap::new(),
            receiver,
            _marker: PhantomData,
        }
    }
}

// NOTE: #[layer(top)] is intentionally omitted here — Source<Lower> is not
// wired up as a layer in this example.  The Resolve impl below exists only to
// demonstrate generic #[resolve_action] support.

#[layer(top)]
impl<Lower: NonTopLayer> TopLayer for Source<Lower> {
    type Error = SourceError;
    type Lower = Lower;

    async fn emit(
        &mut self,
        _ctx: &Context,
    ) -> Result<Option<LayerDeltas<Self::Lower>>, Self::Error> {
        future::pending().await
    }
}

pub struct GetSource;

#[resolve_action]
impl<Lower: NonTopLayer> Resolve<GetSource> for Source<Lower> {
    type Output = String;

    async fn resolve<'a>(
        &'a self,
        _ctx: &'a Context,
        _action: &'a GetSource,
    ) -> Outcome<GetSource, Self> {
        future::pending().await
    }
}
