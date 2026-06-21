use std::{
    any::TypeId,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

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
    semantic_hash: u64,
}

impl Product {
    pub fn new(green: GreenId, data: ProductData) -> Self {
        Self {
            green,
            data,
            semantic_hash: 0,
        }
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

    pub(crate) fn semantic_hash(&self) -> u64 {
        self.semantic_hash
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

    pub fn insert(&mut self, mut product: Product) -> ProductId {
        let id = self.products.len();
        product.semantic_hash = product_semantic_hash(&product, &self.products);
        self.products.push(product);
        id
    }

    pub fn get(&self, id: ProductId) -> Option<&Product> {
        self.products.get(id)
    }
}

fn product_semantic_hash(product: &Product, products: &[Product]) -> u64 {
    match &product.data {
        ProductData::Error { children } => hash_value(&(
            "err",
            product.green,
            children
                .iter()
                .map(|&child| {
                    products.get(child).map_or_else(
                        || hash_value(&("missing-child-product", child)),
                        Product::semantic_hash,
                    )
                })
                .collect::<Vec<_>>(),
        )),
        ProductData::Token {
            fingerprint, ty, ..
        } => hash_value(&("tok", product.green, fingerprint, ty)),
        ProductData::Node { children, ty, .. } => hash_value(&(
            "node",
            product.green,
            ty,
            children
                .iter()
                .map(|&child| {
                    products.get(child).map_or_else(
                        || hash_value(&("missing-child-product", child)),
                        Product::semantic_hash,
                    )
                })
                .collect::<Vec<_>>(),
        )),
    }
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
