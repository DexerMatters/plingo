//! Semantic snapshot digests (follow-up plan §4 items 1–2, 8, 13).
//!
//! A [`SemanticDigest`] is the complete, ID-erased, canonically ordered
//! content of every public view of one example family. Rows are
//! `view::key = value` strings built from TYPED values (semantic payloads,
//! resolved lexemes, structural paths) — never `Debug` insertion order and
//! never raw node ordinals, which differ between a warm workspace and a
//! fresh cold build.
//!
//! The struct is intentionally generic: each example family supplies its own
//! capture function beside its views. Digests answer three Phase 0 gates:
//!
//! 1. **complete key domain** — every public view is enumerated through its
//!    committed inputs, including keys unreachable from an expected root;
//! 2. **warm/cold equivalence** — the same final external state produces
//!    equal digests in any engine;
//! 3. **reversibility** — restoring source text restores the exact initial
//!    digest, per-view row counts, and live-fact count.

use crate::reactive::Snapshot;
use std::collections::BTreeMap;

/// The canonical semantic digest of one workspace state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticDigest {
    rows: BTreeMap<String, String>,
}

impl SemanticDigest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one `view::key = value` row.
    pub fn insert(&mut self, view: &str, key: &str, value: &str) {
        self.rows.insert(format!("{view}::{key}"), value.to_owned());
    }

    /// Records one unordered-domain row (multiset members keep a stable
    /// ordinal within the row key).
    pub fn insert_domain(&mut self, view: &str, ordinal: usize, value: &str) {
        self.rows
            .insert(format!("{view}::#{ordinal:06}"), value.to_owned());
    }

    /// Merges another digest into this one.
    pub fn merge(&mut self, other: SemanticDigest) {
        self.rows.extend(other.rows);
    }

    /// The number of recorded rows (per-view counts come from [`Self::rows_in`]).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Number of rows belonging to one view.
    pub fn rows_in(&self, view: &str) -> usize {
        let prefix = format!("{view}::");
        self.rows
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .count()
    }

    /// Every `(key, value)` row of one view, in canonical order.
    pub fn rows_of<'a>(&'a self, view: &'a str) -> Vec<(&'a str, &'a str)> {
        let prefix = format!("{view}::");
        self.rows
            .iter()
            .filter(move |(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| (&**key, &**value))
            .collect()
    }

    /// Canonical multi-line rendering.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.rows {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(value);
            out.push('\n');
        }
        out
    }

    /// Stable fingerprint across processes.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::Hash;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.render().hash(&mut hasher);
        std::hash::Hasher::finish(&hasher)
    }
}

/// A digest plus the liveness bookkeeping compared on every reverse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FamilyState {
    pub digest: SemanticDigest,
    pub live_facts: u64,
}

impl FamilyState {
    /// Captures the live-fact count alongside a prebuilt digest.
    pub fn capture(digest: SemanticDigest, snapshot: &Snapshot) -> Self {
        Self {
            digest,
            live_facts: snapshot.live_fact_count(),
        }
    }
}

/// Renders a diff between two digests as human-readable rows naming the
/// exact changed/leaked keys per view (plan §4 item 8): a failure must
/// identify the broad component or leaked edge, not only the total count.
pub fn render_diff(before: &SemanticDigest, after: &SemanticDigest) -> String {
    let mut out = String::new();
    for (key, value) in &before.rows {
        match after.rows.get(key) {
            None => out.push_str(&format!("- {key} = {value}\n")),
            Some(next) if next != value => out.push_str(&format!("~ {key} = {value} -> {next}\n")),
            Some(_) => {}
        }
    }
    for (key, value) in &after.rows {
        if !before.rows.contains_key(key) {
            out.push_str(&format!("+ {key} = {value}\n"));
        }
    }
    out
}
