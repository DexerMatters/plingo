use std::{any::Any, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

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
    // Must agree with the lexer's per-document fact domain (FNV-1a over the
    // URI string). Divided hashers here would make TokenFacts lookups miss.
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

    pub(crate) fn from_uri(id: AstId, uri: &Uri<String>) -> Self {
        Self::new(id, document_key(uri))
    }

    pub(crate) const fn raw_id(self) -> AstId {
        self.id
    }

    /// Stable parser-record identity, independent of current source offsets.
    pub fn identity(self) -> u64 {
        self.id as u64
    }

    pub(crate) const fn document_id(self) -> u64 {
        self.document
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

#[derive(Clone)]
pub struct AstArena {
    records: Vec<AstRecord>,
    uri: Uri<String>,
}

impl AstArena {
    pub fn new(uri: Uri<String>) -> Self {
        Self {
            records: Vec::new(),
            uri,
        }
    }

    pub(crate) fn document_id(&self) -> u64 {
        document_key(&self.uri)
    }

    /// Number of live AST records (work instrumentation).
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Allocates an AST value together with the source extent determined by
    /// the shift/reduction that created it.
    pub fn insert<T>(&mut self, value: T, extent: AnchoredSpan) -> AstBox<T>
    where
        T: Send + Sync + 'static,
    {
        let id = self.records.len();
        self.records.push(AstRecord {
            value: Arc::new(value),
            owner: None,
            parent: None,
            extent,
        });
        AstBox::from_uri(id, &self.uri)
    }

    pub fn get<T: 'static>(&self, node: AstBox<T>) -> Option<&T> {
        self.records.get(node.raw_id())?.value.downcast_ref()
    }

    /// Resolves one stable arena record by raw record identity. This is the
    /// parser-to-tree publication seam; callers still need a typed AST value
    /// before exposing it publicly.
    #[doc(hidden)]
    pub fn get_id<T: 'static>(&self, id: AstId) -> Option<&T> {
        self.records.get(id)?.value.downcast_ref()
    }

    pub(crate) fn expect<T: 'static>(&self, id: AstId) -> Option<AstBox<T>> {
        self.records.get(id)?.value.downcast_ref::<T>()?;
        Some(AstBox::from_uri(id, &self.uri))
    }

    pub(crate) fn cloned<T>(&self, id: AstId) -> Option<T>
    where
        T: Clone + 'static,
    {
        self.records.get(id)?.value.downcast_ref::<T>().cloned()
    }

    pub(crate) fn cloned_erased(&self, id: AstId) -> Option<Arc<dyn Any + Send + Sync>> {
        self.records.get(id).map(|record| Arc::clone(&record.value))
    }

    pub(crate) fn extent_of_id(&self, id: AstId) -> Option<AnchoredSpan> {
        self.records.get(id).map(|record| record.extent)
    }

    /// Returns the anchored extent of an opaque AST reference.
    #[doc(hidden)]
    pub fn extent<T>(&self, node: AstBox<T>) -> Option<AnchoredSpan> {
        self.extent_of_id(node.raw_id())
    }

    pub(crate) fn type_of(&self, id: AstId) -> Option<std::any::TypeId> {
        self.records
            .get(id)
            .map(|record| record.value.as_ref().type_id())
    }

    pub(crate) fn bind_product(&mut self, id: AstId, product: ProductId) {
        if let Some(record) = self.records.get_mut(id) {
            record.owner = Some(product);
        }
    }

    pub(crate) fn set_parent(&mut self, id: AstId, parent: Option<AstId>) {
        if let Some(record) = self.records.get_mut(id) {
            record.parent = parent;
        }
    }

    #[doc(hidden)]
    pub fn parent_of(&self, id: AstId) -> Option<AstId> {
        self.records.get(id).and_then(|record| record.parent)
    }

    pub(crate) fn product_of(&self, id: AstId) -> Option<ProductId> {
        self.records.get(id).and_then(|record| record.owner)
    }
}
