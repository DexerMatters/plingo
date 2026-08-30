use std::sync::Arc;

use crate::framework::lex::TokenOccurrenceId;
use indexmap::IndexSet;
use smallvec::SmallVec;

use super::{
    OccurrenceKey, OriginKey, ParseColumn, ParseError, ParseToken, RecoveryJournalEntry,
    ReductionKey, ReductionPath, SessionContext, checkpoint,
};
use crate::framework::parse::{
    build::Action,
    data::{
        green::ErrorKind,
        product::{Product, ProductData, ProductId},
    },
    grammar::{BuildCx, NonTerminalId, Symbol, TerminalId},
    recovery::{self, Repair},
};

impl SessionContext<'_> {
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

    fn build_cx(&mut self, boundary: TokenOccurrenceId) -> BuildCx<'_> {
        BuildCx {
            productions: &self.grammar.productions,
            trees: self.trees,
            products: self.products,
            ast: self.ast,
            lineage: &mut self.state.lineage,
            boundary: usize::try_from(boundary.0).unwrap_or(usize::MAX),
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
        anchor: TokenOccurrenceId,
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
            crate::framework::parse::data::ast::AnchoredSpan::point(
                usize::try_from(anchor.0).unwrap_or(usize::MAX),
            )
        } else {
            crate::framework::parse::data::ast::AnchoredSpan::token(
                usize::try_from(anchor.0).unwrap_or(usize::MAX),
            )
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
        // Indexed dedup: constant time against the session's recovery
        // journal instead of a linear vector scan (Cut H).
        if self.state.diagnostic_index.insert(info.clone()) {
            self.state.diagnostics.push(info);
        }
    }

    pub(super) fn reduce_cached(
        &mut self,
        production: u32,
        children: &[ProductId],
        boundary: TokenOccurrenceId,
    ) -> Result<ProductId, ParseError> {
        // Empty reductions are position-sensitive. Including their stable
        // lookahead anchor prevents a cached zero-width product from being
        // reused at a different source boundary.
        let key = ReductionKey {
            production,
            children: children.iter().copied().collect(),
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
            self.state.reduction_origins.insert(OriginKey(product), key);
        }
        Ok(product)
    }

    /// Records one product into a column's parser-cache segment.
    fn record_column_product(&mut self, product: ProductId, column: usize) -> bool {
        self.state.columns[column].push_product(product)
    }

    pub(crate) fn reduce_until_stable(
        &mut self,
        column: usize,
        lookahead: TerminalId,
        boundary: TokenOccurrenceId,
    ) -> Result<(), ParseError> {
        let mut active_nodes = std::mem::take(&mut self.active_scratch);
        loop {
            let mut changed = false;
            active_nodes.clear();
            active_nodes.extend(self.state.columns[column].active_nodes());

            for &node_id in &active_nodes {
                let Some(node) = self.gss.get_node(node_id) else {
                    return Err(ParseError::MissingGssNode { node: node_id });
                };
                let state = node.state;

                let actions: SmallVec<[Action; 8]> = self
                    .action_set(state, lookahead)
                    .inner
                    .iter()
                    .cloned()
                    .collect();
                for action in actions {
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
        self.active_scratch = active_nodes;
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
        let mut active_nodes = std::mem::take(&mut self.active_scratch);
        active_nodes.clear();
        active_nodes.extend(self.state.columns[from_column].active_nodes());
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

        for &node_id in &active_nodes {
            let Some(node) = self.gss.get_node(node_id) else {
                return Err(ParseError::MissingGssNode { node: node_id });
            };
            let state = node.state;
            let actions: SmallVec<[Action; 8]> = self
                .action_set(state, token.terminal)
                .inner
                .iter()
                .cloned()
                .collect();
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
                    if self.state.token_product(token.column).is_none() {
                        // One source token can shift through multiple GSS paths;
                        // every path must share its single token product.
                        let mut cx = self.build_cx(token.column);
                        let product = cx.alloc_token(
                            token.length,
                            token.terminal,
                            token.entry,
                            token.column.0 as usize,
                        );
                        self.state.token_products.insert(token.column, product);
                    }
                    let product = self
                        .state
                        .token_product(token.column)
                        .expect("token product was inserted");
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
        self.active_scratch = active_nodes;

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
            let product = self
                .state
                .token_product(token.column)
                .expect("token product was inserted");
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
        location: Option<TokenOccurrenceId>,
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
            location.unwrap_or(TokenOccurrenceId(u64::MAX)),
        )?;
        let mut next_active = IndexSet::new();
        let mut active_nodes = std::mem::take(&mut self.active_scratch);
        active_nodes.clear();
        active_nodes.extend(self.state.columns[from_column].active_nodes());

        for &node_id in &active_nodes {
            let Some(node) = self.gss.get_node(node_id) else {
                return Err(ParseError::MissingGssNode { node: node_id });
            };
            let state = node.state;
            let actions: SmallVec<[Action; 8]> = self
                .action_set(state, terminal)
                .inner
                .iter()
                .cloned()
                .collect();
            for action in actions {
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
        self.active_scratch = active_nodes;

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
        self.reduce_until_stable(
            next_column,
            terminal,
            location.unwrap_or(TokenOccurrenceId(u64::MAX)),
        )?;
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
        tail: &mut crate::framework::parse::parsing::TokenTail,
        trigger: TokenOccurrenceId,
    ) -> Result<Option<usize>, ParseError> {
        if !self.error_recovery {
            return Ok(None);
        }
        let column = self.state.current_column();
        let Some(result) = recovery::find_recovery(self, column, tail) else {
            return Ok(None);
        };

        if result.repairs.is_empty() {
            return Ok(None);
        }

        let start_column = self.state.current_column();
        // Plan §14: synthetic tokens carry a deterministic identity beyond
        // this command — a per-document recovery-segment serial plus a
        // within-segment action ordinal. Each repair allocates its identity
        // against the token occurrence it synthesizes or deletes.
        let recovery_segment = self.state.begin_recovery_segment();
        let mut witnesses: Vec<(TokenOccurrenceId, TerminalId)> = Vec::new();
        let mut journaled: Vec<(Repair, TokenOccurrenceId)> = Vec::new();
        let mut index = start;
        for repair in result.repairs.iter().copied() {
            match self.apply_repair(
                repair,
                index,
                tail,
                recovery_segment,
                &mut witnesses,
                &mut journaled,
            )? {
                Some(next) => index = next,
                // The tail ran out mid-repair; the attempt is abandoned.
                None => return Ok(None),
            }
        }

        if index == start && self.state.current_column() == start_column {
            return Ok(None);
        }

        // Journal the proven repair plan (plan §14): a later replay that
        // re-enters this region at an equal frontier with the same witnessed
        // tokens replays these repairs without re-running the search.
        let frontier = {
            let mut cache = crate::framework::parse::data::gss::CanonicalFrontierCache::default();
            checkpoint::frontier_checkpoint_for_column(
                &mut self.state.columns[start_column],
                self.gss,
                self.products,
                &mut cache,
            )
            .clone()
        };
        self.state.recovery_journal.insert(
            OccurrenceKey(recovery_segment),
            Arc::new(RecoveryJournalEntry {
                anchor: trigger,
                frontier,
                witnesses,
                repairs: journaled,
            }),
        );
        crate::framework::workspace::record_parser_work(&self.uri.to_string(), |work| {
            work.recovery_interval_probes += 1;
        });
        Ok(Some(index))
    }

    /// Applies one recovery repair. Returns the advanced tail index, or
    /// `None` when the tail ran out under the repair.
    fn apply_repair(
        &mut self,
        repair: Repair,
        index: usize,
        tail: &mut crate::framework::parse::parsing::TokenTail,
        recovery_segment: u64,
        witnesses: &mut Vec<(TokenOccurrenceId, TerminalId)>,
        journaled: &mut Vec<(Repair, TokenOccurrenceId)>,
    ) -> Result<Option<usize>, ParseError> {
        let column = self.state.current_column();
        match repair {
            Repair::Insert(terminal) => {
                let synthetic = tail.get(index).map(|token| token.column);
                if let Some(anchor_occ) = tail.get(index).map(|token| token.column) {
                    self.state.record_witness(anchor_occ, recovery_segment);
                    witnesses.push((anchor_occ, tail.get(index).map_or(terminal, |t| t.terminal)));
                    crate::framework::workspace::record_parser_work(
                        &self.uri.to_string(),
                        |work| {
                            work.recovery_witness_tokens += 1;
                        },
                    );
                }
                self.reduce_until_stable(
                    column,
                    terminal,
                    tail.get(index)
                        .map_or(TokenOccurrenceId(u64::MAX), |token| token.column),
                )?;
                let unexpected = tail.get(index).map(|token| Symbol::T(token.terminal));
                let location = tail.get(index).map(|token| token.column);
                let _ = synthetic_anchor(synthetic, column, self);
                self.shift_synthetic_terminal(column, terminal, unexpected, location)?;
                if let Some(anchor_occ) = synthetic {
                    journaled.push((repair, anchor_occ));
                }
            }
            Repair::Delete => {
                let Some(token) = tail.get(index) else {
                    return Ok(None);
                };
                self.state.record_witness(token.column, recovery_segment);
                witnesses.push((token.column, token.terminal));
                crate::framework::workspace::record_parser_work(&self.uri.to_string(), |work| {
                    work.recovery_witness_tokens += 1;
                });
                let _ = synthetic_anchor(Some(token.column), column, self);
                self.delete_parse_token(column, token)?;
                journaled.push((repair, token.column));
                return Ok(Some(index + 1));
            }
            Repair::Shift => {
                let Some(token) = tail.get(index) else {
                    return Ok(None);
                };
                self.state.record_witness(token.column, recovery_segment);
                witnesses.push((token.column, token.terminal));
                self.reduce_until_stable(column, token.terminal, token.column)?;
                self.shift_parse_token(column, token)?;
                journaled.push((repair, token.column));
                return Ok(Some(index + 1));
            }
            Repair::ShiftAsError => {
                let Some(token) = tail.get(index) else {
                    return Ok(None);
                };
                self.state.record_witness(token.column, recovery_segment);
                witnesses.push((token.column, token.terminal));
                let modified = ParseToken {
                    entry: token.entry,
                    column: token.column,
                    start: token.start,
                    terminal: self.grammar.error_terminal,
                    merge_source_terminal: Some(token.terminal),
                    ..*token
                };
                self.reduce_until_stable(column, self.grammar.error_terminal, token.column)?;
                self.shift_parse_token(column, &modified)?;
                journaled.push((repair, token.column));
                return Ok(Some(index + 1));
            }
        }
        Ok(Some(index))
    }

    /// Replays a journaled recovery segment (plan §14 reuse path). The
    /// caller has already proven the frontier and witness identity; this
    /// only re-walks the recorded repairs under the original segment serial
    /// so synthetic identities stay deterministic.
    pub(crate) fn replay_recovery_journal(
        &mut self,
        serial: u64,
        entry: &RecoveryJournalEntry,
        start: usize,
        tail: &mut crate::framework::parse::parsing::TokenTail,
    ) -> Result<Option<usize>, ParseError> {
        self.state.active_recovery_segment = Some(serial);
        self.state.next_synthetic_ordinal = 0;
        let mut witnesses = Vec::new();
        let mut journaled = Vec::new();
        let mut index = start;
        for (repair, anchor) in entry.repairs.iter().copied() {
            // Anchor alignment: the consumed token must be exactly the one
            // the proven plan named.
            if !matches!(repair, Repair::Insert(_)) {
                match tail.get(index) {
                    Some(token) if token.column == anchor => {}
                    _ => return Ok(None),
                }
            } else if tail.get(index).map(|token| token.column) != Some(anchor) {
                return Ok(None);
            }
            match self.apply_repair(repair, index, tail, serial, &mut witnesses, &mut journaled)? {
                Some(next) => index = next,
                None => return Ok(None),
            }
        }
        crate::framework::workspace::record_parser_work(&self.uri.to_string(), |work| {
            work.recovery_segments_reused += 1;
        });
        Ok(Some(index))
    }
}

/// Records a deterministic synthetic-token identity for one recovery
/// action, anchored at the token occurrence it synthesizes or removes.
fn synthetic_anchor(
    occurrence: Option<TokenOccurrenceId>,
    fallback_column: usize,
    ctx: &mut SessionContext<'_>,
) -> u64 {
    let anchor = occurrence
        .unwrap_or_else(|| TokenOccurrenceId(u64::try_from(fallback_column).unwrap_or(u64::MAX)));
    let identity = ctx.state.next_synthetic_identity(anchor);
    identity
}
