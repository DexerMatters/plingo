use std::{collections::HashMap, io, marker::PhantomData, sync::Arc};

use crate::{
    scheme::{
        Context, EmittedChanges, NonTopLayer, Outcome, ReplacementBatch, ReplacementChange,
        Resolve, SnapshotId, SnapshotLayer, TopLayer,
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
    pub receiver: Receiver<SourceEdit>,
    _marker: PhantomData<fn() -> Lower>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEdit {
    Insert { key: Span, value: String },
    Delete { key: Span },
}

impl SourceEdit {
    pub fn span(&self) -> &Span {
        match self {
            SourceEdit::Insert { key, .. } | SourceEdit::Delete { key } => key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextUnit {
    pub span: Span,
    pub len: usize,
}

pub type TextChange = ReplacementChange<Uri<&'static str>, TextUnit>;

#[derive(Error, Debug)]
pub enum SourceError {
    #[error("Missing source: {0}")]
    MissingSource(Uri<&'static str>),
    #[error("Failed to read source: {0}")]
    ReadError(Uri<&'static str>, io::Error),
}

impl<Lower> Source<Lower> {
    pub fn new(receiver: Receiver<SourceEdit>) -> Self {
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

    fn modify(&mut self, edit: &SourceEdit) -> Result<(), SourceError> {
        let uri = edit.span().uri;
        self.load(uri);

        let source = self.sources.get_mut(&uri).unwrap();
        let source = Arc::make_mut(source);
        match edit {
            SourceEdit::Delete { key } => {
                let (start_byte, end_byte) = key.trim(&source).range.into();
                let start = source.byte_to_char(start_byte);
                let end = source.byte_to_char(end_byte);
                source.remove(start..end);
            }
            SourceEdit::Insert { key, value } => {
                let byte_offset = key.trim(&source).range.start();
                let char_offset = source.byte_to_char(byte_offset);
                source.insert(char_offset, value);
            }
        }
        Ok(())
    }

    fn lower_change(&self, edit: &SourceEdit) -> TextChange {
        let start = edit.span().range.start();
        match edit {
            SourceEdit::Insert { key, value } => {
                let end = start + value.len();
                let inserted_span = Span {
                    uri: key.uri,
                    range: RangeOrPoint::from_range(start, end),
                };
                ReplacementChange::new(
                    key.uri,
                    ReplacementBatch {
                        old_units: Vec::new(),
                        new_units: vec![TextUnit {
                            span: inserted_span,
                            len: value.len(),
                        }],
                        prefix_len: start,
                        suffix_len: 0,
                        old_changed_range: start..start,
                        new_changed_range: start..end,
                    },
                )
            }
            SourceEdit::Delete { key } => {
                let end = key.range.end();
                ReplacementChange::new(
                    key.uri,
                    ReplacementBatch {
                        old_units: vec![TextUnit {
                            span: *key,
                            len: end.saturating_sub(start),
                        }],
                        new_units: Vec::new(),
                        prefix_len: start,
                        suffix_len: 0,
                        old_changed_range: start..end,
                        new_changed_range: start..start,
                    },
                )
            }
        }
    }

    fn capture_snapshot(&mut self, snapshot: SnapshotId) {
        self.push_state(snapshot);
    }
}

#[layer(top)]
impl<Lower> TopLayer for Source<Lower>
where
    Lower: NonTopLayer<Change = TextChange>,
{
    type Error = SourceError;
    type Lower = Lower;

    fn emit<'a>(
        &'a mut self,
        ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<EmittedChanges<Self::Lower>>, Self::Error>> + Send + 'a
    {
        async move {
            let Some(edit) = self.receiver.recv().await else {
                return Ok(None);
            };
            let mut batch = vec![edit];
            while let Ok(next) = self.receiver.try_recv() {
                batch.push(next);
            }

            for edit in &batch {
                self.modify(edit)?;
            }
            let snapshot_id = ctx.allocate_snapshot();
            self.capture_snapshot(snapshot_id);
            let changes = batch.iter().map(|edit| self.lower_change(edit)).collect();
            Ok(Some(EmittedChanges::new(snapshot_id, changes)))
        }
    }
}

#[resolve_action]
impl<Lower> Resolve<Span> for Source<Lower>
where
    Lower: NonTopLayer<Change = TextChange>,
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

#[resolve_action]
impl<Lower> Resolve<SourceEdit> for Source<Lower>
where
    Lower: NonTopLayer<Change = TextChange>,
{
    type Output = ();

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a SourceEdit,
    ) -> impl Future<Output = Outcome<SourceEdit, Self>> + Send + 'a {
        async move {
            match self.modify(&action) {
                Ok(()) => {
                    let snapshot_id = ctx.snapshot().unwrap_or_else(|| ctx.allocate_snapshot());
                    self.capture_snapshot(snapshot_id);
                    Outcome::emit(vec![self.lower_change(action)])
                }
                Err(err) => Outcome::fail(err),
            }
        }
    }
}
