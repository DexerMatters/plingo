use std::collections::HashMap;

use bitvec::vec::BitVec;

use crate::component::parse::{
    __macro_private::NonTerminalSpec,
    data::{
        ast::{AstArena, AstBox, AstId, TokenEntryId},
        green::{ErrorKind, GreenId, TreeArena},
        product::{Product, ProductArena, ProductData, ProductId},
    },
    identity::TokenFingerprint,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TerminalId {
    pub state_key: &'static str,
    pub token_id: u32,
}

pub type NonTerminalId = u32;
pub type ProductionId = u32;

pub type BuildFn =
    fn(&mut BuildCx<'_>, ProductionId, &[ProductId]) -> Result<ProductId, BuildError>;

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Symbol {
    T(TerminalId),
    N(NonTerminalId),
    Epsilon,
}

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum Associativity {
    Left,
    Right,
}

#[derive(Clone, Copy)]
pub struct Precedence {
    pub level: u8,
    pub assoc: Associativity,
}

#[derive(Clone)]
pub struct NonTerminal {
    pub label: &'static str,
    pub named: bool,
}

#[derive(Clone)]
pub struct Terminal {
    pub id: TerminalId,
    pub label: &'static str,
    pub precedence: Option<Precedence>,
}

#[derive(Clone)]
pub struct Production {
    pub id: ProductionId,
    pub label: &'static str,
    pub lhs: NonTerminalId,
    pub rhs_start: u32,
    pub rhs_len: u16,
    pub precedence: Option<Precedence>,
    pub build: BuildFn,
}

pub const EOF_TERMINAL: TerminalId = TerminalId {
    state_key: "",
    token_id: u32::MAX,
};

pub const ERROR_TERMINAL: TerminalId = TerminalId {
    state_key: "",
    token_id: u32::MAX - 1,
};

#[allow(dead_code)]
pub struct Grammar {
    pub(crate) terminals: Vec<Terminal>,
    pub(crate) non_terminals: Vec<NonTerminal>,
    pub(crate) productions: Vec<Production>,
    pub(crate) rhs_symbols: Vec<Symbol>,
    pub(crate) augmented_start: NonTerminalId,
    pub(crate) productions_for_lhs: Vec<std::ops::Range<u32>>,
    pub(crate) production_ids_by_lhs: Vec<ProductionId>,
    pub(crate) eof: TerminalId,
    pub error_terminal: TerminalId,
    pub error_non_terminal: NonTerminalId,
    pub(crate) terminal_indices: HashMap<TerminalId, usize>,
    pub(crate) is_nullable: BitVec,
    pub(crate) is_at_first: Vec<BitVec>,
}

impl Clone for Grammar {
    fn clone(&self) -> Self {
        Self {
            terminals: self.terminals.clone(),
            non_terminals: self.non_terminals.clone(),
            productions: self.productions.clone(),
            rhs_symbols: self.rhs_symbols.clone(),
            augmented_start: self.augmented_start,
            productions_for_lhs: self.productions_for_lhs.clone(),
            production_ids_by_lhs: self.production_ids_by_lhs.clone(),
            eof: self.eof,
            error_terminal: self.error_terminal,
            error_non_terminal: self.error_non_terminal,
            terminal_indices: self.terminal_indices.clone(),
            is_nullable: self.is_nullable.clone(),
            is_at_first: self.is_at_first.clone(),
        }
    }
}

impl Grammar {
    pub fn from_spec<S: NonTerminalSpec>() -> Self {
        let mut builder = GrammarBuilder::new();
        let Symbol::N(start) = S::register(&mut builder) else {
            panic!("grammar start must be a nonterminal");
        };
        let mut grammar = builder.finish(start);
        grammar.analyze();
        grammar
    }

    pub fn terminal_index(&self, terminal: TerminalId) -> usize {
        self.terminal_indices[&terminal]
    }

    pub fn terminal_count(&self) -> usize {
        self.terminals.len()
    }

    pub fn terminal_at(&self, index: usize) -> TerminalId {
        self.terminals[index].id
    }

    pub fn action_index(&self, state: usize, terminal: TerminalId) -> usize {
        state * self.terminal_count() + self.terminal_index(terminal)
    }

    pub fn goto_index(&self, state: usize, non_terminal: NonTerminalId) -> usize {
        state * self.non_terminals.len() + non_terminal as usize
    }
}

#[doc(hidden)]
#[allow(dead_code)]
pub struct GrammarBuilder {
    terminals: Vec<Terminal>,
    terminal_indices: HashMap<TerminalId, TerminalId>,
    non_terminals: Vec<NonTerminal>,
    non_terminal_open: Vec<bool>,
    productions: Vec<PendingProduction>,
    rhs_symbols: Vec<Symbol>,
}

struct PendingProduction {
    id: ProductionId,
    label: &'static str,
    lhs: NonTerminalId,
    rhs_start: u32,
    rhs_len: u16,
    precedence: Option<Precedence>,
    build: BuildFn,
}

#[derive(Debug)]
pub enum BuildError {
    MissingProduct(ProductId),
    MissingProduction(ProductionId),
    MissingAst(AstId),
    MissingToken(TokenEntryId),
    MissingBuild,
    ExpectedNode { product: ProductId },
    ExpectedToken { product: ProductId },
    UnexpectedErrorProduct { product: ProductId },
    TypeMismatch { product: ProductId },
}

pub struct BuildCx<'a> {
    pub productions: &'a [Production],
    pub trees: &'a mut TreeArena,
    pub products: &'a mut ProductArena,
    pub ast: &'a mut AstArena,
}

impl<'a> BuildCx<'a> {
    pub fn green_of(&self, product: ProductId) -> Result<GreenId, BuildError> {
        Ok(self.product(product)?.green)
    }

    pub fn expect_node<T: 'static>(&self, product: ProductId) -> Result<AstBox<T>, BuildError> {
        match self.product(product)?.data {
            ProductData::Node { ast, .. } => self
                .ast
                .expect(ast)
                .ok_or(BuildError::TypeMismatch { product }),
            ProductData::Error { .. } => Err(BuildError::UnexpectedErrorProduct { product }),
            ProductData::Token { .. } => Err(BuildError::ExpectedNode { product }),
        }
    }

    pub fn expect_token(&self, product: ProductId) -> Result<TokenEntryId, BuildError> {
        match self.product(product)?.data {
            ProductData::Token { entry, .. } => Ok(entry),
            ProductData::Error { .. } => Err(BuildError::UnexpectedErrorProduct { product }),
            ProductData::Node { .. } => Err(BuildError::ExpectedToken { product }),
        }
    }

    pub fn expect_value<T>(&self, product: ProductId) -> Result<T, BuildError>
    where
        T: Clone + 'static,
    {
        match self.product(product)?.data {
            ProductData::Node { ast, .. } => self
                .ast
                .cloned(ast)
                .ok_or(BuildError::TypeMismatch { product }),
            ProductData::Token { ast: Some(ast), .. } => self
                .ast
                .cloned(ast)
                .ok_or(BuildError::TypeMismatch { product }),
            ProductData::Token { ast: None, .. } => Err(BuildError::TypeMismatch { product }),
            ProductData::Error { .. } => Err(BuildError::UnexpectedErrorProduct { product }),
        }
    }

    pub fn alloc_node<T>(
        &mut self,
        production: ProductionId,
        children: &[ProductId],
        value: T,
    ) -> Result<ProductId, BuildError>
    where
        T: Send + Sync + 'static,
    {
        let greens = children
            .iter()
            .map(|&child| self.green_of(child))
            .collect::<Result<Vec<_>, _>>()?;
        let green = self.trees.node(self.lhs(production)?, greens);
        let ast = self.ast.insert(value);
        let product = self
            .products
            .insert(Product::node(green, ast, children.to_vec()));
        self.ast.bind_product(ast.id, product);
        Ok(product)
    }

    pub fn lhs(&self, production: ProductionId) -> Result<NonTerminalId, BuildError> {
        Ok(self.production(production)?.lhs)
    }

    pub fn alloc_token(
        &mut self,
        length: usize,
        terminal: TerminalId,
        entry: TokenEntryId,
        fingerprint: TokenFingerprint,
    ) -> ProductId {
        let green = self.trees.leaf(length, terminal);
        self.products
            .insert(Product::token(green, entry, fingerprint))
    }

    pub fn alloc_typed_token<T>(
        &mut self,
        length: usize,
        terminal: TerminalId,
        entry: TokenEntryId,
        fingerprint: TokenFingerprint,
        value: T,
    ) -> ProductId
    where
        T: Send + Sync + 'static,
    {
        let green = self.trees.leaf(length, terminal);
        let ast = self.ast.insert(value);
        let product = self
            .products
            .insert(Product::typed_token(green, entry, fingerprint, ast));
        self.ast.bind_product(ast.id, product);
        product
    }

    pub fn alloc_error(
        &mut self,
        length: usize,
        kind: ErrorKind,
        node: NonTerminalId,
        children: Vec<GreenId>,
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
    ) -> ProductId {
        let green = self.trees.error(
            length, kind, node, children, unexpected, expected, recovered, None,
        );
        self.products.insert(Product::error(green))
    }

    pub fn alloc_error_with_children(
        &mut self,
        kind: ErrorKind,
        node: NonTerminalId,
        children: &[ProductId],
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
    ) -> Result<ProductId, BuildError> {
        let greens = children
            .iter()
            .map(|&child| self.green_of(child))
            .collect::<Result<Vec<_>, _>>()?;
        let length = greens
            .iter()
            .map(|g| self.trees.get(*g).map_or(0, |t| t.length))
            .sum();
        let green = self.trees.error(
            length, kind, node, greens, unexpected, expected, recovered, None,
        );
        Ok(self
            .products
            .insert(Product::error_with_children(green, children.to_vec())))
    }

    pub fn is_error(&self, product: ProductId) -> Result<bool, BuildError> {
        Ok(matches!(
            self.product(product)?.data,
            ProductData::Error { .. }
        ))
    }

    fn product(&self, product: ProductId) -> Result<&Product, BuildError> {
        self.products
            .get(product)
            .ok_or(BuildError::MissingProduct(product))
    }

    fn production(&self, production: ProductionId) -> Result<&Production, BuildError> {
        self.productions
            .get(production as usize)
            .ok_or(BuildError::MissingProduction(production))
    }
}

#[allow(dead_code)]
fn default_build(
    _: &mut BuildCx<'_>,
    _: ProductionId,
    _: &[ProductId],
) -> Result<ProductId, BuildError> {
    Err(BuildError::MissingBuild)
}

fn augmented_build(
    _: &mut BuildCx<'_>,
    _: ProductionId,
    children: &[ProductId],
) -> Result<ProductId, BuildError> {
    children
        .first()
        .copied()
        .ok_or(BuildError::MissingProduct(0))
}

#[allow(dead_code)]
impl GrammarBuilder {
    pub(crate) fn new() -> Self {
        Self {
            terminals: vec![
                Terminal {
                    id: TerminalId {
                        state_key: "",
                        token_id: u32::MAX,
                    },
                    label: "EOF",
                    precedence: None,
                },
                Terminal {
                    id: ERROR_TERMINAL,
                    label: "error",
                    precedence: None,
                },
            ],
            terminal_indices: HashMap::from([
                (
                    TerminalId {
                        state_key: "",
                        token_id: u32::MAX,
                    },
                    TerminalId {
                        state_key: "",
                        token_id: u32::MAX,
                    },
                ),
                (ERROR_TERMINAL, ERROR_TERMINAL),
            ]),
            non_terminals: vec![
                NonTerminal {
                    label: "S'",
                    named: false,
                },
                NonTerminal {
                    label: "Err",
                    named: false,
                },
            ],
            non_terminal_open: vec![false, false],
            productions: vec![PendingProduction {
                id: 0,
                label: "S'",
                lhs: 0,
                rhs_start: 0,
                rhs_len: 0,
                precedence: None,
                build: augmented_build,
            }],
            rhs_symbols: Vec::new(),
        }
    }

    pub(crate) fn begin_non_terminal(&mut self, label: &'static str) -> (Symbol, bool) {
        let id = match self
            .non_terminals
            .iter()
            .position(|non_terminal| non_terminal.label == label)
        {
            Some(id) => id as NonTerminalId,
            None => {
                let id = self.non_terminals.len() as NonTerminalId;
                self.non_terminals.push(NonTerminal { label, named: true });
                self.non_terminal_open.push(false);
                id
            }
        };
        let open = &mut self.non_terminal_open[id as usize];
        if *open {
            return (Symbol::N(id), false);
        }
        *open = true;
        (Symbol::N(id), true)
    }

    pub fn terminal_symbol(
        &mut self,
        label: &'static str,
        id: TerminalId,
        precedence: Option<Precedence>,
    ) -> Symbol {
        if let Some(id) = self.terminal_indices.get(&id).copied() {
            return Symbol::T(id);
        }
        self.terminals.push(Terminal {
            id,
            label,
            precedence,
        });
        self.terminal_indices.insert(id, id);
        Symbol::T(id)
    }

    pub(crate) fn begin_internal_non_terminal(&mut self, label: &'static str) -> Symbol {
        if let Some(id) = self
            .non_terminals
            .iter()
            .position(|non_terminal| non_terminal.label == label)
        {
            return Symbol::N(id as NonTerminalId);
        }
        let id = self.non_terminals.len() as NonTerminalId;
        self.non_terminals.push(NonTerminal {
            label,
            named: false,
        });
        self.non_terminal_open.push(true);
        Symbol::N(id)
    }

    pub(crate) fn rule(
        &mut self,
        label: &'static str,
        lhs: Symbol,
        rhs: impl IntoIterator<Item = Symbol>,
        precedence: Option<Precedence>,
        build: Option<BuildFn>,
    ) {
        let Symbol::N(lhs) = lhs else {
            panic!("grammar rule lhs must be a nonterminal");
        };
        let rhs_start = self.rhs_symbols.len() as u32;
        self.rhs_symbols.extend(rhs);
        let rhs_len = (self.rhs_symbols.len() as u32 - rhs_start) as u16;
        let id = self.productions.len() as ProductionId;
        self.productions.push(PendingProduction {
            id,
            label,
            lhs,
            rhs_start,
            rhs_len,
            precedence,
            build: build.unwrap_or(default_build),
        });
    }

    pub(crate) fn error_rule(
        &mut self,
        label: &'static str,
        lhs: NonTerminalId,
        builder: Option<BuildFn>,
    ) {
        let rhs_start = self.rhs_symbols.len() as u32;
        self.rhs_symbols.push(Symbol::T(ERROR_TERMINAL));
        let rhs_len = 1u16;
        let id = self.productions.len() as ProductionId;
        self.productions.push(PendingProduction {
            id,
            label,
            lhs,
            rhs_start,
            rhs_len,
            precedence: None,
            build: builder.unwrap_or(default_build),
        });
    }

    fn finish(mut self, start: NonTerminalId) -> Grammar {
        self.productions[0].rhs_start = self.rhs_symbols.len() as u32;
        self.rhs_symbols.push(Symbol::N(start));
        self.rhs_symbols.push(Symbol::T(EOF_TERMINAL));
        self.productions[0].rhs_len = 2;

        Grammar {
            terminals: self.terminals,
            non_terminals: self.non_terminals,
            productions: self
                .productions
                .into_iter()
                .map(|production| Production {
                    id: production.id,
                    label: production.label,
                    lhs: production.lhs,
                    rhs_start: production.rhs_start,
                    rhs_len: production.rhs_len,
                    precedence: production.precedence,
                    build: production.build,
                })
                .collect(),
            rhs_symbols: self.rhs_symbols,
            augmented_start: 0,
            productions_for_lhs: Vec::new(),
            production_ids_by_lhs: Vec::new(),
            eof: EOF_TERMINAL,
            error_terminal: ERROR_TERMINAL,
            error_non_terminal: 1,
            terminal_indices: HashMap::new(),
            is_nullable: BitVec::new(),
            is_at_first: Vec::new(),
        }
    }
}
