use crate::component::parse::grammar::{NonTerminalId, Symbol, TerminalId};

pub type GreenId = usize;

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
        node: NonTerminalId,
        children: Vec<GreenId>,
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
        location: Option<usize>,
    ) -> GreenTree {
        GreenTree {
            length,
            data: TreeData::Error {
                kind,
                node,
                children,
                unexpected,
                expected,
                recovered,
                location,
            },
        }
    }

    pub fn new_unexpected_error(
        length: usize,
        node: NonTerminalId,
        unexpected: Symbol,
        expected: Symbol,
    ) -> GreenTree {
        Self::new_error(
            length,
            ErrorKind::UnexpectedToken,
            node,
            Vec::new(),
            Some(unexpected),
            expected,
            false,
            None,
        )
    }

    pub fn new_missing_error(length: usize, node: NonTerminalId, expected: Symbol) -> GreenTree {
        Self::new_error(
            length,
            ErrorKind::MissingToken,
            node,
            Vec::new(),
            None,
            expected,
            false,
            None,
        )
    }

    pub fn new_eoi_error(node: NonTerminalId, expected: Symbol) -> GreenTree {
        Self::new_error(
            0,
            ErrorKind::UnexpectedEndOfInput,
            node,
            Vec::new(),
            None,
            expected,
            false,
            None,
        )
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
        node: NonTerminalId,
        children: Vec<GreenId>,
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
        location: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    MissingToken,
    UnexpectedToken,
    UnexpectedEndOfInput,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParseErrorInfo {
    pub kind: ErrorKind,
    pub node: NonTerminalId,
    pub length: usize,
    pub unexpected: Option<Symbol>,
    pub expected: Symbol,
    pub recovered: bool,
    pub location: Option<usize>,
}

#[derive(Clone)]
pub struct TreeArena {
    pub trees: indexmap::IndexSet<GreenTree>,
}

impl Default for TreeArena {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeArena {
    pub fn new() -> TreeArena {
        TreeArena {
            trees: indexmap::IndexSet::new(),
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
        node: NonTerminalId,
        children: Vec<GreenId>,
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
        location: Option<usize>,
    ) -> GreenId {
        self.insert(GreenTree::new_error(
            length, kind, node, children, unexpected, expected, recovered, location,
        ))
    }

    fn total_len(&self, children: &[GreenId]) -> usize {
        children
            .iter()
            .map(|&child| self.get(child).map_or(0, |tree| tree.length))
            .sum()
    }
}
