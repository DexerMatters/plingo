use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use indexmap::IndexSet;

use crate::component::parse::{TokenOccurrenceId, recovery};
use crate::component::{
    lex::{GetVisibleTokenBatch, Lexer, LexerRoot},
    parse::{
        IncrementalParseStats, Parser, ParserSnapshotState, SessionArenas, TokenData,
        build::{Action, ActionSet},
        checkpoint::{self, BoundaryCheckpoint},
        data::{
            AstArena, ErrorKind, GssArena, GssNodeId, ParseErrorInfo, Product, ProductArena,
            ProductId, TokenEntryId, TreeArena,
        },
        emit,
        grammar::{BuildCx, BuildError, Grammar, NonTerminalId, Symbol, TerminalId},
        identity::TokenFingerprint,
        incremental::ReplayPlan,
        recovery::Repair,
    },
};
use crate::scheme::{Context, Delta, LayerDeltas, NonTopLayer};
use crate::utils::Span;

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
    token: Option<TokenOccurrenceId>,
    base_active: IndexSet<GssNodeId>,
    active: IndexSet<GssNodeId>,
    accepted: Vec<ProductId>,
    pub(crate) products: Vec<ProductId>,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    pub(crate) error_derived: bool,
}

impl ParseColumn {
    pub(crate) fn new(token: Option<TokenOccurrenceId>, active: IndexSet<GssNodeId>) -> Self {
        Self {
            token,
            base_active: active.clone(),
            active,
            accepted: Vec::new(),
            products: Vec::new(),
            diagnostics: Vec::new(),
            error_derived: false,
        }
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
        for column in columns {
            let index = self.columns.len();
            if let Some(token) = column.token {
                self.token_columns.insert(token, index);
                if !column.error_derived {
                    if let Some(&product) = column.products.first() {
                        self.token_products.insert(token, product);
                    }
                }
            }
            for diagnostic in &column.diagnostics {
                if !self.diagnostics.contains(diagnostic) {
                    self.diagnostics.push(diagnostic.clone());
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
            self.state
                .columns
                .push(ParseColumn::new(Some(token.column), next_active));
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
            self.state
                .columns
                .push(ParseColumn::new(Some(token.column), next_active));
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

        self.state.columns.push(ParseColumn::new(None, next_active));
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
            .push(ParseColumn::new(Some(token.column), active));
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
    pub(crate) async fn parse_delta_batch(
        &mut self,
        working: &mut ParserSnapshotState,
        uri: fluent_uri::Uri<&'static str>,
        _deltas: &[Delta<Span, usize>],
        ctx: &Context,
    ) -> Result<LayerDeltas<Lower>, ParseError>
    where
        Lower: NonTopLayer<_Key = super::ParsePath, _Value = super::ParseForest>,
    {
        let total_start = Instant::now();
        let roots_before = working.roots.get(&uri).cloned().unwrap_or_default();
        let fetch_batch_start = Instant::now();
        let batch = ctx
            .post::<Lexer<Root, Self>, GetVisibleTokenBatch>(GetVisibleTokenBatch(uri.clone()))
            .await
            .map_err(|_| ParseError::Build(BuildError::MissingProduct(0)))?;
        let fetch_batch_elapsed = fetch_batch_start.elapsed();
        let Some(batch) = batch else {
            self.latest_incremental_stats
                .insert(uri.clone(), IncrementalParseStats::default());
            log::debug!(
                target: "Measure",
                "parse {} total={:?} fetch_batch={:?} batch=none",
                uri,
                total_start.elapsed(),
                fetch_batch_elapsed,
            );
            return Ok(Vec::new());
        };

        let plan_start = Instant::now();
        let plan = ReplayPlan::from_batch(batch.clone());
        let plan_elapsed = plan_start.elapsed();

        if !plan.batch.is_changed() {
            let sessions = working.sessions.get(&uri);
            let current_boundary = sessions.map(|state| state.current_column()).unwrap_or(0);
            let recovery_columns = sessions
                .map(|state| {
                    state
                        .columns
                        .iter()
                        .skip(1)
                        .filter(|column| column.error_derived)
                        .count()
                })
                .unwrap_or(0);
            self.latest_incremental_stats.insert(
                uri,
                IncrementalParseStats {
                    restart_boundary: plan.restart_boundary,
                    reconverged_new_boundary: batch.new_tokens.len().checked_sub(1),
                    reconverged_old_boundary: batch.old_tokens.len().checked_sub(1),
                    reparsed: 0,
                    reused: current_boundary,
                    recovery_columns,
                    frontier_converged: true,
                    semantic_reused: true,
                    converged: true,
                },
            );
            log::debug!(
                target: "Measure",
                "parse {} total={:?} fetch_batch={:?} plan={:?} changed=false restart={} reused={} recovery_columns={} old_tokens={} new_tokens={} prefix={} suffix={}",
                uri,
                total_start.elapsed(),
                fetch_batch_elapsed,
                plan_elapsed,
                plan.restart_boundary,
                current_boundary,
                recovery_columns,
                batch.old_tokens.len(),
                batch.new_tokens.len(),
                batch.prefix_len,
                batch.suffix_len,
            );
            return Ok(Vec::new());
        }

        let session_setup_start = Instant::now();
        let arenas = self
            .session_arenas
            .entry(uri.clone())
            .or_insert_with(|| SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: AstArena::new(uri.clone()),
                gss: GssArena::new(),
            });
        let state = working.sessions.entry(uri.clone()).or_default();
        if state.columns.is_empty() {
            let start = arenas.gss.node(0, 0, 0);
            state.columns = vec![ParseColumn::new(None, IndexSet::from([start]))];
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
        let session_setup_elapsed = session_setup_start.elapsed();

        let restart_boundary = plan
            .restart_boundary
            .min(session_ctx.state.current_column());
        let old_reuse_start = plan.old_reuse_start.min(session_ctx.state.columns.len());
        let checkpoint_start = Instant::now();
        let old_suffix_columns = session_ctx.state.columns_from(old_reuse_start);
        let old_checkpoints = old_suffix_columns
            .iter()
            .map(|column| {
                checkpoint::checkpoint_for_column(
                    column,
                    session_ctx.gss,
                    session_ctx.products,
                    session_ctx.trees,
                )
            })
            .collect::<Vec<_>>();
        let checkpoint_elapsed = checkpoint_start.elapsed();
        let old_suffix_len = old_suffix_columns.len();

        let truncate_start = Instant::now();
        session_ctx.state.truncate_to_column(restart_boundary);
        let truncate_elapsed = truncate_start.elapsed();

        let token_materialization_start = Instant::now();
        let parse_tokens = plan
            .replay_tokens()
            .iter()
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
        let token_materialization_elapsed = token_materialization_start.elapsed();

        fn maybe_reuse_suffix<'a>(
            plan: &ReplayPlan,
            old_suffix_columns: &[ParseColumn],
            old_checkpoints: &[BoundaryCheckpoint],
            session_ctx: &mut SessionContext<'a>,
            current_boundary: usize,
            frontier_converged: &mut bool,
            semantic_reused: &mut bool,
            reconverged_new_boundary: &mut Option<usize>,
            reconverged_old_boundary: &mut Option<usize>,
        ) -> Result<bool, ParseError> {
            if current_boundary < plan.new_reuse_start {
                return Ok(false);
            }
            let Some(old_boundary) = plan.translated_old_boundary(current_boundary) else {
                return Ok(false);
            };
            let old_index = old_boundary.saturating_sub(plan.old_reuse_start);
            let Some(old_checkpoint) = old_checkpoints.get(old_index) else {
                return Ok(false);
            };
            let Some(current_column) = session_ctx.state.column(current_boundary) else {
                return Ok(false);
            };
            let current_checkpoint = checkpoint::checkpoint_for_column(
                current_column,
                session_ctx.gss,
                session_ctx.products,
                session_ctx.trees,
            );
            if current_checkpoint.frontier_key == old_checkpoint.frontier_key {
                *frontier_converged = true;
            }
            if current_checkpoint == *old_checkpoint {
                *semantic_reused = true;
                *reconverged_new_boundary = Some(current_boundary);
                *reconverged_old_boundary = Some(old_boundary);
                let reused_columns = old_suffix_columns
                    .iter()
                    .skip(old_index)
                    .cloned()
                    .collect::<Vec<_>>();
                session_ctx.state.discard_columns_from(current_boundary);
                session_ctx.state.append_reused_columns(reused_columns);
                return Ok(true);
            }
            Ok(false)
        }

        let eof = self.grammar.eof;
        let mut frontier_converged = false;
        let mut semantic_reused = false;
        let mut reconverged_new_boundary = None;
        let mut reconverged_old_boundary = None;
        let replay_start = Instant::now();
        let mut reduce_elapsed = Duration::default();
        let mut shift_elapsed = Duration::default();
        let mut recover_elapsed = Duration::default();
        let mut converge_elapsed = Duration::default();
        let mut i = 0usize;
        while i < parse_tokens.len() {
            let token = &parse_tokens[i];
            let column = session_ctx.state.current_column();
            let reduce_start = Instant::now();
            session_ctx.reduce_until_stable(column, token.terminal)?;
            reduce_elapsed += reduce_start.elapsed();
            if token.terminal == eof && !session_ctx.state.accepted().is_empty() {
                session_ctx.compact_accepted_roots();
                let current_boundary = session_ctx.state.current_column();
                let converge_start = Instant::now();
                if maybe_reuse_suffix(
                    &plan,
                    &old_suffix_columns,
                    &old_checkpoints,
                    &mut session_ctx,
                    current_boundary,
                    &mut frontier_converged,
                    &mut semantic_reused,
                    &mut reconverged_new_boundary,
                    &mut reconverged_old_boundary,
                )? {
                    converge_elapsed += converge_start.elapsed();
                    break;
                }
                converge_elapsed += converge_start.elapsed();
                break;
            }

            if token.terminal == session_ctx.grammar.error_terminal && session_ctx.error_recovery {
                let recover_start = Instant::now();
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    recover_elapsed += recover_start.elapsed();
                    let current_boundary = session_ctx.state.current_column();
                    let converge_start = Instant::now();
                    if maybe_reuse_suffix(
                        &plan,
                        &old_suffix_columns,
                        &old_checkpoints,
                        &mut session_ctx,
                        current_boundary,
                        &mut frontier_converged,
                        &mut semantic_reused,
                        &mut reconverged_new_boundary,
                        &mut reconverged_old_boundary,
                    )? {
                        converge_elapsed += converge_start.elapsed();
                        break;
                    }
                    converge_elapsed += converge_start.elapsed();
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
            }

            let shift_start = Instant::now();
            if let Err(ParseError::NoActiveStacks { .. }) = session_ctx.shift_parse_token(column, token) {
                shift_elapsed += shift_start.elapsed();
                let recover_start = Instant::now();
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    recover_elapsed += recover_start.elapsed();
                    let current_boundary = session_ctx.state.current_column();
                    let converge_start = Instant::now();
                    if maybe_reuse_suffix(
                        &plan,
                        &old_suffix_columns,
                        &old_checkpoints,
                        &mut session_ctx,
                        current_boundary,
                        &mut frontier_converged,
                        &mut semantic_reused,
                        &mut reconverged_new_boundary,
                        &mut reconverged_old_boundary,
                    )? {
                        converge_elapsed += converge_start.elapsed();
                        break;
                    }
                    converge_elapsed += converge_start.elapsed();
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
                return Err(ParseError::NoActiveStacks {
                    column: Some(token.column),
                });
            }
            shift_elapsed += shift_start.elapsed();

            if token.terminal == eof {
                let next_column = session_ctx.state.current_column();
                let reduce_start = Instant::now();
                session_ctx.reduce_until_stable(next_column, token.terminal)?;
                reduce_elapsed += reduce_start.elapsed();
            }

            let current_boundary = session_ctx.state.current_column();
            let converge_start = Instant::now();
            if maybe_reuse_suffix(
                &plan,
                &old_suffix_columns,
                &old_checkpoints,
                &mut session_ctx,
                current_boundary,
                &mut frontier_converged,
                &mut semantic_reused,
                &mut reconverged_new_boundary,
                &mut reconverged_old_boundary,
            )? {
                converge_elapsed += converge_start.elapsed();
                break;
            }
            converge_elapsed += converge_start.elapsed();
            i += 1;
        }
        let replay_elapsed = replay_start.elapsed();
        let replay_misc_elapsed = replay_elapsed
            .saturating_sub(reduce_elapsed + shift_elapsed + recover_elapsed + converge_elapsed);

        let compact_start = Instant::now();
        session_ctx.compact_accepted_roots();
        let compact_elapsed = compact_start.elapsed();
        let roots_after = session_ctx.state.accepted().to_vec();
        working.roots.insert(uri.clone(), roots_after.clone());

        let reused = reconverged_old_boundary
            .map(|old_boundary| {
                old_suffix_len.saturating_sub(old_boundary.saturating_sub(old_reuse_start))
            })
            .unwrap_or(0);
        let reparsed = reconverged_new_boundary
            .map(|new_boundary| new_boundary.saturating_sub(restart_boundary))
            .unwrap_or_else(|| {
                session_ctx
                    .state
                    .current_column()
                    .saturating_sub(restart_boundary)
            });
        let recovery_columns = session_ctx
            .state
            .columns
            .iter()
            .skip(restart_boundary.saturating_add(1))
            .filter(|c| c.error_derived)
            .count();
        let stats_start = Instant::now();
        self.latest_incremental_stats.insert(
            uri.clone(),
            IncrementalParseStats {
                restart_boundary,
                reconverged_new_boundary,
                reconverged_old_boundary,
                reparsed,
                reused,
                recovery_columns,
                frontier_converged,
                semantic_reused,
                converged: frontier_converged,
            },
        );
        let stats_elapsed = stats_start.elapsed();
        drop(session_ctx);

        let diff_start = Instant::now();
        let lower_deltas = if roots_before.is_empty() {
            if roots_after.is_empty() {
                Vec::new()
            } else {
                vec![emit::insert_root(uri.clone(), roots_after.clone())]
            }
        } else if roots_after.is_empty() {
            vec![emit::delete_root(uri.clone(), roots_before.len())]
        } else {
            super::diff::compact(super::diff::diff_trees(
                &arenas.products,
                &arenas.trees,
                &roots_before,
                &roots_after,
                uri.clone(),
            ))
        };
        let diff_elapsed = diff_start.elapsed();

        let total_elapsed = total_start.elapsed();
        log::debug!(
            target: "Measure",
            "parse {} total={:?} fetch_batch={:?} plan={:?} session={:?} checkpoints={:?} truncate={:?} tokens={:?} replay={:?} reduce={:?} shift={:?} recover={:?} converge={:?} replay_misc={:?} compact={:?} stats={:?} diff={:?} restart={} reparsed={} reused={} old_suffix={} replay_tokens={} frontier={} semantic={} old_tokens={} new_tokens={} prefix={} suffix={}",
            uri,
            total_elapsed,
            fetch_batch_elapsed,
            plan_elapsed,
            session_setup_elapsed,
            checkpoint_elapsed,
            truncate_elapsed,
            token_materialization_elapsed,
            replay_elapsed,
            reduce_elapsed,
            shift_elapsed,
            recover_elapsed,
            converge_elapsed,
            replay_misc_elapsed,
            compact_elapsed,
            stats_elapsed,
            diff_elapsed,
            restart_boundary,
            reparsed,
            reused,
            old_suffix_len,
            parse_tokens.len(),
            frontier_converged,
            semantic_reused,
            batch.old_tokens.len(),
            batch.new_tokens.len(),
            batch.prefix_len,
            batch.suffix_len,
        );

        Ok(lower_deltas)
    }
}
