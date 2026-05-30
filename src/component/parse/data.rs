use std::{
    any::{Any, TypeId},
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;
use indexmap::IndexSet;

use crate::component::parse::{
    build::LRStateId,
    grammar::{NonTerminalId, Symbol, TerminalId},
};

pub type GreenId = usize;
pub type AstId = usize;
pub type ProductId = usize;
pub type TokenEntryId = usize;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct GreenTree {
    pub length: usize,
    pub data: TreeData,
}

impl GreenTree {
    pub fn new(length: usize, data: TreeData) -> GreenTree {
        GreenTree { length, data }
    }

    pub fn new_leaf(length: usize, id: TerminalId) -> GreenTree {
        GreenTree {
            length,
            data: TreeData::Leaf { id },
        }
    }

    pub fn new_node(length: usize, id: NonTerminalId, children: Vec<GreenId>) -> GreenTree {
        GreenTree {
            length,
            data: TreeData::Node { children, id },
        }
    }

    pub fn new_error(
        length: usize,
        kind: ErrorKind,
        unexpected: Option<Symbol>,
        expected: Symbol,
    ) -> GreenTree {
        GreenTree {
            length,
            data: TreeData::Error {
                kind,
                unexpected,
                expected,
            },
        }
    }

    pub fn new_unexpected_error(length: usize, unexpected: Symbol, expected: Symbol) -> GreenTree {
        Self::new_error(
            length,
            ErrorKind::UnexpectedToken,
            Some(unexpected),
            expected,
        )
    }

    pub fn new_missing_error(length: usize, expected: Symbol) -> GreenTree {
        Self::new_error(length, ErrorKind::MissingToken, None, expected)
    }

    pub fn new_eoi_error(expected: Symbol) -> GreenTree {
        Self::new_error(0, ErrorKind::UnexpectedEndOfInput, None, expected)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum TreeData {
    Node {
        id: NonTerminalId,
        children: Vec<GreenId>,
    },
    Leaf {
        id: TerminalId,
    },
    Error {
        kind: ErrorKind,
        unexpected: Option<Symbol>,
        expected: Symbol,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    MissingToken,
    UnexpectedToken,
    UnexpectedEndOfInput,
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
pub struct Product {
    pub green: GreenId,
    pub data: ProductData,
}

impl Product {
    pub fn new(green: GreenId, data: ProductData) -> Self {
        Self { green, data }
    }

    pub fn error(green: GreenId) -> Self {
        Self::new(green, ProductData::Error)
    }

    pub fn token(green: GreenId, entry: TokenEntryId) -> Self {
        Self::new(
            green,
            ProductData::Token {
                entry,
                ast: None,
                ty: TypeId::of::<()>(),
            },
        )
    }

    pub fn typed_token<T: 'static>(green: GreenId, entry: TokenEntryId, ast: AstBox<T>) -> Self {
        Self::new(
            green,
            ProductData::Token {
                entry,
                ast: Some(ast.id),
                ty: TypeId::of::<T>(),
            },
        )
    }

    pub fn node<T: 'static>(green: GreenId, ast: AstBox<T>) -> Self {
        Self::new(
            green,
            ProductData::Node {
                ast: ast.id,
                ty: TypeId::of::<T>(),
            },
        )
    }
}

#[derive(Clone)]
pub enum ProductData {
    Error,
    Token {
        entry: TokenEntryId,
        ast: Option<AstId>,
        ty: TypeId,
    },
    Node {
        ast: AstId,
        ty: TypeId,
    },
}

#[derive(Clone)]
pub struct TreeArena {
    pub trees: IndexSet<GreenTree>,
}

impl TreeArena {
    pub fn new() -> TreeArena {
        TreeArena {
            trees: IndexSet::new(),
        }
    }

    pub fn insert(&mut self, tree: GreenTree) -> GreenId {
        self.trees.insert_full(tree).0
    }

    pub fn get(&self, id: GreenId) -> Option<&GreenTree> {
        self.trees.get_index(id)
    }

    pub fn node(&mut self, id: NonTerminalId, children: Vec<GreenId>) -> GreenId {
        let length = self.total_len(&children);
        self.insert(GreenTree::new_node(length, id, children))
    }

    pub fn leaf(&mut self, length: usize, id: TerminalId) -> GreenId {
        self.insert(GreenTree::new_leaf(length, id))
    }

    pub fn error(
        &mut self,
        length: usize,
        kind: ErrorKind,
        unexpected: Option<Symbol>,
        expected: Symbol,
    ) -> GreenId {
        self.insert(GreenTree::new_error(length, kind, unexpected, expected))
    }

    fn total_len(&self, children: &[GreenId]) -> usize {
        children
            .iter()
            .map(|&child| self.get(child).map_or(0, |tree| tree.length))
            .sum()
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

#[derive(Clone)]
pub struct AstArena {
    values: Vec<Arc<dyn Any + Send + Sync>>,
    uri: Uri<&'static str>,
}

impl AstArena {
    pub fn new(uri: Uri<&'static str>) -> Self {
        Self {
            values: Vec::new(),
            uri,
        }
    }

    pub fn insert<T>(&mut self, value: T) -> AstBox<T>
    where
        T: Send + Sync + 'static,
    {
        let id = self.values.len();
        self.values.push(Arc::new(value));
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
        let entry = self.values.get(id)?;
        let result = entry.downcast_ref::<T>().cloned();
        if result.is_none() {
            eprintln!(
                "AstArena::cloned TypeMismatch: requested {:?} ({}), stored {:?}",
                std::any::TypeId::of::<T>(),
                std::any::type_name::<T>(),
                entry.as_ref().type_id(),
            );
        }
        result
    }
}

pub(crate) type GssNodeId = usize;
pub(crate) type GssEdgeId = usize;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct GssNode {
    pub state: LRStateId,
    pub column: u16,
    pub generation: u32,
}

impl GssNode {
    fn new(state: LRStateId, column: u16, generation: u32) -> GssNode {
        GssNode {
            state,
            column,
            generation,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct GssEdge {
    pub from: GssNodeId,
    pub to: GssNodeId,
    pub product: ProductId,
    pub generation: u32,
}

impl GssEdge {
    pub fn new(from: GssNodeId, to: GssNodeId, product: ProductId, generation: u32) -> GssEdge {
        GssEdge {
            from,
            to,
            product,
            generation,
        }
    }
}

#[derive(Clone)]
pub(crate) struct GssArena {
    nodes: IndexSet<GssNode>,
    edges: IndexSet<GssEdge>,
    edges_out: Vec<Vec<GssEdgeId>>,
}

impl GssArena {
    pub fn new() -> GssArena {
        GssArena {
            nodes: IndexSet::new(),
            edges: IndexSet::new(),
            edges_out: Vec::new(),
        }
    }

    pub fn node(&mut self, state: LRStateId, column: u16, generation: u32) -> GssNodeId {
        let node = GssNode::new(state, column, generation);
        let (id, inserted) = self.nodes.insert_full(node);

        if inserted {
            self.resize_edge_grid(self.nodes.len());
        }

        id
    }

    pub fn add_edge(
        &mut self,
        from: GssNodeId,
        to: GssNodeId,
        product: ProductId,
        generation: u32,
    ) -> bool {
        let edge = GssEdge::new(from, to, product, generation);
        let (edge_id, inserted) = self.edges.insert_full(edge);

        if inserted {
            self.edges_out[from].push(edge_id);
        }

        inserted
    }

    pub fn get_node(&self, id: GssNodeId) -> Option<&GssNode> {
        self.nodes.get_index(id)
    }

    pub fn get_edge(&self, id: GssEdgeId) -> Option<&GssEdge> {
        self.edges.get_index(id)
    }

    pub fn outgoing_edge_ids(&self, id: GssNodeId) -> Option<&[GssEdgeId]> {
        self.edges_out.get(id).map(Vec::as_slice)
    }

    pub fn outgoing_edges(&self, id: GssNodeId) -> impl Iterator<Item = &GssEdge> {
        self.outgoing_edge_ids(id)
            .into_iter()
            .flatten()
            .filter_map(|&edge_id| self.get_edge(edge_id))
    }

    fn resize_edge_grid(&mut self, rows: usize) {
        if self.edges_out.len() < rows {
            self.edges_out.resize_with(rows, Vec::new);
        }
    }
}
