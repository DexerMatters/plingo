use std::any::TypeId;

use crate::component::parse::{
    data::{
        ast::{AstBox, AstId, TokenEntryId},
        green::GreenId,
    },
    identity::TokenFingerprint,
};

pub type ProductId = usize;

#[derive(Clone)]
pub struct Product {
    pub green: GreenId,
    pub data: ProductData,
}

impl Product {
    pub fn new(green: GreenId, data: ProductData) -> Self {
        Self { green, data }
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
