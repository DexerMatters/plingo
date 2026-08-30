//! Source views and the built-in source pipeline (plan §8.1).
//!
//! [`SourceEdits`] is the external command channel: one map entry per
//! document URI, carrying the exact sparse splice delta of one load/edit
//! batch. The built-in source pipeline folds each entry into
//! [`SourceRevisions`] with one nested computation per URI, so an edit to
//! document A never re-runs document B's fold. Text equality filters no-op
//! edits, and removing a document's [`SourceEdits`] entry retracts its
//! [`SourceRevisions`] entry.

use std::ops::Range;
use std::sync::Arc;

use fluent_uri::Uri;

use crate::reactive::kind::{Map, emit_view, observe_view};
use crate::reactive::{Engine, Error, Result};
use crate::utils::Span;
use reactive_macros::view;
// ---------------------------------------------------------------------------
// Work reporting (plan §10.1).

/// Deterministic source-pipeline work counters for one document command.
///
/// Counters roll back with their command and never enter reactive facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceWork {
    /// Editor operations validated by the batch normalizer.
    pub validated_operations: u64,
    /// Splices that survived no-op filtering.
    pub effective_splices: u64,
    /// Bytes removed by validated splices.
    pub bytes_removed: u64,
    /// Bytes inserted by validated splices.
    pub bytes_inserted: u64,
    /// Unchanged coordinate islands built from a revision delta.
    pub coordinate_islands_built: u64,
    /// Coordinate-island lookup attempts.
    pub coordinate_islands_queried: u64,
    /// Rope chunks traversed while applying or reading text.
    pub rope_chunks_traversed: u64,
    /// Text edit operations applied to stored text.
    pub rope_edit_operations: u64,
    /// Explicit complete-source string materializations on a command path.
    pub full_source_materializations: u64,
}

impl SourceWork {
    /// Merges another counter set into this one (checked addition).
    pub fn merge(&mut self, other: &Self) {
        self.validated_operations += other.validated_operations;
        self.effective_splices += other.effective_splices;
        self.bytes_removed += other.bytes_removed;
        self.bytes_inserted += other.bytes_inserted;
        self.coordinate_islands_built += other.coordinate_islands_built;
        self.coordinate_islands_queried += other.coordinate_islands_queried;
        self.rope_chunks_traversed += other.rope_chunks_traversed;
        self.rope_edit_operations += other.rope_edit_operations;
        self.full_source_materializations += other.full_source_materializations;
    }
}

// ---------------------------------------------------------------------------
// Persistent source revisions (plan §6)
// ---------------------------------------------------------------------------

/// One editor operation against an authoritative source document.
///
/// Coordinates are bytes against the CURRENT revision; the workspace URI
/// selects the document.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DocumentId(pub u64);

/// Per-document monotonic revision counter. Advances only for effective
/// text changes; equality never compares Rope contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SourceRevisionId(pub u64);

/// Global monotonic command counter; equal-looking edits stay distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SourceCommandId(pub u64);

/// The identity of one open document.
#[derive(Clone, Debug)]
pub(crate) struct DocumentIdentity {
    pub(crate) id: DocumentId,
    pub(crate) uri: Arc<Uri<String>>,
}

/// One exact replacement between two revisions: old coordinates in the
/// pre-command revision, new coordinates in the resulting one. Removed or
/// inserted bytes live only in the two Ropes, never in a delta payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSplice {
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
}

/// The exact sparse delta that produced one revision from its predecessor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceDelta {
    Load { new_len: usize },
    Edit { splices: Arc<[SourceSplice]> },
}

/// A maximal unchanged region: equal old/new length and an affine offset,
/// referring only to bytes copied verbatim from the previous Rope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnchangedIsland {
    pub(crate) old: Range<usize>,
    pub(crate) new: Range<usize>,
    pub(crate) delta: isize,
}

/// Sorted non-overlapping islands in both coordinate spaces with boundary
/// arrays for `O(log m)` point mapping.
#[derive(Clone, Debug, Default)]
pub(crate) struct SourceCoordinateMap {
    pub(crate) islands: Arc<[UnchangedIsland]>,
    pub(crate) old_starts: Arc<[usize]>,
    pub(crate) new_starts: Arc<[usize]>,
}

impl SourceCoordinateMap {
    fn signed_offset(new_start: usize, old_start: usize) -> isize {
        if new_start >= old_start {
            isize::try_from(new_start - old_start).unwrap_or(isize::MAX)
        } else {
            -isize::try_from(old_start - new_start).unwrap_or(isize::MAX)
        }
    }

    /// Builds the island complement of normalized splices in `O(m)`.
    pub(crate) fn build(old_len: usize, new_len: usize, splices: &[SourceSplice]) -> Self {
        let mut islands = Vec::with_capacity(splices.len() + 1);
        let mut old_cursor = 0usize;
        let mut new_cursor = 0usize;
        for splice in splices {
            if splice.old_range.start > old_cursor {
                let unchanged_len = splice.old_range.start - old_cursor;
                let delta = Self::signed_offset(new_cursor, old_cursor);
                islands.push(UnchangedIsland {
                    old: old_cursor..splice.old_range.start,
                    new: new_cursor..new_cursor + unchanged_len,
                    delta,
                });
            }
            old_cursor = splice.old_range.end;
            new_cursor = splice.new_range.end;
        }
        if old_cursor < old_len {
            let tail_old = old_len - old_cursor;
            let last_new_start = new_cursor;
            debug_assert_eq!(last_new_start + tail_old, new_len);
            islands.push(UnchangedIsland {
                old: old_cursor..old_len,
                new: last_new_start..last_new_start + tail_old,
                delta: Self::signed_offset(last_new_start, old_cursor),
            });
        } else {
            debug_assert_eq!(new_cursor, new_len);
        }
        let old_starts: Arc<[usize]> = islands.iter().map(|island| island.old.start).collect();
        let new_starts: Arc<[usize]> = islands.iter().map(|island| island.new.start).collect();
        Self {
            islands: islands.into(),
            old_starts,
            new_starts,
        }
    }

    /// Maps an old coordinate into the new space when it lies inside one
    /// island at a whole offset; gaps between changed ranges never map.
    pub(crate) fn map_old_to_new(&self, at: usize) -> Option<(usize, usize)> {
        let index = self
            .old_starts
            .partition_point(|start| *start <= at)
            .saturating_sub(1);
        let island = self.islands.get(index)?;
        if !island.old.contains(&at) {
            return None;
        }
        let offset = at - island.old.start;
        Some((island.new.start + offset, index))
    }
}

/// An immutable committed revision: shared Rope, exact delta, and the
/// coordinate map derived once at construction.
/// An immutable committed revision. Public for façade construction; fields
/// stay behind accessors on [`SourceSnapshot`].
#[derive(Clone)]
pub struct SourceRevision {
    pub(crate) document: DocumentIdentity,
    pub(crate) id: SourceRevisionId,
    pub(crate) previous: Option<SourceRevisionId>,
    pub(crate) text: Arc<ropey::Rope>,
    pub delta: SourceDelta,
    pub(crate) coordinates: SourceCoordinateMap,
}

impl SourceRevision {
    /// The shared authoritative Rope.
    pub fn text(&self) -> &Arc<ropey::Rope> {
        &self.text
    }
}

/// One exact per-document source command: base revision check plus the
/// pre-built next text. Equality is the command id (plan §6).
#[derive(Clone)]
pub struct SourceCommand {
    pub(crate) id: SourceCommandId,
    pub(crate) fresh_document_id: Option<DocumentId>,
    pub base: Option<(DocumentId, SourceRevisionId)>,
    pub delta: SourceDelta,
    pub(crate) next_text: Arc<ropey::Rope>,
}

impl PartialEq for SourceCommand {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for SourceCommand {}

impl PartialEq for SourceRevision {
    fn eq(&self, other: &Self) -> bool {
        self.document.id == other.document.id && self.id == other.id
    }
}
impl Eq for SourceRevision {}

impl std::fmt::Debug for SourceRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRevision")
            .field("doc", &self.document.id.0)
            .field("rev", &self.id.0)
            .finish()
    }
}

impl std::fmt::Debug for SourceCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCommand")
            .field("id", &self.id.0)
            .finish()
    }
}

/// The reactive command channel: one entry per open document per epoch.
#[view]
pub struct SourceEdits(Map<String, SourceCommand>);

/// The committed revision per document. Public read-side façade; writes
/// flow only through the source pipeline (plan §6).
#[view]
pub struct SourceRevisions(Map<String, Arc<SourceRevision>>);

/// Snapshot façade over one committed revision (plan §6).
///
/// Complete-text allocation happens only on explicit [`Self::to_string`].
#[derive(Clone)]
pub struct SourceSnapshot(pub(crate) Arc<SourceRevision>);

impl SourceSnapshot {
    /// The revision this snapshot captured.
    pub fn revision_id(&self) -> u64 {
        self.0.id.0
    }

    /// Authoritative byte length.
    pub fn len_bytes(&self) -> usize {
        self.0.text.len_bytes()
    }

    /// True when the document is empty.
    pub fn is_empty(&self) -> bool {
        self.0.text.len_bytes() == 0
    }

    /// The shared Rope backing this revision.
    pub fn rope(&self) -> &Arc<ropey::Rope> {
        &self.0.text
    }

    /// Checked byte slice; UTF-8 boundaries required.
    pub fn byte_slice(&self, range: Range<usize>) -> Result<String, Error> {
        let rope = self.0.text.clone();
        if rope.try_byte_to_char(range.start).is_err() || rope.try_byte_to_char(range.end).is_err()
        {
            #[allow(clippy::needless_borrows_for_generic_args)]
            return Err(Error::Internal("byte slice off char boundary".into()));
        }
        Ok(rope.slice(range.start..range.end).to_string())
    }

    /// Explicit full-text allocation.
    pub fn to_string(&self) -> String {
        self.0.text.to_string()
    }
}

// ---------------------------------------------------------------------------
// Normalization (plan §6): validation before the engine opens its epoch.
// ---------------------------------------------------------------------------

/// A validated, ordered set of splices for one document plus counters.
#[derive(Debug, Default)]
pub(crate) struct NormalizedBatch {
    pub(crate) inserted: Vec<String>,
    pub(crate) splices: Vec<SourceSplice>,
    pub(crate) validated_operations: u64,
    pub(crate) effective_splices: u64,
    pub(crate) bytes_removed: u64,
    pub(crate) bytes_inserted: u64,
    /// Rope chunks visited while validating exact replacement equality.
    pub(crate) rope_chunks_traversed: u64,
}

/// Validates and normalizes editor operations against the current Rope:
/// bounds + UTF-8 boundaries, stable ordering, overlap rejection, adjacent
/// merging, and exact-equality dropping (no-op batches yield no splices).
pub(crate) fn normalize_edits(
    base: Option<&ropey::Rope>,
    edits: &[SourceEdit],
) -> Result<NormalizedBatch, Error> {
    let mut out = NormalizedBatch::default();
    out.validated_operations = edits.len() as u64;
    if edits.is_empty() {
        return Ok(out);
    }
    let Some(base) = base else {
        return Err(Error::Internal("edits against an unopened document".into()));
    };
    let len = base.len_bytes();

    #[derive(PartialEq)]
    enum Kind<'a> {
        Delete { start: usize, end: usize },
        Insert { at: usize, value: &'a str },
    }
    struct Op<'a> {
        ordinal: usize,
        kind: Kind<'a>,
    }

    let mut ops: Vec<Op> = Vec::with_capacity(edits.len());
    for (ordinal, edit) in edits.iter().enumerate() {
        match edit {
            SourceEdit::Insert { key, value } => {
                let at = key.range.start();
                if at > len || base.try_byte_to_char(at).is_err() {
                    return Err(Error::Internal(
                        format!("insert at {at} out of bounds").into(),
                    ));
                }
                ops.push(Op {
                    ordinal,
                    kind: Kind::Insert { at, value },
                });
            }
            SourceEdit::Delete { key } => {
                let start = key.range.start();
                let end = key.range.end();
                if start > end
                    || end > len
                    || base.try_byte_to_char(start).is_err()
                    || base.try_byte_to_char(end).is_err()
                {
                    return Err(Error::Internal(
                        format!("delete {start}..{end} invalid").into(),
                    ));
                }
                ops.push(Op {
                    ordinal,
                    kind: Kind::Delete { start, end },
                });
            }
        }
    }

    // Stable sort: points before ranges at equal offsets, caller order kept.
    ops.sort_by(|a, b| {
        let ka = (&a.kind, a.ordinal);
        let kb = (&b.kind, b.ordinal);
        let pa = match &ka.0 {
            Kind::Insert { at, .. } => *at,
            Kind::Delete { start, .. } => *start,
        };
        let pb = match &kb.0 {
            Kind::Insert { at, .. } => *at,
            Kind::Delete { start, .. } => *start,
        };
        pa.cmp(&pb).then_with(|| match (&ka.0, &kb.0) {
            (Kind::Insert { .. }, Kind::Delete { .. }) => std::cmp::Ordering::Less,
            (Kind::Delete { .. }, Kind::Insert { .. }) => std::cmp::Ordering::Greater,
            _ => ka.1.cmp(&kb.1),
        })
    });

    // Merge into replacements; reject overlaps.
    struct Repl {
        old_range: Range<usize>,
        inserted: String,
        ordinal: usize,
    }
    let mut replacements: Vec<Repl> = Vec::new();
    let mut index = 0usize;
    while index < ops.len() {
        let first_delete = match &ops[index].kind {
            Kind::Delete { start, end } => Some((*start, *end)),
            Kind::Insert { .. } => None,
        };
        match first_delete {
            None => {
                // Insert run at one point; a delete starting at the same
                // offset joins it into one replacement (insert-before-delete
                // ordering from the stable sort).
                let at = match &ops[index].kind {
                    Kind::Insert { at, .. } => *at,
                    _ => unreachable!(),
                };
                let mut value = String::new();
                let mut delete_end = at;
                let mut delete_taken = false;
                while index < ops.len() {
                    match &ops[index].kind {
                        Kind::Insert {
                            at: other_at,
                            value: part,
                        } if *other_at == at => {
                            value.push_str(part);
                            index += 1;
                        }
                        Kind::Delete { start, end } if !delete_taken && *start == at => {
                            // One replacement combines the delete with the
                            // inserts at its old start (plan §6 step 3).
                            debug_assert!(*end >= delete_end);
                            delete_end = *end;
                            delete_taken = true;
                            index += 1;
                        }
                        _ => break,
                    }
                }
                replacements.push(Repl {
                    old_range: at..delete_end,
                    inserted: value,
                    ordinal: 0,
                });
            }
            Some((delete_start, delete_end)) => {
                let mut inserted = String::new();
                index += 1;
                while index < ops.len() {
                    match &ops[index].kind {
                        Kind::Insert { at, value } if *at == delete_start => {
                            inserted.push_str(value);
                            index += 1;
                        }
                        _ => break,
                    }
                }
                replacements.push(Repl {
                    old_range: delete_start..delete_end,
                    inserted,
                    ordinal: 0,
                });
            }
        }
    }

    // Overlap rejection across replacements (deletes may not nest).
    for pair in replacements.windows(2) {
        if pair[0].old_range.end > pair[1].old_range.start {
            return Err(Error::Internal(
                format!(
                    "overlapping source edits {}..{} and {}..{}",
                    pair[0].old_range.start,
                    pair[0].old_range.end,
                    pair[1].old_range.start,
                    pair[1].old_range.end
                )
                .into(),
            ));
        }
    }

    // Compute new ranges with a running signed shift; drop exact equals by
    // comparing replacement bytes directly against the old rope range.
    let mut shift: isize = 0;
    for repl in &mut replacements {
        let old_start = repl
            .old_range
            .start
            .checked_add_signed(shift)
            .ok_or_else(|| Error::Internal("source batch overflow".into()))?;
        // Exact-equality drop: compare without materializing the old range.
        let equal = repl.inserted.len() == repl.old_range.len() && {
            let mut old_chunks = 0u64;
            let old_bytes = base
                .slice(repl.old_range.start..repl.old_range.end)
                .chunks()
                .flat_map(|chunk| {
                    old_chunks += 1;
                    chunk.as_bytes().iter().copied()
                });
            let equal = repl
                .inserted
                .as_bytes()
                .iter()
                .copied()
                .zip(old_bytes)
                .all(|(inserted_byte, old_byte)| inserted_byte == old_byte);
            out.rope_chunks_traversed += old_chunks;
            equal
        };
        if equal {
            continue;
        }
        let new_start = old_start;
        let new_end = old_start + repl.inserted.len();
        out.inserted.push(repl.inserted.clone());
        out.splices.push(SourceSplice {
            old_range: repl.old_range.start..repl.old_range.end,
            new_range: new_start..new_end,
        });
        out.bytes_removed += (repl.old_range.end - repl.old_range.start) as u64;
        out.bytes_inserted += repl.inserted.len() as u64;
        shift += repl.inserted.len() as isize - repl.old_range.len() as isize;
    }
    // Merge adjacent splices when both coordinate ranges are contiguous.
    let mut merged_splices: Vec<SourceSplice> = Vec::with_capacity(out.splices.len());
    let mut merged_inserted: Vec<String> = Vec::with_capacity(out.splices.len());
    for (splice, text) in out.splices.drain(..).zip(out.inserted.drain(..)) {
        match (merged_splices.last_mut(), merged_inserted.last_mut()) {
            (Some(last), Some(last_text))
                if last.old_range.end == splice.old_range.start
                    && last.new_range.end == splice.new_range.start =>
            {
                last.old_range.end = splice.old_range.end;
                last.new_range.end = splice.new_range.end;
                last_text.push_str(&text);
            }
            _ => {
                merged_splices.push(splice);
                merged_inserted.push(text);
            }
        }
    }
    out.effective_splices = merged_splices.len() as u64;
    out.splices = merged_splices;
    out.inserted = merged_inserted;
    Ok(out)
}

/// Applies normalized splices to an `O(1)`-cloned Rope in descending order,
/// converting each byte boundary to a Rope character boundary exactly once.
pub(crate) fn apply_splices(
    base: &ropey::Rope,
    splices: &[SourceSplice],
    inserted: &[String],
) -> Result<ropey::Rope, Error> {
    let mut rope = base.clone();
    // Descending application keeps earlier offsets valid.
    for (splice, text) in splices.iter().rev().zip(inserted.iter().rev()) {
        let start = rope.byte_to_char(splice.old_range.start);
        let end = rope.byte_to_char(splice.old_range.end);
        rope.remove(start..end);
        if !text.is_empty() {
            rope.insert(start, text);
        }
    }
    Ok(rope)
}

// ---------------------------------------------------------------------------
// Keyed source pipeline
// ---------------------------------------------------------------------------

/// Folds one document's command into its next revision and publishes the
/// authoritative [`SourceRevisions`] entry.
fn source_document(uri: String) -> Result<()> {
    let commands = observe_view::<SourceEdits>()?;
    let revisions_emit = emit_view::<SourceRevisions>()?;

    let Some(command) = commands.get(&uri)? else {
        return revisions_emit.remove(uri);
    };

    // Base check: stale commands are rejected instead of guessed.
    let previous = crate::reactive::peek_committed::<SourceRevisions>(uri.clone())?;
    let expected_base = previous
        .as_ref()
        .map(|revision| (revision.document.id, revision.id));
    if command.base != expected_base {
        return Err(Error::StaleSourceRevision { uri: uri.clone() });
    }

    // A reopened URI is a new document lineage. The workspace supplies a
    // fresh id only for a URI whose previous membership was closed; the
    // first open retains the deterministic URI-derived identity used by
    // independent workspaces.
    let document_identity = || -> DocumentIdentity {
        previous
            .as_ref()
            .map(|revision| revision.document.clone())
            .unwrap_or(DocumentIdentity {
                id: command
                    .fresh_document_id
                    .unwrap_or(DocumentId(fnv1a_uri(uri.as_str()))),
                uri: Arc::new(Uri::parse(uri.to_string()).expect("workspace uris parse")),
            })
    }();

    let next_revision_id = SourceRevisionId(
        previous
            .as_ref()
            .map(|revision| revision.id.0 + 1)
            .unwrap_or(1),
    );

    let (coordinates, new_len) = match &command.delta {
        SourceDelta::Load { new_len } => (SourceCoordinateMap::default(), *new_len),
        SourceDelta::Edit { splices } => {
            let old_len = previous.as_ref().map(|r| r.text.len_bytes()).unwrap_or(0);
            (
                SourceCoordinateMap::build(old_len, command.next_text.len_bytes(), splices),
                command.next_text.len_bytes(),
            )
        }
    };

    crate::framework::workspace::record_source_work(&uri, |work| {
        work.coordinate_islands_built += coordinates.islands.len() as u64;
        work.rope_edit_operations += match &command.delta {
            SourceDelta::Load { .. } => 1,
            SourceDelta::Edit { splices } => splices.len() as u64,
        };
    });

    let revision = Arc::new(SourceRevision {
        document: document_identity,
        id: next_revision_id,
        previous: previous.as_ref().map(|revision| revision.id),
        text: Arc::clone(&command.next_text),
        delta: command.delta.clone(),
        coordinates,
    });
    revisions_emit.insert(uri.clone(), Arc::clone(&revision))?;
    let _ = new_len;
    Ok(())
}

/// FNV-1a over the URI bytes — the same stable derivation the lexer uses
/// for `StableDocumentId` (plan §3.2: identity from final text/URI, never
/// process state).
pub(crate) fn fnv1a_uri(uri: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in uri.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

static NEXT_COMMAND_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub(crate) fn next_command_id_pub() -> u64 {
    next_command_id()
}

fn next_command_id() -> u64 {
    std::sync::atomic::AtomicU64::fetch_add(
        &NEXT_COMMAND_ID,
        1,
        std::sync::atomic::Ordering::Relaxed,
    )
}

/// Reads one committed revision from an engine snapshot (plan §6 façade).
///
/// Free-standing so the reactive module never names framework types.
pub fn source_snapshot(snapshot: &crate::reactive::Snapshot, uri: &str) -> Option<SourceSnapshot> {
    snapshot
        .observe::<SourceRevisions>(uri.to_string())
        .map(|value| SourceSnapshot(Arc::clone(&*value)))
}

/// Installs the built-in source fold as one first-class component (Cut C):
/// membership lifecycle over `SourceEdits`, identity = definition + URI.
pub fn install_source(engine: &mut Engine) -> Result<()> {
    engine.install_component_each_key::<SourceFoldDefinition, SourceEdits, _>(|uri| {
        source_document(uri)
    })?;
    Ok(())
}

/// Definition marker for the framework source fold (Cut C).
#[doc(hidden)]
pub struct SourceFoldDefinition;

impl crate::reactive::component::ComponentDefinition for SourceFoldDefinition {
    fn __descriptor() -> &'static str {
        "plingo::framework::source::source_fold"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_drops_exact_equals_and_merges_adjacent() {
        let base = ropey::Rope::from_str("hello world");
        let uri = Span::point("t://norm".into(), 0).unwrap().uri;
        // Equal-width equal-value delete+insert drops out entirely.
        let edits = [
            SourceEdit::Delete {
                key: Span::new_uri(uri.clone(), 0, 5).unwrap(),
            },
            SourceEdit::Insert {
                key: Span::new_uri(uri.clone(), 0, 0).unwrap(),
                value: "hello".into(),
            },
        ];
        let batch = normalize_edits(Some(&base), &edits).unwrap();
        assert_eq!(batch.splices.len(), 0);

        let edits = [
            SourceEdit::Delete {
                key: Span::new_uri(uri.clone(), 5, 6).unwrap(),
            },
            SourceEdit::Delete {
                key: Span::new_uri(uri.clone(), 6, 7).unwrap(),
            },
        ];
        let batch = normalize_edits(Some(&base), &edits).unwrap();
        assert_eq!(batch.splices.len(), 1);
        assert_eq!(batch.splices[0].old_range, 5..7);
    }

    #[test]
    fn coordinate_map_round_trips_unchanged_regions() {
        let splices = vec![SourceSplice {
            old_range: 3..4,
            new_range: 3..6,
        }];
        let map = SourceCoordinateMap::build(10, 12, &splices);
        // Byte 0..3 unchanged: maps 1:1 inside island 0.
        assert_eq!(map.map_old_to_new(2), Some((2, 0)));
        // The changed range never maps as unchanged.
        assert_eq!(map.map_old_to_new(3), None);
    }

    #[test]
    fn coordinate_map_tracks_each_prior_splice_shift() {
        let splices = vec![
            SourceSplice {
                old_range: 2..3,
                new_range: 2..5,
            },
            SourceSplice {
                old_range: 8..10,
                new_range: 10..11,
            },
        ];
        let map = SourceCoordinateMap::build(20, 21, &splices);
        assert_eq!(map.map_old_to_new(1), Some((1, 0)));
        // The second unchanged island includes the +2 shift from the first
        // splice, not the old-coordinate distance from the previous cursor.
        assert_eq!(map.map_old_to_new(4), Some((6, 1)));
        assert_eq!(map.map_old_to_new(10), Some((11, 2)));
        assert_eq!(map.map_old_to_new(19), Some((20, 2)));
    }

    #[test]
    fn apply_splices_produce_expected_text() {
        let base = ropey::Rope::from_str("abcdef");
        let splices = vec![
            SourceSplice {
                old_range: 1..2,
                new_range: 1..1,
            },
            SourceSplice {
                old_range: 4..5,
                new_range: 3..5,
            },
        ];
        let inserted = vec![String::new(), "XY".into()];
        let next = apply_splices(&base, &splices, &inserted).unwrap();
        assert_eq!(next.to_string(), "acdXYf");
    }
}
