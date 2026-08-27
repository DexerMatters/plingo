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
            debug_assert!(
                self.updated.binary_search(a).is_err(),
                "inserted overlaps updated"
            );
            debug_assert!(
                self.removed.binary_search(a).is_err(),
                "inserted overlaps removed"
            );
        }
        for a in &*self.updated {
            debug_assert!(
                self.removed.binary_search(a).is_err(),
                "updated overlaps removed"
            );
        }
    }
}

fn is_sorted<K: Ord>(values: &[K]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

/// An ordered sequence splice for one parent's child list (plan §8.5).
/// `before`/`after` are the retained neighbor anchors the splice sits
/// between. The delta carries only the bounded middle; the committed
/// order is reconstructed from the previous publication plus this splice.
#[derive(Debug, Clone, Default)]
pub struct OrderedDelta<K: Clone + Ord> {
    pub before: Option<K>,
    pub removed: Arc<[K]>,
    pub inserted: Arc<[K]>,
    pub after: Option<K>,
}

/// One parent's child-order change. The bounded splice carries the old
/// records needed to retract dead links and the live records needed to
/// publish inserted links.
#[derive(Debug, Clone)]
pub struct ChildSplice {
    pub parent: SyntaxNodeId,
    pub delta: OrderedDelta<SyntaxNodeId>,
    /// Dropped children: stable identity paired with the (possibly dead)
    /// arena record that resolves their former link identity.
    pub removed_children: Arc<[(SyntaxNodeId, u64)]>,
    /// Inserted children, same pairing, all alive at publication time.
    pub inserted_children: Arc<[(SyntaxNodeId, u64)]>,
}

impl ChildSplice {
    /// Validates anchor membership against the old order (debug).
    pub(crate) fn assert_anchored(&self, order_before: &[SyntaxNodeId]) {
        let start = self
            .delta
            .before
            .as_ref()
            .map(|before| {
                order_before
                    .iter()
                    .position(|id| id == before)
                    .expect("splice before-anchor missing from old order")
                    + 1
            })
            .unwrap_or(0);
        let end = self
            .delta
            .after
            .as_ref()
            .map(|after| {
                order_before
                    .iter()
                    .position(|id| id == after)
                    .expect("splice after-anchor missing from old order")
            })
            .unwrap_or(order_before.len());
        debug_assert!(start <= end, "splice anchors cross in old order");
        debug_assert_eq!(
            order_before.get(start..end).unwrap_or_default(),
            self.delta.removed.as_ref(),
            "splice removed run differs from old order"
        );
        for id in &*self.delta.inserted {
            debug_assert!(
                !order_before[..start].contains(id) && !order_before[end..].contains(id),
                "splice inserts a link that survives outside the replaced run"
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

/// One removed raw AST record plus the old topology needed for exact
/// retraction without touching the dying arena. Ordered by lineage so
/// canonical delta domains stay sorted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemovedRecord {
    /// The stable syntax identity that leaves publication.
    pub lineage: SyntaxNodeId,
    /// The dead arena record (debug/oracle reference only).
    pub record: u64,
    /// The record's former parent, when it had one.
    pub parent_record: Option<u64>,
    /// The former parent's stable lineage, when it had one.
    pub parent_lineage: Option<u64>,
    /// The record's former direct child records, in order.
    pub child_records: Arc<[u64]>,
}


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
    /// Cut E: the post-publication stable child orders for every parent
    /// touched this command, consumed by the caller to seed the next
    /// command's splice oracle. Not a public wire field.
    pub(crate) child_orders_next: Arc<std::collections::HashMap<u64, Vec<u64>>>,
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
    /// Removed syntax records with the OLD topology metadata publication
    /// needs to retract exactly (follow-up plan §5 items 6–7): the dead
    /// arena record must never be consulted after lineage death, so its
    /// former parent and former direct child records ride in the delta.
    pub removed_records: Arc<[RemovedRecord]>,
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
        Self {
            document_id: 0,
            ordinal: 0,
        }
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
