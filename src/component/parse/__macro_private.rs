use std::{any::TypeId, sync::Arc};

use crate::component::parse::{
    data::{
        ast::{AstBox, AstToken, TokenEntryId},
        green::{ParseErrorInfo, TreeData},
        product::{ProductData, ProductId},
    },
    grammar::{
        BuildCx, BuildError, BuildFn, GrammarBuilder, Precedence, ProductionId, Symbol, TerminalId,
    },
};
use crate::utils::Either;

pub fn begin_non_terminal(grammar: &mut GrammarBuilder, label: &'static str) -> (Symbol, bool) {
    grammar.begin_non_terminal(label)
}

pub fn begin_internal_non_terminal(grammar: &mut GrammarBuilder, label: &'static str) -> Symbol {
    grammar.begin_internal_non_terminal(label)
}

pub fn terminal_symbol(
    grammar: &mut GrammarBuilder,
    label: &'static str,
    id: TerminalId,
    precedence: Option<Precedence>,
) -> Symbol {
    grammar.terminal_symbol(label, id, precedence)
}

pub fn rule(
    grammar: &mut GrammarBuilder,
    label: &'static str,
    lhs: Symbol,
    rhs: impl IntoIterator<Item = Symbol>,
    precedence: Option<Precedence>,
    build: Option<BuildFn>,
) {
    grammar.rule(label, lhs, rhs, precedence, build)
}

pub trait NonTerminalSpec: Sized + Send + Sync + 'static {
    fn register(grammar: &mut GrammarBuilder) -> Symbol;
}

pub trait TokenVariantSpec {
    fn register_terminal(grammar: &mut GrammarBuilder, variant: &'static str) -> Symbol;
}

pub trait BuildField: Sized {
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError>;
}

pub trait TokenField: Sized {
    fn from_token_entry(cx: &BuildCx<'_>, entry: TokenEntryId) -> Result<Self, BuildError>;
}

/// Persistent repetition values avoid copying an already-built prefix on each
/// grammar reduction; public `Vec` values are materialized only at extraction.
pub struct Repeat<T>(Arc<RepeatNode<T>>);

enum RepeatNode<T> {
    Empty,
    One(T),
    Concat(Repeat<T>, Repeat<T>),
}

impl<T> Clone for Repeat<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn repeat_empty<T>() -> Repeat<T> {
    Repeat(Arc::new(RepeatNode::Empty))
}

pub fn repeat_push<T>(values: Repeat<T>, value: T) -> Repeat<T> {
    Repeat(Arc::new(RepeatNode::Concat(
        values,
        Repeat(Arc::new(RepeatNode::One(value))),
    )))
}

pub fn repeat_prepend<T>(value: T, values: Repeat<T>) -> Repeat<T> {
    Repeat(Arc::new(RepeatNode::Concat(
        Repeat(Arc::new(RepeatNode::One(value))),
        values,
    )))
}

pub fn repeat_from_product<T>(cx: &BuildCx<'_>, product: ProductId) -> Result<Vec<T>, BuildError>
where
    T: Clone + Send + Sync + 'static,
{
    let values = cx.expect_value::<Repeat<T>>(product)?;
    let mut result = Vec::new();
    let mut pending = vec![values.0.as_ref()];
    while let Some(node) = pending.pop() {
        match node {
            RepeatNode::Empty => {}
            RepeatNode::One(value) => result.push(value.clone()),
            RepeatNode::Concat(left, right) => {
                pending.push(right.0.as_ref());
                pending.push(left.0.as_ref());
            }
        }
    }
    Ok(result)
}

impl<T> BuildField for AstBox<T>
where
    T: 'static,
{
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        cx.expect_node(product)
    }
}

impl BuildField for TokenEntryId {
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        cx.expect_token(product)?;
        Ok(product)
    }
}

impl<T: 'static> BuildField for AstToken<T> {
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        let p = cx
            .products
            .get(product)
            .ok_or(BuildError::MissingProduct(product))?;
        match &p.data {
            ProductData::Token { entry, .. } => Ok(AstToken::new(*entry)),
            _ => Err(BuildError::ExpectedToken { product }),
        }
    }
}

impl<T: 'static> TokenField for AstToken<T> {
    fn from_token_entry(cx: &BuildCx<'_>, entry: TokenEntryId) -> Result<Self, BuildError> {
        match &cx
            .products
            .get(entry)
            .ok_or(BuildError::MissingProduct(entry))?
            .data
        {
            ProductData::Token {
                entry: tok_entry, ..
            } => Ok(AstToken::new(*tok_entry)),
            _ => Err(BuildError::ExpectedToken { product: entry }),
        }
    }
}

impl BuildField for ParseErrorInfo {
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        let p = cx
            .products
            .get(product)
            .ok_or(BuildError::MissingProduct(product))?;
        if !matches!(p.data, ProductData::Error { .. }) {
            return Err(BuildError::TypeMismatch { product });
        }
        let tree = cx
            .trees
            .get(p.green)
            .ok_or(BuildError::MissingProduct(product))?;
        match &tree.data {
            TreeData::Error {
                kind,
                node,
                unexpected,
                expected,
                recovered,
                location,
                ..
            } => Ok(ParseErrorInfo {
                kind: kind.clone(),
                node: *node,
                length: tree.length,
                unexpected: *unexpected,
                expected: *expected,
                recovered: *recovered,
                location: *location,
            }),
            _ => Err(BuildError::TypeMismatch { product }),
        }
    }
}

impl<T> BuildField for Option<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        cx.expect_value(product)
    }
}

impl<T> BuildField for Vec<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        match cx.expect_value(product) {
            Ok(value) => Ok(value),
            Err(BuildError::TypeMismatch { .. }) => repeat_from_product(cx, product),
            Err(error) => Err(error),
        }
    }
}

impl<L, R> BuildField for Either<L, R>
where
    L: Clone + Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    fn from_product(cx: &BuildCx<'_>, product: ProductId) -> Result<Self, BuildError> {
        cx.expect_value(product)
    }
}

pub fn production_child(children: &[ProductId], index: usize) -> Result<ProductId, BuildError> {
    children
        .get(index)
        .copied()
        .ok_or(BuildError::MissingProduct(index))
}

pub fn production_green(
    cx: &BuildCx<'_>,
    children: &[ProductId],
) -> Result<Vec<usize>, BuildError> {
    children.iter().map(|&child| cx.green_of(child)).collect()
}

pub fn production_node<T>(
    cx: &mut BuildCx<'_>,
    production: ProductionId,
    children: &[ProductId],
    value: T,
) -> Result<ProductId, BuildError>
where
    T: Send + Sync + 'static,
{
    cx.alloc_node(production, children, value)
}

pub fn expect_product_type(
    cx: &BuildCx<'_>,
    product: ProductId,
    expected: TypeId,
) -> Result<(), BuildError> {
    match &cx
        .products
        .get(product)
        .ok_or(BuildError::MissingProduct(product))?
        .data
    {
        ProductData::Node { ty, .. } if *ty == expected => Ok(()),
        ProductData::Node { .. } => Err(BuildError::TypeMismatch { product }),
        ProductData::Token { .. } => Err(BuildError::ExpectedNode { product }),
        ProductData::Error { .. } => Err(BuildError::UnexpectedErrorProduct { product }),
    }
}
