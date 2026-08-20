use indexmap::IndexSet;

use super::{ParseColumn, ParseError, ParseToken, ReductionKey, ReductionPath, SessionContext};
use crate::framework::parse::{
    TokenData,
    build::Action,
    data::{
        green::ErrorKind,
        product::{Product, ProductData, ProductId},
    },
    grammar::{BuildCx, NonTerminalId, Symbol, TerminalId},
    identity::eof_fingerprint,
    recovery::{self, Repair},
};

impl SessionContext<'_> {
    pub(crate) fn resolve_terminal(&self, data: &TokenData) -> TerminalId {
        match data.terminal {
            Some(terminal) => terminal,
            None if data.fingerprint == eof_fingerprint() => self.grammar.eof,
            None => self.grammar.error_terminal,
        }
    }

    fn action_set(
        &self,
        state: usize,
        terminal: TerminalId,
    ) -> &crate::framework::parse::build::ActionSet {
        &self.actions[self.grammar.action_index(state, terminal)]
    }

    fn goto_state(&self, state: usize, non_terminal: u32) -> Option<usize> {
        self.gotos[self.grammar.goto_index(state, non_terminal)]
    }

    fn build_cx(&mut self, boundary: usize) -> BuildCx<'_> {
        BuildCx {
            productions: &self.grammar.productions,
            trees: self.trees,
            products: self.products,
            ast: self.ast,
            boundary,
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
        anchor: usize,
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
        let extent = if length == 0 {
            crate::framework::parse::data::ast::AnchoredSpan::point(anchor)
        } else {
            crate::framework::parse::data::ast::AnchoredSpan::token(anchor)
        };
        let product = self
            .products
            .insert(Product::error(green).with_metadata(extent, Vec::new()));
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
    ) -> crate::framework::parse::data::green::ParseErrorInfo {
        crate::framework::parse::data::green::ParseErrorInfo {
            kind,
            node,
            length,
            unexpected,
            expected,
            recovered,
            location,
        }
    }

    fn record_diagnostic(
        &mut self,
        _column: usize,
        info: crate::framework::parse::data::green::ParseErrorInfo,
    ) {
        let diagnostics = &mut self.state.diagnostics;
        if !diagnostics.contains(&info) {
            diagnostics.push(info);
        }
    }

    pub(super) fn reduce_cached(
        &mut self,
        production: u32,
        children: &[ProductId],
        boundary: usize,
    ) -> Result<ProductId, ParseError> {
        // Empty reductions are position-sensitive. Including their stable
        // lookahead anchor prevents a cached zero-width product from being
        // reused at a different source boundary.
        let key = ReductionKey {
            production,
            children: children.to_vec(),
            boundary: children.is_empty().then_some(boundary),
        };
        if let Some(&product) = self.state.reduced_products.get(&key) {
            return Ok(product);
        }
        let build_fn = self.grammar.productions[production as usize].build;
        let mut cx = self.build_cx(boundary);
        let product = build_fn(&mut cx, production, children)?;
        self.state.reduced_products.insert(key.clone(), product);
        // A self-referential reduction cannot be recursively remapped. The
        // former origin scan excluded it as well; omitting it here makes reuse
        // reject that suffix instead of recursing indefinitely.
        if !key.children.contains(&product) {
            self.state.reduction_origins.insert(product, key);
        }
        Ok(product)
    }

    fn record_column_product(&mut self, product: ProductId, column: usize) -> bool {
        self.state.columns[column].push_product(product)
    }

    pub(crate) fn reduce_until_stable(
        &mut self,
        column: usize,
        lookahead: TerminalId,
        boundary: usize,
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
                                let product =
                                    self.reduce_cached(production, &path.products, boundary)?;
                                if self.record_column_product(product, column) {
                                    changed = true;
                                }
                                let goto_node =
                                    self.gss.node(goto_state, column, self.state.generation);
                                let inserted = self.state.columns[column].insert_active(goto_node);
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
                                let product = self.reduce_cached(0, &path.products, boundary)?;
                                if self.record_column_product(product, column) {
                                    changed = true;
                                }
                                if self.state.columns[column].push_accepted(product) {
                                    changed = true;
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
        node: crate::framework::parse::data::gss::GssNodeId,
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
                    Some(ProductData::Error { .. })
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

    pub(crate) fn shift_parse_token(
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
                token.column,
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
                        // One source token can shift through multiple GSS paths;
                        // every path must share its single token product.
                        let mut cx = self.build_cx(token.column);
                        let product = cx.alloc_token(
                            token.length,
                            token.terminal,
                            token.entry,
                            token.column,
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
            self.state.columns[next_column].push_product(product);
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
            self.state.columns[next_column].set_error_derived();
            self.reduce_until_stable(next_column, self.grammar.error_terminal, token.column)?;
        } else {
            let product = self.state.token_products[&token.column];
            self.state
                .columns
                .push(ParseColumn::new(Some(token.column), next_active));
            self.state.columns[next_column].push_product(product);
            self.state.token_columns.insert(token.column, next_column);
        }
        Ok(next_column)
    }

    fn shift_synthetic_terminal(
        &mut self,
        from_column: usize,
        terminal: TerminalId,
        unexpected: Option<Symbol>,
        location: Option<usize>,
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
            location.unwrap_or(0),
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
        self.state.columns[next_column].push_product(product);
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
        self.state.columns[next_column].set_error_derived();
        self.reduce_until_stable(next_column, terminal, location.unwrap_or(0))?;
        Ok(next_column)
    }

    pub(crate) fn delete_parse_token(
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
            token.column,
        )?;
        let active = self.state.columns[from_column].active.clone();
        self.state
            .columns
            .push(ParseColumn::new(Some(token.column), active));
        self.state.columns[next_column].push_product(product);
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
        self.state.columns[next_column].set_error_derived();
        self.state.token_columns.insert(token.column, next_column);
        self.reduce_until_stable(next_column, self.grammar.error_terminal, token.column)?;
        Ok(next_column)
    }

    pub(crate) fn recover_tokens(
        &mut self,
        start: usize,
        tokens: &[ParseToken],
    ) -> Result<Option<usize>, ParseError> {
        if !self.error_recovery {
            return Ok(None);
        }
        let column = self.state.current_column();
        let Some(result) = recovery::find_recovery(self, column, &tokens[start..]) else {
            return Ok(None);
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
                    self.reduce_until_stable(
                        column,
                        terminal,
                        tokens.get(index).map_or(0, |token| token.column),
                    )?;
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
                    self.reduce_until_stable(column, token.terminal, token.column)?;
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
                    self.reduce_until_stable(column, self.grammar.error_terminal, token.column)?;
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
}
