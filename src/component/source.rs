use std::{collections::HashMap, io, marker::PhantomData, sync::Arc};

use crate::{
    scheme::{
        Context, Delta, EmittedDeltas, NonTopLayer, Outcome, Resolve, SnapshotId, SnapshotLayer,
        TopLayer,
    },
    utils::{OwnedRopeSlice, RangeOrPoint, Span},
};
use fluent_uri::Uri;
use plingo_macros::{layer, resolve_action};
use ropey::Rope;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;

#[layer]
pub struct Source<Lower> {
    #[snapshot]
    pub sources: HashMap<Uri<&'static str>, Arc<Rope>>,
    pub receiver: Receiver<Delta<Span, String>>,
    _marker: PhantomData<fn() -> Lower>,
}

#[derive(Error, Debug)]
pub enum SourceError {
    #[error("Missing source: {0}")]
    MissingSource(Uri<&'static str>),
    #[error("Failed to read source: {0}")]
    ReadError(Uri<&'static str>, io::Error),
}

impl<Lower> Source<Lower> {
    pub fn new(receiver: Receiver<Delta<Span, String>>) -> Self {
        Self {
            sources: HashMap::new(),
            receiver,
            _marker: PhantomData,
            _snapshot: HashMap::new(),
        }
    }

    fn load(&mut self, uri: Uri<&'static str>) {
        self.sources
            .entry(uri)
            .or_insert_with(|| Arc::new(Rope::new()));
    }

    pub async fn get(&mut self, span: Span) -> Result<OwnedRopeSlice, SourceError> {
        self.get_at(None, span).await
    }

    pub async fn get_at(
        &mut self,
        snapshot: Option<SnapshotId>,
        span: Span,
    ) -> Result<OwnedRopeSlice, SourceError> {
        let uri = span.uri;
        if let Some(snapshot) = snapshot {
            if let Some(source) = self
                .state(Some(snapshot))
                .and_then(|sources| sources.get(&uri))
            {
                let (start, end) = span.trim(source).range.into();
                return Ok(OwnedRopeSlice::new(Arc::clone(source), start, end));
            }
        }

        self.load(uri);
        let source = self.sources.get(&uri).unwrap();
        let (start, end) = span.trim(source).range.into();
        Ok(OwnedRopeSlice::new(Arc::clone(source), start, end))
    }

    fn modify(&mut self, delta: &Delta<Span, String>) -> Result<(), SourceError> {
        let uri = delta.key().uri;
        self.load(uri);

        let source = self.sources.get_mut(&uri).unwrap();
        let source = Arc::make_mut(source);
        match delta {
            Delta::Delete { key, .. } => {
                let (start_byte, end_byte) = key.trim(&source).range.into();
                let start = source.byte_to_char(start_byte);
                let end = source.byte_to_char(end_byte);
                source.remove(start..end);
            }
            Delta::Insert { key, value } => {
                let byte_offset = key.trim(&source).range.start();
                let char_offset = source.byte_to_char(byte_offset);
                source.insert(char_offset, &value);
            }
        }
        Ok(())
    }

    fn capture_snapshot(&mut self, snapshot: SnapshotId) {
        self.push_state(snapshot);
    }
}

#[layer(top)]
impl<Lower> TopLayer for Source<Lower>
where
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
{
    type Error = SourceError;
    type Lower = Lower;

    fn emit<'a>(
        &'a mut self,
        ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<EmittedDeltas<Self::Lower>>, Self::Error>> + Send + 'a
    {
        async move {
            let Some(delta) = self.receiver.recv().await else {
                return Ok(None);
            };
            let start = delta.key().range.start();
            self.modify(&delta)?;
            let snapshot_id = ctx.allocate_snapshot();
            self.capture_snapshot(snapshot_id);
            let deltas = match &delta {
                Delta::Insert { key, value } => {
                    let end = start + value.len();
                    vec![Delta::Insert {
                        key: Span {
                            uri: key.uri,
                            range: RangeOrPoint::from_range(start, end),
                        },
                        value: value.len(),
                    }]
                }
                Delta::Delete { key, .. } => {
                    vec![Delta::Delete { key: *key }]
                }
            };
            Ok(Some(EmittedDeltas::new(snapshot_id, deltas)))
        }
    }
}

#[resolve_action]
impl<Lower> Resolve<Span> for Source<Lower>
where
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
{
    type Output = OwnedRopeSlice;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a Span,
    ) -> impl Future<Output = Outcome<Span, Self>> + Send + 'a {
        async move {
            match self.get_at(ctx.snapshot(), *action).await {
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
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
{
    type Output = ();

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        Change(action): &'a Change,
    ) -> impl Future<Output = Outcome<Change, Self>> + Send + 'a {
        async move {
            match self.modify(&action) {
                Ok(()) => {
                    let start = action.key().range.start();
                    let snapshot_id = ctx.snapshot().unwrap_or_else(|| ctx.allocate_snapshot());
                    self.capture_snapshot(snapshot_id);
                    let deltas = match &action {
                        Delta::Insert { key, value } => {
                            let end = start + value.len();
                            vec![Delta::Insert {
                                key: Span {
                                    uri: key.uri,
                                    range: RangeOrPoint::from_range(start, end),
                                },
                                value: value.len(),
                            }]
                        }
                        Delta::Delete { key, .. } => {
                            vec![Delta::Delete { key: *key }]
                        }
                    };
                    Outcome::emit(deltas)
                }
                Err(err) => Outcome::fail(err),
            }
        }
    }
}
