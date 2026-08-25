//! The canonical, authoritative parser output (plan §8.4–§9).
//!
//! [`ParseDelta`] is the exact adjacent-revision output of one parser
//! command: disjoint inserted/updated/removed key sets per granular fact
//! domain, sorted by key, plus ordered child/root splices. The tree
//! publisher consumes nothing else (plan §12). All identities are stable
//! per-document syntax lineages (`SyntaxNodeId`), so a retained node keeps
//! its key while its payload or children change.

use std::sync::Arc;

/// A disjoint key delta with sorted inserted/updated/removed sets (plan
/// §9): `updated` means the fact payload changed under a retained stable
/// key; a retained equal fact never appears.
#[derive(Debug, Clone, Default)]
pub struct KeyDelta<K: Ord + Clone> {
    pub inserted: Arc<[K]>,
    pub updated: Arc<[K]>,
    pub removed: Arc<[K]>,
}

impl<K: Ord + Clone> KeyDelta<K> {
    /// True when nothing changed.
    pub fn is_empty(&self) -> bool {
        self.inserted.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }

    /// Total touched key count.
    pub fn len(&self) -> usize {
        self.inserted.len() + self.updated.len() + self.removed.len()
    }

    /// Validates disjointness and sort order (debug builds).
    pub(crate) fn assert_sorted_disjoint(&self) {
        debug_assert!(is_sorted(&self.inserted), "inserted not sorted");
        debug_assert!(is_sorted(&self.updated), "updated not sorted");
        debug_assert!(is_sorted(&self.removed), "removed not sorted");
        for a in &*self.inserted {
            debug_assert!(self.updated.binary_search(a).is_err(), "inserted overlaps updated");
            debug_assert!(self.removed.binary_search(a).is_err(), "inserted overlaps removed");
        }
        for a in &*self.updated {
            debug_assert!(self.removed.binary_search(a).is_err(), "updated overlaps removed");
        }
    }
}

fn is_sorted<K: Ord>(values: &[K]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

/// An ordered sequence splice for one parent's child list (plan §8.5).
/// `before`/`after` are the retained neighbor anchors the splice sits
/// between; `order_after` is the complete resulting order so publication
/// stays `O(splice)` without reading previous facts.
#[derive(Debug, Clone, Default)]
pub struct OrderedDelta<K: Clone + Ord> {
    pub before: Option<K>,
    pub removed: Arc<[K]>,
    pub inserted: Arc<[K]>,
    pub after: Option<K>,
    pub order_after: Arc<[K]>,
}

/// One parent's child-order change: the splice plus the resulting order.
/// `removed_children` pairs each retracted link's stable identity with the
/// dead arena record that resolves its kind for publication.
#[derive(Debug, Clone)]
pub struct ChildSplice {
    pub parent: SyntaxNodeId,
    pub delta: OrderedDelta<SyntaxNodeId>,
    pub removed_children: Arc<[(SyntaxNodeId, u64)]>,
}

impl ChildSplice {
    /// Validates anchor membership against the old/new orders (debug).
    pub(crate) fn assert_anchored(&self, order_before: &[SyntaxNodeId]) {
        if let Some(before) = &self.delta.before {
            debug_assert!(
                order_before.contains(before) || self.delta.order_after.contains(before),
                "splice before-anchor missing from both orders"
            );
        }
        let mut spliced: Vec<SyntaxNodeId> = Vec::new();
        for id in order_before {
            if !self.delta.removed.contains(id) {
                spliced.push(*id);
            }
        }
        // Insertions land between `before`/`after`; verify only that every
        // inserted link exists in the resulting order and no removed one does.
        for id in &*self.delta.inserted {
            debug_assert!(
                self.delta.order_after.contains(id),
                "inserted link absent from resulting order"
            );
            debug_assert!(!spliced.contains(id) || order_before.is_empty());
        }
        for id in &*self.delta.removed {
            debug_assert!(
                !self.delta.order_after.contains(id),
                "removed link still present in resulting order"
            );
        }
    }
}

/// A stable syntax identity observed by downstream components (plan §8.7).
/// Allocated per document from a checked serial; proofs may inherit an
/// older node's identity when correspondence is provable. Equality across
/// warm/cold parses is guaranteed only where the identity is derivable
/// canonically without history (plan §3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxNodeId(pub u64);

/// A synthesized (recovery) token identity (plan §14): deterministic
/// `(document, recovery segment, action ordinal)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntheticTokenId(pub u64);

/// The exact parse output for one document revision (plan §9). Every set
/// is sorted, disjoint, and exact; freeze asserts this in debug builds.
#[derive(Debug, Clone, Default)]
pub struct ParseDelta {
    /// Framework-private raw arena-record membership (inserts/removes only;
    /// records are immutable). Tree consumers never read this domain — it
    /// feeds debug oracles and reclamation accounting.
    pub ast_records: KeyDelta<u64>,
    /// Payload facts under stable syntax keys.
    pub syntax_payloads: KeyDelta<SyntaxNodeId>,
    /// Parent facts under stable syntax keys.
    pub parents: KeyDelta<SyntaxNodeId>,
    /// One ordered splice per changed parent.
    pub child_splices: Arc<[ChildSplice]>,
    /// Root-list splice.
    pub roots: OrderedDelta<SyntaxNodeId>,
    /// Synthesized recovery tokens.
    pub synthesized_tokens: KeyDelta<SyntheticTokenId>,
    /// Diagnostics keyed by stable diagnostic identity.
    pub diagnostics: KeyDelta<ParseDiagnosticKey>,
    /// Status fact when it changed.
    pub status: Option<ParsedStatus>,
    /// Framework-private alignment pairs: inserted syntax key → the live
    /// arena record that carries its payload/kind.
    pub inserted_records: Arc<[(SyntaxNodeId, u64)]>,
    /// Framework-private alignment pairs: removed syntax key → the dead
    /// arena record that still resolves its kind (arenas are append-only).
    pub removed_records: Arc<[(SyntaxNodeId, u64)]>,
    /// Framework-private alignment pairs: updated syntax key → the live
    /// arena record carrying its new payload.
    pub(crate) updated_records: Arc<[(SyntaxNodeId, u64)]>,
    /// Framework-private: the post-command persistent live-record set, so
    /// membership facts commit without cloning previous state.
    pub(crate) live_records: Arc<crate::reactive::store::RadixMap<()>>,
}

impl ParseDelta {
    /// The committed persistent live-record set.
    pub(crate) fn live_records(&self) -> &Arc<crate::reactive::store::RadixMap<()>> {
        &self.live_records
    }
}

impl ParseDelta {
    /// True when no fact domain changed.
    pub fn is_empty(&self) -> bool {
        self.ast_records.is_empty()
            && self.syntax_payloads.is_empty()
            && self.parents.is_empty()
            && self.child_splices.is_empty()
            && self.roots.removed.is_empty()
            && self.roots.inserted.is_empty()
            && self.synthesized_tokens.is_empty()
            && self.diagnostics.is_empty()
            && self.status.is_none()
    }

    /// Debug gate (plan Phase 7 exit): journal disjointness and anchors.
    pub fn assert_valid(&self) {
        self.ast_records.assert_sorted_disjoint();
        self.syntax_payloads.assert_sorted_disjoint();
        self.parents.assert_sorted_disjoint();
        self.synthesized_tokens.assert_sorted_disjoint();
        self.diagnostics.assert_sorted_disjoint();
        let mut splices = self.child_splices.as_ref().to_vec();
        splices.sort_by_key(|splice| splice.parent);
        splices.dedup_by_key(|splice| splice.parent);
        debug_assert_eq!(
            splices.len(),
            self.child_splices.len(),
            "duplicate child splices for one parent"
        );
    }
}

/// Typed parse status without stats (stats leave equality).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParsedStatus {
    Clean,
    Recovered { segments: usize },
    Unrecovered { regions: usize },
}

/// A parse diagnostic with stable identity derived from content plus a
/// deterministic disambiguation ordinal (plan §14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParseDiagnosticKey {
    pub document_id: u64,
    pub ordinal: u64,
}

impl Default for ParseDiagnosticKey {
    fn default() -> Self {
        Self { document_id: 0, ordinal: 0 }
    }
}

/// Recovery segment identity for persistent reuse (plan §14): stable while
/// its witness interval and canonical repair remain equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecoverySegmentId(pub u64);

/// Token anchor: BOF, an occurrence, or EOF (plan §7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenAnchor {
    Bof,
    Occurrence(u64),
    Eof,
}
