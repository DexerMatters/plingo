//! Deterministic per-structure work counters (follow-up plan §4 item 7).
//!
//! Every primitive persistent-index operation increments plain thread-owned
//! integers on a preallocated fixed page; there is no hot-path map lookup,
//! allocation, cross-worker atomic, or key formatting. Counters live inside
//! the primitive operations (never at callers), so a hidden scan cannot evade
//! them. Reset and page merge occur outside measured closures.
//!
//! `PathWork` groups one [`PathWorkCounters`] page per [`StructureKind`].
//! Kinds whose owning structures arrive with later phases stay present in
//! the enum from day one so report schemas never change shape: they simply
//! remain zero until their structure exists.

use std::cell::RefCell;
use std::fmt;

/// The primitive persistent-index structures whose physical work is gated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub(crate) enum StructureKind {
    /// Source Rope boundary paths and splices.
    RopeTree = 0,
    /// The source coordinate island index.
    SourceIndex,
    /// Persistent map of open lexer document roots.
    LexerDocumentIndex,
    /// The lexical token tape.
    TokenTape,
    /// The token occurrence index.
    TokenIndex,
    /// Committed fact-store HAMTs (`SnapshotView`, `OwnerSet`).
    FactHamt,
    /// Component-instance ownership HAMTs (Phase 2).
    OwnershipHamt,
    /// Dependency-index HAMTs.
    DependencyHamt,
    /// The persistent collision B+ tree inside one HAMT bucket (Phase 2).
    CollisionTree,
    /// Parser radix indexes (`RadixMap` users inside the parser).
    ParserRadix,
    /// Parser exact-key indexes (generation stores, reduction caches).
    ParserIndex,
    /// The parser column sequence.
    ColumnSeq,
    /// Generated measured-field sequences (Phase 2+).
    MeasuredSeq,
    /// Dynamic SCC forward/reverse adjacency maps (Phase 2+).
    SccAdjacency,
    /// SCC membership maps (Phase 2+).
    SccMembers,
    /// Condensation-edge maps (Phase 2+).
    SccCondensation,
    /// The deterministic dirty queue.
    DirtyQueue,
}

impl StructureKind {
    /// Stable wire name used by reports and artifacts.
    pub fn name(self) -> &'static str {
        match self {
            StructureKind::RopeTree => "rope_tree",
            StructureKind::SourceIndex => "source_index",
            StructureKind::LexerDocumentIndex => "lexer_document_index",
            StructureKind::TokenTape => "token_tape",
            StructureKind::TokenIndex => "token_index",
            StructureKind::FactHamt => "fact_hamt",
            StructureKind::OwnershipHamt => "ownership_hamt",
            StructureKind::DependencyHamt => "dependency_hamt",
            StructureKind::CollisionTree => "collision_tree",
            StructureKind::ParserRadix => "parser_radix",
            StructureKind::ParserIndex => "parser_index",
            StructureKind::ColumnSeq => "column_seq",
            StructureKind::MeasuredSeq => "measured_seq",
            StructureKind::SccAdjacency => "scc_adjacency",
            StructureKind::SccMembers => "scc_members",
            StructureKind::SccCondensation => "scc_condensation",
            StructureKind::DirtyQueue => "dirty_queue",
        }
    }

    pub(crate) const ALL: [StructureKind; 17] = [
        StructureKind::RopeTree,
        StructureKind::SourceIndex,
        StructureKind::LexerDocumentIndex,
        StructureKind::TokenTape,
        StructureKind::TokenIndex,
        StructureKind::FactHamt,
        StructureKind::OwnershipHamt,
        StructureKind::DependencyHamt,
        StructureKind::CollisionTree,
        StructureKind::ParserRadix,
        StructureKind::ParserIndex,
        StructureKind::ColumnSeq,
        StructureKind::MeasuredSeq,
        StructureKind::SccAdjacency,
        StructureKind::SccMembers,
        StructureKind::SccCondensation,
        StructureKind::DirtyQueue,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// One fixed counter page. Plain integers only: increments compile to adds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathWorkCounters {
    pub operations: u64,
    pub key_comparisons: u64,
    pub nodes_visited: u64,
    pub nodes_copied: u64,
    pub nodes_created: u64,
    pub rebalances: u64,
    pub max_depth: u64,
}

impl PathWorkCounters {
    const FIELDS: usize = 7;

    fn zero() -> [u64; Self::FIELDS] {
        [0; Self::FIELDS]
    }

    fn observe(&mut self, depth: u64) {
        if depth > self.max_depth {
            self.max_depth = depth;
        }
    }

    /// Adds `other` into this page (merge of worker pages).
    pub fn merge(&mut self, other: &Self) {
        self.operations += other.operations;
        self.key_comparisons += other.key_comparisons;
        self.nodes_visited += other.nodes_visited;
        self.nodes_copied += other.nodes_copied;
        self.nodes_created += other.nodes_created;
        self.rebalances += other.rebalances;
        self.max_depth = self.max_depth.max(other.max_depth);
    }

    /// True when no operation was recorded.
    pub fn is_zero(&self) -> bool {
        self.operations == 0
            && self.key_comparisons == 0
            && self.nodes_visited == 0
            && self.nodes_copied == 0
            && self.nodes_created == 0
            && self.rebalances == 0
            && self.max_depth == 0
    }
}

/// One thread's full counter page: plain integers indexed by kind.
#[derive(Clone, Copy)]
struct ThreadPage {
    // [kind][field] layout keeps the hot increment a single indexed add.
    fields: [[u64; PathWorkCounters::FIELDS]; StructureKind::ALL.len()],
}

impl ThreadPage {
    fn zeroed() -> Self {
        Self {
            fields: [[0; PathWorkCounters::FIELDS]; StructureKind::ALL.len()],
        }
    }

    #[inline]
    fn bump(&mut self, kind: StructureKind, field: usize) {
        self.fields[kind.index()][field] += 1;
    }

    #[inline]
    fn add(&mut self, kind: StructureKind, field: usize, amount: u64) {
        self.fields[kind.index()][field] += amount;
    }
}

// Field column indices into `ThreadPage::fields`.
const OP: usize = 0;
const CMP: usize = 1;
const VISIT: usize = 2;
const COPY: usize = 3;
const CREATE: usize = 4;
const REBALANCE: usize = 5;
// max_depth is handled through `note_depth` because it is a max, not a sum.
const DEPTH: usize = 6;

thread_local! {
    static PAGE: RefCell<ThreadPage> = RefCell::new(ThreadPage::zeroed());
}

/// Records one operation of `kind` (hot path: one TLS borrow + one add).
#[inline]
pub(crate) fn note_operation(kind: StructureKind) {
    PAGE.with_borrow_mut(|page| page.bump(kind, OP));
}

/// Records one exact-key comparison.
#[inline]
pub(crate) fn note_comparison(kind: StructureKind) {
    PAGE.with_borrow_mut(|page| page.bump(kind, CMP));
}

/// Records one node visited during a descent or scan.
#[inline]
pub(crate) fn note_visit(kind: StructureKind) {
    PAGE.with_borrow_mut(|page| page.bump(kind, VISIT));
}

/// Records one path-copied node.
#[inline]
pub(crate) fn note_copy(kind: StructureKind) {
    PAGE.with_borrow_mut(|page| page.bump(kind, COPY));
}

/// Records one freshly allocated node.
#[inline]
pub(crate) fn note_create(kind: StructureKind) {
    PAGE.with_borrow_mut(|page| page.bump(kind, CREATE));
}

/// Records one rebalance (borrow/merge/split-fix beyond the copied path).
#[inline]
pub(crate) fn note_rebalance(kind: StructureKind) {
    PAGE.with_borrow_mut(|page| page.bump(kind, REBALANCE));
}

/// Records several nodes visited in one operation.
#[inline]
pub(crate) fn note_visits(kind: StructureKind, count: u64) {
    PAGE.with_borrow_mut(|page| page.add(kind, VISIT, count));
}

/// Notes the deepest level reached by the current operation family.
pub(crate) fn note_depth(kind: StructureKind, depth: u64) {
    PAGE.with_borrow_mut(|page| {
        let slot = &mut page.fields[kind.index()][DEPTH];
        if depth > *slot {
            *slot = depth;
        }
    });
}

/// A frozen snapshot of every structure's counters.
///
/// Clone-cheap and comparable; tests assert bounds against these values and
/// reports serialize them under each structure's stable [`name`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathWorkReport {
    pages: Vec<PathWorkCounters>,
}

impl PathWorkReport {
    /// Counters for one structure kind (zero when absent).
    pub fn get(&self, kind: StructureKind) -> PathWorkCounters {
        self.pages.get(kind.index()).copied().unwrap_or_default()
    }

    /// True when every tracked structure is untouched.
    pub fn is_zero(&self) -> bool {
        self.pages.iter().all(PathWorkCounters::is_zero)
    }

    /// Total operations across every tracked structure.
    pub fn total_operations(&self) -> u64 {
        self.pages.iter().map(|page| page.operations).sum()
    }

    fn from_page(page: &ThreadPage) -> Self {
        let pages = StructureKind::ALL
            .iter()
            .map(|kind| {
                let fields = &page.fields[kind.index()];
                let mut counters = PathWorkCounters {
                    max_depth: fields[DEPTH],
                    ..PathWorkCounters::default()
                };
                counters.operations = fields[OP];
                counters.key_comparisons = fields[CMP];
                counters.nodes_visited = fields[VISIT];
                counters.nodes_copied = fields[COPY];
                counters.nodes_created = fields[CREATE];
                counters.rebalances = fields[REBALANCE];
                counters
            })
            .collect();
        Self { pages }
    }
}

impl fmt::Display for PathWorkReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for kind in StructureKind::ALL {
            let page = self.get(kind);
            write!(
                formatter,
                "{}={{ops:{},cmp:{},visited:{},copied:{},created:{},rebal:{},depth:{}}}",
                kind.name(),
                page.operations,
                page.key_comparisons,
                page.nodes_visited,
                page.nodes_copied,
                page.nodes_created,
                page.rebalances,
                page.max_depth,
            )?;
        }
        Ok(())
    }
}

/// Resets the calling thread's page to zeros. Call outside measured regions
/// (command start), never mid-operation.
pub fn reset() {
    PAGE.with_borrow_mut(|page| *page = ThreadPage::zeroed());
}

/// Freezes and returns the calling thread's counters without resetting.
pub fn snapshot() -> PathWorkReport {
    PAGE.with_borrow(|page| PathWorkReport::from_page(page))
}

/// Freezes and returns the counters, then resets the page. The canonical
/// command-end capture: merge happens outside the measured closure.
pub fn take() -> PathWorkReport {
    let report = snapshot();
    reset();
    report
}

/// Serializes every tracked structure's counters as one JSON object with
/// stable wire names: `{"rope_tree":{"operations":..,"key_comparisons":..,
/// "nodes_visited":..,"nodes_copied":..,"nodes_created":..,"rebalances":..,
/// "max_depth":..}, ...}`. Deterministic field order (benchmark artifacts
/// and schema validation consume it); callers that need `serde` can parse
/// it after the fact.
pub fn pathwork_path_work_json(report: &PathWorkReport) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(StructureKind::ALL.len());
    for kind in StructureKind::ALL {
        let page = report.get(kind);
        parts.push(format!(
            "{:?}:{{\"operations\":{},\"key_comparisons\":{},\"nodes_visited\":{},\"nodes_copied\":{},\"nodes_created\":{},\"rebalances\":{},\"max_depth\":{}}}",
            kind.name(),
            page.operations,
            page.key_comparisons,
            page.nodes_visited,
            page.nodes_copied,
            page.nodes_created,
            page.rebalances,
            page.max_depth,
        ));
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_reset_per_thread() {
        reset();
        note_operation(StructureKind::FactHamt);
        note_comparison(StructureKind::FactHamt);
        note_visits(StructureKind::FactHamt, 13);
        note_depth(StructureKind::FactHamt, 13);
        note_depth(StructureKind::FactHamt, 5);
        let report = take();
        let fact = report.get(StructureKind::FactHamt);
        assert_eq!(fact.operations, 1);
        assert_eq!(fact.key_comparisons, 1);
        assert_eq!(fact.nodes_visited, 13);
        assert_eq!(fact.max_depth, 13);
        // Take resets: the next snapshot is clean.
        assert!(snapshot().is_zero());
    }

    #[test]
    fn every_kind_has_a_stable_name() {
        for kind in StructureKind::ALL {
            assert!(!kind.name().is_empty());
        }
    }
}
