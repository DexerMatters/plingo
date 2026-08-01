use std::{any::Any, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use super::product::ProductId;
use crate::component::parse::AstKey;

pub type AstId = usize;
pub type TokenEntryId = usize;

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

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct AstBox<T> {
    pub id: AstId,
    pub uri: Uri<&'static str>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for AstBox<T> {}
impl<T> Clone for AstBox<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> AstBox<T> {
    pub fn new(id: AstId, uri: Uri<&'static str>) -> Self {
        Self {
            id,
            uri,
            _marker: PhantomData,
        }
    }

    /// Erases the AST value type while retaining this artifact's stable key.
    pub fn key(self) -> AstKey {
        AstKey {
            uri: self.uri,
            id: self.id,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct AstToken<T> {
    pub id: TokenEntryId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for AstToken<T> {}
impl<T> Clone for AstToken<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> AstToken<T> {
    pub fn new(id: TokenEntryId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }
}

#[derive(Clone)]
struct AstRecord {
    value: Arc<dyn Any + Send + Sync>,
    owner: Option<ProductId>,
    extent: AnchoredSpan,
}

#[derive(Clone)]
pub struct AstArena {
    records: Vec<AstRecord>,
    uri: Uri<&'static str>,
}

impl AstArena {
    pub fn new(uri: Uri<&'static str>) -> Self {
        Self {
            records: Vec::new(),
            uri,
        }
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
            extent,
        });
        AstBox::new(id, self.uri)
    }

    pub fn get<T: 'static>(&self, node: AstBox<T>) -> Option<&T> {
        self.records.get(node.id)?.value.downcast_ref()
    }

    pub fn expect<T: 'static>(&self, id: AstId) -> Option<AstBox<T>> {
        self.records.get(id)?.value.downcast_ref::<T>()?;
        Some(AstBox::new(id, self.uri))
    }

    pub fn cloned<T>(&self, id: AstId) -> Option<T>
    where
        T: Clone + 'static,
    {
        self.records.get(id)?.value.downcast_ref::<T>().cloned()
    }

    pub(crate) fn cloned_erased(&self, id: AstId) -> Option<Arc<dyn Any + Send + Sync>> {
        self.records.get(id).map(|record| Arc::clone(&record.value))
    }

    pub(crate) fn extent_of(&self, id: AstId) -> Option<AnchoredSpan> {
        self.records.get(id).map(|record| record.extent)
    }

    pub(crate) fn type_of(&self, id: AstId) -> Option<std::any::TypeId> {
        self.records
            .get(id)
            .map(|record| record.value.as_ref().type_id())
    }

    pub fn bind_product(&mut self, id: AstId, product: ProductId) {
        if let Some(record) = self.records.get_mut(id) {
            record.owner = Some(product);
        }
    }

    pub fn product_of(&self, id: AstId) -> Option<ProductId> {
        self.records.get(id).and_then(|record| record.owner)
    }
}
