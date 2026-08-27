use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
};

use indexmap::IndexSet;

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
use crate::framework::{
    lex::TokenPatch,
    parse::types::{ParserTokenDocument, TokenData, TokenOccurrenceId},
};
use crate::utils::{persistent_seq::SeqMeasureWeight, PersistentSeq, SeqMeasure};

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
    pub(crate) fn new(
        document: &'a ParserTokenDocument,
        start: usize,
        grammar: &'a Grammar,
    ) -> Self {
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
        self.get(index).map_or(eof, |token| token.terminal)
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
    InvalidReachability {
        kind: &'static str,
        key: u64,
        before: u32,
        delta: i64,
    },
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
            Self::InvalidReachability {
                kind,
                key,
                before,
                delta,
            } => write!(
                f,
                "invalid {kind} reachability for key {key}: {before} + {delta}"
            ),
        }
    }
}

/// One parse column: the GSS frontier at one token boundary plus the
/// products reduced into it. Published liveness is derived exclusively
/// from accepted-root reach counts; columns remain parser-cache state.
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
        }
    }
}
/// Immutable metadata for one materialized parser segment. Later range views
/// share this storage without copying retained columns.
#[derive(Clone)]
struct SegmentData {
    columns: PersistentSeq<ParseColumn>,
    frontiers: PersistentSeq<checkpoint::FrontierCheckpoint>,
    token_columns: Arc<HashMap<TokenOccurrenceId, usize>>,
    token_products: Arc<HashMap<TokenOccurrenceId, ProductId>>,
    error_suffix_counts: PersistentSeq<usize>,
    first_dirty: Option<usize>,
    products_cache_stable: bool,
}

#[derive(Clone)]
struct SegmentPart {
    data: Arc<SegmentData>,
    start: usize,
    end: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct SegmentMeasure {
    columns: usize,
    error_columns: usize,
    first_dirty: Option<usize>,
    products_cache_stable: bool,
}

impl SeqMeasure<SegmentPart> for SegmentMeasure {
    fn measure_leaf(values: &[SegmentPart]) -> Self {
        values.iter().fold(
            Self {
                columns: 0,
                error_columns: 0,
                first_dirty: None,
                products_cache_stable: true,
            },
            |left, part| Self::combine(&left, &part.measure()),
        )
    }

    fn combine(left: &Self, right: &Self) -> Self {
        Self {
            columns: left.columns + right.columns,
            error_columns: left.error_columns + right.error_columns,
            first_dirty: left
                .first_dirty
                .or_else(|| right.first_dirty.map(|dirty| left.columns + dirty)),
            products_cache_stable: left.products_cache_stable && right.products_cache_stable,
        }
    }
}

impl SeqMeasureWeight for SegmentMeasure {
    fn weight(&self) -> usize {
        self.columns
    }
}

impl SegmentPart {
    fn measure(&self) -> SegmentMeasure {
        let error_columns = self
            .data
            .error_suffix_counts
            .get(self.start)
            .copied()
            .unwrap_or_default()
            .saturating_sub(
                self.data
                    .error_suffix_counts
                    .get(self.end)
                    .copied()
                    .unwrap_or_default(),
            );
        SegmentMeasure {
            columns: self.end - self.start,
            error_columns,
            first_dirty: self
                .data
                .first_dirty
                .filter(|&dirty| (self.start..self.end).contains(&dirty))
                .map(|dirty| dirty - self.start),
            products_cache_stable: self.data.products_cache_stable,
        }
    }

    fn split_at(&self, offset: usize) -> (Self, Self) {
        debug_assert!(offset > 0 && offset < self.end - self.start);
        (
            Self {
                data: Arc::clone(&self.data),
                start: self.start,
                end: self.start + offset,
            },
            Self {
                data: Arc::clone(&self.data),
                start: self.start + offset,
                end: self.end,
            },
        )
    }
}

/// Persistent parser suffix storage. Slices and concatenations share the
/// underlying column arrays and only allocate bounded piece metadata.
///
/// A rebased segment keeps its original immutable columns. Only the accepted
/// products and token-product lookup are overlaid; the parser never needs to
/// walk or rewrite the retained columns after a bounded seam proof.
#[derive(Clone, Default)]
pub(crate) struct ParseSegment {
    // Segment metadata is itself persistent. Concatenating a replay prefix
    // with a retained suffix path-copies only the metadata spine instead of
    // cloning every prior piece descriptor.
    parts: PersistentSeq<SegmentPart, SegmentMeasure>,
    length: usize,
    raw_accepted: Arc<[ProductId]>,
    accepted: Arc<[ProductId]>,
    product_map: Arc<HashMap<ProductId, ProductId>>,
}

impl ParseSegment {
    pub(crate) fn from_columns(mut columns: Vec<ParseColumn>, gss: &GssArena) -> Arc<Self> {
        let mut frontiers = Vec::with_capacity(columns.len());
        let mut token_columns = HashMap::new();
        let mut token_products = HashMap::new();
        let mut first_dirty = None;
        for (index, column) in columns.iter_mut().enumerate() {
            frontiers.push(checkpoint::frontier_checkpoint_for_column(column, gss).clone());
            if let Some(token) = column.token {
                token_columns.insert(token, index);
                if !column.error_derived
                    && let Some(&product) = column.products.first()
                {
                    token_products.insert(token, product);
                }
            }
            if column.error_derived && first_dirty.is_none() {
                first_dirty = Some(index);
            }
        }
        let mut error_suffix_counts = vec![0usize; columns.len() + 1];
        for index in (0..columns.len()).rev() {
            error_suffix_counts[index] =
                error_suffix_counts[index + 1] + usize::from(columns[index].error_derived);
        }
        let raw_accepted: Arc<[ProductId]> = columns
            .last()
            .map(|column| column.accepted.clone())
            .unwrap_or_default()
            .into();
        let length = columns.len();
        let data = Arc::new(SegmentData {
            columns: PersistentSeq::from_iter(columns),
            frontiers: PersistentSeq::from_iter(frontiers),
            token_columns: Arc::new(token_columns),
            token_products: Arc::new(token_products),
            error_suffix_counts: PersistentSeq::from_iter(error_suffix_counts),
            first_dirty,
            // Reduction products are keyed by stable token anchors. A
            // materialized committed segment therefore starts cache-stable.
            products_cache_stable: true,
        });
        Arc::new(Self {
            parts: PersistentSeq::from_iter([SegmentPart {
                start: 0,
                end: data.columns.len(),
                data,
            }]),
            length,
            raw_accepted: raw_accepted.clone(),
            accepted: raw_accepted,
            product_map: Arc::new(HashMap::new()),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub(crate) fn accepted(&self) -> &[ProductId] {
        &self.accepted
    }

    /// The accepted products stored in the immutable source columns. This is
    /// the old side of a later frontier rebase; [`Self::accepted`] is the
    /// logical current view.
    pub(crate) fn raw_accepted(&self) -> &[ProductId] {
        &self.raw_accepted
    }

    pub(crate) fn column(&self, index: usize) -> Option<&ParseColumn> {
        let (_, local, part) = self.parts.weighted_get(index)?;
        part.data.columns.get(part.start + local)
    }

    pub(crate) fn frontier(&self, index: usize) -> Option<&checkpoint::FrontierCheckpoint> {
        let (_, local, part) = self.parts.weighted_get(index)?;
        part.data.frontiers.get(part.start + local)
    }

    pub(crate) fn token_column(&self, token: TokenOccurrenceId) -> Option<usize> {
        let mut base = 0;
        for part in self.parts.iter() {
            let len = part.end - part.start;
            if let Some(&index) = part.data.token_columns.get(&token)
                && (part.start..part.end).contains(&index)
            {
                return Some(base + index - part.start);
            }
            base += len;
        }
        None
    }

    pub(crate) fn token_product(&self, token: TokenOccurrenceId) -> Option<ProductId> {
        self.parts.iter().find_map(|part| {
            part.data
                .token_products
                .get(&token)
                .copied()
                .filter(|_| {
                    part.data
                        .token_columns
                        .get(&token)
                        .is_some_and(|index| (part.start..part.end).contains(index))
                })
                .map(|product| self.product_map.get(&product).copied().unwrap_or(product))
        })
    }

    /// Rebase the logical view of a retained segment without copying or
    /// rewriting its columns. The raw columns remain the old convergence
    /// oracle for the next command.
    pub(crate) fn rebase(
        &self,
        product_map: HashMap<ProductId, ProductId>,
        accepted: Arc<[ProductId]>,
    ) -> Arc<Self> {
        Arc::new(Self {
            parts: self.parts.clone(),
            length: self.length,
            raw_accepted: Arc::clone(&self.raw_accepted),
            accepted,
            product_map: Arc::new(product_map),
        })
    }

    fn split_parts_at(
        parts: &PersistentSeq<SegmentPart, SegmentMeasure>,
        total: usize,
        offset: usize,
    ) -> (
        PersistentSeq<SegmentPart, SegmentMeasure>,
        PersistentSeq<SegmentPart, SegmentMeasure>,
    ) {
        debug_assert!(offset <= total);
        if offset == 0 {
            return (PersistentSeq::new(), parts.clone());
        }
        if offset == total {
            return (parts.clone(), PersistentSeq::new());
        }

        let (part_index, local, part) = parts
            .weighted_get(offset)
            .expect("interior weighted split must resolve a segment part");
        let (prefix, suffix) = parts.split_at(part_index);
        if local == 0 {
            return (prefix, suffix);
        }

        let (left_part, right_part) = part.split_at(local);
        let left_piece = PersistentSeq::from_iter([left_part]);
        let right_piece = PersistentSeq::from_iter([right_part]);
        (prefix.concat(&left_piece), right_piece.concat(&suffix))
    }

    pub(crate) fn slice(&self, range: std::ops::Range<usize>) -> Arc<Self> {
        assert!(range.start <= range.end && range.end <= self.len());
        let tail_len = self.length - range.start;
        let (_, tail) = Self::split_parts_at(&self.parts, self.length, range.start);
        let (parts, _) = Self::split_parts_at(&tail, tail_len, range.end - range.start);
        let accepts = range.end == self.len();
        Arc::new(Self {
            parts,
            length: range.end - range.start,
            raw_accepted: accepts
                .then(|| Arc::clone(&self.raw_accepted))
                .unwrap_or_default(),
            accepted: accepts
                .then(|| Arc::clone(&self.accepted))
                .unwrap_or_default(),
            product_map: Arc::clone(&self.product_map),
        })
    }
    pub(crate) fn concat(left: Arc<Self>, right: Arc<Self>) -> Arc<Self> {
        if left.is_empty() {
            return right;
        }
        if right.is_empty() {
            return left;
        }
        let mut product_map = (*left.product_map).clone();
        product_map.extend(right.product_map.iter().map(|(&old, &new)| (old, new)));
        Arc::new(Self {
            parts: left.parts.concat(&right.parts),
            length: left.length + right.length,
            raw_accepted: Arc::clone(&right.raw_accepted),
            accepted: Arc::clone(&right.accepted),
            product_map: Arc::new(product_map),
        })
    }

    pub(crate) fn is_clean_from(&self, index: usize) -> bool {
        if index >= self.len() {
            return true;
        }
        let Some((part_index, local, part)) = self.parts.weighted_get(index) else {
            return true;
        };
        if part
            .measure()
            .first_dirty
            .is_some_and(|dirty| dirty >= local)
        {
            return false;
        }
        self.parts
            .measure_after_items(part_index + 1)
            .is_none_or(|measure| measure.first_dirty.is_none())
    }

    pub(crate) fn products_cache_stable(&self) -> bool {
        self.parts
            .measure()
            .is_none_or(|measure| measure.products_cache_stable)
    }

    pub(crate) fn error_count_after(&self, index: usize) -> usize {
        if index >= self.len() {
            return 0;
        }
        let Some((part_index, local, part)) = self.parts.weighted_get(index) else {
            return 0;
        };
        let current_end = part
            .data
            .error_suffix_counts
            .get(part.end)
            .copied()
            .unwrap_or_default();
        let current_start = part
            .data
            .error_suffix_counts
            .get(part.start + local)
            .copied()
            .unwrap_or_default();
        let current = current_start.saturating_sub(current_end);
        current
            + self
                .parts
                .measure_after_items(part_index + 1)
                .map_or(0, |measure| measure.error_columns)
    }

    pub(crate) fn materialize(&self) -> Vec<ParseColumn> {
        (0..self.len())
            .filter_map(|index| self.column(index).cloned())
            .collect()
    }
}

#[derive(Clone, Default)]
pub struct ParserSessionState {
    pub(crate) columns: Vec<ParseColumn>,
    /// Immutable suffix attached after replay convergence. Replay detaches
    /// this handle before mutating the working prefix.
    pub(crate) retained_suffix: Option<Arc<ParseSegment>>,
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
    /// Stable syntax-lineage identities for live records.
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
        (self.document_serial << 48) | ((segment & 0xFFFF_FFFF) << 16) | (ordinal & 0xFFFF)
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
        for &occurrence in self
            .witness_intervals
            .range(start..=end)
            .flat_map(|(k, _)| std::iter::once(k))
        {
            if let Some(bucket) = self.witness_intervals.get(&occurrence) {
                segments.extend(bucket.iter().copied());
            }
        }
        segments.into_iter().collect()
    }
}


/// The direct AST record of one product, if any (plan §9.2). A column's
/// record segment lists exactly these records for the products it holds,
/// so liveness follows product membership in both directions.
pub(crate) fn product_direct_record(products: &ProductArena, product: ProductId) -> Option<u64> {
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
                            .and_then(|document| document.rank_of_occurrence(occurrence.0 as usize))
                    })
                    .map_or(0, |rank| rank.saturating_add(1));
                let old_end = splice
                    .after
                    .and_then(|occurrence| {
                        old.as_ref()
                            .and_then(|document| document.rank_of_occurrence(occurrence.0 as usize))
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
