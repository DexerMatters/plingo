//! Stable syntax-lineage identities (follow-up plan §18 state machine).
//!
//! Every live AST record carries a document-stable serial that downstream
//! components observe. A rebuilt record inherits its predecessor's identity
//! only through the plan's proof order; ambiguity falls back to a reserved
//! fresh serial. Identities are never reused except through an explicit
//! proof (plan §3.2 rule 6).
//!
//! §18 invariant: `record_lineages` (effective identity) and `holders`
//! (committed bearer) are mutated ATOMICALLY by [`LineageState::settle`]
//! against final liveness. During replay a proof only records a pending
//! candidate; the old committed holder is never overwritten prematurely,
//! and publication can never observe two live bearers of one identity.

use std::collections::{HashMap, HashSet};

use crate::framework::parse::data::ast::{AnchoredSpan, AstArena, AstId};
use crate::framework::parse::data::product::{ProductArena, ProductData};

/// Proof-order candidate key: grammar role plus retained token anchors.
type CandidateKey = (u32, AnchoredSpan);

/// One unresolved inheritance proof: if `old_record` leaves the final live
/// set, `record` takes over `old_lineage`; otherwise `record` keeps its
/// reserved `fresh_lineage`.
#[derive(Clone, Copy, Debug)]
struct PendingInheritance {
    old_record: AstId,
    old_lineage: u64,
    fresh_lineage: u64,
}

#[derive(Clone, Default)]
pub(crate) struct LineageState {
    /// Effective identity per record during the command window. Settle
    /// makes it final for every live record.
    record_lineages: HashMap<AstId, u64>,
    /// Identity → current bearer record. Only settle mutates entries for
    /// inherited identities; fresh reservations write their own slot.
    holders: HashMap<u64, AstId>,
    /// Reserved identity for every record created this command.
    fresh_lineages: HashMap<AstId, u64>,
    /// Unresolved proofs awaiting final liveness.
    pending: HashMap<AstId, PendingInheritance>,
    /// Live records indexed by (role, extent) — the proof-2 candidate set.
    anchor_index: HashMap<CandidateKey, Vec<AstId>>,
    /// Reverse anchor key per live record, for exact removal.
    record_anchor_key: HashMap<AstId, CandidateKey>,
    /// Per-document checked serial (plan §3.2).
    next_lineage: u64,
    /// Records created during the running command, in creation order
    /// (bottom-up). Drives the post-replay context pass and settle order.
    created: Vec<CreatedRecord>,
    /// Identities already proven onto one new record this command; guards
    /// against ambiguous double inheritance.
    claimed: HashSet<u64>,
    /// This command's proven correspondences: new record → old record.
    inherited: HashMap<AstId, AstId>,
    /// Records released during the running command → their identity.
    /// Feeds exact removal publication and proof lookups against dead
    /// predecessors.
    died_lineages: HashMap<AstId, u64>,
    /// Anchor metadata retained with a dead record so an exact cached
    /// resurrection can restore its proof-2 candidate registration.
    died_anchor_keys: HashMap<AstId, CandidateKey>,
}

/// One record created during the running command.
#[derive(Clone, Copy)]
struct CreatedRecord {
    record: AstId,
    production: u32,
}

impl LineageState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Begins one command's journal window.
    ///
    /// `died_lineages` intentionally PERSISTS across commands (follow-up
    /// plan section 5/18): staged splice drops reference stable identities
    /// of records that may have died in EARLIER commands, so the death
    /// journal lives for the whole session.
    pub(crate) fn begin_command(&mut self) {
        self.created.clear();
        self.pending.clear();
        self.fresh_lineages.clear();
        self.claimed.clear();
        self.inherited.clear();
    }

    /// The live identity of one record.
    pub(crate) fn lineage_of(&self, record: AstId) -> Option<u64> {
        self.record_lineages.get(&record).copied()
    }

    /// The identity a record held when it was released this command.
    pub(crate) fn died_lineage_of(&self, record: u64) -> Option<u64> {
        self.died_lineages.get(&(record as usize)).copied()
    }

    /// Iterates this command's death journal as `(record, lineage)`.
    pub(crate) fn iter_died(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        self.died_lineages
            .iter()
            .map(|(record, lin)| (*record, *lin))
    }

    /// The current bearer record of one stable identity.
    pub(crate) fn holder_of(&self, lineage: u64) -> Option<u64> {
        self.holders.get(&lineage).map(|record| *record as u64)
    }

    /// The old counterpart this command proved for a new record.
    pub(crate) fn inherited_from(&self, record: AstId) -> Option<AstId> {
        self.inherited.get(&record).copied()
    }

    /// Whether a record currently carries a live identity.
    pub(crate) fn is_live(&self, record: AstId) -> bool {
        self.record_lineages.contains_key(&record)
    }

    /// Assigns an identity to a freshly built node (plan §8.7 / §18.3):
    ///
    /// - proof 1 (exact reused record) never reaches here — the reduction
    ///   cache returns the original product before any build runs;
    /// - proof 2: same role and retained token-occurrence extent with
    ///   exactly one live old candidate whose identity is unclaimed this
    ///   command → PENDING inheritance (the old holder keeps its entry);
    /// - proofs 3 and 4 run inside [`Self::settle`], once every final
    ///   record exists;
    /// - otherwise (proof 5) the reserved fresh serial becomes effective.
    ///
    /// The reserved fresh identity always exists so a failed proof falls
    /// back deterministically without minting at publication time.
    pub(crate) fn assign_new(
        &mut self,
        production: u32,
        extent: AnchoredSpan,
        record: AstId,
    ) -> u64 {
        let fresh = self.reserve_fresh(record);
        let mut pending = None;
        let candidates: Vec<AstId> = self
            .anchor_index
            .get(&(production, extent))
            .into_iter()
            .flatten()
            .copied()
            .filter(|candidate| {
                !self
                    .created
                    .iter()
                    .any(|created| created.record == *candidate)
            })
            .collect();
        if candidates.len() == 1 {
            let old_record = candidates[0];
            if let Some(lineage) = self.record_lineages.get(&old_record).copied()
                && !old_record.eq(&record)
                && self.claimed.insert(lineage)
            {
                // Effective identity during the command window; the old
                // committed holder is NOT overwritten (§18.3).
                self.record_lineages.insert(record, lineage);
                self.inherited.insert(record, old_record);
                pending = Some(PendingInheritance {
                    old_record,
                    old_lineage: lineage,
                    fresh_lineage: fresh,
                });
            }
        }
        if let Some(pending) = pending {
            self.pending.insert(record, pending);
        } else {
            self.holders.insert(fresh, record);
        }
        self.created.push(CreatedRecord { record, production });
        self.anchor_index
            .entry((production, extent))
            .or_default()
            .push(record);
        self.record_anchor_key.insert(record, (production, extent));
        self.lineage_of(record)
            .expect("assign_new leaves an effective identity")
    }

    /// Reserves a fresh deterministic serial for one new record.
    fn reserve_fresh(&mut self, record: AstId) -> u64 {
        self.next_lineage += 1;
        let fresh = self.next_lineage;
        self.fresh_lineages.insert(record, fresh);
        self.record_lineages.entry(record).or_insert(fresh);
        fresh
    }

    /// Resolves proofs 3 and 4 as part of settle: a pending-fresh record
    /// gains an old node's identity when parent-lineage plus field position
    /// bounded by retained neighbor anchors prove correspondence.
    ///
    /// Creation is bottom-up, so reverse creation order visits ancestors
    /// before descendants.
    fn resolve_contexts(&mut self, products: &ProductArena, ast: &AstArena) {
        // Reductions normally create descendants before their ancestors, but
        // cached-product reuse can interleave those records. Iterate to a
        // fixed point so a context proof never depends on that incidental
        // creation order.
        let mut changed = true;
        while changed {
            changed = false;
            for index in (0..self.created.len()).rev() {
                let candidate = self.created[index];
                if self.pending.contains_key(&candidate.record) {
                    continue;
                }
                let Some(parent) = ast.parent_of(candidate.record) else {
                    continue;
                };
                let Some(old_parent) = self.inherited.get(&parent).copied() else {
                    continue;
                };
                let fresh = match self.fresh_lineages.get(&candidate.record) {
                    Some(fresh) => *fresh,
                    None => continue,
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
                // The old counterpart may already have left liveness earlier
                // in this command; consult the death journal.
                let Some(old_lineage) = self
                    .lineage_of(old_sibling)
                    .or_else(|| self.died_lineage_of(old_sibling as u64))
                else {
                    continue;
                };
                if !self.claimed.insert(old_lineage) {
                    // Ambiguity: another new record claimed this identity.
                    // Deterministic local replacement wins.
                    continue;
                }
                if !self.neighbors_agree(&new_siblings, &old_siblings, ordinal) {
                    self.claimed.remove(&old_lineage);
                    continue;
                }
                // Convert to a pending transfer; holders stay untouched.
                self.record_lineages.insert(candidate.record, old_lineage);
                self.inherited
                    .entry(candidate.record)
                    .or_insert(old_sibling);
                self.pending.insert(
                    candidate.record,
                    PendingInheritance {
                        old_record: old_sibling,
                        old_lineage,
                        fresh_lineage: fresh,
                    },
                );
                changed = true;
            }
        }
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
        if ordinal + 1 < new_siblings.len().min(old_siblings.len()) && !side_agrees(ordinal + 1) {
            return false;
        }
        true
    }

    /// inherited dead record must not erase the new bearer written by
    /// step 2.
    pub(crate) fn settle(
        &mut self,
        _previous_live: &crate::reactive::store::RadixMap<()>,
        final_live: &crate::reactive::store::RadixMap<()>,
        newly_live: &[u64],
        newly_dead: &[u64],
        products: &ProductArena,
        ast: &AstArena,
    ) {
        // Only records whose reachability changed can acquire or lose a
        // lineage.  The accepted-root set is persistent; scanning it here
        // would turn every local edit into a document-sized operation.
        let dormant: Vec<AstId> = newly_live
            .iter()
            .copied()
            .map(|record| record as usize)
            .filter(|record| !self.record_lineages.contains_key(record))
            .collect();
        for record in dormant {
            // A cached product can leave the accepted root and later
            // re-enter it unchanged (notably on an exact reverse).
            let revived = self
                .died_lineages
                .get(&record)
                .copied()
                .filter(|lineage| !self.holders.contains_key(lineage));
            if let Some(lineage) = revived {
                self.record_lineages.insert(record, lineage);
                self.holders.insert(lineage, record);
                if let Some(key) = self.died_anchor_keys.remove(&record) {
                    self.anchor_index.entry(key).or_default().push(record);
                    self.record_anchor_key.insert(record, key);
                }
                self.died_lineages.remove(&record);
                continue;
            }
            let extent = ast.extent_of_id(record).unwrap_or(AnchoredSpan {
                start: 0,
                end: 0,
                end_at_token_end: false,
            });
            self.assign_new(u32::MAX, extent, record);
        }

        // Resolve context proofs against final liveness without walking the
        // complete accepted-root record set.
        self.resolve_contexts(products, ast);

        // Commit transfers or retain the reserved fresh identity when the
        // old bearer remains live.
        let created_order: Vec<AstId> = self.created.iter().map(|c| c.record).collect();
        for record in created_order {
            let Some(pending) = self.pending.get(&record).copied() else {
                continue;
            };
            if final_live.get(pending.old_record as u64).is_some() {
                self.record_lineages.insert(record, pending.fresh_lineage);
                self.holders.insert(pending.fresh_lineage, record);
                self.inherited.remove(&record);
            } else {
                if self.holders.get(&pending.fresh_lineage).copied() == Some(record) {
                    self.holders.remove(&pending.fresh_lineage);
                }
                self.record_lineages.insert(record, pending.old_lineage);
                self.holders.insert(pending.old_lineage, record);
            }
        }

        // Removed records plus newly-created records that failed to reach an
        // accepted root are the only possible dead candidates.
        let mut dead: Vec<u64> = newly_dead
            .iter()
            .copied()
            .chain(
                self.created
                    .iter()
                    .map(|created| created.record as u64)
                    .filter(|record| final_live.get(*record).is_none()),
            )
            .collect();
        dead.sort_unstable();
        dead.dedup();
        for record in dead {
            self.unbind_dead(record);
        }

        // Clear the command window.
        self.pending.clear();
        self.claimed.clear();
        self.fresh_lineages.clear();

        #[cfg(debug_assertions)]
        self.validate_settled(final_live);
    }

    fn registered_this_command(&self) -> Vec<u64> {
        self.anchor_index
            .values()
            .flat_map(|bucket| bucket.iter())
            .map(|record| *record as u64)
            .collect()
    }

    /// Releases one dead record's registrations and journals its identity.
    fn unbind_dead(&mut self, record: u64) {
        let record = record as usize;
        let lineage = self.record_lineages.remove(&record);
        if let Some(lineage) = lineage {
            self.died_lineages.insert(record, lineage);
        }
        if let Some(key) = self.record_anchor_key.remove(&record) {
            self.died_anchor_keys.insert(record, key);
            if let Some(bucket) = self.anchor_index.get_mut(&key) {
                bucket.retain(|&candidate| candidate != record);
                if bucket.is_empty() {
                    self.anchor_index.remove(&key);
                }
            }
        }
        // Only erase the holder when THIS record still bears the identity:
        // an inherited successor may have taken it over in step 2 (§18.4).
        if let Some(lineage) = lineage
            && self.holders.get(&lineage).copied() == Some(record)
        {
            self.holders.remove(&lineage);
        }
        self.pending.remove(&record);
    }

    /// Debug validation immediately after settle (§18.5).
    #[cfg(debug_assertions)]
    fn validate_settled(&self, final_live: &HashSet<u64>) {
        let mut seen_lineages: HashSet<u64> = HashSet::new();
        for record in final_live {
            let record = *record as usize;
            let Some(lineage) = self.record_lineages.get(&record) else {
                panic!("settled live record {record} carries no lineage");
            };
            assert!(
                seen_lineages.insert(*lineage),
                "two final live records share lineage {lineage}"
            );
            assert_eq!(
                self.holders.get(lineage),
                Some(&record),
                "holder mismatch for lineage {lineage}"
            );
        }
        for (lineage, record) in &self.holders {
            assert!(
                final_live.contains(&(*record as u64)),
                "holder of lineage {lineage} points at non-live record {record}"
            );
            assert_eq!(
                self.record_lineages.get(record),
                Some(lineage),
                "record {record} disagrees with its holder entry"
            );
        }
        assert!(self.pending.is_empty(), "pending proofs survived settle");
        assert!(self.claimed.is_empty(), "claims survived settle");
        assert!(
            self.fresh_lineages.is_empty(),
            "fresh reservations survived settle"
        );
    }
}

/// The direct AST child records of one node, in order.
pub(crate) fn direct_child_records(
    products: &ProductArena,
    ast: &AstArena,
    record: AstId,
) -> Vec<AstId> {
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

/// The direct AST records represented by generated tree child fields. Typed
/// token products are parser metadata, not tree nodes, so they are excluded
/// from parent orders and link deltas.
pub(crate) fn direct_tree_child_records(
    products: &ProductArena,
    ast: &AstArena,
    record: AstId,
) -> Vec<AstId> {
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
                ProductData::Token { .. } | ProductData::Error { .. } => None,
            })
            .collect(),
        ProductData::Token { .. } | ProductData::Error { .. } => Vec::new(),
    }
}
