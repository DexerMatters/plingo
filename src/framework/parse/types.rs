use crate::utils::Span;
use fluent_uri::Uri;
use ropey::Rope;
use std::{any::TypeId, fmt, ops::Deref, sync::Arc};

use super::data::ast::AstArena;
use crate::framework::{
    lex::{
        LayoutRevisionId, LexerRoot, LexicalDocument, ParseTokenRef, PersistentUriMap,
        StableDocumentId, TokenLayoutEntry, TokenOccurrenceId,
    },
    parse::{
        data::{
            ast::{AnchoredSpan, AstBox, AstToken, TokenEntryId},
            green::TreeArena,
            gss::GssArena,
            product::{ProductArena, ProductId},
        },
        grammar::TerminalId,
        identity::TokenFingerprint,
        parsing::ParserSessionState,
    },
    tape::{PersistentOccurrenceIndex, StableTape, TapeCursor},
};

#[derive(Debug, Clone)]
pub struct ParserConfig {
    /// Recovery is part of normal incremental replay and publishes partial
    /// error products plus diagnostics rather than triggering a rebuild.
    pub error_recovery: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            error_recovery: true,
        }
    }
}

pub(crate) type ParseSnapshotId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstTokenSnapshotEntry {
    pub terminal: Option<TerminalId>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstLookupError {
    WrongDocument,
    Deleted,
    TypeMismatch,
}

impl fmt::Display for AstLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDocument => write!(f, "AST box belongs to a different document"),
            Self::Deleted => write!(f, "AST node is not live in this snapshot"),
            Self::TypeMismatch => write!(f, "AST node has a different runtime type"),
        }
    }
}

/// Snapshot-bound AST access. The contained value dereferences to `T`, while
/// [`ResolvedAst::span`] returns the exact source extent from the same parser
/// revision.
pub struct ResolvedAst<T> {
    value: Arc<T>,
    span: Span,
}

impl<T> Deref for ResolvedAst<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> ResolvedAst<T> {
    pub fn arc(&self) -> Arc<T> {
        Arc::clone(&self.value)
    }

    pub fn span(&self) -> Span {
        self.span.clone()
    }
}

/// Immutable parser result for one committed document revision. It owns the
/// Rope used by consumers that want line/column formatting, so historical
/// snapshots keep their original coordinate system.
#[derive(Clone)]
pub struct AstSnapshot {
    id: ParseSnapshotId,
    uri: Uri<String>,
    source: Arc<Rope>,
    arena: Arc<AstArena>,
    live_records: Arc<crate::reactive::store::RadixMap<()>>,
    token_document: Arc<ParserTokenDocument>,
}

impl std::fmt::Debug for AstSnapshot {
    /// Debug shows only the stable snapshot identity; the payload maps are
    /// opaque and never part of reactive equality.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AstSnapshot").field("id", &self.id).finish()
    }
}

/// A document-typed handle to one committed AST snapshot (plan §8.4
/// precursor). Equality is the snapshot identity; `A` participates only in
/// resolution typing.
pub struct DocumentSnapshot<A: 'static> {
    snapshot: Arc<AstSnapshot>,
    _marker: std::marker::PhantomData<fn() -> A>,
}

impl<A: 'static> Clone for DocumentSnapshot<A> {
    fn clone(&self) -> Self {
        Self {
            snapshot: Arc::clone(&self.snapshot),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<A: 'static> DocumentSnapshot<A> {
    pub(crate) fn new(snapshot: Arc<AstSnapshot>) -> Self {
        Self {
            snapshot,
            _marker: std::marker::PhantomData,
        }
    }

    /// The committed snapshot.
    pub fn snapshot(&self) -> &AstSnapshot {
        &self.snapshot
    }

    /// The committed snapshot handle.
    pub fn arc(&self) -> &Arc<AstSnapshot> {
        &self.snapshot
    }
}

impl<A: 'static> PartialEq for DocumentSnapshot<A> {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot == other.snapshot
    }
}
impl<A: 'static> Eq for DocumentSnapshot<A> {}

impl<A: 'static> std::fmt::Debug for DocumentSnapshot<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentSnapshot")
            .field("snapshot", &self.snapshot)
            .finish()
    }
}

impl AstSnapshot {
    pub(crate) fn new(
        id: ParseSnapshotId,
        uri: Uri<String>,
        source: Arc<Rope>,
        arena: Arc<AstArena>,
        live_records: Arc<crate::reactive::store::RadixMap<()>>,
        token_document: Arc<ParserTokenDocument>,
    ) -> Self {
        Self {
            id,
            uri,
            source,
            arena,
            live_records,
            token_document,
        }
    }

    /// Debug oracle surface (plan §20.2): the live record ids this
    /// snapshot publishes. Never used on production command paths.
    #[doc(hidden)]
    pub fn __live_record_ids(&self) -> Vec<u64> {
        self.live_records.iter().map(|(id, ())| id).collect()
    }

    pub fn resolve<T>(&self, node: AstBox<T>) -> Result<ResolvedAst<T>, AstLookupError>
    where
        T: Send + Sync + 'static,
    {
        if node.document_id() != self.arena.document_id() {
            return Err(AstLookupError::WrongDocument);
        }
        let id = node.raw_id();
        if self.live_records.get(id as u64).is_none() {
            return Err(AstLookupError::Deleted);
        }
        if self.arena.type_of(id) != Some(TypeId::of::<T>()) {
            return Err(AstLookupError::TypeMismatch);
        }
        let value = self
            .arena
            .cloned_erased(id)
            .ok_or(AstLookupError::Deleted)?
            .downcast::<T>()
            .map_err(|_| AstLookupError::TypeMismatch)?;
        let extent = self.arena.extent_of_id(id).ok_or(AstLookupError::Deleted)?;
        Ok(ResolvedAst {
            value,
            span: self.span_for_extent(extent),
        })
    }

    /// Convenience for consumers that only need ownership of the value.
    /// Snapshot-bound `AstBox::resolve` remains the richer API.
    pub fn get<T>(&self, node: AstBox<T>) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.resolve(node).ok().map(|resolved| resolved.arc())
    }

    #[inline]
    pub fn token<T>(&self, token: AstToken<T>) -> Option<AstTokenSnapshotEntry> {
        let occurrence = token.occurrence();
        let data = self
            .token_document
            .rank_of_occurrence(TokenOccurrenceId(occurrence as u64))
            .and_then(|rank| self.token_document.token_at(rank))?;
        let end = data
            .start
            .saturating_add(data.length)
            .min(self.source.len_bytes());
        Some(AstTokenSnapshotEntry {
            terminal: data.terminal,
            span: Span::new_uri(self.uri.clone(), data.start.min(end), data.start.max(end))
                .expect("parser token coordinates are UTF-8 source boundaries"),
        })
    }

    fn coordinate_at(&self, occurrence: usize) -> usize {
        self.token_document
            .rank_of_occurrence(TokenOccurrenceId(occurrence as u64))
            .and_then(|rank| self.token_document.token_at(rank))
            .map_or(self.source.len_bytes(), |token| token.start)
    }

    fn span_for_extent(&self, extent: AnchoredSpan) -> Span {
        let start = self.coordinate_at(extent.start);
        let end = self
            .token_document
            .rank_of_occurrence(TokenOccurrenceId(extent.end as u64))
            .and_then(|rank| self.token_document.token_at(rank))
            .map_or(self.source.len_bytes(), |token| {
                if extent.end_at_token_end {
                    token.start.saturating_add(token.length)
                } else {
                    token.start
                }
            });
        Span::new_uri(self.uri.clone(), start.min(end), start.max(end))
            .expect("parser token coordinates are UTF-8 source boundaries")
    }
    pub(crate) fn source_text(&self, span: Span) -> String {
        let span = span.trim(&self.source);
        self.source
            .byte_slice(span.range.start()..span.range.end())
            .to_string()
    }
}

impl<T> AstBox<T>
where
    T: Send + Sync + 'static,
{
    pub fn resolve(self, snapshot: &AstSnapshot) -> Result<ResolvedAst<T>, AstLookupError> {
        snapshot.resolve(self)
    }

    pub fn span(self, snapshot: &AstSnapshot) -> Result<Span, AstLookupError> {
        Ok(self.resolve(snapshot)?.span())
    }
}

impl PartialEq for AstSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for AstSnapshot {}

/// Materialized parser state for editor-facing consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseStatus {
    Clean,
    Recovered { diagnostics: usize },
    Unrecoverable { diagnostics: usize },
}

/// The reason a parser replay reached EOF without attaching a retained
/// suffix.  This is part of deterministic work attribution: a valid full
/// replay is never reported as an unclassified rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FullReplayReason {
    ExplicitColdParse,
    GrammarOrPolicyVersionChanged,
    NoRetainedRightBoundary,
    NoCleanRestartCheckpoint,
    RecoveryProofFailed,
    FrontierProofInconclusive,
    NoEqualFrontierBeforeEof,
}

impl FullReplayReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitColdParse => "ExplicitColdParse",
            Self::GrammarOrPolicyVersionChanged => "GrammarOrPolicyVersionChanged",
            Self::NoRetainedRightBoundary => "NoRetainedRightBoundary",
            Self::NoCleanRestartCheckpoint => "NoCleanRestartCheckpoint",
            Self::RecoveryProofFailed => "RecoveryProofFailed",
            Self::FrontierProofInconclusive => "FrontierProofInconclusive",
            Self::NoEqualFrontierBeforeEof => "NoEqualFrontierBeforeEof",
        }
    }
}
/// Stable parser boundary at the gap before the next semantic token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TokenBoundaryId {
    pub(crate) document: StableDocumentId,
    pub(crate) kind: TokenBoundaryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum TokenBoundaryKind {
    Before(TokenOccurrenceId),
    Eof,
}

impl TokenBoundaryId {
    pub(crate) const fn before(document: StableDocumentId, occurrence: TokenOccurrenceId) -> Self {
        Self {
            document,
            kind: TokenBoundaryKind::Before(occurrence),
        }
    }

    pub(crate) const fn eof(document: StableDocumentId) -> Self {
        Self {
            document,
            kind: TokenBoundaryKind::Eof,
        }
    }
}

/// Identity of a parser column. Source gaps and recovery-created gaps occupy
/// separate namespaces so a synthetic column can never be mistaken for a
/// source boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ParserBoundaryId {
    Source(TokenBoundaryId),
    Recovery(RecoveryBoundaryId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RecoveryBoundaryId {
    pub(crate) left: Option<TokenOccurrenceId>,
    pub(crate) right: Option<TokenOccurrenceId>,
    pub(crate) witness_ordinal: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoundaryTrace {
    /// Lookahead occurrence at the current replay gap.
    pub current_lookahead_occurrence: Option<u64>,
    /// Anchor stored on the current parser column.
    pub current_column_anchor: Option<u64>,
    /// Occurrence selected from the old token root for comparison.
    pub selected_old_occurrence: Option<u64>,
    /// Anchor stored on the selected old parser column.
    pub selected_old_column_anchor: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncrementalParseStats {
    pub restart_boundary: usize,
    pub reconverged_new_boundary: Option<usize>,
    pub reconverged_old_boundary: Option<usize>,
    pub convergence_checks: usize,
    pub checkpoint_matches: usize,
    pub frontier_matches: usize,
    pub reparsed: usize,
    pub reused: usize,
    /// Suffix columns physically rewritten during reuse (cache-stable
    /// columns are attached verbatim — plan §8.6 fast path).
    pub suffix_rewritten: usize,
    pub recovery_columns: usize,
    /// Test-only characterization of the old adjacent-boundary mismatch.
    pub boundary_trace: Option<BoundaryTrace>,
}

/// Deterministic parser work counters for one document command (plan §10.1).
/// Every field is an always-on integer attribution; timing and allocation
/// probes stay outside this structure so parser decisions never depend on
/// instrumentation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParserWork {
    /// Total parser component invocations. Kept as the sum of the two
    /// explicit scheduling domains for existing report consumers.
    pub component_runs: u64,
    /// Semantic parser invocations.
    pub semantic_runs: u64,
    /// Layout-only snapshot projection invocations.
    pub layout_projection_runs: u64,
    /// Restart occurrence anchors chosen for replay.
    pub restart_occurrences: u64,
    /// Restart-boundary lookups and their persistent-map descent work.
    pub restart_boundary_lookups: u64,
    pub restart_lookup_depth: u64,
    /// Legacy report spelling for restart lookup work.
    pub restart_columns: u64,
    /// Parse tokens decoded by the lazy cursor.
    pub semantic_tokens_decoded: u64,
    pub tokens_decoded: u64,
    /// Historical report name retained while benchmark artifacts migrate.
    pub tokens_replayed: u64,
    /// Prefix tokens reused without decoding.
    pub tokens_reused: u64,
    /// Source/parser boundaries rebuilt inside the replay window.
    pub source_boundaries_replayed: u64,
    pub recovery_boundaries_replayed: u64,
    /// Legacy report spelling for replayed parser columns.
    pub columns_replayed: u64,
    /// Suffix columns transferred unchanged.
    pub columns_reused: u64,
    pub suffix_columns_physically_visited: u64,
    /// Candidate and exact checkpoint/frontier proof work.
    pub convergence_candidates: u64,
    pub checkpoint_fingerprint_rejects: u64,
    pub checkpoint_exact_comparisons: u64,
    pub checkpoint_matches: u64,
    pub frontier_exact_comparisons: u64,
    pub frontier_matches: u64,
    /// Historical aggregate names retained for schema readers.
    pub checkpoint_comparisons: u64,
    pub frontier_comparisons: u64,
    pub frontier_proof_nodes_refined: u64,
    pub frontier_proof_edges_refined: u64,
    pub frontier_sccs: u64,
    pub frontier_inconclusive_symmetry: u64,
    /// Persistent segment and seam operations.
    pub segments_split: u64,
    pub segments_attached: u64,
    pub seam_bindings_created: u64,
    pub seam_bindings_flattened: u64,
    pub seam_binding_slots: u64,
    /// GSS, product, and AST record construction/reuse.
    pub gss_nodes_created: u64,
    pub gss_nodes_reused: u64,
    pub gss_edges_created: u64,
    pub gss_edges_reused: u64,
    pub products_created: u64,
    pub products_reused: u64,
    pub ast_records_created: u64,
    pub ast_records_reused: u64,
    /// Reachability, lineage, and syntax journal work.
    pub reachability_keys_read: u64,
    pub reachability_keys_written: u64,
    pub reachability_queue_pops: u64,
    pub lineage_candidates: u64,
    pub lineage_proofs: u64,
    pub lineage_transfers: u64,
    pub lineage_fresh_identities: u64,
    /// Historical aggregate names retained for schema readers.
    pub gss_records_created: u64,
    pub gss_records_reused: u64,
    pub product_records_created: u64,
    pub product_records_reused: u64,
    pub syntax_journal_entries: u64,
    pub syntax_payload_ops: u64,
    pub syntax_parent_ops: u64,
    pub syntax_field_ops: u64,
    pub syntax_order_splices: u64,
    /// Existing parser publication counters.
    pub record_journal_touches: u64,
    pub parser_records_inserted: u64,
    pub parser_records_updated: u64,
    pub parser_records_removed: u64,
    pub snapshot_entries_changed: u64,
    pub snapshot_entries_materialized: u64,
    pub synthesized_token_facts: u64,
    pub syntax_facts_patched: u64,
    pub tree_publisher_node_scans: u64,
    /// Forbidden full-domain work, retained as explicit zero/non-zero gates.
    pub full_token_vector_clones: u64,
    pub full_store_scans: u64,
    pub full_maps_cloned: u64,
    pub full_rebuild_fallbacks: u64,
    pub eof_replays: u64,
    /// Enumerated classification for the one EOF fallback in this command.
    pub full_replay_reason: Option<FullReplayReason>,
    pub full_replay_reason_count: u64,
    /// Recovery and transaction marks.
    pub recovery_searches: u64,
    pub recovery_segments_reused: u64,
    pub recovery_segments_invalidated: u64,
    pub recovery_columns: u64,
    pub recovery_configurations_expanded: u64,
    pub recovery_witness_tokens: u64,
    pub recovery_interval_probes: u64,
    pub rollback_marks_restored: u64,
    /// Committed immutable root/snapshot allocations.
    pub document_roots_allocated: u64,
    pub snapshot_roots_allocated: u64,
}

impl ParserWork {
    /// Records one classified EOF replay. A command can never expose two
    /// fallback causes, which makes the reason a deterministic proof result.
    pub fn record_full_replay(&mut self, reason: FullReplayReason) {
        self.eof_replays += 1;
        if self.full_replay_reason.is_none() {
            self.full_replay_reason = Some(reason);
            self.full_replay_reason_count += 1;
        }
    }

    /// Merges another counter set into this one (checked addition).
    pub fn merge(&mut self, other: &Self) {
        self.component_runs += other.component_runs;
        self.semantic_runs += other.semantic_runs;
        self.layout_projection_runs += other.layout_projection_runs;
        self.restart_occurrences += other.restart_occurrences;
        self.restart_boundary_lookups += other.restart_boundary_lookups;
        self.restart_lookup_depth += other.restart_lookup_depth;
        self.restart_columns += other.restart_columns;
        self.semantic_tokens_decoded += other.semantic_tokens_decoded;
        self.tokens_decoded += other.tokens_decoded;
        self.tokens_replayed += other.tokens_replayed;
        self.tokens_reused += other.tokens_reused;
        self.source_boundaries_replayed += other.source_boundaries_replayed;
        self.recovery_boundaries_replayed += other.recovery_boundaries_replayed;
        self.columns_replayed += other.columns_replayed;
        self.columns_reused += other.columns_reused;
        self.suffix_columns_physically_visited += other.suffix_columns_physically_visited;
        self.convergence_candidates += other.convergence_candidates;
        self.checkpoint_fingerprint_rejects += other.checkpoint_fingerprint_rejects;
        self.checkpoint_exact_comparisons += other.checkpoint_exact_comparisons;
        self.checkpoint_matches += other.checkpoint_matches;
        self.frontier_exact_comparisons += other.frontier_exact_comparisons;
        self.frontier_matches += other.frontier_matches;
        self.checkpoint_comparisons += other.checkpoint_comparisons;
        self.frontier_comparisons += other.frontier_comparisons;
        self.frontier_proof_nodes_refined += other.frontier_proof_nodes_refined;
        self.frontier_proof_edges_refined += other.frontier_proof_edges_refined;
        self.frontier_sccs += other.frontier_sccs;
        self.frontier_inconclusive_symmetry += other.frontier_inconclusive_symmetry;
        self.segments_split += other.segments_split;
        self.segments_attached += other.segments_attached;
        self.seam_bindings_created += other.seam_bindings_created;
        self.seam_bindings_flattened += other.seam_bindings_flattened;
        self.seam_binding_slots += other.seam_binding_slots;
        self.gss_nodes_created += other.gss_nodes_created;
        self.gss_nodes_reused += other.gss_nodes_reused;
        self.gss_edges_created += other.gss_edges_created;
        self.gss_edges_reused += other.gss_edges_reused;
        self.products_created += other.products_created;
        self.products_reused += other.products_reused;
        self.ast_records_created += other.ast_records_created;
        self.ast_records_reused += other.ast_records_reused;
        self.gss_records_created += other.gss_records_created;
        self.gss_records_reused += other.gss_records_reused;
        self.product_records_created += other.product_records_created;
        self.product_records_reused += other.product_records_reused;
        self.reachability_keys_read += other.reachability_keys_read;
        self.reachability_keys_written += other.reachability_keys_written;
        self.reachability_queue_pops += other.reachability_queue_pops;
        self.lineage_candidates += other.lineage_candidates;
        self.lineage_proofs += other.lineage_proofs;
        self.lineage_transfers += other.lineage_transfers;
        self.lineage_fresh_identities += other.lineage_fresh_identities;
        self.syntax_journal_entries += other.syntax_journal_entries;
        self.syntax_payload_ops += other.syntax_payload_ops;
        self.syntax_parent_ops += other.syntax_parent_ops;
        self.syntax_field_ops += other.syntax_field_ops;
        self.syntax_order_splices += other.syntax_order_splices;
        self.record_journal_touches += other.record_journal_touches;
        self.parser_records_inserted += other.parser_records_inserted;
        self.parser_records_updated += other.parser_records_updated;
        self.parser_records_removed += other.parser_records_removed;
        self.snapshot_entries_changed += other.snapshot_entries_changed;
        self.snapshot_entries_materialized += other.snapshot_entries_materialized;
        self.synthesized_token_facts += other.synthesized_token_facts;
        self.syntax_facts_patched += other.syntax_facts_patched;
        self.tree_publisher_node_scans += other.tree_publisher_node_scans;
        self.full_token_vector_clones += other.full_token_vector_clones;
        self.full_store_scans += other.full_store_scans;
        self.full_maps_cloned += other.full_maps_cloned;
        self.full_rebuild_fallbacks += other.full_rebuild_fallbacks;
        self.eof_replays += other.eof_replays;
        if self.full_replay_reason.is_none() {
            self.full_replay_reason = other.full_replay_reason;
        }
        self.full_replay_reason_count += other.full_replay_reason_count;
        self.recovery_searches += other.recovery_searches;
        self.recovery_segments_reused += other.recovery_segments_reused;
        self.recovery_segments_invalidated += other.recovery_segments_invalidated;
        self.recovery_columns += other.recovery_columns;
        self.recovery_configurations_expanded += other.recovery_configurations_expanded;
        self.recovery_witness_tokens += other.recovery_witness_tokens;
        self.recovery_interval_probes += other.recovery_interval_probes;
        self.rollback_marks_restored += other.rollback_marks_restored;
        self.document_roots_allocated += other.document_roots_allocated;
        self.snapshot_roots_allocated += other.snapshot_roots_allocated;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TokenData {
    pub(crate) id: TokenEntryId,
    pub(crate) terminal: Option<TerminalId>,
    pub(crate) start: usize,
    pub(crate) length: usize,

    /// Stable occurrence identity; it is independent of byte and token positions.
    pub(crate) column: TokenOccurrenceId,
    pub(crate) fingerprint: TokenFingerprint,
}
/// parser-relevant persistent projections; typed lexical payloads remain in
/// the lexer arena. Token decoding is rank-addressed and never requires a
/// document-wide `Vec<TokenData>`.
pub(crate) struct ParserTokenDocument {
    pub(crate) document: StableDocumentId,
    source_len: usize,
    /// Layout revision of the lexical root this projection was built from.
    /// The semantic parser keying is structural; this field lets the layout
    /// refresh child detect whether a parser token root is stale for
    /// coordinate-only edits.
    layout_revision: LayoutRevisionId,
    semantic: StableTape<ParseTokenRef>,
    semantic_index: PersistentOccurrenceIndex,
    layout: StableTape<TokenLayoutEntry>,
    layout_index: PersistentOccurrenceIndex,
}

impl ParserTokenDocument {
    pub(crate) fn from_lexical<R: LexerRoot>(document: &LexicalDocument<R>) -> Self {
        Self {
            document: document.document,
            source_len: document.source.len_bytes(),
            layout_revision: document.layout_revision,
            semantic: document.semantic.clone(),
            semantic_index: document.semantic_index.clone(),
            layout: document.layout.clone(),
            layout_index: document.layout_index.clone(),
        }
    }

    pub(crate) fn boundary_at_rank(&self, rank: usize) -> TokenBoundaryId {
        if rank < self.semantic.len() {
            TokenBoundaryId::before(
                self.document,
                self.semantic
                    .get(rank)
                    .expect("rank is inside semantic token tape")
                    .occurrence,
            )
        } else {
            TokenBoundaryId::eof(self.document)
        }
    }

    pub(crate) fn layout_revision(&self) -> LayoutRevisionId {
        self.layout_revision
    }

    pub(crate) fn semantic_len(&self) -> usize {
        self.semantic.len()
    }

    pub(crate) fn rank_of_occurrence(&self, occurrence: TokenOccurrenceId) -> Option<usize> {
        self.semantic.rank_of_id(occurrence.0, &self.semantic_index)
    }

    pub(crate) fn token_at(&self, rank: usize) -> Option<TokenData> {
        if rank == self.semantic.len() {
            return Some(TokenData {
                id: usize::MAX,
                terminal: None,
                start: self.source_len,
                length: 0,
                column: TokenOccurrenceId(u64::MAX),
                fingerprint: crate::framework::parse::identity::eof_fingerprint(),
            });
        }
        let token = self.semantic.get(rank)?;
        let layout_rank = self
            .layout
            .rank_of_id(token.occurrence.0, &self.layout_index)?;
        let layout = self.layout.get(layout_rank)?;
        Some(TokenData {
            id: usize::try_from(token.occurrence.0).ok()?,
            terminal: token.terminal,
            start: usize::try_from(self.layout.metric_before(layout_rank).source_bytes).ok()?,
            length: layout.byte_len as usize,
            column: token.occurrence,
            fingerprint: layout.fingerprint.0,
        })
    }
}

/// A rank-addressed cursor over a parser token root. The semantic cursor moves
/// inside tape leaves in amortized O(1); only coordinate lookup for the token
/// currently being decoded uses the lexical occurrence index.
pub(crate) struct ParserTokenCursor {
    document: Arc<ParserTokenDocument>,
    semantic: Option<TapeCursor<ParseTokenRef>>,
    at_eof: bool,
}

impl ParserTokenDocument {
    pub(crate) fn cursor_at(self: &Arc<Self>, rank: usize) -> ParserTokenCursor {
        let semantic_len = self.semantic.len();
        ParserTokenCursor {
            document: Arc::clone(self),
            semantic: self.semantic.cursor_at(rank),
            at_eof: rank >= semantic_len,
        }
    }
}

impl ParserTokenCursor {
    pub(crate) fn rank(&self) -> usize {
        self.semantic
            .as_ref()
            .map_or(self.document.semantic_len(), TapeCursor::rank)
    }

    pub(crate) fn current(&self) -> Option<TokenData> {
        if self.at_eof {
            return self.document.token_at(self.document.semantic_len());
        }
        let semantic = self.semantic.as_ref()?.current();
        let layout_rank = self
            .document
            .layout
            .rank_of_id(semantic.occurrence.0, &self.document.layout_index)?;
        let layout = self.document.layout.get(layout_rank)?;
        Some(TokenData {
            id: usize::try_from(semantic.occurrence.0).ok()?,
            terminal: semantic.terminal,
            start: usize::try_from(self.document.layout.metric_before(layout_rank).source_bytes)
                .ok()?,
            length: layout.byte_len as usize,
            column: semantic.occurrence,
            fingerprint: layout.fingerprint.0,
        })
    }

    /// Moves to the next semantic token, then to the synthetic EOF token.
    /// Returns false only once EOF has already been consumed.
    pub(crate) fn advance(&mut self) -> bool {
        if self.at_eof {
            return false;
        }
        if self.semantic.as_mut().is_some_and(TapeCursor::advance) {
            return true;
        }
        self.semantic = None;
        self.at_eof = true;
        true
    }
}

/// Persistent reach-count keys for the accepted-root DAG domain.
///
/// Product and record counts use distinct key types even though both are
/// encoded as compact `u64` trie keys. Keeping the domains typed prevents a
/// product ID from being accidentally looked up in the record index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProductReachKey(pub(crate) ProductId);

impl crate::reactive::store::TrieKey for ProductReachKey {
    fn trie_hash(&self) -> u64 {
        self.0 as u64
    }

    fn trie_eq(&self, other: &Self) -> bool {
        self == other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RecordReachKey(pub(crate) u64);

impl crate::reactive::store::TrieKey for RecordReachKey {
    fn trie_hash(&self) -> u64 {
        self.0
    }

    fn trie_eq(&self, other: &Self) -> bool {
        self == other
    }
}

/// One accepted-root reach-count transition captured during freeze.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordTransition {
    pub(crate) before_count: u32,
    pub(crate) after_count: u32,
}

#[derive(Clone, Default)]
pub(crate) struct ParserTreeFacts {
    pub(crate) records: Arc<crate::reactive::store::RadixMap<()>>,
    pub(crate) root: Option<u64>,
    /// Product reach counts in the accepted-root DAG. Parser-column/cache
    /// retention never updates this domain.
    pub(crate) product_reach_counts: Arc<crate::reactive::store::Hamt<ProductReachKey, u32>>,
    /// Direct AST-record reach counts induced by accepted products.
    pub(crate) record_reach_counts: Arc<crate::reactive::store::Hamt<RecordReachKey, u32>>,
    /// Stable lineage for every record in `records` at the last commit.
    /// This preserves exact removal keys when the mutable parser arena
    /// reuses a cached product across commands.
    pub(crate) record_lineages: Arc<crate::reactive::store::RadixMap<u64>>,
    /// Last published child order per parent lineage, expressed as
    /// lineage identities for the parser splice oracle. This is a persistent
    /// path-copying radix root: local parent changes never clone all orders.
    pub(crate) published_child_orders: Arc<crate::reactive::store::RadixMap<PublishedChildOrder>>,
    pub(crate) child_orders: Arc<crate::reactive::store::RadixMap<Vec<u64>>>,
}

/// One successfully published parent/child order. The parser freeze keeps
/// lineage order for its delta oracle; publication additionally records the
/// complete generated syntax identities so a later command can retract links
/// after the originating arena/session has been replaced.
#[derive(Clone)]
pub(crate) struct PublishedChildOrder {
    pub(crate) parent_node: PublishedNodeIdentity,
    pub(crate) children: Arc<[(u64, PublishedNodeIdentity)]>,
}

/// A node identity retained across parser arena/session replacement.
///
/// `raw` is the stable fact-key hash. `identity` is the complete logical
/// syntax key needed to reconstruct a typed `Node` without consulting a
/// dead arena record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PublishedNodeIdentity {
    pub(crate) raw: u64,
    pub(crate) identity: crate::reactive::view::SyntaxNodeIdentity,
}

/// Mutable generation arenas owned by one document root.
#[derive(Clone)]
pub(crate) struct SessionArenas {
    pub trees: TreeArena,
    pub products: ProductArena,
    pub ast: Arc<AstArena>,
    pub gss: GssArena,
}
impl SessionArenas {
    pub(crate) fn seal_generations(&mut self) {
        self.trees.seal_generation();
        self.products.seal_generation();
        self.gss.seal_generation();
        Arc::make_mut(&mut self.ast).seal_generation();
    }
}

/// Immutable per-document parser root. A command path-copies this small
/// handle and stages mutable session/arena generations behind its `Arc`s;
/// snapshots retain the prior root without retaining a mutable global map.
#[derive(Clone)]
pub(crate) struct ParserDocumentRoot {
    pub(crate) session: Arc<ParserSessionState>,
    pub(crate) arenas: Arc<SessionArenas>,
    pub(crate) token: Option<Arc<ParserTokenDocument>>,
    pub(crate) roots: Arc<Vec<ProductId>>,
    pub(crate) incremental_stats: IncrementalParseStats,
    pub(crate) tree_facts: Arc<ParserTreeFacts>,
    pub(crate) tree_delta: Arc<crate::framework::parse::delta::ParseDelta>,
    pub(crate) semantic_revision: u64,
    pub(crate) published_status: Option<crate::framework::parse::delta::ParsedStatus>,
    pub(crate) published_diagnostics:
        Arc<Vec<crate::framework::parse::data::green::ParseErrorInfo>>,
}

impl ParserDocumentRoot {
    pub(crate) fn with_document(_uri: &Uri<String>, document: StableDocumentId) -> Self {
        Self {
            session: Arc::new(ParserSessionState::default()),
            arenas: Arc::new(SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: Arc::new(AstArena::with_document(document.0)),
                gss: GssArena::new(),
            }),
            token: None,
            roots: Arc::new(Vec::new()),
            incremental_stats: IncrementalParseStats::default(),
            tree_facts: Arc::new(ParserTreeFacts::default()),
            tree_delta: Arc::new(crate::framework::parse::delta::ParseDelta::default()),
            semantic_revision: 0,
            published_status: None,
            published_diagnostics: Arc::new(Vec::new()),
        }
    }
}

/// Immutable parser snapshot root. The URI map is a persistent HAMT: updating
/// one document path-copies only the affected lookup path while every
/// unrelated document root remains pointer-shared.
#[derive(Clone)]
pub(crate) struct ParserSnapshotState {
    pub(crate) documents: PersistentUriMap<Arc<ParserDocumentRoot>>,
}

impl Default for ParserSnapshotState {
    fn default() -> Self {
        Self {
            documents: PersistentUriMap::with_kind(
                crate::reactive::pathwork::StructureKind::ParserIndex,
            ),
        }
    }
}

impl ParserSnapshotState {
    pub(crate) fn replace_document(&mut self, uri: Uri<String>, root: Arc<ParserDocumentRoot>) {
        self.documents.insert(uri, root);
    }

    pub(crate) fn remove_document(&mut self, uri: &Uri<String>) {
        self.documents.remove(uri);
    }
}
