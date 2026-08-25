use std::{collections::{BTreeMap, HashMap}, fmt, sync::Arc};

use indexmap::IndexSet;

use crate::framework::{
    lex::TokenPatch,
    parse::types::{ParserTokenDocument, TokenData, TokenOccurrenceId},
};
use crate::framework::parse::{
    build::ActionSet,
    data::{
        ast::{AstArena, TokenEntryId},
        green::{ParseErrorInfo, TreeArena},
        gss::{GssArena, GssNodeId},
        product::{ProductArena, ProductData, ProductId},
    },
    grammar::{BuildError, Grammar, TerminalId},
};

mod checkpoint;
mod incremental;
pub(crate) mod lineage;
mod session;
mod state;

use checkpoint::ColumnCheckpointCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ParseToken {
    pub(crate) entry: TokenEntryId,
    pub(crate) column: TokenOccurrenceId,
    pub(crate) start: usize,
    pub(crate) terminal: TerminalId,
    pub(crate) length: usize,
    pub(crate) merge_source_terminal: Option<TerminalId>,
}

/// Lazily decoded tail of a parser token root (hard invariant 6). The
/// recovery search decodes only the prefix it actually touches — never a
/// restart-to-EOF materialization — while `terminal` maps any position
/// beyond the document to the EOF terminal, exactly like the slice form.
pub(crate) struct TokenTail<'a> {
    document: &'a ParserTokenDocument,
    start: usize,
    grammar: &'a Grammar,
    decoded: Vec<ParseToken>,
}

impl<'a> TokenTail<'a> {
    pub(crate) fn new(document: &'a ParserTokenDocument, start: usize, grammar: &'a Grammar) -> Self {
        Self {
            document,
            start,
            grammar,
            decoded: Vec::new(),
        }
    }

    pub(crate) fn get(&mut self, index: usize) -> Option<&ParseToken> {
        while self.decoded.len() <= index {
            let rank = self.start.saturating_add(self.decoded.len());
            let Some(data) = self.document.token_at(rank) else {
                return None;
            };
            self.decoded
                .push(incremental::decode_data(data, self.grammar));
        }
        self.decoded.get(index)
    }

    pub(crate) fn terminal(&mut self, index: usize) -> TerminalId {
        let eof = self.grammar.eof;
        self.get(index)
            .map_or(eof, |token| token.terminal)
    }

    /// Number of tokens actually decoded so far (bounded by the search's
    /// consumption, never the whole tail).
    pub(crate) fn decoded(&self) -> usize {
        self.decoded.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReductionPath {
    predecessor: GssNodeId,
    products: Vec<ProductId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReductionKey {
    production: u32,
    children: Vec<ProductId>,
    /// Empty reductions are anchored at their lookahead occurrence.
    boundary: Option<TokenOccurrenceId>,
}

#[derive(Debug)]
pub enum ParseError {
    MissingGoto { state: usize, non_terminal: u32 },
    NoActiveStacks { column: Option<TokenOccurrenceId> },
    MissingGssNode { node: GssNodeId },
    Build(BuildError),
    Recovered { product: ProductId },
}

impl From<BuildError> for ParseError {
    fn from(value: BuildError) -> Self {
        Self::Build(value)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingGoto {
                state,
                non_terminal,
            } => {
                write!(
                    f,
                    "missing goto from state {state} on nonterminal {non_terminal}"
                )
            }
            Self::NoActiveStacks { column } => match column {
                Some(column) => write!(f, "no active parse stacks at token column {column}"),
                None => write!(f, "no active parse stacks"),
            },
            Self::MissingGssNode { node } => write!(f, "missing GSS node {node}"),
            Self::Build(error) => write!(f, "build error: {error:?}"),
            Self::Recovered { .. } => write!(f, "parse recovered with errors"),
        }
    }
}

/// One parse column: the GSS frontier at one token boundary plus the
/// products reduced into it. `records` is the column's record segment
/// (plan §8.2): the AST ids first made live by products created at this
/// column. Segments make reachability incremental — truncation retires
/// exactly the dropped segments' records, and suffix reattachment
/// restores them — without ever walking the whole live set.
#[derive(Clone)]
pub(crate) struct ParseColumn {
    token: Option<TokenOccurrenceId>,
    base_active: IndexSet<GssNodeId>,
    active: IndexSet<GssNodeId>,
    accepted: Vec<ProductId>,
    pub(crate) products: Vec<ProductId>,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    pub(crate) error_derived: bool,
    checkpoint_cache: ColumnCheckpointCache,
    pub(crate) records: Vec<u64>,
}

impl ParseColumn {
    pub(crate) fn new(token: Option<TokenOccurrenceId>, active: IndexSet<GssNodeId>) -> Self {
        Self {
            token,
            base_active: active.clone(),
            active,
            accepted: Vec::new(),
            products: Vec::new(),
            diagnostics: Vec::new(),
            error_derived: false,
            checkpoint_cache: Default::default(),
            records: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ParserSessionState {
    pub(crate) columns: Vec<ParseColumn>,
    pub(crate) generation: u32,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    token_columns: HashMap<TokenOccurrenceId, usize>,
    token_products: HashMap<TokenOccurrenceId, ProductId>,
    /// Per-document recovery segment serial (plan §14): monotonically
    /// increasing across commands, so synthetic identities never collide.
    pub(crate) next_recovery_segment: u64,
    /// This command's synthetic tokens: occurrence -> deterministic
    /// (document, segment, ordinal) identity, published in the delta.
    pub(crate) synthetic_tokens: BTreeMap<TokenOccurrenceId, u64>,
    /// Within-segment synthetic ordinal counter for this command.
    pub(crate) next_synthetic_ordinal: u64,
    /// The recovery segment currently being applied, or None outside a
    /// recovery invocation.
    pub(crate) active_recovery_segment: Option<u64>,
    /// Stable per-document serial (URI FNV hash) for synthetic identity.
    pub(crate) document_serial: u64,
    /// Persistent witness-interval index (plan §14): token occurrence ->
    /// recovery-segment serials whose repairs touched it. A later structural
    /// patch probes only segments whose intervals intersect the changed
    /// region, never scanning the whole segment table.
    pub(crate) witness_intervals: std::collections::BTreeMap<TokenOccurrenceId, Vec<u64>>,
    reduced_products: HashMap<ReductionKey, ProductId>,
    /// Inverse reduction cache used by suffix product rebasing. Keeping this
    /// index avoids rediscovering origins by scanning every cached reduction.
    reduction_origins: HashMap<ProductId, ReductionKey>,
    /// Live-record reference counts (plan §9.2): how many kept column
    /// segments reference each AST record. Zero means the record left the
    /// live set and its tree fact is retracted.
    pub(crate) record_live_counts: HashMap<u64, u64>,
    /// Journal of record liveness for this command (plan §9.1): every
    /// record whose live count changed maps to its final live state.
    pub(crate) record_journal: BTreeMap<u64, bool>,
    /// Stable syntax-lineage identities for live records (plan §8.7).
    pub(crate) lineage: lineage::LineageState,
}

impl ParserSessionState {
    /// Begins a deterministic recovery segment (plan §14): the segment id
    /// is a stable per-document serial, and ordinals restart at zero within
    /// the segment so every synthetic token id is unique and reproducible
    /// from the (document, segment, ordinal) triple alone.
    pub(crate) fn begin_recovery_segment(&mut self) -> u64 {
        self.next_recovery_segment = self.next_recovery_segment.wrapping_add(1);
        self.next_synthetic_ordinal = 0;
        self.active_recovery_segment = Some(self.next_recovery_segment);
        self.next_recovery_segment
    }

    /// Allocates the next deterministic synthetic identity within the
    /// active recovery segment.
    pub(crate) fn next_synthetic_identity(&mut self, occurrence: TokenOccurrenceId) -> u64 {
        let segment = self.active_recovery_segment.unwrap_or(0);
        let ordinal = self.next_synthetic_ordinal;
        self.next_synthetic_ordinal = self.next_synthetic_ordinal.wrapping_add(1);
        let identity = self.synthetic_bytes(segment, ordinal);
        self.synthetic_tokens.insert(occurrence, identity);
        identity
    }

    fn synthetic_bytes(&self, segment: u64, ordinal: u64) -> u64 {
        // (document_serial << 48) | (segment << 24) | ordinal — deterministic
        // and compact. The document serial is the stable URI FNV hash.
        (self.document_serial << 48)
            | ((segment & 0xFFFF_FFFF) << 16)
            | (ordinal & 0xFFFF)
    }

    /// Records that one real source token was a witness to the given
    /// recovery segment (plan §14): the persistent interval index maps the
    /// occurrence to the segments that consumed it, so later structural
    /// patches can probe exactly the affected segments.
    pub(crate) fn record_witness(&mut self, occurrence: TokenOccurrenceId, segment: u64) {
        let bucket = self.witness_intervals.entry(occurrence).or_default();
        if !bucket.contains(&segment) {
            bucket.push(segment);
        }
    }

    /// Probes the persistent witness index for the recovery segments whose
    /// intervals intersect `[start, end]`. Returns segments in ascending
    /// order; used by recovery-delta generation to keep unaffected segments
    /// cold (plan §14 locality).
    pub(crate) fn intersecting_witness_segments(
        &self,
        start: TokenOccurrenceId,
        end: TokenOccurrenceId,
    ) -> Vec<u64> {
        let mut segments = std::collections::BTreeSet::new();
        for &occurrence in self.witness_intervals.range(start..=end).flat_map(|(k, _)| std::iter::once(k))
        {
            if let Some(bucket) = self.witness_intervals.get(&occurrence) {
                segments.extend(bucket.iter().copied());
            }
        }
        segments.into_iter().collect()
    }
}

impl ParserSessionState {
    /// Marks one record live inside a kept column segment (plan §9.2):
    /// the per-segment reference count increments, and the journal
    /// records the 0→1 transition exactly once per command.
    pub(crate) fn record_became_live(&mut self, record: u64) {
        let count = self.record_live_counts.entry(record).or_insert(0);
        *count += 1;
        if *count == 1 {
            self.record_journal.insert(record, true);
        }
    }

    /// Marks one record's segment reference dropped. When the last
    /// segment reference goes away the record leaves the live set and
    /// the journal records the 1→0 transition.
    pub(crate) fn record_died(&mut self, record: u64) {
        // Recovery's `*state = snapshot` restore can legitimately revert
        // counters below the bookkeeping baseline of the running command;
        // these counts are observational until Phase 7 rewires them, so
        // they saturate instead of panicking.
        if let Some(count) = self.record_live_counts.get_mut(&record) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.record_journal.insert(record, false);
            }
        }
    }

    /// Adopts one column's record segment: every record it lists becomes
    /// live inside it.
    pub(crate) fn adopt_column_records(&mut self, column: &ParseColumn) {
        for &record in &column.records {
            self.record_became_live(record);
        }
    }

    /// Releases one column's record segment (plan §9.2): truncation and
    /// reuse staging call this for every column they drop.
    pub(crate) fn drop_column_records(&mut self, column: &ParseColumn) {
        for &record in &column.records {
            self.record_died(record);
        }
    }
}

/// The direct AST record of one product, if any (plan §9.2). A column's
/// record segment lists exactly these records for the products it holds,
/// so liveness follows product membership in both directions.
pub(crate) fn product_direct_record(
    products: &ProductArena,
    product: ProductId,
) -> Option<u64> {
    match &products.get(product)?.data {
        ProductData::Node { ast, .. } | ProductData::Token { ast: Some(ast), .. } => {
            Some(*ast as u64)
        }
        ProductData::Error { .. } | ProductData::Token { ast: None, .. } => None,
    }
}

pub(crate) struct SessionContext<'a> {
    pub uri: fluent_uri::Uri<String>,
    pub state: &'a mut ParserSessionState,
    pub trees: &'a mut TreeArena,
    pub products: &'a mut ProductArena,
    pub ast: &'a mut AstArena,
    pub gss: &'a mut GssArena,
    pub(crate) grammar: &'a Grammar,
    pub(crate) actions: &'a [ActionSet],
    pub(crate) gotos: &'a [Option<usize>],
    pub(crate) error_recovery: bool,
}

#[derive(Clone)]
pub(crate) struct ReplayPlan {
    pub(crate) old: Option<Arc<ParserTokenDocument>>,
    pub(crate) new: Arc<ParserTokenDocument>,
    pub(crate) old_extent: usize,
    pub(crate) new_extent: usize,
    pub(crate) prefix_len: usize,
    pub(crate) suffix_len: usize,
    pub(crate) restart_boundary: usize,
    pub(crate) old_reuse_start: usize,
    pub(crate) new_reuse_start: usize,
}

impl ReplayPlan {
    pub(crate) fn from_token_patch(
        old: Option<Arc<ParserTokenDocument>>,
        new: Arc<ParserTokenDocument>,
        patch: &TokenPatch,
    ) -> Self {
        let old_extent = old
            .as_ref()
            .map_or(0, |document| document.semantic_len().saturating_add(1));
        let new_extent = new.semantic_len().saturating_add(1);
        let mut ranges = Vec::with_capacity(patch.order_splices.len().max(1));
        if patch.order_splices.is_empty() {
            // A new document has no retained anchor; its implicit splice also
            // includes the synthetic EOF token. Payload-only patches bypass
            // replay before reaching this constructor.
            if old_extent == 0 && new_extent > 0 {
                ranges.push((0, 0, 0, new_extent));
            }
        } else {
            for splice in patch.order_splices.iter() {
                let old_start = splice
                    .before
                    .and_then(|occurrence| {
                        old.as_ref()
                            .and_then(|document| {
                                document.rank_of_occurrence(occurrence.0 as usize)
                            })
                    })
                    .map_or(0, |rank| rank.saturating_add(1));
                let old_end = splice
                    .after
                    .and_then(|occurrence| {
                        old.as_ref()
                            .and_then(|document| {
                                document.rank_of_occurrence(occurrence.0 as usize)
                            })
                    })
                    .unwrap_or(old_extent);
                let new_start = splice
                    .before
                    .and_then(|occurrence| new.rank_of_occurrence(occurrence.0 as usize))
                    .map_or(0, |rank| rank.saturating_add(1));
                let new_end = splice
                    .after
                    .and_then(|occurrence| new.rank_of_occurrence(occurrence.0 as usize))
                    .unwrap_or(new_extent);
                debug_assert!(old_start <= old_end && new_start <= new_end);
                ranges.push((old_start, old_end, new_start, new_end));
            }
        }
        let prefix_len = ranges.first().map_or(old_extent, |range| range.0);
        let (old_reuse_start, new_reuse_start) = ranges
            .last()
            .map_or((old_extent, new_extent), |range| (range.1, range.3));
        Self {
            old,
            new,
            old_extent,
            new_extent,
            prefix_len,
            suffix_len: old_extent.saturating_sub(old_reuse_start),
            restart_boundary: prefix_len,
            old_reuse_start,
            new_reuse_start,
        }
    }

    pub(crate) fn old_unit(&self, rank: usize) -> Option<TokenData> {
        self.old.as_ref()?.token_at(rank)
    }

    pub(crate) fn new_unit(&self, rank: usize) -> Option<TokenData> {
        self.new.token_at(rank)
    }
}
