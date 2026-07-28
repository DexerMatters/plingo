use std::{any::TypeId, sync::Arc};

use crate::component::parse::{
    data::{
        ast::{AnchoredSpan, AstBox, AstId, TokenEntryId},
        green::GreenId,
    },
    identity::TokenFingerprint,
};

pub type ProductId = usize;

/// Parser-computed metadata shared by every projection of a committed parse.
/// Both fields are assembled at shift/reduction time; snapshot publication does
/// not need to walk child products to rediscover AST reachability or spans.
#[derive(Clone)]
pub struct Product {
    pub green: GreenId,
    pub data: ProductData,
    pub extent: AnchoredSpan,
    pub ast_ids: Arc<[AstId]>,
}

impl Product {
    pub fn new(green: GreenId, data: ProductData) -> Self {
        Self {
            green,
            data,
            extent: AnchoredSpan::point(0),
            ast_ids: Arc::from([]),
        }
    }

    pub fn with_metadata(mut self, extent: AnchoredSpan, ast_ids: impl Into<Arc<[AstId]>>) -> Self {
        self.extent = extent;
        self.ast_ids = ast_ids.into();
        self
    }

    pub fn error(green: GreenId) -> Self {
        Self::new(
            green,
            ProductData::Error {
                children: Vec::new(),
            },
        )
    }

    pub fn error_with_children(green: GreenId, children: Vec<ProductId>) -> Self {
        Self::new(green, ProductData::Error { children })
    }

    pub fn token(green: GreenId, entry: TokenEntryId, fingerprint: TokenFingerprint) -> Self {
        Self::new(
            green,
            ProductData::Token {
                entry,
                fingerprint,
                ast: None,
                ty: TypeId::of::<()>(),
            },
        )
    }

    pub fn typed_token<T: 'static>(
        green: GreenId,
        entry: TokenEntryId,
        fingerprint: TokenFingerprint,
        ast: AstBox<T>,
    ) -> Self {
        Self::new(
            green,
            ProductData::Token {
                entry,
                fingerprint,
                ast: Some(ast.id),
                ty: TypeId::of::<T>(),
            },
        )
    }

    pub fn node<T: 'static>(green: GreenId, ast: AstBox<T>, children: Vec<ProductId>) -> Self {
        Self::new(
            green,
            ProductData::Node {
                ast: ast.id,
                ty: TypeId::of::<T>(),
                children,
            },
        )
    }
}

#[derive(Debug, Clone)]
pub enum ProductData {
    Error {
        children: Vec<ProductId>,
    },
    Token {
        entry: TokenEntryId,
        fingerprint: TokenFingerprint,
        ast: Option<AstId>,
        ty: TypeId,
    },
    Node {
        ast: AstId,
        ty: TypeId,
        children: Vec<ProductId>,
    },
}

impl ProductData {
    pub fn token_fingerprint(&self) -> Option<TokenFingerprint> {
        match self {
            Self::Token { fingerprint, .. } => Some(*fingerprint),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct ProductArena {
    pub products: Vec<Product>,
}

impl Default for ProductArena {
    fn default() -> Self {
        Self::new()
    }
}

impl ProductArena {
    pub fn new() -> Self {
        Self {
            products: Vec::new(),
        }
    }

    pub fn insert(&mut self, product: Product) -> ProductId {
        let id = self.products.len();
        self.products.push(product);
        id
    }

    pub fn get(&self, id: ProductId) -> Option<&Product> {
        self.products.get(id)
    }
}
