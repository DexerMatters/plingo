//! Persistent lexical documents and granular semantic publications.
//!
//! A lexer command mutates only an immutable tape root and the bounded index
//! paths needed for its replay window.  All source positions are derived from
//! tape prefix metrics; a suffix never stores, clones, or rebases offsets.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

use fluent_uri::Uri;
use reactive_macros::view;

use crate::{
    framework::{
        lex::{LexErrorInfo, LexErrorKind, LexToken, LexerRoot, LexerState, State},
        parse::{
            AstToken, TokenData,
            grammar::TerminalId,
            identity::{error_fingerprint, token_fingerprint},
        },
        tape::{
            ExactHashPrefilter, PersistentOccurrenceIndex, SequenceMetric, StableTape, TapeEntry,
            TapeIdAllocator,
        },
    },
    reactive::{
        Result,
        kind::{Map, observe_view},
    },
};

/// A checked monotonic token-occurrence identity.  It is scoped to one source
/// document and is never reused within that document lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenOccurrenceId(pub u64);

/// A fixed-seed prefilter of terminal + semantic value.  Equality of values is
/// still checked before an occurrence is retained or a fact is suppressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TokenFingerprint(pub u64);

/// Advances exactly when a parser-visible token payload, membership, order, or
/// terminal changes. Layout-only edits remain outside this domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SemanticRevisionId(pub u64);

/// Stable per-document namespace identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StableDocumentId(pub u64);

/// Advances for every effective source-coordinate change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayoutRevisionId(pub u64);

/// Advances only for retained semantic occurrences whose fact payload changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenValueRevisionId(pub u64);

/// Advances only for semantic membership, order, or terminal-structure change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenStructureRevisionId(pub u64);

/// A canonical, immutable lexer stack.  Offsets deliberately do not live
/// here: equal nested mode/slot states at different source coordinates are the
/// same checkpoint state.
pub(crate) struct CanonicalLexerState<R: LexerRoot> {
    stack: Arc<[State<R>]>,
}

impl<R: LexerRoot> Clone for CanonicalLexerState<R> {
    fn clone(&self) -> Self {
        Self {
            stack: Arc::clone(&self.stack),
        }
    }
}

impl<R: LexerRoot> std::fmt::Debug for CanonicalLexerState<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanonicalLexerState")
            .field("depth", &self.stack.len())
            .finish()
    }
}

impl<R: LexerRoot> PartialEq for CanonicalLexerState<R> {
    fn eq(&self, other: &Self) -> bool {
        self.stack == other.stack
    }
}
impl<R: LexerRoot> Eq for CanonicalLexerState<R> {}

impl<R: LexerRoot> CanonicalLexerState<R> {
    pub(crate) fn from_state(state: &LexerState<R>) -> Self {
        Self {
            stack: state.state_stack.iter().cloned().collect(),
        }
    }

    pub(crate) fn materialize(&self, offset: usize) -> LexerState<R> {
        LexerState {
            offset,
            state_stack: self.stack.iter().cloned().collect(),
        }
    }

    pub(crate) fn ptr_or_exact_eq(left: &Arc<Self>, right: &Arc<Self>) -> bool {
        Arc::ptr_eq(left, right) || left == right
    }
}

/// Document-local immutable-state interner.  Hashes choose buckets only;
/// structural equality validates every hit.
pub(crate) struct LexerStateInterner<R: LexerRoot> {
    buckets: HashMap<u64, Vec<Arc<CanonicalLexerState<R>>>>,
}

impl<R: LexerRoot> Clone for LexerStateInterner<R> {
    fn clone(&self) -> Self {
        Self {
            buckets: self.buckets.clone(),
        }
    }
}

impl<R: LexerRoot> Default for LexerStateInterner<R> {
    fn default() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }
}

impl<R: LexerRoot> LexerStateInterner<R> {
    pub(crate) fn intern(&mut self, state: &LexerState<R>) -> Arc<CanonicalLexerState<R>> {
        let canonical = CanonicalLexerState::from_state(state);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        canonical.stack.hash(&mut hasher);
        let bucket = self.buckets.entry(hasher.finish()).or_default();
        if let Some(existing) = bucket
            .iter()
            .find(|existing| existing.as_ref() == &canonical)
        {
            return Arc::clone(existing);
        }
        let canonical = Arc::new(canonical);
        bucket.push(Arc::clone(&canonical));
        canonical
    }
}
/// A persistent URI-keyed map used for lexer document roots and metrics.
///
/// The HAMT root is copied by `Arc`; changing one URI copies only its lookup
/// path and preserves every unrelated document root without cloning a map of
/// all open documents.
#[derive(Clone, Debug)]
pub(crate) struct PersistentUriMap<V: Clone> {
    entries: crate::reactive::store::Hamt<UriMapKey, V>,
}

#[derive(Clone, Debug)]
pub(crate) struct UriMapKey(Uri<String>);

impl UriMapKey {
    pub(crate) fn uri(&self) -> &Uri<String> {
        &self.0
    }
}

impl crate::reactive::store::TrieKey for UriMapKey {
    fn trie_hash(&self) -> u64 {
        crate::framework::source::fnv1a_uri(self.0.as_str())
    }

    fn trie_eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<V: Clone> Default for PersistentUriMap<V> {
    fn default() -> Self {
        Self {
            entries: crate::reactive::store::Hamt::with_kind(
                crate::reactive::pathwork::StructureKind::LexerDocumentIndex,
            ),
        }
    }
}

impl<V: Clone> PersistentUriMap<V> {
    pub(crate) fn get(&self, uri: &Uri<String>) -> Option<&V> {
        self.entries.get(&UriMapKey(uri.clone()))
    }

    pub(crate) fn contains_key(&self, uri: &Uri<String>) -> bool {
        self.get(uri).is_some()
    }

    pub(crate) fn insert(&mut self, uri: Uri<String>, value: V) {
        self.entries.insert(UriMapKey(uri), value);
    }

    pub(crate) fn remove(&mut self, uri: &Uri<String>) -> bool {
        self.entries.remove(&UriMapKey(uri.clone()))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Uri<String>, &V)> {
        self.entries.iter().map(|(key, value)| (key.uri(), value))
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|(_, value)| value)
    }
}


/// One local immutable lexical occurrence.  The occurrence never carries a
/// global byte offset; its span is the prefix-source-byte metric at its rank.
#[derive(Clone)]
pub(crate) struct LexicalOccurrence<R: LexerRoot> {
    pub(crate) id: TokenOccurrenceId,
    pub(crate) byte_len: u32,
    pub(crate) terminal: Option<TerminalId>,
    pub(crate) skip: bool,
    pub(crate) value: Arc<R>,
    pub(crate) error: Option<LexErrorKind>,
    pub(crate) state_after: Arc<CanonicalLexerState<R>>,
}

impl<R: LexerRoot> std::fmt::Debug for LexicalOccurrence<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LexicalOccurrence")
            .field("id", &self.id)
            .field("byte_len", &self.byte_len)
            .field("terminal", &self.terminal)
            .field("skip", &self.skip)
            .field("error", &self.error)
            .finish()
    }
}

impl<R: LexerRoot> PartialEq for LexicalOccurrence<R> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.byte_len == other.byte_len
            && self.terminal == other.terminal
            && self.skip == other.skip
            && self.value == other.value
            && self.error == other.error
            && CanonicalLexerState::ptr_or_exact_eq(&self.state_after, &other.state_after)
    }
}
impl<R: LexerRoot> Eq for LexicalOccurrence<R> {}

impl<R: LexerRoot> LexicalOccurrence<R> {
    pub(crate) fn is_semantic(&self) -> bool {
        self.error.is_some() || (!self.skip && self.terminal.is_some())
    }

    pub(crate) fn terminal_key(&self) -> usize {
        self.terminal
            .map_or(usize::MAX, |terminal| terminal.token_id as usize)
    }

    pub(crate) fn fingerprint(&self) -> TokenFingerprint {
        let width = self.byte_len as usize;
        let value = match self.error {
            Some(error) => error_fingerprint(&error, width),
            None => token_fingerprint(self.terminal, self.value.as_ref(), width),
        };
        TokenFingerprint(value)
    }

    pub(crate) fn structural_eq(&self, other: &Self) -> bool {
        self.terminal == other.terminal
            && self.skip == other.skip
            && self.error.is_some() == other.error.is_some()
    }

    pub(crate) fn exact_payload_eq(&self, other: &Self) -> bool {
        self.byte_len == other.byte_len
            && self.value == other.value
            && self.error == other.error
            && self.terminal == other.terminal
            && self.skip == other.skip
    }

    pub(crate) fn materialize(&self, start: usize) -> Option<LexToken<R>>
    where
        R: Clone,
    {
        Some(LexToken {
            id: usize::try_from(self.id.0).ok()?,
            start,
            length: self.byte_len as usize,
            terminal: self.terminal,
            error: self.error.map(|kind| LexErrorInfo {
                kind,
                start,
                end: start.saturating_add(self.byte_len as usize),
            }),
            value: self.value.as_ref().clone(),
        })
    }
}

impl<R: LexerRoot> TapeEntry for LexicalOccurrence<R> {
    fn stable_id(&self) -> u64 {
        self.id.0
    }

    fn metric(&self) -> SequenceMetric {
        // This is a rejection prefilter only.  Semantic value deliberately does
        // not participate: same-terminal value edits preserve parser shape.
        let terminal = self
            .terminal
            .map_or(u64::MAX, |terminal| terminal.token_id as u64);
        let error = self.error.map_or(0, |kind| match kind {
            LexErrorKind::UnexpectedInput => 1,
            LexErrorKind::RequiredBoundary => 2,
        });
        SequenceMetric {
            lexical_count: 1,
            semantic_count: u64::from(self.is_semantic()),
            source_bytes: self.byte_len as u64,
            structural_hash: ExactHashPrefilter(
                terminal.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17)
                    ^ error
                    ^ u64::from(self.skip),
            ),
        }
    }
}

/// The parser-visible structural projection.  It intentionally excludes
/// source coordinates and semantic value/fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseTokenRef {
    pub(crate) occurrence: TokenOccurrenceId,
    pub(crate) terminal: Option<TerminalId>,
    pub(crate) error: bool,
}

impl TapeEntry for ParseTokenRef {
    fn stable_id(&self) -> u64 {
        self.occurrence.0
    }

    fn metric(&self) -> SequenceMetric {
        let terminal = self
            .terminal
            .map_or(u64::MAX, |terminal| terminal.token_id as u64);
        SequenceMetric {
            lexical_count: 0,
            semantic_count: 1,
            source_bytes: 0,
            structural_hash: ExactHashPrefilter(terminal ^ u64::from(self.error)),
        }
    }
}

impl<R: LexerRoot> From<&LexicalOccurrence<R>> for ParseTokenRef {
    fn from(token: &LexicalOccurrence<R>) -> Self {
        Self {
            occurrence: token.id,
            terminal: token.terminal,
            error: token.error.is_some(),
        }
    }
}

/// Non-generic coordinate/fingerprint projection retained by parser and editor
/// snapshots. It makes lazy parser token decoding independent of typed lexer
/// payload storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TokenLayoutEntry {
    pub(crate) occurrence: TokenOccurrenceId,
    pub(crate) byte_len: u32,
    pub(crate) terminal: Option<TerminalId>,
    pub(crate) skip: bool,
    pub(crate) error: bool,
    pub(crate) fingerprint: TokenFingerprint,
}

impl TapeEntry for TokenLayoutEntry {
    fn stable_id(&self) -> u64 {
        self.occurrence.0
    }

    fn metric(&self) -> SequenceMetric {
        let terminal = self
            .terminal
            .map_or(u64::MAX, |terminal| terminal.token_id as u64);
        SequenceMetric {
            lexical_count: 1,
            semantic_count: u64::from(self.error || (!self.skip && self.terminal.is_some())),
            source_bytes: self.byte_len as u64,
            structural_hash: ExactHashPrefilter(
                terminal.rotate_left(17) ^ u64::from(self.skip) ^ u64::from(self.error),
            ),
        }
    }
}

impl<R: LexerRoot> From<&LexicalOccurrence<R>> for TokenLayoutEntry {
    fn from(token: &LexicalOccurrence<R>) -> Self {
        Self {
            occurrence: token.id,
            byte_len: token.byte_len,
            terminal: token.terminal,
            skip: token.skip,
            error: token.error.is_some(),
            fingerprint: token.fingerprint(),
        }
    }
}

/// Per-document persistent state.  It owns every mutable identity allocator;
/// snapshots only retain immutable roots and no global token arena exists.
// The command-local interner is intentionally reset when a document root is
// cloned.  All committed lexical state is represented by `initial_state` and
// occurrence state arcs; replay only needs a cache for states visited in the
// current bounded window.
pub(crate) struct LexicalDocument<R: LexerRoot> {
    pub(crate) document: StableDocumentId,
    /// Source revision that produced this lexical root.  Lexer adjacency is
    /// proved by this handle, never by comparing source bytes.
    pub(crate) source_revision: crate::framework::source::SourceRevisionId,
    pub(crate) source: Arc<ropey::Rope>,
    pub(crate) lexical: StableTape<LexicalOccurrence<R>>,
    pub(crate) lexical_index: PersistentOccurrenceIndex,
    pub(crate) layout: StableTape<TokenLayoutEntry>,
    pub(crate) layout_index: PersistentOccurrenceIndex,
    pub(crate) semantic: StableTape<ParseTokenRef>,
    pub(crate) semantic_index: PersistentOccurrenceIndex,
    pub(crate) initial_state: Arc<CanonicalLexerState<R>>,
    pub(crate) state_interner: LexerStateInterner<R>,
    pub(crate) tape_ids: TapeIdAllocator,
    pub(crate) next_occurrence: u64,
    pub(crate) layout_revision: LayoutRevisionId,
    pub(crate) semantic_revision: SemanticRevisionId,
    pub(crate) value_revision: TokenValueRevisionId,
    pub(crate) structure_revision: TokenStructureRevisionId,
}

impl<R: LexerRoot> Clone for LexicalDocument<R> {
    fn clone(&self) -> Self {
        Self {
            document: self.document,
            source_revision: self.source_revision,
            source: Arc::clone(&self.source),
            lexical: self.lexical.clone(),
            lexical_index: self.lexical_index.clone(),
            layout: self.layout.clone(),
            layout_index: self.layout_index.clone(),
            semantic: self.semantic.clone(),
            semantic_index: self.semantic_index.clone(),
            initial_state: Arc::clone(&self.initial_state),
            state_interner: LexerStateInterner::default(),
            tape_ids: self.tape_ids.clone(),
            next_occurrence: self.next_occurrence,
            layout_revision: self.layout_revision,
            semantic_revision: self.semantic_revision,
            value_revision: self.value_revision,
            structure_revision: self.structure_revision,
        }
    }
}

impl<R: LexerRoot> std::fmt::Debug for LexicalDocument<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LexicalDocument")
            .field("document", &self.document)
            .field("lexical_len", &self.lexical.len())
            .field("semantic_len", &self.semantic.len())
            .field("layout_revision", &self.layout_revision)
            .field("semantic_revision", &self.semantic_revision)
            .field("value_revision", &self.value_revision)
            .field("structure_revision", &self.structure_revision)
            .finish()
    }
}
impl<R: LexerRoot> LexicalDocument<R> {
    pub(crate) fn empty(document: StableDocumentId, root: State<R>) -> Self {
        let initial = LexerState::new(root);
        let mut state_interner = LexerStateInterner::default();
        let initial_state = state_interner.intern(&initial);
        Self {
            document,
            source_revision: crate::framework::source::SourceRevisionId(0),
            source: Arc::new(ropey::Rope::new()),
            lexical: StableTape::new(),
            lexical_index: PersistentOccurrenceIndex::default(),
            layout: StableTape::new(),
            layout_index: PersistentOccurrenceIndex::default(),
            semantic: StableTape::new(),
            semantic_index: PersistentOccurrenceIndex::default(),
            initial_state,
            state_interner,
            tape_ids: TapeIdAllocator::new(),
            next_occurrence: 0,
            layout_revision: LayoutRevisionId(0),
            semantic_revision: SemanticRevisionId(0),
            value_revision: TokenValueRevisionId(0),
            structure_revision: TokenStructureRevisionId(0),
        }
    }

    pub(crate) fn lexical_rank_of(&self, occurrence: TokenOccurrenceId) -> Option<usize> {
        self.layout.rank_of_id(occurrence.0, &self.layout_index)
    }

    pub(crate) fn semantic_rank_of(&self, occurrence: TokenOccurrenceId) -> Option<usize> {
        self.semantic.rank_of_id(occurrence.0, &self.semantic_index)
    }

    pub(crate) fn lexical_start(&self, rank: usize) -> usize {
        usize::try_from(self.layout.metric_before(rank).source_bytes)
            .expect("lexer source offset exceeds usize")
    }

    pub(crate) fn lexical_at(&self, rank: usize) -> Option<&LexicalOccurrence<R>> {
        self.lexical.get(rank)
    }

    pub(crate) fn semantic_at(&self, rank: usize) -> Option<&ParseTokenRef> {
        self.semantic.get(rank)
    }

    pub(crate) fn state_before_rank(&self, rank: usize) -> LexerState<R> {
        let offset = self.lexical_start(rank);
        match rank
            .checked_sub(1)
            .and_then(|previous| self.lexical.get(previous))
        {
            Some(previous) => previous.state_after.materialize(offset),
            None => self.initial_state.materialize(offset),
        }
    }

    pub(crate) fn lexical_token(&self, occurrence: TokenOccurrenceId) -> Option<LexToken<R>>
    where
        R: Clone,
    {
        let rank = self.lexical_rank_of(occurrence)?;
        self.lexical
            .get(rank)?
            .materialize(self.lexical_start(rank))
    }

    pub(crate) fn token_data_at_semantic_rank(&self, rank: usize) -> Option<TokenData> {
        let token = self.semantic.get(rank)?;
        let layout_rank = self.lexical_rank_of(token.occurrence)?;
        let layout = self.layout.get(layout_rank)?;
        Some(TokenData {
            id: usize::try_from(token.occurrence.0).ok()?,
            terminal: token.terminal,
            start: self.lexical_start(layout_rank),
            length: layout.byte_len as usize,
            column: usize::try_from(token.occurrence.0).ok()?,
            fingerprint: layout.fingerprint.0,
        })
    }

    /// Explicit snapshot-only projection. Command paths must use cursors and
    /// patch roots rather than this materialization.
    pub(crate) fn token_data_snapshot(&self) -> Vec<TokenData> {
        let mut data = Vec::with_capacity(self.semantic.len().saturating_add(1));
        for rank in 0..self.semantic.len() {
            if let Some(token) = self.token_data_at_semantic_rank(rank) {
                data.push(token);
            }
        }
        data.push(TokenData {
            id: crate::framework::lex::token::SYNTHETIC_EOF_ID,
            terminal: None,
            start: self.source.len_bytes(),
            length: 0,
            column: crate::framework::lex::token::SYNTHETIC_EOF_ID,
            fingerprint: crate::framework::parse::identity::eof_fingerprint(),
        });
        data
    }

    pub(crate) fn tokens_in_span(&self, span: &crate::utils::Span) -> Vec<LexToken<R>>
    where
        R: Clone,
    {
        let mut out = Vec::new();
        let start_rank = self.lexical.lexical_rank_at_byte(span.range.start() as u64);
        for rank in start_rank..self.lexical.len() {
            let start = self.lexical_start(rank);
            let Some(token) = self.lexical.get(rank) else {
                break;
            };
            let end = start.saturating_add(token.byte_len as usize);
            if start >= span.range.end() {
                break;
            }
            if end > span.range.start() {
                if let Some(token) = token.materialize(start) {
                    out.push(token);
                }
            }
        }
        out
    }

    pub(crate) fn lexical_range(
        &self,
        range: Range<usize>,
    ) -> impl Iterator<Item = &LexicalOccurrence<R>> {
        self.lexical.iter_range(range)
    }
}

/// One parser-visible semantic token, used only by explicit snapshot façades.
#[derive(Clone, Debug)]
pub struct SemanticToken<R> {
    pub occurrence: TokenOccurrenceId,
    pub terminal_id: usize,
    pub fingerprint: TokenFingerprint,
    pub value: Arc<R>,
}

/// One exact structural splice between semantic token roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeSplice {
    pub before: Option<TokenOccurrenceId>,
    pub removed: Arc<[TokenOccurrenceId]>,
    pub inserted: Arc<[TokenOccurrenceId]>,
    pub after: Option<TokenOccurrenceId>,
}

/// Exact lexer patch.  The three key sets are sorted, pairwise disjoint, and
/// contain only semantic facts whose committed value changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenPatch {
    pub old_structure: TokenStructureRevisionId,
    pub new_structure: TokenStructureRevisionId,
    pub order_splices: Arc<[TapeSplice]>,
    pub inserted: Arc<[TokenOccurrenceId]>,
    pub updated: Arc<[TokenOccurrenceId]>,
    pub removed: Arc<[TokenOccurrenceId]>,
}

impl TokenPatch {
    pub(crate) fn unchanged(structure: TokenStructureRevisionId) -> Self {
        Self {
            old_structure: structure,
            new_structure: structure,
            order_splices: Arc::from([]),
            inserted: Arc::from([]),
            updated: Arc::from([]),
            removed: Arc::from([]),
        }
    }

    pub fn structure_unchanged(&self) -> bool {
        self.order_splices.is_empty()
            && self.inserted.is_empty()
            && self.removed.is_empty()
            && self.old_structure == self.new_structure
    }

    pub fn is_empty(&self) -> bool {
        self.structure_unchanged() && self.updated.is_empty()
    }
}

/// Parser-facing O(1) semantic root handle. Equality tracks parser-visible
/// payload and structure changes while ignoring layout-only edits.
#[derive(Clone, Debug)]
pub struct SemanticTokenDocument<R: LexerRoot + std::fmt::Debug> {
    pub document_uri: String,
    pub document_id: u64,
    pub revision: SemanticRevisionId,
    pub structure_revision: TokenStructureRevisionId,
    pub patch: TokenPatch,
    pub(crate) document: Arc<LexicalDocument<R>>,
}

impl<R: LexerRoot + std::fmt::Debug> PartialEq for SemanticTokenDocument<R> {
    fn eq(&self, other: &Self) -> bool {
        self.document_id == other.document_id && self.revision == other.revision
    }
}
impl<R: LexerRoot + std::fmt::Debug> Eq for SemanticTokenDocument<R> {}

impl<R: LexerRoot + std::fmt::Debug> SemanticTokenDocument<R> {
    pub(crate) fn parse_token(&self, rank: usize) -> Option<&ParseTokenRef> {
        self.document.semantic_at(rank)
    }

    pub(crate) fn token_count(&self) -> usize {
        self.document.semantic.len()
    }

    pub(crate) fn token_data_at(&self, rank: usize) -> Option<TokenData> {
        self.document.token_data_at_semantic_rank(rank)
    }

    pub(crate) fn source(&self) -> Arc<ropey::Rope> {
        Arc::clone(&self.document.source)
    }

    /// Pointer-sharing probe for oracle tests (plan §20.3): the semantic
    /// tape root of a later revision shares at least one immutable node
    /// with an earlier revision exactly when the edit reattached an
    /// unchanged suffix by pointer instead of rebuilding it.
    #[doc(hidden)]
    pub fn semantic_tape_shares_subtree_with(&self, other: &Self) -> bool {
        self.document
            .semantic
            .shares_subtree(&other.document.semantic)
    }
}

/// Layout-facing O(1) lexical root handle.  This changes for every source
/// layout revision and is intentionally separate from parser structure.
#[derive(Clone, Debug)]
pub struct TokenLayoutDocument<R: LexerRoot + std::fmt::Debug> {
    pub document_uri: String,
    pub document_id: u64,
    pub layout_revision: LayoutRevisionId,
    pub(crate) document: Arc<LexicalDocument<R>>,
}

impl<R: LexerRoot + std::fmt::Debug> PartialEq for TokenLayoutDocument<R> {
    fn eq(&self, other: &Self) -> bool {
        self.document_id == other.document_id && self.layout_revision == other.layout_revision
    }
}
impl<R: LexerRoot + std::fmt::Debug> Eq for TokenLayoutDocument<R> {}

#[view]
pub struct SemanticTokenDocuments<R: LexerRoot + std::fmt::Debug>(
    Map<String, SemanticTokenDocument<R>>,
);

#[view]
pub struct TokenLayoutDocuments<R: LexerRoot + std::fmt::Debug>(
    Map<String, TokenLayoutDocument<R>>,
);

/// Compatibility name for one parser-facing semantic root publication.
pub type LexedDocument<R: LexerRoot + std::fmt::Debug> = SemanticTokenDocument<R>;
/// Compatibility name retained for consumers that imported the previous
/// semantic-document view. It no longer denotes a whole token vector.
pub type LexedDocuments<R: LexerRoot + std::fmt::Debug> = SemanticTokenDocuments<R>;

/// Stable document identity derived only from the document URI.  The FNV-1a
/// seed is explicit rather than process-random.
pub(crate) fn document_id(uri: &str) -> StableDocumentId {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in uri.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    StableDocumentId(hash)
}

/// Identifies one granular token fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenFactId {
    Source(TokenOccurrenceId),
    Synthetic(SyntheticTokenId),
}

/// Monotonic parser-owned synthetic token identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyntheticTokenId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenFactKey {
    pub document_id: u64,
    pub token: TokenFactId,
}

pub struct TokenFact<R> {
    pub terminal_id: usize,
    pub fingerprint: TokenFingerprint,
    pub value: Arc<R>,
}

impl<R: std::fmt::Debug> std::fmt::Debug for TokenFact<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenFact")
            .field("terminal_id", &self.terminal_id)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl<R> Clone for TokenFact<R> {
    fn clone(&self) -> Self {
        Self {
            terminal_id: self.terminal_id,
            fingerprint: self.fingerprint,
            value: Arc::clone(&self.value),
        }
    }
}

impl<R: PartialEq> PartialEq for TokenFact<R> {
    fn eq(&self, other: &Self) -> bool {
        self.terminal_id == other.terminal_id
            && self.fingerprint == other.fingerprint
            && self.value == other.value
    }
}
impl<R: PartialEq> Eq for TokenFact<R> {}

#[view]
pub struct TokenFacts<R: Clone + PartialEq + Send + Sync + std::fmt::Debug + 'static>(
    Map<TokenFactKey, TokenFact<R>>,
);

/// Observes the exact parser-owned token value fact.  Syntax trees retain only
/// stable handles, so same-terminal value edits schedule downstream consumers
/// without re-running structural parsing.
pub fn observe_token<R>(token: AstToken<R>) -> Result<Option<Arc<R>>>
where
    R: LexerRoot + Clone + std::fmt::Debug,
{
    let key = TokenFactKey {
        document_id: token.document_id(),
        token: TokenFactId::Source(TokenOccurrenceId(token.occurrence() as u64)),
    };
    Ok(observe_view::<TokenFacts<R>>()?
        .get(&key)?
        .map(|fact| Arc::clone(&fact.value)))
}
