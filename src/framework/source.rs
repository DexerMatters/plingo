//! Source views and the built-in `source` component (plan §8.1).
//!
//! [`SourceEdits`] is the external command channel: one map entry per
//! document uri, carrying the exact sparse splice delta of one load/edit
//! batch. The built-in `source` component folds each entry into
//! [`SourceText`] with one visitor per uri, so an edit to document A never
//! re-runs document B's fold. Text equality filters no-op edits (T4), and
//! removing a document's `SourceEdits` entry retracts its `SourceText`
//! entry through visitor retirement.

use std::ops::Range;
use std::sync::Arc;

use crate::reactive::api::{MapEmittedExt, MapObservedExt};
use crate::reactive::prelude::*;
use crate::reactive_component as component;
use crate::reactive_view as view;
use crate::utils::Span;

// ---------------------------------------------------------------------------
// Vocabulary (moved unchanged from `component::source`)
// ---------------------------------------------------------------------------

/// One editor operation against an authoritative source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEdit {
    Insert { key: Span, value: String },
    Delete { key: Span },
}

impl SourceEdit {
    pub fn span(&self) -> &Span {
        match self {
            Self::Insert { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// One exact source replacement between the command's original and final
/// document revisions. A batch retains disjoint sparse splices with
/// old/new coordinate maps rather than rediscovering one broad
/// whole-text diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSplice {
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    pub removed: Arc<str>,
    pub inserted: Arc<str>,
}

/// The exact sparse source delta that produced the current document
/// revision. Splices are ascending and disjoint in old coordinates; a
/// `replace` delta (a document load) discards the committed text instead
/// of splicing into it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceDelta {
    pub splices: Arc<[SourceSplice]>,
    /// True for a document load: the fold replaces the whole text.
    pub replace: bool,
}

impl SourceDelta {
    fn full_text(text: &str) -> SourceDelta {
        SourceDelta {
            splices: Arc::from([SourceSplice {
                old_range: 0..0,
                new_range: 0..text.len(),
                removed: Arc::from(""),
                inserted: Arc::from(text),
            }]),
            replace: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One exact per-document source delta (external command channel).
#[view(map, key = String, value = SourceDelta)]
pub struct SourceEdits;

/// The authoritative text of each open document (built-in `source`).
#[view(map, key = String, value = Arc<str>)]
pub struct SourceText;

// ---------------------------------------------------------------------------
// The built-in component
// ---------------------------------------------------------------------------

/// Folds `SourceEdits` into `SourceText`: one child visitor per uri, each
/// applying its entry's splices to the committed text (`Previous`, so the
/// fold never reads the fact it writes — the exact §8.1 contract) and
/// writing the entry. The `Previous` edge reschedules the child at the
/// next epoch when a downstream text read changed the base.
#[component]
fn source(deltas: Observed<SourceEdits>, text: Previous<SourceText>) -> (SourceText,) {
    let out = Emitted::<SourceText>::new()?;
    let out_outer = out.clone();
    let text_outer = text.clone();
    deltas.visit_each(move |uri, delta| -> Result<()> {
        // A removed entry retires this child; an absent value is a no-op.
        let Some(delta) = delta else {
            return Ok(());
        };
        let current = text_outer
            .get(&uri)?
            .map(|value| (*value).to_string())
            .unwrap_or_default();
        let folded = fold_delta(&current, &delta)?;
        out_outer.set(uri, Arc::from(folded))?;
        Ok(())
    })?;
    Ok((out,))
}

/// Applies ascending, disjoint splices to `current`, returning the final
/// text. A `replace` delta discards `current` (document loads). Splices
/// are validated: each must be ordered and within bounds.
pub(crate) fn fold_delta(current: &str, delta: &SourceDelta) -> Result<String, Error> {
    if delta.replace {
        return Ok(delta
            .splices
            .iter()
            .map(|splice| splice.inserted.to_string())
            .collect::<Vec<_>>()
            .concat());
    }
    let mut text = current.to_string();
    let mut cursor = 0usize;
    for splice in delta.splices.iter() {
        if splice.old_range.start < cursor {
            return Err(Error::Internal(format!(
                "overlapping or out-of-order source splice: {:?}",
                splice.old_range
            )));
        }
        let end = splice.old_range.end;
        if end > text.len() {
            return Err(Error::Internal(format!(
                "source splice out of bounds: {:?} (text len {})",
                splice.old_range,
                text.len()
            )));
        }
        text.replace_range(splice.old_range.clone(), &splice.inserted);
        cursor = splice.old_range.start + splice.inserted.len();
        let _ = end;
    }
    Ok(text)
}

/// Installs the built-in source pipeline: the external `SourceEdits`
/// channel and the `source` fold component.
pub fn install_source(engine: &mut Engine) -> Result<()> {
    engine.external::<SourceEdits>()?;
    engine.install(source)?;
    Ok(())
}

/// Builds the `SourceDelta` for *loading* a document: one insert of the
/// full text.
pub(crate) fn load_delta(text: &str) -> SourceDelta {
    SourceDelta::full_text(text)
}

/// Builds the `SourceDelta` for a batch of editor operations on one
/// document. Edits are applied in ascending original-position order; each
/// edit's coordinates are mapped through the cumulative effect of the
/// edits before it (a point before a deleted region is not shifted), and
/// overlapping replacements are rejected.
pub(crate) fn edits_delta(edits: &[SourceEdit]) -> Result<SourceDelta, Error> {
    let mut sorted: Vec<&SourceEdit> = edits.iter().collect();
    // Points sort before ranges at the same offset (an insert at the start
    // of a deletion is applied first).
    sorted.sort_by(|a, b| {
        let pa = a.span().range.start();
        let pb = b.span().range.start();
        pa.cmp(&pb)
            .then_with(|| a.span().range.is_point().cmp(&b.span().range.is_point()).reverse())
    });
    let mut splices: Vec<SourceSplice> = Vec::new();
    // Running totals over *previously processed* edits: an insert before
    // position p shifts it right; a delete ending at/before p shifts it
    // left.
    let (mut ins_before, mut del_before) = (0isize, 0isize);
    let mut cursor = 0usize; // current-coordinates end of the last splice
    for edit in sorted {
        let span = edit.span();
        let start = span.range.start() as isize;
        let pos = start + ins_before - del_before;
        if pos < 0 {
            return Err(Error::Internal(format!(
                "overlapping source edits: {:?}",
                span
            )));
        }
        match edit {
            SourceEdit::Insert { value, .. } => {
                let at = pos as usize;
                if at < cursor {
                    return Err(Error::Internal(format!(
                        "overlapping source edits: {:?}",
                        span
                    )));
                }
                splices.push(SourceSplice {
                    old_range: at..at,
                    new_range: at..at + value.len(),
                    removed: Arc::from(""),
                    inserted: Arc::from(value.as_str()),
                });
                ins_before += value.len() as isize;
                cursor = at + value.len();
            }
            SourceEdit::Delete { .. } => {
                let at = pos as usize;
                if at < cursor {
                    return Err(Error::Internal(format!(
                        "overlapping source edits: {:?}",
                        span
                    )));
                }
                let len = span.range.end() - span.range.start();
                splices.push(SourceSplice {
                    old_range: at..at + len,
                    new_range: at..at,
                    removed: Arc::from(""), // filled by the fold via the old text
                    inserted: Arc::from(""),
                });
                del_before += len as isize;
                cursor = at + len;
            }
        }
    }
    Ok(SourceDelta {
        replace: false,
        splices: splices.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_applies_ordered_splices() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct S(SourceSplice);
        let delta = SourceDelta {
            replace: false,
            splices: Arc::from([
                SourceSplice {
                    old_range: 0..0,
                    new_range: 0..3,
                    removed: Arc::from(""),
                    inserted: Arc::from("abc"),
                },
                SourceSplice {
                    old_range: 3..4,
                    new_range: 6..7,
                    removed: Arc::from("X"),
                    inserted: Arc::from("x"),
                },
            ]),
        };
        let folded = fold_delta("XYZ", &delta).unwrap();
        assert_eq!(folded, "abcxYZ");
    }

    #[test]
    fn fold_rejects_out_of_bounds() {
        let delta = SourceDelta {
            replace: false,
            splices: Arc::from([SourceSplice {
                old_range: 10..12,
                new_range: 10..12,
                removed: Arc::from(""),
                inserted: Arc::from(""),
            }]),
        };
        assert!(fold_delta("abc", &delta).is_err());
    }

    #[test]
    fn edits_delta_shifts_later_coordinates() {
        let uri = Span::point("t://u".into(), 0).unwrap().uri;
        let edits = [
            SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "ab".into(),
            },
            // Position 2 in original coordinates shifts to 4.
            SourceEdit::Insert {
                key: Span::point_uri(uri, 2).unwrap(),
                value: "cd".into(),
            },
        ];
        let delta = edits_delta(&edits).unwrap();
        assert_eq!(delta.splices.len(), 2);
        assert_eq!(delta.splices[0].old_range, 0..0);
        assert_eq!(delta.splices[1].old_range, 4..4);
        let folded = fold_delta("XY", &delta).unwrap();
        assert_eq!(folded, "abXYcd");
    }
}