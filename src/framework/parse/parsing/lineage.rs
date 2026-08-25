//! Stable syntax-lineage identities (plan §8.7).
//!
//! Every live AST record carries a document-stable serial that downstream
//! components observe. A rebuilt record inherits its predecessor's
//! identity only through the plan's proof order; ambiguity falls back to
//! deterministic local replacement (a fresh serial). Identities are never
//! reused except through an explicit proof (plan §3.2 rule 6).

use std::collections::{HashMap, HashSet};

use crate::framework::parse::data::ast::{AnchoredSpan, AstArena, AstId};
use crate::framework::parse::data::product::{ProductArena, ProductData};

/// Proof-order candidate key: grammar role plus retained token anchors.
type CandidateKey = (u32, AnchoredSpan);

#[derive(Clone, Default)]
pub(crate) struct LineageState {
    /// Live record → stable syntax identity.
    record_lineages: HashMap<AstId, u64>,
    /// Live records indexed by (role, extent) — the proof-2 candidate set.
    anchor_index: HashMap<CandidateKey, Vec<AstId>>,
    /// Reverse anchor key per live record, for exact removal.
    record_anchor_key: HashMap<AstId, CandidateKey>,
    /// Identity → current bearer record.
    holders: HashMap<u64, AstId>,
    /// Per-document checked serial (plan §3.2).
    next_lineage: u64,
    /// Records created during the running command, in creation order
    /// (bottom-up). Drives the post-replay context pass.
    created: Vec<CreatedRecord>,
    /// Identities already proven onto one new record this command; guards
    /// against ambiguous double inheritance.
    claimed: HashSet<u64>,
    /// This command's proven correspondences: new record → old record.
    inherited: HashMap<AstId, AstId>,
    /// Records released during the running command → their identity.
    /// Feeds exact removal publication and proof-3 lookups against dead
    /// predecessors.
    died_lineages: HashMap<AstId, u64>,
}

/// One record created during the running command.
#[derive(Clone, Copy)]
struct CreatedRecord {
    record: AstId,
    production: u32,
    inherited_from: Option<AstId>,
}

impl LineageState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Begins one command's journal window.
    pub(crate) fn begin_command(&mut self) {
        self.created.clear();
        self.claimed.clear();
        self.inherited.clear();
        self.died_lineages.clear();
    }

    /// The live identity of one record.
    pub(crate) fn lineage_of(&self, record: AstId) -> Option<u64> {
        self.record_lineages.get(&record).copied()
    }

    /// The identity a record held when it was released this command.
    pub(crate) fn died_lineage_of(&self, record: u64) -> Option<u64> {
        self.died_lineages.get(&(record as usize)).copied()
    }

    /// The current bearer record of one stable identity.
    pub(crate) fn holder_of(&self, lineage: u64) -> Option<u64> {
        self.holders.get(&lineage).map(|record| *record as u64)
    }

    /// Replaces a record's inherited identity with a fresh serial. Used
    /// when the freeze discovers the proven counterpart is STILL live —
    /// i.e., two genuinely distinct nodes matched one candidate, and the
    /// ambiguity must resolve to local replacement (plan §21).
    pub(crate) fn freshen(&mut self, record: AstId) -> u64 {
        self.next_lineage += 1;
        let lineage = self.next_lineage;
        self.record_lineages.insert(record, lineage);
        self.holders.insert(lineage, record);
        lineage
    }

    /// Releases every record the freeze proved dead this command. Must run
    /// AFTER all lookups against dead counterparts.
    pub(crate) fn finalize_deaths(&mut self, records: Vec<usize>) {
        for record in records {
            let lineage = self.record_lineages.remove(&record);
            if let Some(lineage) = lineage {
                self.died_lineages.insert(record, lineage);
            }
            if let Some(key) = self.record_anchor_key.remove(&record) {
                if let Some(bucket) = self.anchor_index.get_mut(&key) {
                    bucket.retain(|&candidate| candidate != record);
                    if bucket.is_empty() {
                        self.anchor_index.remove(&key);
                    }
                }
            }
        }
    }

    /// The old counterpart this command proved for a new record.
    pub(crate) fn inherited_from(&self, record: AstId) -> Option<AstId> {
        self.inherited.get(&record).copied()
    }

    /// Registers a RETAINED record re-entering liveness through suffix
    /// attachment. Its identity was assigned when first built.
    pub(crate) fn register_retained(&mut self, record: AstId) {
        debug_assert!(
            self.record_lineages.contains_key(&record),
            "retained record must already carry an identity"
        );
    }

    /// Whether a record currently carries a live identity.
    pub(crate) fn is_live(&self, record: AstId) -> bool {
        self.record_lineages.contains_key(&record)
    }

    /// Assigns an identity to a freshly built node (plan §8.7):
    ///
    /// - proof 1 (exact reused record) never reaches here — the reduction
    ///   cache returns the original product before any build runs;
    /// - proof 2: same role and retained token-occurrence extent with
    ///   exactly one live old candidate whose identity is unclaimed this
    ///   command → inherit;
    /// - proofs 3 and 4 run after replay in [`Self::resolve_contexts`],
    ///   once parent lineages have settled;
    /// - otherwise (proof 5) allocate a fresh deterministic serial.
    pub(crate) fn assign_new(
        &mut self,
        production: u32,
        extent: AnchoredSpan,
        record: AstId,
    ) -> u64 {
        let mut inherited_pair = None;
        if let Some(candidates) = self.anchor_index.get(&(production, extent)) {
            if candidates.len() == 1 {
                let old_record = candidates[0];
                if let Some(lineage) = self.record_lineages.get(&old_record).copied()
                    && self.claimed.insert(lineage)
                {
                    inherited_pair = Some((old_record, lineage));
                }
            }
        }
        let lineage = match inherited_pair {
            Some((old_record, lineage)) => {
                self.inherited.insert(record, old_record);
                lineage
            }
            None => {
                self.next_lineage += 1;
                self.next_lineage
            }
        };
        self.created.push(CreatedRecord {
            record,
            production,
            inherited_from: inherited_pair.map(|(old_record, _)| old_record),
        });
        self.record_lineages.insert(record, lineage);
        self.anchor_index
            .entry((production, extent))
            .or_default()
            .push(record);
        self.record_anchor_key.insert(record, (production, extent));
        self.holders.insert(lineage, record);
        lineage
    }

    /// Allocates a fresh identity unconditionally (error/synthetic nodes).
    pub(crate) fn assign_fresh(&mut self, record: AstId) -> u64 {
        self.next_lineage += 1;
        let lineage = self.next_lineage;
        self.record_lineages.insert(record, lineage);
        self.holders.insert(lineage, record);
        lineage
    }

    /// Releases a dead record's registrations and returns the identity it
    /// held, so the command journal publishes exact removals.
    pub(crate) fn release(&mut self, record: AstId) -> Option<u64> {
        let lineage = self.record_lineages.remove(&record)?;
        self.died_lineages.insert(record, lineage);
        if let Some(key) = self.record_anchor_key.remove(&record) {
            if let Some(bucket) = self.anchor_index.get_mut(&key) {
                bucket.retain(|&candidate| candidate != record);
                if bucket.is_empty() {
                    self.anchor_index.remove(&key);
                }
            }
        }
        Some(lineage)
    }

    /// Resolves proofs 3 and 4 after replay: a fresh identity is replaced
    /// by an old node's identity when parent-lineage plus field position
    /// bounded by retained neighbor anchors prove correspondence.
    ///
    /// Creation is bottom-up, so iterating the created set in reverse
    /// visits ancestors before descendants; by the time children are
    /// examined their parents' identities have settled.
    pub(crate) fn resolve_contexts(&mut self, products: &ProductArena, ast: &AstArena) {
        // Only records whose parent PROVABLY corresponds (the parent itself
        // inherited its identity this command) can be position-matched.
        for index in (0..self.created.len()).rev() {
            let candidate = self.created[index];
            if candidate.inherited_from.is_some() {
                continue;
            }
            let Some(parent) = ast.parent_of(candidate.record) else {
                continue;
            };
            let Some(old_parent) = self.inherited.get(&parent).copied() else {
                continue;
            };
            let new_siblings = direct_child_records(products, ast, parent);
            let old_siblings = direct_child_records(products, ast, old_parent);
            if new_siblings.len() != old_siblings.len() {
                // List length changed: positional correspondence is not
                // provable from position alone.
                continue;
            }
            let Some(ordinal) = new_siblings
                .iter()
                .position(|&sibling| sibling == candidate.record)
            else {
                continue;
            };
            let old_sibling = old_siblings[ordinal];
            // The old counterpart may have died this command; its identity
            // is still discoverable through the release journal.
            let Some(old_lineage) =
                self.lineage_of(old_sibling).or_else(|| self.died_lineage_of(old_sibling as u64))
            else {
                continue;
            };
            if !self.claimed.insert(old_lineage) {
                // Ambiguity: another new node already claimed this
                // identity this command. Deterministic local replacement
                // wins (plan §21 stable-lineage decision).
                continue;
            }
            if !self.neighbors_agree(&new_siblings, &old_siblings, ordinal) {
                self.claimed.remove(&old_lineage);
                continue;
            }
            self.rekey(candidate.record, old_sibling, old_lineage);
            self.created[index].inherited_from = Some(old_sibling);
        }
        self.created.clear();
        self.claimed.clear();
    }

    /// Neighbor check around `ordinal` (proof 4): any neighbor pair whose
    /// identities are both known must match; unknown pairs anchor only by
    /// raw-record equality.
    fn neighbors_agree(
        &self,
        new_siblings: &[AstId],
        old_siblings: &[AstId],
        ordinal: usize,
    ) -> bool {
        let side_agrees = |index: usize| -> bool {
            let new_side = new_siblings[index];
            let old_side = old_siblings[index];
            match (
                self.record_lineages.get(&new_side),
                self.record_lineages.get(&old_side),
            ) {
                (Some(new_lineage), Some(old_lineage)) => new_lineage == old_lineage,
                _ => new_side == old_side,
            }
        };
        if ordinal > 0 && !side_agrees(ordinal - 1) {
            return false;
        }
        if ordinal + 1 < new_siblings.len().min(old_siblings.len())
            && !side_agrees(ordinal + 1)
        {
            return false;
        }
        true
    }

    fn rekey(&mut self, record: AstId, old_sibling: AstId, lineage: u64) {
        // Drop the fresh registration entirely, then adopt the proven one.
        if let Some(key) = self.record_anchor_key.remove(&record) {
            if let Some(bucket) = self.anchor_index.get_mut(&key) {
                bucket.retain(|&candidate| candidate != record);
                if bucket.is_empty() {
                    self.anchor_index.remove(&key);
                }
            }
        }
        // Re-keying onto an identical value is a no-op (the record may
        // already carry the proven identity through an earlier proof in
        // the same command).
        let _ = self.record_lineages.insert(record, lineage);
        self.holders.insert(lineage, record);
        self.inherited.entry(record).or_insert(old_sibling);
    }
}

/// The direct AST child records of one node, in order.
pub(crate) fn direct_child_records(products: &ProductArena, ast: &AstArena, record: AstId) -> Vec<AstId> {
    let Some(owner) = ast.product_of(record) else {
        return Vec::new();
    };
    let Some(product) = products.get(owner) else {
        return Vec::new();
    };
    match &product.data {
        ProductData::Node { children, .. } => children
            .iter()
            .filter_map(|&child| products.get(child))
            .filter_map(|child| match &child.data {
                ProductData::Node { ast, .. } => Some(*ast),
                ProductData::Token { ast: Some(ast), .. } => Some(*ast),
                ProductData::Error { .. } | ProductData::Token { ast: None, .. } => None,
            })
            .collect(),
        ProductData::Token { .. } | ProductData::Error { .. } => Vec::new(),
    }
}
