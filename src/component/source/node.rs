//! Source views and commands for the node graph runtime.
//!
//! Source documents are root views; edit and load commands select the
//! authoritative document without prescribing any downstream topology.

use std::{ops::Range, sync::Arc};

use crate::{
    component::source::{SourceDelta, SourceEdit, SourceSplice},
    scheme::node::{Command, CommandCx, Graph, NodeError, View},
};
use fluent_uri::Uri;

/// The authoritative text of one document in the node graph.
pub struct DocumentText;

impl View for DocumentText {
    type Key = Uri<&'static str>;
    type Value = Arc<str>;
}

/// The exact edit sequence that produced the current [`DocumentText`] value.
/// Consumers use it to retain incremental boundaries without diffing complete
/// document strings.
pub(crate) struct DocumentChange;

impl View for DocumentChange {
    type Key = Uri<&'static str>;
    type Value = Arc<SourceDelta>;
}

/// Root component for source documents.
///
/// Root components do not derive another view: commands are their authority.
/// `SourceNode` is therefore a namespace for the document view and commands,
/// rather than a fake pass-through node.
#[derive(Debug, Default, Clone, Copy)]
pub struct SourceNode;

impl SourceNode {
    pub const fn new() -> Self {
        Self
    }

    pub fn text(graph: &Graph, uri: Uri<&'static str>) -> Option<Arc<str>> {
        graph.read::<DocumentText>(uri)
    }

    pub fn apply(edit: SourceEdit) -> ApplySourceEdit {
        ApplySourceEdit { edit }
    }

    /// Applies an ordered edit batch as one source-root command, so downstream
    /// nodes observe only the completed document rather than invalid
    /// intermediate text while a replacement is being performed.
    pub fn apply_all(edits: Vec<SourceEdit>) -> ApplySourceEdits {
        ApplySourceEdits { edits }
    }

    pub fn load(uri: Uri<&'static str>) -> LoadSource {
        LoadSource { uri }
    }

    pub fn load_text(uri: Uri<&'static str>, text: impl Into<Arc<str>>) -> LoadSourceText {
        LoadSourceText {
            uri,
            text: text.into(),
        }
    }
}

/// Applies one UTF-8-safe edit to [`DocumentText`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplySourceEdit {
    pub edit: SourceEdit,
}

impl Command for ApplySourceEdit {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
        ApplySourceEdits {
            edits: vec![self.edit],
        }
        .apply(cx)
    }
}

/// Applies a sequence of edits atomically to one source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplySourceEdits {
    pub edits: Vec<SourceEdit>,
}

impl Command for ApplySourceEdits {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
        let Some(first) = self.edits.first() else {
            return Ok(());
        };
        let uri = first.span().uri;
        if self.edits.iter().any(|edit| edit.span().uri != uri) {
            return Err(NodeError::message(
                "an atomic source edit batch must target one document",
            ));
        }
        let previous = cx
            .read::<DocumentText>(uri)
            .unwrap_or_else(|| Arc::from(""));
        let mut text = previous.to_string();
        let mut pieces = vec![Piece::Original(0..previous.len())];
        for edit in &self.edits {
            let evolving = apply_edit(&mut text, edit)?;
            replace_piece_range(&mut pieces, evolving.old_range, evolving.inserted)?;
        }
        let splices = normalize_splices(previous.as_ref(), &pieces);
        cx.set::<DocumentText>(uri, Arc::from(text))?;
        cx.set::<DocumentChange>(
            uri,
            Arc::new(SourceDelta {
                splices: splices.into(),
            }),
        )
    }
}

/// Ensures that a document exists as an empty source value.
///
/// This preserves the existing `Source::load` meaning: loading is a specialized
/// edit-like operation rather than a separate mandatory source layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSource {
    pub uri: Uri<&'static str>,
}

impl Command for LoadSource {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
        if cx.read::<DocumentText>(self.uri).is_none() {
            cx.set::<DocumentText>(self.uri, Arc::from(""))?;
            cx.set::<DocumentChange>(self.uri, Arc::new(SourceDelta::default()))?;
        }
        Ok(())
    }
}

/// Materializes externally loaded text only when the document has not already
/// been edited into existence locally. A loader therefore cannot overwrite an
/// editor transaction that won the race with the post-commit effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadSourceText {
    pub uri: Uri<&'static str>,
    pub text: Arc<str>,
}

impl Command for LoadSourceText {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
        if cx.read::<DocumentText>(self.uri).is_none() {
            let length = self.text.len();
            cx.set::<DocumentText>(self.uri, Arc::clone(&self.text))?;
            cx.set::<DocumentChange>(
                self.uri,
                Arc::new(SourceDelta {
                    splices: vec![SourceSplice {
                        old_range: 0..0,
                        new_range: 0..length,
                        removed: Arc::from(""),
                        inserted: self.text,
                    }]
                    .into(),
                }),
            )?;
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Piece {
    Original(Range<usize>),
    Inserted(Arc<str>),
}

impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Original(range) => range.len(),
            Self::Inserted(text) => text.len(),
        }
    }
}

/// Splits the final-text piece sequence at an evolving byte coordinate and
/// returns the index of that boundary.
fn split_piece_at(pieces: &mut Vec<Piece>, offset: usize) -> Result<usize, NodeError> {
    let mut cursor = 0;
    for index in 0..pieces.len() {
        let end = cursor + pieces[index].len();
        if offset == cursor {
            return Ok(index);
        }
        if offset == end {
            return Ok(index + 1);
        }
        if offset < end {
            let local = offset - cursor;
            let split = match &pieces[index] {
                Piece::Original(range) => vec![
                    Piece::Original(range.start..range.start + local),
                    Piece::Original(range.start + local..range.end),
                ],
                Piece::Inserted(text) => {
                    if !text.is_char_boundary(local) {
                        return Err(NodeError::message(
                            "source edit splits an inserted UTF-8 code point",
                        ));
                    }
                    vec![
                        Piece::Inserted(Arc::from(&text[..local])),
                        Piece::Inserted(Arc::from(&text[local..])),
                    ]
                }
            };
            pieces.splice(index..=index, split);
            return Ok(index + 1);
        }
        cursor = end;
    }
    if offset == cursor {
        Ok(pieces.len())
    } else {
        Err(NodeError::message(
            "source edit exceeds the evolving piece sequence",
        ))
    }
}

fn replace_piece_range(
    pieces: &mut Vec<Piece>,
    range: Range<usize>,
    inserted: Arc<str>,
) -> Result<(), NodeError> {
    let start = split_piece_at(pieces, range.start)?;
    let end = split_piece_at(pieces, range.end)?;
    pieces.drain(start..end);
    if !inserted.is_empty() {
        pieces.insert(start, Piece::Inserted(inserted));
    }
    Ok(())
}

/// Converts an evolving edit batch into sparse source splices between the
/// command's original and final text revisions.
fn normalize_splices(original: &str, pieces: &[Piece]) -> Vec<SourceSplice> {
    let mut old_cursor = 0;
    let mut new_cursor = 0;
    let mut gap_new_start = 0;
    let mut inserted = String::new();
    let mut splices = Vec::new();

    for piece in pieces {
        match piece {
            Piece::Inserted(text) => inserted.push_str(text),
            Piece::Original(range) => {
                if old_cursor != range.start || !inserted.is_empty() {
                    let new_end = gap_new_start + inserted.len();
                    splices.push(SourceSplice {
                        old_range: old_cursor..range.start,
                        new_range: gap_new_start..new_end,
                        removed: Arc::from(&original[old_cursor..range.start]),
                        inserted: Arc::from(inserted.as_str()),
                    });
                    new_cursor = new_end;
                    inserted.clear();
                }
                old_cursor = range.end;
                new_cursor += range.len();
                gap_new_start = new_cursor;
            }
        }
    }
    if old_cursor != original.len() || !inserted.is_empty() {
        splices.push(SourceSplice {
            old_range: old_cursor..original.len(),
            new_range: gap_new_start..gap_new_start + inserted.len(),
            removed: Arc::from(&original[old_cursor..]),
            inserted: Arc::from(inserted),
        });
    }
    splices
}

fn apply_edit(text: &mut String, edit: &SourceEdit) -> Result<SourceSplice, NodeError> {
    match edit {
        SourceEdit::Insert { key, value } => {
            let at = checked_boundary(text, key.range.start())?;
            text.insert_str(at, value);
            Ok(SourceSplice {
                old_range: at..at,
                new_range: at..at + value.len(),
                removed: Arc::from(""),
                inserted: Arc::from(value.as_str()),
            })
        }
        SourceEdit::Delete { key } => {
            let start = checked_boundary(text, key.range.start())?;
            let end = checked_boundary(text, key.range.end())?;
            if end < start {
                return Err(NodeError::message(
                    "source delete range ends before it starts",
                ));
            }
            let removed = Arc::from(&text[start..end]);
            text.replace_range(start..end, "");
            Ok(SourceSplice {
                old_range: start..end,
                new_range: start..start,
                removed,
                inserted: Arc::from(""),
            })
        }
    }
}

fn checked_boundary(text: &str, offset: usize) -> Result<usize, NodeError> {
    if offset > text.len() {
        return Err(NodeError::message(format!(
            "source edit offset {offset} exceeds document length {}",
            text.len()
        )));
    }
    if !text.is_char_boundary(offset) {
        return Err(NodeError::message(format!(
            "source edit offset {offset} is not a UTF-8 boundary"
        )));
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{scheme::node::ViewUpdate, utils::Span};

    #[test]
    fn source_edits_reject_non_boundary_offsets() {
        let uri = Span::new("test://node-source-boundary", 0, 0).unwrap().uri;
        let mut graph = Graph::new();
        graph.command(SourceNode::load(uri)).unwrap();
        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "α".into(),
            }))
            .unwrap();

        let error = graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 1).unwrap(),
                value: "x".into(),
            }))
            .expect_err("a byte inside a UTF-8 code point must not be rounded");
        assert!(error.to_string().contains("UTF-8 boundary"));
        assert_eq!(SourceNode::text(&graph, uri).as_deref(), Some("α"));
    }

    #[test]
    fn batched_replacements_publish_disjoint_revision_splices() {
        let uri = Span::new("test://node-source-splices", 0, 0).unwrap().uri;
        let base = "head=11111;middle;tail=22222";
        let mut graph = Graph::new();
        graph.command(SourceNode::load(uri)).unwrap();
        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: base.into(),
            }))
            .unwrap();
        let first = base.find("11111").unwrap();
        let second = base.find("22222").unwrap();
        graph
            .command(SourceNode::apply_all(vec![
                SourceEdit::Delete {
                    key: Span::new_uri(uri, first, first + 5).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::point_uri(uri, first).unwrap(),
                    value: "54321".into(),
                },
                SourceEdit::Delete {
                    key: Span::new_uri(uri, second, second + 5).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::point_uri(uri, second).unwrap(),
                    value: "76543".into(),
                },
            ]))
            .unwrap();

        let delta = graph.read::<DocumentChange>(uri).unwrap();
        assert_eq!(delta.splices.len(), 2);
        assert_eq!(delta.splices[0].old_range, first..first + 5);
        assert_eq!(delta.splices[0].inserted.as_ref(), "54321");
        assert_eq!(delta.splices[1].old_range, second..second + 5);
        assert_eq!(delta.splices[1].inserted.as_ref(), "76543");
        assert_eq!(
            SourceNode::text(&graph, uri).as_deref(),
            Some("head=54321;middle;tail=76543")
        );
    }

    #[test]
    fn source_node_commands_are_versioned_and_subscribable() {
        let uri = Span::new("test://node-source", 0, 0).unwrap().uri;
        let mut graph = Graph::new();
        graph.command(SourceNode::load(uri)).unwrap();
        assert_eq!(SourceNode::text(&graph, uri).as_deref(), Some(""));

        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "α".into(),
            }))
            .unwrap();
        let snapshot = graph.snapshot();
        let subscription = graph
            .subscribe_view::<DocumentText>(uri)
            .expect("loaded source is materialized");
        assert!(matches!(
            subscription.recv().unwrap(),
            ViewUpdate::Initial { .. }
        ));

        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, "α".len()).unwrap(),
                value: "β".into(),
            }))
            .unwrap();
        assert_eq!(
            subscription.recv().unwrap(),
            ViewUpdate::Changed {
                snapshot: graph.revision(),
                value: Arc::from("αβ"),
            }
        );
        assert_eq!(
            graph.read_at::<DocumentText>(&snapshot, uri).as_deref(),
            Some("α")
        );
    }
}
