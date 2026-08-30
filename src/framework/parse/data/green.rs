use std::{collections::HashMap, sync::Arc};

use crate::framework::parse::grammar::{NonTerminalId, Symbol, TerminalId};

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
    /// Immutable green-record generations. Green IDs are offsets in this
    /// append-only sequence, so retaining an old root retains its records
    /// without copying them into the next command's arena.
    chunks: Arc<Vec<Arc<[GreenTree]>>>,
    chunk_starts: Arc<Vec<usize>>,
    /// Hash-consing directory for frozen generations.
    indexes: Arc<Vec<Arc<HashMap<GreenTree, GreenId>>>>,
    tail: Vec<GreenTree>,
    tail_index: HashMap<GreenTree, GreenId>,
    total_len: usize,
}

impl Default for TreeArena {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeArena {
    pub fn new() -> TreeArena {
        TreeArena {
            chunks: Arc::new(Vec::new()),
            chunk_starts: Arc::new(Vec::new()),
            indexes: Arc::new(Vec::new()),
            tail: Vec::new(),
            tail_index: HashMap::new(),
            total_len: 0,
        }
    }

    pub fn insert(&mut self, tree: GreenTree) -> GreenId {
        if let Some(&id) = self.tail_index.get(&tree) {
            return id;
        }
        for index in self.indexes.iter().rev() {
            if let Some(&id) = index.get(&tree) {
                return id;
            }
        }
        let id = self.total_len;
        self.tail_index.insert(tree.clone(), id);
        self.tail.push(tree);
        self.total_len = self.total_len.saturating_add(1);
        id
    }

    pub fn get(&self, id: GreenId) -> Option<&GreenTree> {
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

    /// Publishes the current append-only green generation and its lookup
    /// directory. The directory vectors are the only metadata copied.
    pub(crate) fn seal_generation(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        let start = self.total_len - self.tail.len();
        let trees: Arc<[GreenTree]> = std::mem::take(&mut self.tail).into();
        let index = std::mem::take(&mut self.tail_index);
        let mut chunks = self.chunks.as_ref().clone();
        chunks.push(trees);
        self.chunks = Arc::new(chunks);
        let mut starts = self.chunk_starts.as_ref().clone();
        starts.push(start);
        self.chunk_starts = Arc::new(starts);
        let mut indexes = self.indexes.as_ref().clone();
        indexes.push(Arc::new(index));
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
