use crate::utils::Span;
use fluent_uri::Uri;
use ropey::Rope;
use std::{
    any::TypeId,
    collections::{BTreeSet, HashMap},
    fmt,
    ops::Deref,
    sync::Arc,
};

use super::data::ast::AstArena;
use crate::framework::{
    lex::{LayoutRevisionId, LexerRoot, LexicalDocument, ParseTokenRef, TokenLayoutEntry},
    parse::{
        data::{
            ast::{AnchoredSpan, AstBox, AstId, AstToken, TokenEntryId, document_key},
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

/// Metadata and source span for one AST value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AstSnapshotEntry {
    pub(crate) product: ProductId,
    pub(crate) type_id: TypeId,
    pub(crate) span: Span,
}

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
        if node.document_id() != document_key(&self.uri) {
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
            .rank_of_occurrence(occurrence)
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
            .rank_of_occurrence(occurrence)
            .and_then(|rank| self.token_document.token_at(rank))
            .map_or(self.source.len_bytes(), |token| token.start)
    }

    fn span_for_extent(&self, extent: AnchoredSpan) -> Span {
        let start = self.coordinate_at(extent.start);
        let end = self
            .token_document
            .rank_of_occurrence(extent.end)
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

pub(crate) type TokenOccurrenceId = usize;

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
}

/// Deterministic parser work counters for one document command (plan §10.1).
/// Counters roll back with their command and never enter reactive facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParserWork {
    /// Parser component invocations for this document.
    pub component_runs: u64,
    /// Restart occurrence anchors chosen for replay.
    pub restart_occurrences: u64,
    /// Restart boundary anchors chosen for replay.
    pub restart_columns: u64,
    /// Parse tokens decoded by the lazy cursor.
    pub tokens_decoded: u64,
    /// Legacy alias counter retained for report compatibility.
    pub tokens_replayed: u64,
    /// Prefix tokens reused without decoding.
    pub tokens_reused: u64,
    /// Columns rebuilt inside the replay window.
    pub columns_replayed: u64,
    /// Suffix columns transferred unchanged.
    pub columns_reused: u64,
    /// Retained suffix columns physically visited after convergence.
    pub suffix_columns_physically_visited: u64,
    /// Immutable parse segments attached by pointer.
    pub segments_attached: u64,
    /// Checkpoint equality comparisons.
    pub checkpoint_comparisons: u64,
    /// Frontier equality comparisons.
    pub frontier_comparisons: u64,
    /// GSS nodes created.
    pub gss_records_created: u64,
    /// GSS nodes reused by identity.
    pub gss_records_reused: u64,
    /// Products created.
    pub product_records_created: u64,
    /// Products reused through reduction caches.
    pub product_records_reused: u64,
    /// AST records created.
    pub ast_records_created: u64,
    /// AST records reused by identity.
    pub ast_records_reused: u64,
    /// Exact parser record mutations journaled.
    pub record_journal_touches: u64,
    /// Parser records entering reachability.
    pub parser_records_inserted: u64,
    /// Retained parser records with changed payloads.
    pub parser_records_updated: u64,
    /// Parser records leaving reachability.
    pub parser_records_removed: u64,
    /// Snapshot entries changed by publication.
    pub snapshot_entries_changed: u64,
    /// Snapshot entries eagerly materialized by a command.
    pub snapshot_entries_materialized: u64,
    /// Synthesized token facts published.
    pub synthesized_token_facts: u64,
    /// Syntax node/root facts patched.
    pub syntax_facts_patched: u64,
    /// Tree-publisher node scans.
    pub tree_publisher_node_scans: u64,
    /// Full parser-token vector clones.
    pub full_token_vector_clones: u64,
    /// Full parser-store or tree scans.
    pub full_store_scans: u64,
    /// Defensive full rebuilds; valid local edits must leave this zero.
    pub full_rebuild_fallbacks: u64,
    /// Replays that ran to EOF instead of converging.
    pub eof_replays: u64,
    /// Recovery searches started.
    pub recovery_searches: u64,
    /// Recovery segments reused without search.
    pub recovery_segments_reused: u64,
    /// Recovery segments invalidated by intersecting edits.
    pub recovery_segments_invalidated: u64,
    /// Recovery-derived columns in the final session.
    pub recovery_columns: u64,
    /// Configurations expanded by canonical recovery search.
    pub recovery_configurations_expanded: u64,
    /// Witness tokens/gaps recorded for recovery reuse proofs.
    pub recovery_witness_tokens: u64,
    /// Recovery interval-index probes.
    pub recovery_interval_probes: u64,
}

impl ParserWork {
    /// Merges another counter set into this one (checked addition).
    pub fn merge(&mut self, other: &Self) {
        self.component_runs += other.component_runs;
        self.restart_occurrences += other.restart_occurrences;
        self.restart_columns += other.restart_columns;
        self.tokens_decoded += other.tokens_decoded;
        self.tokens_replayed += other.tokens_replayed;
        self.tokens_reused += other.tokens_reused;
        self.columns_replayed += other.columns_replayed;
        self.columns_reused += other.columns_reused;
        self.suffix_columns_physically_visited += other.suffix_columns_physically_visited;
        self.segments_attached += other.segments_attached;
        self.checkpoint_comparisons += other.checkpoint_comparisons;
        self.frontier_comparisons += other.frontier_comparisons;
        self.gss_records_created += other.gss_records_created;
        self.gss_records_reused += other.gss_records_reused;
        self.product_records_created += other.product_records_created;
        self.product_records_reused += other.product_records_reused;
        self.ast_records_created += other.ast_records_created;
        self.ast_records_reused += other.ast_records_reused;
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
        self.full_rebuild_fallbacks += other.full_rebuild_fallbacks;
        self.eof_replays += other.eof_replays;
        self.recovery_searches += other.recovery_searches;
        self.recovery_segments_reused += other.recovery_segments_reused;
        self.recovery_segments_invalidated += other.recovery_segments_invalidated;
        self.recovery_columns += other.recovery_columns;
        self.recovery_configurations_expanded += other.recovery_configurations_expanded;
        self.recovery_witness_tokens += other.recovery_witness_tokens;
        self.recovery_interval_probes += other.recovery_interval_probes;
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
#[derive(Clone)]
pub(crate) struct ParserTokenDocument {
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
            source_len: document.source.len_bytes(),
            layout_revision: document.layout_revision,
            semantic: document.semantic.clone(),
            semantic_index: document.semantic_index.clone(),
            layout: document.layout.clone(),
            layout_index: document.layout_index.clone(),
        }
    }

    pub(crate) fn layout_revision(&self) -> LayoutRevisionId {
        self.layout_revision
    }

    pub(crate) fn semantic_len(&self) -> usize {
        self.semantic.len()
    }

    pub(crate) fn occurrence_at(&self, rank: usize) -> Option<TokenOccurrenceId> {
        self.semantic
            .get(rank)
            .and_then(|token| usize::try_from(token.occurrence.0).ok())
    }

    pub(crate) fn rank_of_occurrence(&self, occurrence: TokenOccurrenceId) -> Option<usize> {
        self.semantic
            .rank_of_id(occurrence as u64, &self.semantic_index)
    }

    pub(crate) fn token_at(&self, rank: usize) -> Option<TokenData> {
        if rank == self.semantic.len() {
            return Some(TokenData {
                id: usize::MAX,
                terminal: None,
                start: self.source_len,
                length: 0,
                column: usize::MAX,
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
            column: usize::try_from(token.occurrence.0).ok()?,
            fingerprint: layout.fingerprint.0,
        })
    }

    pub(crate) fn structure_ptr_eq(&self, other: &Self) -> bool {
        self.semantic.root_ptr_eq(&other.semantic)
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
            column: usize::try_from(semantic.occurrence.0).ok()?,
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
    pub(crate) product_reach_counts:
        Arc<crate::reactive::store::Hamt<ProductReachKey, u32>>,
    /// Direct AST-record reach counts induced by accepted products.
    pub(crate) record_reach_counts:
        Arc<crate::reactive::store::Hamt<RecordReachKey, u32>>,
    /// Stable lineage for every record in `records` at the last commit.
    /// This preserves exact removal keys when the mutable parser arena
    /// reuses a cached product across commands.
    pub(crate) record_lineages: Arc<std::collections::HashMap<u64, u64>>,
    /// Last lineage-to-node order committed by the tree publisher.
    pub(crate) published_child_orders: Arc<std::collections::HashMap<u64, PublishedChildOrder>>,
    /// Last published child order per parent lineage, expressed as
    /// lineage identities for the parser splice oracle.
    pub(crate) child_orders: Arc<std::collections::HashMap<u64, Vec<u64>>>,
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


impl ParserTreeFacts {
    pub(crate) fn contains(&self, record: u64) -> bool {
        self.records.get(record).is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }
}

/// Per-document mutable parser arenas and working session state.
#[derive(Clone)]
pub(crate) struct SessionArenas {
    pub trees: TreeArena,
    pub products: ProductArena,
    pub ast: Arc<AstArena>,
    pub gss: GssArena,
}

#[derive(Clone, Default)]
pub(crate) struct ParserSnapshotState {
    pub(crate) sessions: HashMap<Uri<String>, Arc<ParserSessionState>>,
    pub(crate) roots: HashMap<Uri<String>, Arc<Vec<ProductId>>>,
    pub(crate) tokens: HashMap<Uri<String>, Arc<ParserTokenDocument>>,
    pub(crate) incremental_stats: HashMap<Uri<String>, IncrementalParseStats>,
    pub(crate) tree_facts: HashMap<Uri<String>, Arc<ParserTreeFacts>>,
    /// The canonical adjacent-revision output consumed by the tree
    /// publisher (plan §9, §12).
    pub(crate) tree_deltas: HashMap<Uri<String>, Arc<crate::framework::parse::delta::ParseDelta>>,
    /// Per-document semantic-revision ordinals: bumped once per non-empty
    /// ParseDelta so tree consumers get an O(1) equality handle (§12.7).
    pub(crate) semantic_revisions: HashMap<Uri<String>, u64>,
    /// The last published status fact, for exact status deltas.
    pub(crate) published_status: HashMap<Uri<String>, crate::framework::parse::delta::ParsedStatus>,
    /// The last published diagnostics, for exact diagnostic key deltas.
    pub(crate) published_diagnostics:
        HashMap<Uri<String>, Arc<Vec<crate::framework::parse::data::green::ParseErrorInfo>>>,
}
