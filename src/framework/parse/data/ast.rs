use std::{any::Any, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use crate::reactive::{pathwork::StructureKind, store::RadixMap};

use super::product::ProductId;

pub(crate) type AstId = usize;
pub(crate) type TokenEntryId = usize;

/// A source extent expressed in stable token-occurrence coordinates.
///
/// The lexer may shift a token's bytes while preserving its occurrence ID. A
/// parser product therefore stores these anchors rather than stale byte
/// offsets. `end_at_token_end` distinguishes a real one-token range from a
/// zero-width point before that token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnchoredSpan {
    pub start: usize,
    pub end: usize,
    pub end_at_token_end: bool,
}

impl AnchoredSpan {
    pub const fn token(occurrence: usize) -> Self {
        Self {
            start: occurrence,
            end: occurrence,
            end_at_token_end: true,
        }
    }

    pub const fn point(occurrence: usize) -> Self {
        Self {
            start: occurrence,
            end: occurrence,
            end_at_token_end: false,
        }
    }

    pub fn cover(children: impl IntoIterator<Item = Self>, fallback: usize) -> Self {
        let mut children = children.into_iter();
        let Some(first) = children.next() else {
            return Self::point(fallback);
        };
        children.fold(first, |extent, child| Self {
            start: extent.start.min(child.start),
            end: extent.end.max(child.end),
            end_at_token_end: if child.end >= extent.end {
                child.end_at_token_end
            } else {
                extent.end_at_token_end
            },
        })
    }
}

pub struct AstBox<T> {
    id: AstId,
    document: u64,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for AstBox<T> {}
impl<T> Clone for AstBox<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> PartialEq for AstBox<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.document == other.document
    }
}

impl<T> Eq for AstBox<T> {}

impl<T> std::hash::Hash for AstBox<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.document.hash(state);
    }
}

impl<T> std::fmt::Debug for AstBox<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AstBox")
    }
}

pub(crate) fn document_key(uri: &Uri<String>) -> u64 {
    crate::framework::lex::lexed::document_id(uri.to_string().as_str()).0
}

impl<T> AstBox<T> {
    pub(crate) const fn new(id: AstId, document: u64) -> Self {
        Self {
            id,
            document,
            _marker: PhantomData,
        }
    }

    pub(crate) const fn raw_id(self) -> AstId {
        self.id
    }

    /// Stable parser-record identity, independent of current source offsets.
    pub fn identity(self) -> u64 {
        self.id as u64
    }

    /// Reference form of [`AstBox::identity`] for pattern-bound values.
    pub fn identity_ref(&self) -> u64 {
        self.id as u64
    }

    pub(crate) const fn document_id(self) -> u64 {
        self.document
    }
}
impl<T> crate::reactive::abstract_tree::SyntaxChild for AstBox<T> {
    fn __syntax_child_id(&self) -> u64 {
        self.id as u64
    }
}

pub struct AstToken<T> {
    id: TokenEntryId,
    occurrence: usize,
    document: u64,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for AstToken<T> {}
impl<T> Clone for AstToken<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> PartialEq for AstToken<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.occurrence == other.occurrence
            && self.document == other.document
    }
}

impl<T> Eq for AstToken<T> {}

impl<T> std::hash::Hash for AstToken<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.occurrence.hash(state);
        self.document.hash(state);
    }
}

impl<T> std::fmt::Debug for AstToken<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AstToken")
    }
}

impl<T> AstToken<T> {
    pub(crate) const fn new(id: TokenEntryId, occurrence: usize, document: u64) -> Self {
        Self {
            id,
            occurrence,
            document,
            _marker: PhantomData,
        }
    }

    pub(crate) const fn raw_id(self) -> TokenEntryId {
        self.id
    }

    pub(crate) const fn occurrence(self) -> usize {
        self.occurrence
    }

    pub(crate) const fn document_id(self) -> u64 {
        self.document
    }
}
#[derive(Clone)]
struct AstRecord {
    value: Arc<dyn Any + Send + Sync>,
    owner: Option<ProductId>,
    parent: Option<AstId>,
    extent: AnchoredSpan,
}

#[derive(Clone, Copy)]
struct AstMetadata {
    owner: Option<ProductId>,
    parent: Option<AstId>,
}

#[derive(Clone)]
pub struct AstArena {
    /// Immutable AST records grouped by parser transaction generation.
    chunks: Arc<Vec<Arc<[AstRecord]>>>,
    chunk_starts: Arc<Vec<usize>>,
    tail: Vec<AstRecord>,
    total_len: usize,
    /// Parent/product bindings are sparse persistent updates. A command
    /// copies only the radix paths it changes, not all prior records.
    metadata: RadixMap<AstMetadata>,
    document: u64,
}

impl AstArena {
    pub fn new(uri: Uri<String>) -> Self {
        Self::with_document(document_key(&uri))
    }

    pub(crate) fn with_document(document: u64) -> Self {
        Self {
            chunks: Arc::new(Vec::new()),
            chunk_starts: Arc::new(Vec::new()),
            tail: Vec::new(),
            total_len: 0,
            metadata: RadixMap::with_kind(StructureKind::ParserRadix),
            document,
        }
    }

    pub(crate) fn document_id(&self) -> u64 {
        self.document
    }

    /// Number of live AST records (work instrumentation).
    pub(crate) fn len(&self) -> usize {
        self.total_len
    }

    fn base_record(&self, id: AstId) -> Option<&AstRecord> {
        let sealed_len = self.total_len.saturating_sub(self.tail.len());
        if id >= sealed_len {
            return self.tail.get(id - sealed_len);
        }
        let index = self
            .chunk_starts
            .partition_point(|&start| start <= id)
            .checked_sub(1)?;
        let start = self.chunk_starts[index];
        self.chunks[index].get(id - start)
    }

    fn metadata(&self, id: AstId) -> Option<AstMetadata> {
        let record = self.base_record(id)?;
        Some(
            self.metadata
                .get(id as u64)
                .copied()
                .unwrap_or(AstMetadata {
                    owner: record.owner,
                    parent: record.parent,
                }),
        )
    }

    /// Allocates an AST value together with the source extent determined by
    /// the shift/reduction that created it.
    pub fn insert<T>(&mut self, value: T, extent: AnchoredSpan) -> AstBox<T>
    where
        T: Send + Sync + 'static,
    {
        let id = self.total_len;
        self.tail.push(AstRecord {
            value: Arc::new(value),
            owner: None,
            parent: None,
            extent,
        });
        self.total_len = self.total_len.saturating_add(1);
        AstBox::new(id, self.document)
    }

    pub fn get<T: 'static>(&self, node: AstBox<T>) -> Option<&T> {
        self.base_record(node.raw_id())?.value.downcast_ref()
    }

    /// Returns one arena value without exposing parser storage to generated
    /// code.  The framework publication adapter performs the erased downcast.
    pub(crate) fn erased(&self, id: AstId) -> Option<&(dyn Any + Send + Sync)> {
        self.base_record(id).map(|record| record.value.as_ref())
    }

    pub(crate) fn contains_id(&self, id: AstId) -> bool {
        self.base_record(id).is_some()
    }

    /// Resolves a raw arena record for parser-internal typed access.
    #[doc(hidden)]
    pub fn get_id<T: 'static>(&self, id: AstId) -> Option<&T> {
        self.base_record(id)?.value.downcast_ref()
    }

    pub(crate) fn expect<T: 'static>(&self, id: AstId) -> Option<AstBox<T>> {
        self.base_record(id)?.value.downcast_ref::<T>()?;
        Some(AstBox::new(id, self.document))
    }

    pub(crate) fn cloned<T>(&self, id: AstId) -> Option<T>
    where
        T: Clone + 'static,
    {
        self.base_record(id)?.value.downcast_ref::<T>().cloned()
    }

    pub(crate) fn cloned_erased(&self, id: AstId) -> Option<Arc<dyn Any + Send + Sync>> {
        self.base_record(id).map(|record| Arc::clone(&record.value))
    }

    pub(crate) fn extent_of_id(&self, id: AstId) -> Option<AnchoredSpan> {
        self.base_record(id).map(|record| record.extent)
    }

    /// Returns the anchored extent of an opaque AST reference.
    #[doc(hidden)]
    pub fn extent<T>(&self, node: AstBox<T>) -> Option<AnchoredSpan> {
        self.extent_of_id(node.raw_id())
    }

    pub(crate) fn type_of(&self, id: AstId) -> Option<std::any::TypeId> {
        self.base_record(id)
            .map(|record| record.value.as_ref().type_id())
    }

    pub(crate) fn bind_product(&mut self, id: AstId, product: ProductId) {
        let Some(mut metadata) = self.metadata(id) else {
            return;
        };
        metadata.owner = Some(product);
        self.metadata.insert(id as u64, metadata);
    }

    pub(crate) fn set_parent(&mut self, id: AstId, parent: Option<AstId>) {
        let Some(mut metadata) = self.metadata(id) else {
            return;
        };
        metadata.parent = parent;
        self.metadata.insert(id as u64, metadata);
    }

    /// Publishes the current append-only record generation. Metadata remains a
    /// persistent radix root and is consequently shared by old snapshots.
    pub(crate) fn seal_generation(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        let start = self.total_len - self.tail.len();
        let records: Arc<[AstRecord]> = std::mem::take(&mut self.tail).into();
        let mut chunks = self.chunks.as_ref().clone();
        chunks.push(records);
        self.chunks = Arc::new(chunks);
        let mut starts = self.chunk_starts.as_ref().clone();
        starts.push(start);
        self.chunk_starts = Arc::new(starts);
    }

    #[doc(hidden)]
    pub fn parent_of(&self, id: AstId) -> Option<AstId> {
        self.metadata(id)?.parent
    }

    pub(crate) fn product_of(&self, id: AstId) -> Option<ProductId> {
        self.metadata(id)?.owner
    }
}
