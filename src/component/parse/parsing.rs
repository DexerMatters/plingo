use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use indexmap::IndexSet;

use crate::component::parse::{TokenOccurrenceId, recovery};
use crate::component::{
    lex::{GetVisibleTokenBatch, Lexer, LexerRoot, VisibleTokenBatch},
    parse::{
        IncrementalParseStats, Parser, ParserSnapshotState, SessionArenas, TokenData,
        build::{Action, ActionSet},
        checkpoint::{self, BoundaryCheckpoint},
        data::{
            AstArena, ErrorKind, GssArena, GssNodeId, ParseErrorInfo, Product, ProductArena,
            ProductId, TokenEntryId, TreeArena, TreeData,
        },
        emit,
        grammar::{BuildCx, BuildError, Grammar, NonTerminalId, Symbol, TerminalId},
        identity::TokenFingerprint,
        incremental::ReplayPlan,
        recovery::{RecoveryError, Repair},
    },
};
use crate::scheme::{Context, Delta, LayerDeltas, NonTopLayer};
use crate::utils::{RangeOrPoint, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ParseToken {
    pub(crate) entry: TokenEntryId,
    pub(crate) column: TokenOccurrenceId,
    pub(crate) start: usize,
    pub(crate) terminal: TerminalId,
    pub(crate) length: usize,
    pub(crate) fingerprint: TokenFingerprint,
    pub(crate) merge_source_terminal: Option<TerminalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReductionPath {
    predecessor: GssNodeId,
    products: Vec<ProductId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReductionKey {
    production: u32,
    children: Vec<ProductId>,
}

#[derive(Debug)]
pub enum ParseError {
    MissingGoto { state: usize, non_terminal: u32 },
    NoActiveStacks { column: Option<TokenOccurrenceId> },
    MissingGssNode { node: GssNodeId },
    Build(BuildError),
    RecoveryTimeout { elapsed: Duration },
    Recovered { product: ProductId },
}

impl From<BuildError> for ParseError {
    fn from(value: BuildError) -> Self {
        Self::Build(value)
    }
}

impl From<recovery::RecoveryError> for ParseError {
    fn from(value: recovery::RecoveryError) -> Self {
        match value {
            recovery::RecoveryError::Timeout { elapsed } => Self::RecoveryTimeout { elapsed },
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingGoto {
                state,
                non_terminal,
            } => {
                write!(
                    f,
                    "missing goto from state {state} on nonterminal {non_terminal}"
                )
            }
            Self::NoActiveStacks { column } => match column {
                Some(column) => write!(f, "no active parse stacks at token column {column}"),
                None => write!(f, "no active parse stacks"),
            },
            Self::MissingGssNode { node } => write!(f, "missing GSS node {node}"),
            Self::Build(error) => write!(f, "build error: {error:?}"),
            Self::RecoveryTimeout { elapsed } => {
                write!(f, "recovery search timed out after {elapsed:?}")
            }
            Self::Recovered { .. } => write!(f, "parse recovered with errors"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ParseColumn {
    index: usize,
    token: Option<TokenOccurrenceId>,
    base_active: IndexSet<GssNodeId>,
    active: IndexSet<GssNodeId>,
    accepted: Vec<ProductId>,
    pub(crate) products: Vec<ProductId>,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    pub(crate) error_derived: bool,
}

impl ParseColumn {
    pub(crate) fn new(
        index: usize,
        token: Option<TokenOccurrenceId>,
        active: IndexSet<GssNodeId>,
    ) -> Self {
        Self {
            index,
            token,
            base_active: active.clone(),
            active,
            accepted: Vec::new(),
            products: Vec::new(),
            diagnostics: Vec::new(),
            error_derived: false,
        }
    }

    pub fn token(&self) -> Option<TokenOccurrenceId> {
        self.token
    }

    pub(crate) fn active_nodes(&self) -> impl Iterator<Item = GssNodeId> + '_ {
        self.active.iter().copied()
    }

    pub(crate) fn base_active_nodes(&self) -> impl Iterator<Item = GssNodeId> + '_ {
        self.base_active.iter().copied()
    }

    pub fn accepted(&self) -> &[ProductId] {
        &self.accepted
    }

    fn reset_for_replay(&mut self) {
        self.active = self.base_active.clone();
        self.accepted.clear();
        self.products.clear();
        self.diagnostics.clear();
        self.error_derived = false;
    }
}

#[derive(Clone, Default)]
pub struct ParserSessionState {
    pub(crate) columns: Vec<ParseColumn>,
    pub(crate) generation: u32,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    token_columns: HashMap<TokenOccurrenceId, usize>,
    token_products: HashMap<TokenOccurrenceId, ProductId>,
    reduced_products: HashMap<ReductionKey, ProductId>,
}

impl ParserSessionState {
    pub fn accepted(&self) -> &[ProductId] {
        self.columns.last().map_or(&[], ParseColumn::accepted)
    }

    pub fn current_column(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }

    pub fn column_before_token(&self, token: TokenOccurrenceId) -> Option<usize> {
        self.token_columns.get(&token).map(|c| c.saturating_sub(1))
    }

    pub fn truncate_to_column(&mut self, column: usize) {
        assert!(column < self.columns.len(), "parse column out of range");
        self.columns.truncate(column + 1);
        self.generation += 1;
        self.columns[column].reset_for_replay();
        self.reduced_products.clear();
        self.diagnostics
            .retain(|info| info.location.is_some_and(|loc| loc < column));

        self.token_columns.retain(|_, c| *c < column);
        self.token_products
            .retain(|token, _| self.token_columns.contains_key(token));
    }

    pub(crate) fn columns_from(&self, start: usize) -> Vec<ParseColumn> {
        self.columns.get(start..).unwrap_or_default().to_vec()
    }

    pub(crate) fn append_reused_columns(&mut self, columns: impl IntoIterator<Item = ParseColumn>) {
        for mut column in columns {
            column.index = self.columns.len();
            if let Some(token) = column.token {
                self.token_columns.insert(token, column.index);
                if !column.error_derived {
                    if let Some(&product) = column.products.first() {
                        self.token_products.insert(token, product);
                    }
                }
            }
            self.columns.push(column);
        }
    }

    pub(crate) fn column(&self, index: usize) -> Option<&ParseColumn> {
        self.columns.get(index)
    }

    pub(crate) fn discard_columns_from(&mut self, start: usize) {
        if start >= self.columns.len() {
            return;
        }
        self.columns.truncate(start);
        self.token_columns.retain(|_, c| *c < start);
        self.token_products
            .retain(|token, _| self.token_columns.contains_key(token));
    }
}

pub(crate) struct SessionContext<'a> {
    pub state: &'a mut ParserSessionState,
    pub trees: &'a mut TreeArena,
    pub products: &'a mut ProductArena,
    pub ast: &'a mut AstArena,
    pub gss: &'a mut GssArena,
    pub(crate) grammar: &'a Grammar,
    pub(crate) actions: &'a [ActionSet],
    pub(crate) gotos: &'a [Option<usize>],
    pub(crate) error_recovery: bool,
    pub(crate) error_recovery_timeout: Duration,
}

impl SessionContext<'_> {
    fn resolve_terminal(&self, data: &TokenData) -> TerminalId {
        match data.terminal {
            Some(t) => t,
            None if data.length == 0 => self.grammar.eof,
            None => self.grammar.error_terminal,
        }
    }

    fn action_set(&self, state: usize, terminal: TerminalId) -> &ActionSet {
        &self.actions[self.grammar.action_index(state, terminal)]
    }

    fn goto_state(&self, state: usize, non_terminal: u32) -> Option<usize> {
        self.gotos[self.grammar.goto_index(state, non_terminal)]
    }

    fn build_cx(&mut self) -> BuildCx<'_> {
        BuildCx {
            productions: &self.grammar.productions,
            trees: self.trees,
            products: self.products,
            ast: self.ast,
        }
    }

    fn alloc_error_node(
        &mut self,
        length: usize,
        kind: ErrorKind,
        node: NonTerminalId,
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
        location: Option<usize>,
    ) -> Result<ProductId, ParseError> {
        let green = self.trees.error(
            length,
            kind,
            node,
            Vec::new(),
            unexpected,
            expected,
            recovered,
            location,
        );
        let product = self.products.insert(Product::error(green));
        Ok(product)
    }

    fn error_info(
        &self,
        length: usize,
        kind: ErrorKind,
        node: NonTerminalId,
        unexpected: Option<Symbol>,
        expected: Symbol,
        recovered: bool,
        location: Option<usize>,
    ) -> ParseErrorInfo {
        ParseErrorInfo {
            kind,
            node,
            length,
            unexpected,
            expected,
            recovered,
            location,
        }
    }

    fn record_diagnostic(&mut self, _column: usize, info: ParseErrorInfo) {
        let diagnostics = &mut self.state.diagnostics;
        if !diagnostics.contains(&info) {
            diagnostics.push(info);
        }
    }

    fn root_score(
        &self,
        product_id: ProductId,
        memo: &mut HashMap<ProductId, (usize, usize, usize)>,
    ) -> (usize, usize, usize) {
        if let Some(score) = memo.get(&product_id) {
            return *score;
        }

        let Some(product) = self.products.get(product_id) else {
            return (usize::MAX, 0, 0);
        };
        let Some(tree) = self.trees.get(product.green) else {
            return (usize::MAX, 0, 0);
        };

        let score = match &product.data {
            crate::component::parse::data::ProductData::Error => (1, 1, tree.length),
            crate::component::parse::data::ProductData::Token { .. } => (0, 1, tree.length),
            crate::component::parse::data::ProductData::Node { children, .. } => {
                let mut errors = 0usize;
                let mut nodes = 1usize;
                for &child in children {
                    let (child_errors, child_nodes, _) = self.root_score(child, memo);
                    errors = errors.saturating_add(child_errors);
                    nodes = nodes.saturating_add(child_nodes);
                }
                (errors, nodes, tree.length)
            }
        };

        memo.insert(product_id, score);
        score
    }

    fn choose_best_root(&self, roots: &[ProductId]) -> Option<ProductId> {
        let mut memo = HashMap::new();
        roots.iter().copied().min_by_key(|&product_id| {
            let (errors, nodes, length) = self.root_score(product_id, &mut memo);
            (Reverse(length), errors, Reverse(nodes), product_id)
        })
    }

    fn compact_accepted_roots(&mut self) {
        let Some(column) = self.state.columns.last() else {
            return;
        };
        if column.accepted.len() <= 1 {
            return;
        }

        let Some(best) = self.choose_best_root(&column.accepted) else {
            return;
        };
        let Some(column) = self.state.columns.last_mut() else {
            return;
        };
        column.accepted.clear();
        column.accepted.push(best);
    }

    fn reduce_cached(
        &mut self,
        production: u32,
        children: &[ProductId],
    ) -> Result<ProductId, ParseError> {
        let key = ReductionKey {
            production,
            children: children.to_vec(),
        };
        if let Some(&product) = self.state.reduced_products.get(&key) {
            return Ok(product);
        }
        let build_fn = self.grammar.productions[production as usize].build;
        let mut cx = self.build_cx();
        let product = build_fn(&mut cx, production, children)?;
        self.state.reduced_products.insert(key, product);
        Ok(product)
    }

    fn record_column_product(&mut self, product: ProductId, column: usize) -> bool {
        let col_products = &mut self.state.columns[column].products;
        if !col_products.contains(&product) {
            col_products.push(product);
            return true;
        }
        false
    }

    fn reduce_until_stable(
        &mut self,
        column: usize,
        lookahead: TerminalId,
    ) -> Result<(), ParseError> {
        loop {
            let mut changed = false;
            let active_nodes: Vec<_> = self.state.columns[column].active_nodes().collect();

            for node_id in active_nodes {
                let Some(node) = self.gss.get_node(node_id) else {
                    return Err(ParseError::MissingGssNode { node: node_id });
                };
                let state = node.state;

                for action in self.action_set(state, lookahead).inner.clone() {
                    match action {
                        Action::Reduce(production) => {
                            let rhs_len = self.grammar.production_rhs_len(production);
                            let lhs = self.grammar.production_lhs(production);

                            for path in self.reduce_paths(column, node_id, production, rhs_len) {
                                let Some(pred) = self.gss.get_node(path.predecessor) else {
                                    return Err(ParseError::MissingGssNode {
                                        node: path.predecessor,
                                    });
                                };
                                let pred_state = pred.state;
                                let Some(goto_state) = self.goto_state(pred_state, lhs) else {
                                    return Err(ParseError::MissingGoto {
                                        state: pred_state,
                                        non_terminal: lhs,
                                    });
                                };
                                let product = self.reduce_cached(production, &path.products)?;
                                if self.record_column_product(product, column) {
                                    changed = true;
                                }
                                let goto_node =
                                    self.gss.node(goto_state, column, self.state.generation);
                                let inserted = self.state.columns[column].active.insert(goto_node);
                                let edge_added = self.gss.add_edge(
                                    goto_node,
                                    path.predecessor,
                                    product,
                                    self.state.generation,
                                );
                                if inserted || edge_added {
                                    changed = true;
                                }
                            }
                        }
                        Action::Accept => {
                            let rhs_len = self.grammar.production_rhs_len(0);
                            for path in self.reduce_paths(column, node_id, 0, rhs_len) {
                                let product = self.reduce_cached(0, &path.products)?;
                                if self.record_column_product(product, column) {
                                    changed = true;
                                }
                                let accepted = &mut self.state.columns[column].accepted;
                                if !accepted.contains(&product) {
                                    accepted.push(product);
                                }
                            }
                        }
                        Action::Shift(_) | Action::Error => {}
                    }
                }
            }

            if !changed {
                break;
            }
        }
        Ok(())
    }

    fn reduce_paths(
        &self,
        column: usize,
        node: GssNodeId,
        production: u32,
        depth: usize,
    ) -> Vec<ReductionPath> {
        if depth == 0 {
            return vec![ReductionPath {
                predecessor: node,
                products: Vec::new(),
            }];
        }
        let mut paths = Vec::new();
        for edge in self.gss.outgoing_edges(node) {
            for mut suffix in self.reduce_paths(column, edge.to, production, depth - 1) {
                suffix.products.push(edge.product);
                paths.push(suffix);
            }
        }
        if paths.is_empty()
            && depth == 1
            && self
                .grammar
                .productions
                .get(production as usize)
                .is_some_and(|prod| {
                    self.grammar
                        .rhs_symbols
                        .get(prod.rhs_start as usize)
                        .is_some_and(|sym| *sym == Symbol::T(self.grammar.error_terminal))
                })
        {
            for &product in &self.state.columns[column].products {
                if matches!(
                    self.products.get(product).map(|p| &p.data),
                    Some(crate::component::parse::data::ProductData::Error)
                ) {
                    paths.push(ReductionPath {
                        predecessor: node,
                        products: vec![product],
                    });
                }
            }
        }
        paths
    }

    fn shift_parse_token(
        &mut self,
        from_column: usize,
        token: &ParseToken,
    ) -> Result<usize, ParseError> {
        let mut next_active = IndexSet::new();
        let active_nodes: Vec<_> = self.state.columns[from_column].active_nodes().collect();
        let next_column = from_column + 1;

        let is_error_token = token.terminal == self.grammar.error_terminal;
        let error_product = if is_error_token {
            let product = self.alloc_error_node(
                token.length,
                ErrorKind::UnexpectedToken,
                self.grammar.error_non_terminal,
                token.merge_source_terminal.map(Symbol::T),
                Symbol::T(self.grammar.error_terminal),
                true,
                Some(next_column),
            )?;
            Some(product)
        } else {
            None
        };

        for node_id in active_nodes {
            let Some(node) = self.gss.get_node(node_id) else {
                return Err(ParseError::MissingGssNode { node: node_id });
            };
            let state = node.state;
            let actions = self.action_set(state, token.terminal).inner.clone();
            for action in actions {
                let Action::Shift(next_state) = action else {
                    continue;
                };

                if is_error_token {
                    let product = error_product.expect("error product must exist");
                    let next_node =
                        self.gss
                            .node(next_state, from_column + 1, self.state.generation);
                    if self
                        .gss
                        .add_edge(next_node, node_id, product, self.state.generation)
                    {
                        next_active.insert(next_node);
                    }
                } else {
                    if !self.state.token_products.contains_key(&token.column) {
                        let mut cx = self.build_cx();
                        let product = cx.alloc_token(
                            token.length,
                            token.terminal,
                            token.entry,
                            token.fingerprint,
                        );
                        self.state.token_products.insert(token.column, product);
                    }
                    let product = self.state.token_products[&token.column];
                    let next_node =
                        self.gss
                            .node(next_state, from_column + 1, self.state.generation);
                    if self
                        .gss
                        .add_edge(next_node, node_id, product, self.state.generation)
                    {
                        next_active.insert(next_node);
                    }
                }
            }
        }

        if next_active.is_empty() {
            return Err(ParseError::NoActiveStacks {
                column: Some(token.column),
            });
        }

        if is_error_token {
            let product = error_product.expect("error product must exist");
            self.state.columns.push(ParseColumn::new(
                next_column,
                Some(token.column),
                next_active,
            ));
            self.state.columns[next_column].products.push(product);
            self.record_diagnostic(
                next_column,
                self.error_info(
                    token.length,
                    ErrorKind::UnexpectedToken,
                    self.grammar.error_non_terminal,
                    token.merge_source_terminal.map(Symbol::T),
                    Symbol::T(self.grammar.error_terminal),
                    true,
                    Some(next_column),
                ),
            );
            self.state.token_columns.insert(token.column, next_column);
            self.state.columns[next_column].error_derived = true;
            self.reduce_until_stable(next_column, self.grammar.error_terminal)?;
        } else {
            let product = self.state.token_products[&token.column];
            self.state.columns.push(ParseColumn::new(
                next_column,
                Some(token.column),
                next_active,
            ));
            self.state.columns[next_column].products.push(product);
            self.state.token_columns.insert(token.column, next_column);
        }
        Ok(next_column)
    }

    fn shift_synthetic_terminal(
        &mut self,
        from_column: usize,
        terminal: TerminalId,
        unexpected: Option<Symbol>,
        _location: Option<usize>,
    ) -> Result<usize, ParseError> {
        let next_column = from_column + 1;
        let product = self.alloc_error_node(
            0,
            ErrorKind::MissingToken,
            self.grammar.error_non_terminal,
            unexpected,
            Symbol::T(terminal),
            true,
            Some(next_column),
        )?;
        let mut next_active = IndexSet::new();
        let active_nodes: Vec<_> = self.state.columns[from_column].active_nodes().collect();

        for node_id in active_nodes {
            let Some(node) = self.gss.get_node(node_id) else {
                return Err(ParseError::MissingGssNode { node: node_id });
            };
            let state = node.state;
            for action in self.action_set(state, terminal).inner.clone() {
                let Action::Shift(next_state) = action else {
                    continue;
                };
                let next_node = self
                    .gss
                    .node(next_state, from_column + 1, self.state.generation);
                if self
                    .gss
                    .add_edge(next_node, node_id, product, self.state.generation)
                {
                    next_active.insert(next_node);
                }
            }
        }

        if next_active.is_empty() {
            return Err(ParseError::NoActiveStacks { column: None });
        }

        self.state
            .columns
            .push(ParseColumn::new(next_column, None, next_active));
        self.state.columns[next_column].products.push(product);
        self.record_diagnostic(
            next_column,
            self.error_info(
                0,
                ErrorKind::MissingToken,
                self.grammar.error_non_terminal,
                unexpected,
                Symbol::T(terminal),
                true,
                Some(next_column),
            ),
        );
        self.state.columns[next_column].error_derived = true;
        self.reduce_until_stable(next_column, terminal)?;
        Ok(next_column)
    }

    fn delete_parse_token(
        &mut self,
        from_column: usize,
        token: &ParseToken,
    ) -> Result<usize, ParseError> {
        let next_column = from_column + 1;
        let product = self.alloc_error_node(
            token.length,
            ErrorKind::UnexpectedToken,
            self.grammar.error_non_terminal,
            Some(Symbol::T(token.terminal)),
            Symbol::T(self.grammar.error_terminal),
            true,
            Some(next_column),
        )?;
        let active = self.state.columns[from_column].active.clone();
        self.state
            .columns
            .push(ParseColumn::new(next_column, Some(token.column), active));
        self.state.columns[next_column].products.push(product);
        self.record_diagnostic(
            next_column,
            self.error_info(
                token.length,
                ErrorKind::UnexpectedToken,
                self.grammar.error_non_terminal,
                Some(Symbol::T(token.terminal)),
                Symbol::T(self.grammar.error_terminal),
                true,
                Some(next_column),
            ),
        );
        self.state.columns[next_column].error_derived = true;
        self.state.token_columns.insert(token.column, next_column);
        self.reduce_until_stable(next_column, self.grammar.error_terminal)?;
        Ok(next_column)
    }

    fn recover_tokens(
        &mut self,
        start: usize,
        tokens: &[ParseToken],
    ) -> Result<Option<usize>, ParseError> {
        if !self.error_recovery {
            return Ok(None);
        }
        let column = self.state.current_column();
        let result = match recovery::find_recovery(
            self,
            column,
            &tokens[start..],
            self.error_recovery_timeout,
        ) {
            Ok(Some(result)) => result,
            Ok(None) => return Ok(None),
            Err(err) => return Err(ParseError::from(err)),
        };

        if result.repairs.is_empty() {
            return Ok(None);
        }

        let start_column = self.state.current_column();
        let mut index = start;
        for repair in result.repairs {
            let column = self.state.current_column();
            match repair {
                Repair::Insert(terminal) => {
                    self.reduce_until_stable(column, terminal)?;
                    let unexpected = tokens.get(index).map(|token| Symbol::T(token.terminal));
                    let location = tokens.get(index).map(|token| token.column);
                    self.shift_synthetic_terminal(column, terminal, unexpected, location)?;
                }
                Repair::Delete => {
                    let Some(token) = tokens.get(index) else {
                        return Ok(None);
                    };
                    self.delete_parse_token(column, token)?;
                    index += 1;
                }
                Repair::Shift => {
                    let Some(token) = tokens.get(index) else {
                        return Ok(None);
                    };
                    self.reduce_until_stable(column, token.terminal)?;
                    self.shift_parse_token(column, token)?;
                    index += 1;
                }
                Repair::ShiftAsError => {
                    let Some(token) = tokens.get(index) else {
                        return Ok(None);
                    };
                    let modified = ParseToken {
                        entry: token.entry,
                        column: token.column,
                        start: token.start,
                        terminal: self.grammar.error_terminal,
                        fingerprint: token.fingerprint,
                        merge_source_terminal: Some(token.terminal),
                        ..*token
                    };
                    self.reduce_until_stable(column, self.grammar.error_terminal)?;
                    self.shift_parse_token(column, &modified)?;
                    index += 1;
                }
            }
        }

        if index == start && self.state.current_column() == start_column {
            return Ok(None);
        }

        Ok(Some(index))
    }

    pub fn parse_tokens(&mut self, tokens: &[TokenData]) -> Result<(), ParseError> {
        let tokens = tokens
            .iter()
            .map(|data| ParseToken {
                entry: data.id,
                column: data.column,
                start: data.start,
                terminal: self.resolve_terminal(data),
                length: data.length,
                fingerprint: data.fingerprint,
                merge_source_terminal: None,
            })
            .collect::<Vec<_>>();
        let tokens = tokens;
        let mut i = 0usize;
        while i < tokens.len() {
            let token = &tokens[i];
            let column = self.state.current_column();
            self.reduce_until_stable(column, token.terminal)?;
            if token.terminal == self.grammar.eof && !self.state.accepted().is_empty() {
                self.compact_accepted_roots();
                return Ok(());
            }
            if token.terminal == self.grammar.error_terminal && self.error_recovery {
                if let Some(next) = self.recover_tokens(i, &tokens)? {
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
            }
            if let Err(ParseError::NoActiveStacks { .. }) = self.shift_parse_token(column, token) {
                if let Some(next) = self.recover_tokens(i, &tokens)? {
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                return Err(ParseError::NoActiveStacks {
                    column: Some(token.column),
                });
            }
            if token.terminal == self.grammar.eof {
                let next_column = self.state.current_column();
                self.reduce_until_stable(next_column, token.terminal)?;
            }
            i += 1;
        }
        self.compact_accepted_roots();
        Ok(())
    }
}

impl<Root: LexerRoot + Clone, Lower> Parser<Root, Lower> {
    fn product_signature(
        products: &ProductArena,
        trees: &TreeArena,
        product_id: ProductId,
        memo: &mut HashMap<ProductId, String>,
    ) -> String {
        if let Some(sig) = memo.get(&product_id) {
            return sig.clone();
        }
        let Some(product) = products.get(product_id) else {
            return "missing-product".to_string();
        };
        let Some(tree) = trees.get(product.green) else {
            return "missing-tree".to_string();
        };
        let sig = match (&product.data, &tree.data) {
            (
                crate::component::parse::data::ProductData::Token { fingerprint, .. },
                TreeData::Leaf { id },
            ) => {
                format!("tok:{id:?}:{fingerprint}:{len}", len = tree.length)
            }
            (
                crate::component::parse::data::ProductData::Node { children, .. },
                TreeData::Node { id, .. },
            ) => {
                let child_sigs = children
                    .iter()
                    .map(|&child| Self::product_signature(products, trees, child, memo))
                    .collect::<Vec<_>>();
                format!("node:{id}:{}", child_sigs.join("|"))
            }
            (
                crate::component::parse::data::ProductData::Error,
                TreeData::Error {
                    kind,
                    node,
                    expected,
                    unexpected,
                    recovered,
                    location,
                    ..
                },
            ) => {
                format!(
                    "err:{kind:?}:{node}:{expected:?}:{unexpected:?}:{recovered}:{location:?}:{}",
                    tree.length
                )
            }
            _ => format!("tree:{}", tree.length),
        };
        memo.insert(product_id, sig.clone());
        sig
    }

    fn product_list_equivalent(
        a: &[ProductId],
        b: &[ProductId],
        products: &ProductArena,
        trees: &TreeArena,
        memo: &mut HashMap<ProductId, String>,
    ) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(&pa, &pb)| {
                Self::product_signature(products, trees, pa, memo)
                    == Self::product_signature(products, trees, pb, memo)
            })
    }

    fn node_signature(
        gss: &GssArena,
        products: &ProductArena,
        trees: &TreeArena,
        node_id: GssNodeId,
        node_memo: &mut HashMap<GssNodeId, String>,
        product_memo: &mut HashMap<ProductId, String>,
    ) -> String {
        if let Some(sig) = node_memo.get(&node_id) {
            return sig.clone();
        }
        let Some(node) = gss.get_node(node_id) else {
            return "missing".to_string();
        };
        let mut edge_sigs = gss
            .outgoing_edges(node_id)
            .map(|edge| {
                format!(
                    "{}>{}",
                    Self::product_signature(products, trees, edge.product, product_memo),
                    Self::node_signature(gss, products, trees, edge.to, node_memo, product_memo)
                )
            })
            .collect::<Vec<_>>();
        edge_sigs.sort();
        let sig = format!("{}@{}[{}]", node.state, node.column, edge_sigs.join(","));
        node_memo.insert(node_id, sig.clone());
        sig
    }

    fn frontier_state_node_signature(
        gss: &GssArena,
        node_id: GssNodeId,
        memo: &mut HashMap<GssNodeId, String>,
    ) -> String {
        if let Some(sig) = memo.get(&node_id) {
            return sig.clone();
        }
        let Some(node) = gss.get_node(node_id) else {
            return "missing".to_string();
        };
        let mut edge_sigs = gss
            .outgoing_edges(node_id)
            .map(|edge| Self::frontier_state_node_signature(gss, edge.to, memo))
            .collect::<Vec<_>>();
        edge_sigs.sort();
        let sig = format!("{}@{}[{}]", node.state, node.column, edge_sigs.join(","));
        memo.insert(node_id, sig.clone());
        sig
    }

    fn frontier_state_signature(
        nodes: impl Iterator<Item = GssNodeId>,
        gss: &GssArena,
        memo: &mut HashMap<GssNodeId, String>,
    ) -> Vec<String> {
        let mut sigs = nodes
            .map(|node_id| Self::frontier_state_node_signature(gss, node_id, memo))
            .collect::<Vec<_>>();
        sigs.sort();
        sigs
    }

    fn frontier_signature(
        nodes: impl Iterator<Item = GssNodeId>,
        gss: &GssArena,
        products: &ProductArena,
        trees: &TreeArena,
        node_memo: &mut HashMap<GssNodeId, String>,
        product_memo: &mut HashMap<ProductId, String>,
    ) -> Vec<String> {
        let mut sigs = nodes
            .map(|node_id| {
                Self::node_signature(gss, products, trees, node_id, node_memo, product_memo)
            })
            .collect::<Vec<_>>();
        sigs.sort();
        sigs
    }

    fn columns_equivalent(
        a: &ParseColumn,
        b: &ParseColumn,
        gss: &GssArena,
        products: &ProductArena,
        trees: &TreeArena,
    ) -> bool {
        if a.token != b.token || a.error_derived != b.error_derived {
            return false;
        }
        let mut node_memo = HashMap::new();
        let mut product_memo = HashMap::new();
        Self::product_list_equivalent(&a.products, &b.products, products, trees, &mut product_memo)
            && Self::product_list_equivalent(
                &a.accepted,
                &b.accepted,
                products,
                trees,
                &mut product_memo,
            )
            && Self::frontier_signature(
                a.base_active_nodes(),
                gss,
                products,
                trees,
                &mut node_memo,
                &mut product_memo,
            ) == Self::frontier_signature(
                b.base_active_nodes(),
                gss,
                products,
                trees,
                &mut node_memo,
                &mut product_memo,
            )
            && Self::frontier_signature(
                a.active_nodes(),
                gss,
                products,
                trees,
                &mut node_memo,
                &mut product_memo,
            ) == Self::frontier_signature(
                b.active_nodes(),
                gss,
                products,
                trees,
                &mut node_memo,
                &mut product_memo,
            )
    }

    fn frontier_equivalent(a: &ParseColumn, b: &ParseColumn, gss: &GssArena) -> bool {
        if a.token != b.token || a.error_derived != b.error_derived {
            return false;
        }
        let mut memo = HashMap::new();
        Self::frontier_state_signature(a.base_active_nodes(), gss, &mut memo)
            == Self::frontier_state_signature(b.base_active_nodes(), gss, &mut memo)
            && Self::frontier_state_signature(a.active_nodes(), gss, &mut memo)
                == Self::frontier_state_signature(b.active_nodes(), gss, &mut memo)
    }

    fn restart_column_for_offset(tokens: &[TokenData], offset: usize) -> usize {
        let mut restart = 0usize;
        for data in tokens {
            if data.start < offset {
                restart = data.column;
                continue;
            }
            break;
        }
        restart
    }

    fn compare_current_column(
        current: &ParseColumn,
        old_index_by_token: &HashMap<Option<TokenOccurrenceId>, Vec<usize>>,
        old_columns: &[ParseColumn],
        gss: &GssArena,
        products: &ProductArena,
        trees: &TreeArena,
        allow_reuse: bool,
        frontier_converged_at_old: &mut Option<usize>,
        converged_at_old: &mut Option<usize>,
    ) {
        let Some(indices) = old_index_by_token.get(&current.token()) else {
            return;
        };
        for &old_idx in indices.iter().rev() {
            if Self::frontier_equivalent(current, &old_columns[old_idx], gss) {
                frontier_converged_at_old.get_or_insert(old_idx);
            }
            if allow_reuse
                && Self::columns_equivalent(current, &old_columns[old_idx], gss, products, trees)
            {
                *converged_at_old = Some(old_idx);
                break;
            }
        }
    }

    pub(crate) async fn parse_delta_batch(
        &mut self,
        working: &mut ParserSnapshotState,
        uri: fluent_uri::Uri<&'static str>,
        deltas: &[Delta<Span, usize>],
        ctx: &Context,
    ) -> Result<LayerDeltas<Lower>, ParseError>
    where
        Lower: NonTopLayer<_Key = super::ParsePath, _Value = super::ParseForest>,
    {
        let roots_before = working.roots.get(&uri).cloned().unwrap_or_default();
        let eof = self.grammar.eof;

        let span = Span {
            uri,
            range: RangeOrPoint::Range(0, usize::MAX),
        };
        let tokens: Vec<TokenData> = ctx
            .post::<Lexer<Root, Self>, super::GetParseTokens>(super::GetParseTokens(span))
            .await
            .map_err(|_| ParseError::Build(BuildError::MissingProduct(0)))?;

        if deltas.len() > 1 {
            let mut fresh_arenas = SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: AstArena::new(uri),
                gss: GssArena::new(),
            };
            let mut fresh_state = ParserSessionState::default();
            if fresh_state.columns.is_empty() {
                let start = fresh_arenas.gss.node(0, 0, 0);
                fresh_state.columns = vec![ParseColumn::new(0, None, IndexSet::from([start]))];
            }
            let mut fresh_ctx = SessionContext {
                state: &mut fresh_state,
                trees: &mut fresh_arenas.trees,
                products: &mut fresh_arenas.products,
                ast: &mut fresh_arenas.ast,
                gss: &mut fresh_arenas.gss,
                grammar: &self.grammar,
                actions: &self.actions,
                gotos: &self.gotos,
                error_recovery: self.config.error_recovery,
                error_recovery_timeout: self.config.error_recovery_timeout,
            };
            let repair_start = Instant::now();
            fresh_ctx.parse_tokens(&tokens)?;
            let roots_after = fresh_ctx.state.accepted().to_vec();
            let recovery_columns = fresh_ctx
                .state
                .columns
                .iter()
                .filter(|c| c.error_derived)
                .count();
            drop(fresh_ctx);

            let lower_deltas = if roots_before.is_empty() {
                vec![emit::insert_root(uri.clone(), roots_after.clone())]
            } else if !roots_after.is_empty() {
                let raw = super::diff::diff_trees(
                    &fresh_arenas.products,
                    &fresh_arenas.trees,
                    &roots_before,
                    &roots_after,
                    uri.clone(),
                );
                let compacted = super::diff::compact(raw);
                if compacted.is_empty() && !deltas.is_empty() {
                    emit::replace_root(uri.clone(), roots_after.clone(), roots_before.len())
                } else {
                    compacted
                }
            } else {
                vec![emit::delete_root(uri.clone(), roots_before.len())]
            };

            self.session_arenas.insert(uri.clone(), fresh_arenas);
            working.sessions.insert(uri.clone(), fresh_state);
            working.roots.insert(uri.clone(), roots_after.clone());
            self.latest_incremental_stats.insert(
                uri.clone(),
                IncrementalParseStats {
                    reparsed: tokens.len(),
                    reused: 0,
                    recovery_columns,
                    converged: false,
                },
            );

            let elapsed = repair_start.elapsed();
            log::debug!(
                target: "Measure",
                "{} new={} full-batch in {:?}",
                uri,
                tokens.len(),
                elapsed,
            );

            return Ok(lower_deltas);
        }

        let arenas = self
            .session_arenas
            .entry(uri)
            .or_insert_with(|| SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: AstArena::new(uri),
                gss: GssArena::new(),
            });
        let state = working.sessions.entry(uri).or_default();
        if state.columns.is_empty() {
            let start = arenas.gss.node(0, 0, 0);
            state.columns = vec![ParseColumn::new(0, None, IndexSet::from([start]))];
        }
        let mut session_ctx = SessionContext {
            state,
            trees: &mut arenas.trees,
            products: &mut arenas.products,
            ast: &mut arenas.ast,
            gss: &mut arenas.gss,
            grammar: &self.grammar,
            actions: &self.actions,
            gotos: &self.gotos,
            error_recovery: self.config.error_recovery,
            error_recovery_timeout: self.config.error_recovery_timeout,
        };

        let restart = if deltas.len() > 1 {
            0
        } else {
            deltas
                .iter()
                .map(|delta| match delta {
                    Delta::Insert { key, .. } => {
                        Self::restart_column_for_offset(&tokens, key.range.start())
                            .saturating_sub(1)
                    }
                    Delta::Delete { key, .. } => {
                        Self::restart_column_for_offset(&tokens, key.range.start())
                    }
                })
                .min()
                .unwrap_or(0)
                .min(session_ctx.state.current_column())
        };
        let old_columns = session_ctx.state.columns_from(restart + 1);
        let old_columns_len = old_columns.len();
        let mut old_index_by_token: HashMap<Option<TokenOccurrenceId>, Vec<usize>> = HashMap::new();
        for (idx, column) in old_columns.iter().enumerate() {
            old_index_by_token
                .entry(column.token())
                .or_default()
                .push(idx);
        }

        let columns_before = session_ctx.state.columns.len();

        session_ctx.state.truncate_to_column(restart);

        let repair_start = Instant::now();

        let parse_tokens = tokens
            .iter()
            .filter(|data| data.column >= restart)
            .map(|data| ParseToken {
                entry: data.id,
                column: data.column,
                start: data.start,
                terminal: session_ctx.resolve_terminal(data),
                length: data.length,
                fingerprint: data.fingerprint,
                merge_source_terminal: None,
            })
            .collect::<Vec<_>>();

        let mut converged_at_old = None;
        let mut frontier_converged_at_old = None;
        let mut saw_recovery = false;
        let mut i = 0usize;
        while i < parse_tokens.len() {
            let token = &parse_tokens[i];
            let column = session_ctx.state.current_column();
            session_ctx.reduce_until_stable(column, token.terminal)?;
            if token.terminal == eof && !session_ctx.state.accepted().is_empty() {
                if let Some(current) = session_ctx.state.columns.last() {
                    Self::compare_current_column(
                        current,
                        &old_index_by_token,
                        &old_columns,
                        session_ctx.gss,
                        session_ctx.products,
                        session_ctx.trees,
                        true,
                        &mut frontier_converged_at_old,
                        &mut converged_at_old,
                    );
                }
                break;
            }
            if token.terminal == session_ctx.grammar.error_terminal && session_ctx.error_recovery {
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    saw_recovery = true;
                    frontier_converged_at_old = None;
                    converged_at_old = None;
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
            }
            if let Err(ParseError::NoActiveStacks { .. }) =
                session_ctx.shift_parse_token(column, token)
            {
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    saw_recovery = true;
                    frontier_converged_at_old = None;
                    converged_at_old = None;
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                return Err(ParseError::NoActiveStacks {
                    column: Some(token.column),
                });
            }
            if token.terminal == eof {
                let next_column = session_ctx.state.current_column();
                session_ctx.reduce_until_stable(next_column, token.terminal)?;
            }
            if session_ctx
                .state
                .columns
                .last()
                .is_some_and(|current| current.error_derived)
            {
                saw_recovery = true;
                frontier_converged_at_old = None;
                converged_at_old = None;
            }
            if !saw_recovery && let Some(current) = session_ctx.state.columns.last() {
                Self::compare_current_column(
                    current,
                    &old_index_by_token,
                    &old_columns,
                    session_ctx.gss,
                    session_ctx.products,
                    session_ctx.trees,
                    false,
                    &mut frontier_converged_at_old,
                    &mut converged_at_old,
                );
            }
            i += 1;
        }

        session_ctx.compact_accepted_roots();
        let roots_after = session_ctx.state.accepted().to_vec();
        if let Some(old_idx) = converged_at_old {
            let current_column = session_ctx.state.current_column();
            session_ctx.state.discard_columns_from(current_column);
            session_ctx
                .state
                .append_reused_columns(old_columns.into_iter().skip(old_idx));
        }

        working.roots.insert(uri, roots_after.clone());

        let reused = converged_at_old
            .map(|old_idx| old_columns_len.saturating_sub(old_idx))
            .unwrap_or(0);
        let reparsed = if let Some(old_idx) = converged_at_old {
            old_idx + 1
        } else {
            session_ctx.state.current_column().saturating_sub(restart)
        };
        let recovery_columns = session_ctx
            .state
            .columns
            .iter()
            .skip(restart + 1)
            .filter(|c| c.error_derived)
            .count();
        self.latest_incremental_stats.insert(
            uri,
            IncrementalParseStats {
                reparsed,
                reused,
                recovery_columns,
                converged: frontier_converged_at_old.is_some(),
            },
        );
        drop(session_ctx);

        let lower_deltas = if roots_before.is_empty() {
            vec![emit::insert_root(uri, roots_after.clone())]
        } else if !roots_after.is_empty() {
            let raw = super::diff::diff_trees(
                &arenas.products,
                &arenas.trees,
                &roots_before,
                &roots_after,
                uri,
            );
            let compacted = super::diff::compact(raw);
            if compacted.is_empty() && !deltas.is_empty() {
                emit::replace_root(uri, roots_after.clone(), roots_before.len())
            } else {
                compacted
            }
        } else {
            vec![emit::delete_root(uri, roots_before.len())]
        };

        let elapsed = repair_start.elapsed();
        let conv_flag = if frontier_converged_at_old.is_some() {
            "conv"
        } else {
            "full"
        };
        if columns_before > 1 {
            let total_suffix = columns_before.saturating_sub(restart);
            let mut fields = format!("new={} old={}", reparsed, total_suffix);
            if recovery_columns > 0 {
                fields.push_str(&format!(" recov={}", recovery_columns));
            }
            if reused > 0 {
                let old_suffix_len = total_suffix.saturating_sub(1);
                fields.push_str(&format!(" reused={}/{}", reused, old_suffix_len));
            }
            log::debug!(
                target: "Measure",
                "{} {} {} in {:?}",
                uri, fields, conv_flag, elapsed,
            );
        } else {
            log::debug!(
                target: "Measure",
                "{} new={} {} in {:?}",
                uri, reparsed, conv_flag, elapsed,
            );
        }

        Ok(lower_deltas)
    }
}

#[cfg(test)]
mod tests {
    use super::ParseError;

    #[test]
    fn parse_error_displays() {
        let e = ParseError::MissingGoto {
            state: 0,
            non_terminal: 1,
        };
        assert!(format!("{e}").contains("missing goto"));
    }
}
