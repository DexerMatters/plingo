use std::{any::Any, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use super::product::ProductId;

pub type AstId = usize;
pub type TokenEntryId = usize;

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
pub struct AstArena {
    values: Vec<Arc<dyn Any + Send + Sync>>,
    owners: Vec<Option<ProductId>>,
    uri: Uri<&'static str>,
}

impl AstArena {
    pub fn new(uri: Uri<&'static str>) -> Self {
        Self {
            values: Vec::new(),
            owners: Vec::new(),
            uri,
        }
    }

    pub fn insert<T>(&mut self, value: T) -> AstBox<T>
    where
        T: Send + Sync + 'static,
    {
        let id = self.values.len();
        self.values.push(Arc::new(value));
        self.owners.push(None);
        AstBox::new(id, self.uri)
    }

    pub fn get<T: 'static>(&self, node: AstBox<T>) -> Option<&T> {
        self.values.get(node.id)?.downcast_ref()
    }

    pub fn expect<T: 'static>(&self, id: AstId) -> Option<AstBox<T>> {
        self.values.get(id)?.downcast_ref::<T>()?;
        Some(AstBox::new(id, self.uri))
    }

    pub fn cloned<T>(&self, id: AstId) -> Option<T>
    where
        T: Clone + 'static,
    {
        self.values.get(id)?.downcast_ref::<T>().cloned()
    }

    pub fn bind_product(&mut self, id: AstId, product: ProductId) {
        if let Some(owner) = self.owners.get_mut(id) {
            *owner = Some(product);
        }
    }

    pub fn product_of(&self, id: AstId) -> Option<ProductId> {
        self.owners.get(id).copied().flatten()
    }
}
