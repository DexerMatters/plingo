use std::{collections::HashMap, io, marker::PhantomData};

use crate::{
    scheme::{Context, Delta, LayerDeltas, NonTopLayer, Outcome, Resolve, TopLayer},
    utils::Span,
};
use async_stream::try_stream;
use fluent_uri::Uri;
use plingo_macros::{layer, resolve_action};
use ropey::Rope;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc::Receiver};
use tokio_stream::Stream;

pub struct Source<Lower: NonTopLayer> {
    pub sources: Mutex<HashMap<Uri<&'static str>, Rope>>,
    pub receiver: Mutex<Receiver<Delta<Span, String>>>,
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
            sources: Mutex::new(HashMap::new()),
            receiver: Mutex::new(receiver),
            _marker: PhantomData,
        }
    }

    async fn load(&self, uri: Uri<&'static str>) -> Result<(), SourceError> {
        if self.sources.lock().await.contains_key(&uri) {
            return Ok(());
        }
        let content = std::fs::read_to_string(uri.path().as_str())
            .map_err(|e| SourceError::ReadError(uri, e))?;
        self.sources
            .lock()
            .await
            .insert(uri, Rope::from_str(&content));
        Ok(())
    }

    pub async fn get(&self, span: Span) -> Result<String, SourceError> {
        let uri = span.uri;
        self.load(uri).await?;
        let sources = self.sources.lock().await;
        // SAFETY: We just loaded this source if it wasn't already present, so it must be present now.
        let source = sources.get(&uri).unwrap();
        let (start, end) = span.trim(&source).range.into();
        Ok(source.slice(start..end).to_string())
    }

    async fn modify(&self, delta: &Delta<Span, String>) -> Result<(), SourceError> {
        let uri = delta.key().uri;
        self.load(uri).await?;
        let mut sources = self.sources.lock().await;

        // SAFETY: We just loaded this source if it wasn't already present, so
        // it must be present now.
        let source = sources.get_mut(&uri).unwrap();
        match delta {
            Delta::Delete { key } => {
                let (start, end) = key.trim(&source).range.into();
                source.remove(start..=end);
            }
            Delta::Insert { key, value } => {
                let offset = key.trim(&source).range.start();
                source.insert(offset, &value);
            }
            Delta::Update { key, value } => {
                let (start, end) = key.trim(&source).range.into();
                source.remove(start..=end);
                source.insert(start, &value);
            }
        }
        Ok(())
    }
}

#[layer(top)]
impl<Lower> TopLayer for Source<Lower>
where
    Lower: NonTopLayer<_Key = Span> + Resolve<Span, Output = String>,
{
    type Error = SourceError;
    type Lower = Lower;

    fn emit(
        &self,
        _ctx: &Context,
    ) -> impl Stream<Item = Result<LayerDeltas<Self::Lower>, Self::Error>> + Send + '_ {
        try_stream! {
            while let Some(delta) = self.receiver.lock().await.recv().await {
                self.modify(&delta).await?;
                yield vec![delta];
            }
        }
    }
}

#[resolve_action]
impl<Lower> Resolve<Span> for Source<Lower>
where
    Lower: NonTopLayer<_Key = Span> + Resolve<Span, Output = String>,
{
    type Output = String;

    fn resolve<'a>(
        &'a self,
        _ctx: &'a Context,
        action: &'a Span,
    ) -> impl Future<Output = Outcome<Span, Self>> + Send + 'a {
        async move {
            match self.get(*action).await {
                Ok(value) => Outcome::ok(value),
                Err(err) => Outcome::fail(err),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Change(pub Delta<Span, String>);

#[resolve_action]
impl<Lower> Resolve<Change> for Source<Lower>
where
    Lower: NonTopLayer<_Key = Span> + Resolve<Span, Output = String>,
{
    type Output = ();

    fn resolve<'a>(
        &'a self,
        _ctx: &'a Context,
        Change(action): &'a Change,
    ) -> impl Future<Output = Outcome<Change, Self>> + Send + 'a {
        async move {
            match self.modify(&action).await {
                Ok(()) => Outcome::emit(vec![action.clone()]),
                Err(err) => Outcome::fail(err),
            }
        }
    }
}
