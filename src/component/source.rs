use std::{collections::HashMap, io, marker::PhantomData, sync::Arc};

use crate::{
    context_callable,
    scheme::{
        call::CallOutcome,
        change::{AddressChange, ChangeSet, FlowUnit, LayerChanges, Revision, Splice},
        context::{Context, SnapshotId},
        layer::{NonTopLayer, SnapshotLayer, TopLayer},
    },
    utils::{OwnedRopeSlice, Span},
};
use fluent_uri::Uri;
use plingo_macros::layer;
use ropey::Rope;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;

#[layer]
pub struct Source<Lower> {
    #[snapshot]
    pub sources: Arc<HashMap<Uri<&'static str>, Arc<Rope>>>,
    pub receiver: Receiver<SourceEdit>,
    revision: SnapshotId,
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
pub struct TextChunk(pub Arc<str>);

impl FlowUnit for TextChunk {
    fn extent(&self) -> usize {
        self.0.len()
    }
}

pub type TextChanges = ChangeSet<Uri<&'static str>, TextChunk>;

#[derive(Clone)]
enum TextPiece {
    Base(std::ops::Range<usize>),
    Inserted(String),
}

impl TextPiece {
    fn len(&self) -> usize {
        match self {
            Self::Base(range) => range.len(),
            Self::Inserted(text) => text.len(),
        }
    }
}

#[derive(Error, Debug)]
pub enum SourceError {
    #[error("Missing source: {0}")]
    MissingSource(Uri<&'static str>),
    #[error("Failed to read source: {0}")]
    ReadError(Uri<&'static str>, io::Error),
    #[error("Snapshot {0} is unavailable")]
    MissingSnapshot(SnapshotId),
}

impl<Lower> Source<Lower> {
    pub fn new(receiver: Receiver<SourceEdit>) -> Self {
        Self {
            sources: Arc::new(HashMap::new()),
            receiver,
            revision: 0,
            _marker: PhantomData,
            _snapshot: Default::default(),
        }
    }

    fn load(&mut self, uri: Uri<&'static str>) {
        Arc::make_mut(&mut self.sources)
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
            let sources = self
                .state(Some(snapshot))
                .ok_or(SourceError::MissingSnapshot(snapshot))?;
            let source = sources.get(&uri).ok_or(SourceError::MissingSource(uri))?;
            let (start, end) = span.trim(source).range.into();
            return Ok(OwnedRopeSlice::new(Arc::clone(source), start, end));
        }

        self.load(uri);
        let source = &self.sources[&uri];
        let (start, end) = span.trim(source).range.into();
        Ok(OwnedRopeSlice::new(Arc::clone(source), start, end))
    }

    fn modify(
        sources: &mut HashMap<Uri<&'static str>, Arc<Rope>>,
        edit: &SourceEdit,
    ) -> std::ops::Range<usize> {
        let uri = edit.span().uri;
        let source = Arc::make_mut(sources.entry(uri).or_insert_with(|| Arc::new(Rope::new())));
        match edit {
            SourceEdit::Delete { key } => {
                let (start, end) = key.trim(source).range.into();
                let start = source.char_to_byte(source.byte_to_char(start));
                let end = source.char_to_byte(source.byte_to_char(end));
                source.remove(source.byte_to_char(start)..source.byte_to_char(end));
                start..end
            }
            SourceEdit::Insert { key, value } => {
                let offset = key.trim(source).range.start();
                let offset = source.char_to_byte(source.byte_to_char(offset));
                source.insert(source.byte_to_char(offset), value);
                offset..offset
            }
        }
    }

    fn apply_batch(
        &self,
        revision: Revision,
        edits: &[SourceEdit],
    ) -> (Arc<HashMap<Uri<&'static str>, Arc<Rope>>>, TextChanges) {
        let mut new = (*self.sources).clone();
        let mut pieces = HashMap::<Uri<&'static str>, (String, Vec<TextPiece>)>::new();
        for edit in edits {
            let uri = edit.span().uri;
            let (_, sequence) = pieces.entry(uri).or_insert_with(|| {
                let old = self
                    .sources
                    .get(&uri)
                    .map_or_else(String::new, ToString::to_string);
                let len = old.len();
                (
                    old,
                    (len > 0)
                        .then(|| TextPiece::Base(0..len))
                        .into_iter()
                        .collect(),
                )
            });
            let range = Self::modify(&mut new, edit);

            let split = |sequence: &mut Vec<TextPiece>, offset: usize| {
                let mut cursor = 0;
                for index in 0..sequence.len() {
                    let end = cursor + sequence[index].len();
                    if offset == cursor {
                        return index;
                    }
                    if offset < end {
                        let at = offset - cursor;
                        let right = match &mut sequence[index] {
                            TextPiece::Base(range) => {
                                let right = range.start + at..range.end;
                                range.end = right.start;
                                TextPiece::Base(right)
                            }
                            TextPiece::Inserted(text) => TextPiece::Inserted(text.split_off(at)),
                        };
                        sequence.insert(index + 1, right);
                        return index + 1;
                    }
                    cursor = end;
                }
                sequence.len()
            };

            match edit {
                SourceEdit::Delete { .. } => {
                    let start = split(sequence, range.start);
                    let end = split(sequence, range.end);
                    sequence.drain(start..end);
                }
                SourceEdit::Insert { value, .. } if !value.is_empty() => {
                    let at = split(sequence, range.start);
                    sequence.insert(at, TextPiece::Inserted(value.clone()));
                }
                SourceEdit::Insert { .. } => {}
            }
        }

        let mut changes = Vec::new();
        for (uri, (old, sequence)) in pieces {
            let final_text = new.get(&uri).map_or_else(String::new, ToString::to_string);
            let mut old_cursor = 0;
            let mut new_cursor = 0;
            let mut new_changed_start = 0;
            let mut splices = Vec::new();
            for piece in sequence
                .into_iter()
                .chain(std::iter::once(TextPiece::Base(old.len()..old.len())))
            {
                match piece {
                    TextPiece::Inserted(text) => new_cursor += text.len(),
                    TextPiece::Base(range) => {
                        if old_cursor != range.start || new_changed_start != new_cursor {
                            let removed = &old[old_cursor..range.start];
                            let inserted = &final_text[new_changed_start..new_cursor];
                            if removed != inserted {
                                splices.push(Splice {
                                    old_range: old_cursor..range.start,
                                    new_range: new_changed_start..new_cursor,
                                    removed: (!removed.is_empty())
                                        .then(|| TextChunk(Arc::from(removed)))
                                        .into_iter()
                                        .collect(),
                                    inserted: (!inserted.is_empty())
                                        .then(|| TextChunk(Arc::from(inserted)))
                                        .into_iter()
                                        .collect(),
                                });
                            }
                        }
                        old_cursor = range.end;
                        new_cursor += range.len();
                        new_changed_start = new_cursor;
                    }
                }
            }
            if splices.is_empty() {
                continue;
            }
            changes.push(AddressChange {
                address: uri,
                old_extent: old.len(),
                new_extent: final_text.len(),
                splices,
            });
        }
        let changes = ChangeSet { revision, changes };
        (
            if changes.changes.is_empty() {
                Arc::clone(&self.sources)
            } else {
                Arc::new(new)
            },
            changes,
        )
    }

    #[context_callable]
    pub async fn read_span<'a>(
        &'a mut self,
        ctx: &'a Context,
        span: &'a Span,
    ) -> CallOutcome<Self, OwnedRopeSlice>
    where
        Lower: NonTopLayer<Address = Uri<&'static str>, Unit = TextChunk>,
    {
        match self.get_at(ctx.snapshot(), *span).await {
            Ok(value) => CallOutcome::ok(value),
            Err(err) => CallOutcome::fail(err),
        }
    }

    #[context_callable]
    pub async fn apply_edit<'a>(
        &'a mut self,
        ctx: &'a Context,
        edit: &'a SourceEdit,
    ) -> CallOutcome<Self, ()>
    where
        Lower: NonTopLayer<Address = Uri<&'static str>, Unit = TextChunk>,
    {
        let target = ctx.allocate_snapshot(self.revision);
        let (new, changes) = self.apply_batch(
            Revision {
                base: self.revision,
                target,
            },
            std::slice::from_ref(edit),
        );
        self.sources = new;
        self.revision = target;
        self.push_state(target);
        CallOutcome::emit(changes)
    }
}

#[layer(top)]
impl<Lower> TopLayer for Source<Lower>
where
    Lower: NonTopLayer<Address = Uri<&'static str>, Unit = TextChunk>,
{
    type Error = SourceError;
    type Lower = Lower;

    fn emit<'a>(
        &'a mut self,
        ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<LayerChanges<Self::Lower>>, Self::Error>> + Send + 'a
    {
        async move {
            let Some(edit) = self.receiver.recv().await else {
                return Ok(None);
            };
            let mut batch = vec![edit];
            while let Ok(next) = self.receiver.try_recv() {
                batch.push(next);
            }

            let target = ctx.allocate_snapshot(self.revision);
            let (new, changes) = self.apply_batch(
                Revision {
                    base: self.revision,
                    target,
                },
                &batch,
            );
            self.sources = new;
            self.revision = target;
            self.push_state(target);
            Ok(Some(changes))
        }
    }

    fn rollback_transaction(&mut self, revision: Revision) {
        if self.rollback_state(revision) {
            self.revision = revision.base;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distant_and_unicode_edits_stay_sparse() {
        let uri = Span::new("test://source-sparse", 0, 0).unwrap().uri;
        let old = format!(
            "{}α{}β{}",
            "a".repeat(1_000),
            "b".repeat(1_000),
            "c".repeat(1_000)
        );
        let alpha = old.find('α').unwrap();
        let beta = old.find('β').unwrap();
        let (_tx, receiver) = tokio::sync::mpsc::channel(1);
        let mut source = Source::<()>::new(receiver);
        Arc::make_mut(&mut source.sources).insert(uri, Arc::new(Rope::from_str(&old)));

        let (new, changes) = source.apply_batch(
            Revision { base: 4, target: 5 },
            &[
                SourceEdit::Delete {
                    key: Span::new_uri(uri, beta, beta + 'β'.len_utf8()).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::new_uri(uri, beta, beta).unwrap(),
                    value: "δ".into(),
                },
                SourceEdit::Delete {
                    key: Span::new_uri(uri, alpha, alpha + 'α'.len_utf8()).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::new_uri(uri, alpha, alpha).unwrap(),
                    value: "λ".into(),
                },
            ],
        );

        changes.validate().unwrap();
        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].splices.len(), 2);
        let payload = changes.changes[0]
            .splices
            .iter()
            .flat_map(|splice| splice.removed.iter().chain(splice.inserted.iter()))
            .map(FlowUnit::extent)
            .sum::<usize>();
        assert_eq!(payload, 8);
        let final_text = new[&uri].to_string();
        assert_eq!(&final_text[alpha..alpha + 'λ'.len_utf8()], "λ");
        assert_eq!(&final_text[beta..beta + 'δ'.len_utf8()], "δ");
        assert!(payload * 100 < old.len());
    }

    #[tokio::test]
    async fn historical_reads_do_not_fall_back_to_latest() {
        let uri = Span::new("test://strict-source", 0, 0).unwrap().uri;
        let (_tx, receiver) = tokio::sync::mpsc::channel(1);
        let mut source = Source::<()>::new(receiver);
        source.load(uri);
        source.initialize_snapshots();
        Arc::make_mut(Arc::make_mut(&mut source.sources).get_mut(&uri).unwrap())
            .insert(0, "latest");
        assert!(matches!(
            source
                .get_at(Some(99), Span::new_uri(uri, 0, 0).unwrap())
                .await,
            Err(SourceError::MissingSnapshot(99))
        ));
    }
}
