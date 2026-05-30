use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::Instant;

use indexmap::IndexSet;

use crate::component::{
    lex::{Lexer, LexerRoot},
    parse::{
        Parser, ParserSnapshotState, SessionArenas, TokenData,
        build::{Action, ActionSet},
        data::{AstArena, GssArena, GssNodeId, ProductArena, ProductId, TokenEntryId, TreeArena},
        grammar::{BuildCx, BuildError, Grammar, TerminalId},
    },
};
use crate::scheme::{Context, Delta, LayerDeltas, NonTopLayer};
use crate::utils::{RangeOrPoint, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ParseToken {
    entry: TokenEntryId,
    terminal: TerminalId,
    length: usize,
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
    NoActiveStacks { entry: TokenEntryId },
    Build(BuildError),
}

impl From<BuildError> for ParseError {
    fn from(value: BuildError) -> Self {
        Self::Build(value)
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
            Self::NoActiveStacks { entry } => write!(f, "no active parse stacks at entry {entry}"),
            Self::Build(error) => write!(f, "build error: {error:?}"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ParseColumn {
    index: usize,
    token: Option<TokenEntryId>,
    active: IndexSet<GssNodeId>,
    accepted: Vec<ProductId>,
    pub(crate) products: Vec<ProductId>,
}

impl ParseColumn {
    pub(crate) fn new(
        index: usize,
        token: Option<TokenEntryId>,
        active: IndexSet<GssNodeId>,
    ) -> Self {
        Self {
            index,
            token,
            active,
            accepted: Vec::new(),
            products: Vec::new(),
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn token(&self) -> Option<TokenEntryId> {
        self.token
    }

    pub(crate) fn active_nodes(&self) -> impl Iterator<Item = GssNodeId> + '_ {
        self.active.iter().copied()
    }

    pub fn accepted(&self) -> &[ProductId] {
        &self.accepted
    }
}

#[derive(Clone, Default)]
pub struct ParserSessionState {
    pub(crate) columns: Vec<ParseColumn>,
    pub(crate) generation: u32,
    token_columns: HashMap<TokenEntryId, usize>,
    token_products: HashMap<TokenEntryId, ProductId>,
    reduced_products: HashMap<ReductionKey, ProductId>,
}

impl ParserSessionState {
    pub fn accepted(&self) -> &[ProductId] {
        self.columns.last().map_or(&[], ParseColumn::accepted)
    }

    pub fn current_column(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }

    pub fn column_before_token(&self, token: TokenEntryId) -> Option<usize> {
        self.token_columns.get(&token).map(|c| c.saturating_sub(1))
    }

    pub fn truncate_to_column(&mut self, column: usize) {
        assert!(column < self.columns.len(), "parse column out of range");
        self.columns.truncate(column + 1);
        self.generation += 1;

        self.token_columns.retain(|_, c| *c <= column);
        self.token_products
            .retain(|t, _| self.token_columns.contains_key(t));
    }

    pub(crate) fn columns_from(&self, start: usize) -> Vec<ParseColumn> {
        self.columns.get(start..).unwrap_or_default().to_vec()
    }

    pub(crate) fn append_reused_columns(&mut self, columns: impl IntoIterator<Item = ParseColumn>) {
        for mut column in columns {
            column.index = self.columns.len();
            if let Some(token) = column.token {
                self.token_columns.insert(token, column.index);
            }
            self.columns.push(column);
        }
    }

    pub fn column(&self, index: usize) -> Option<&ParseColumn> {
        self.columns.get(index)
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
}

impl SessionContext<'_> {
    fn action_set(&self, state: usize, terminal: TerminalId) -> &ActionSet {
        &self.actions[self.grammar.action_index(state, terminal)]
    }

    fn goto_state(&self, state: usize, non_terminal: u32) -> Option<usize> {
        self.gotos[self.grammar.goto_index(state, non_terminal)]
    }

    fn gss_node_state(&self, node: GssNodeId) -> Option<usize> {
        self.gss.get_node(node).map(|n| n.state)
    }

    fn active_lr_states(&self) -> Vec<usize> {
        let mut states: Vec<usize> = self
            .state
            .columns
            .last()
            .into_iter()
            .flat_map(|c| c.active_nodes())
            .filter_map(|n| self.gss.get_node(n).map(|n| n.state))
            .collect();
        states.sort_unstable();
        states.dedup();
        states
    }

    fn build_cx(&mut self) -> BuildCx<'_> {
        BuildCx {
            productions: &self.grammar.productions,
            trees: self.trees,
            products: self.products,
            ast: self.ast,
        }
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

    fn record_column_product(&mut self, product: ProductId, column: usize) {
        let col_products = &mut self.state.columns[column].products;
        if !col_products.contains(&product) {
            col_products.push(product);
        }
    }

    fn reduce_until_stable(
        &mut self,
        column: usize,
        lookahead: TerminalId,
    ) -> Result<(), ParseError> {
        let mut worklist: VecDeque<_> = self.state.columns[column].active_nodes().collect();
        let mut consumed: IndexSet<GssNodeId> = IndexSet::new();

        while let Some(node_id) = worklist.pop_front() {
            let state = self
                .gss
                .get_node(node_id)
                .expect("GSS node must exist")
                .state;

            for action in self.action_set(state, lookahead).inner.clone() {
                match action {
                    Action::Reduce(production) => {
                        let rhs_len = self.grammar.production_rhs_len(production);
                        let lhs = self.grammar.production_lhs(production);

                        for path in self.reduce_paths(node_id, rhs_len) {
                            let pred_state = self.gss.get_node(path.predecessor).unwrap().state;
                            let Some(goto_state) = self.goto_state(pred_state, lhs) else {
                                return Err(ParseError::MissingGoto {
                                    state: pred_state,
                                    non_terminal: lhs,
                                });
                            };
                            let product = self.reduce_cached(production, &path.products)?;
                            self.record_column_product(product, column);
                            let goto_node =
                                self.gss
                                    .node(goto_state, column as u16, self.state.generation);
                            self.state.columns[column].active.insert(goto_node);
                            consumed.insert(node_id);
                            if self.gss.add_edge(
                                goto_node,
                                path.predecessor,
                                product,
                                self.state.generation,
                            ) {
                                worklist.push_back(goto_node);
                            }
                        }
                    }
                    Action::Accept => {
                        let rhs_len = self.grammar.production_rhs_len(0);
                        for path in self.reduce_paths(node_id, rhs_len) {
                            let product = self.reduce_cached(0, &path.products)?;
                            self.record_column_product(product, column);
                            let accepted = &mut self.state.columns[column].accepted;
                            if !accepted.contains(&product) {
                                accepted.push(product);
                            }
                            consumed.insert(node_id);
                        }
                    }
                    Action::Shift(_) | Action::Error => {}
                }
            }
        }

        self.state.columns[column]
            .active
            .retain(|n| !consumed.contains(n));
        Ok(())
    }

    fn reduce_paths(&self, node: GssNodeId, depth: usize) -> Vec<ReductionPath> {
        if depth == 0 {
            return vec![ReductionPath {
                predecessor: node,
                products: Vec::new(),
            }];
        }
        let mut paths = Vec::new();
        for edge in self.gss.outgoing_edges(node) {
            for mut suffix in self.reduce_paths(edge.to, depth - 1) {
                suffix.products.push(edge.product);
                paths.push(suffix);
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

        for node_id in active_nodes {
            let state = self.gss.get_node(node_id).unwrap().state;
            let actions = self.action_set(state, token.terminal).inner.clone();
            for action in actions {
                let Action::Shift(next_state) = action else {
                    continue;
                };

                if !self.state.token_products.contains_key(&token.entry) {
                    let mut cx = self.build_cx();
                    let product = cx.alloc_token(token.length, token.terminal, token.entry);
                    self.state.token_products.insert(token.entry, product);
                }
                let product = self.state.token_products[&token.entry];
                let next_node =
                    self.gss
                        .node(next_state, (from_column + 1) as u16, self.state.generation);
                if self.gss.add_edge(next_node, node_id, product, self.state.generation) {
                    next_active.insert(next_node);
                }
            }
        }

        if next_active.is_empty() {
            return Err(ParseError::NoActiveStacks { entry: token.entry });
        }

        let next_column = from_column + 1;
        let product = self.state.token_products[&token.entry];
        self.state.columns.push(ParseColumn::new(
            next_column,
            Some(token.entry),
            next_active,
        ));
        self.state.columns[next_column].products.push(product);
        self.state.token_columns.insert(token.entry, next_column);
        Ok(next_column)
    }

    pub fn parse_tokens(&mut self, tokens: &[TokenData]) -> Result<(), ParseError> {
        for data in tokens {
            let terminal = data.terminal.unwrap_or(self.grammar.eof);
            let token = ParseToken {
                entry: data.id,
                terminal,
                length: data.length,
            };
            let column = self.state.current_column();
            self.reduce_until_stable(column, token.terminal)?;
            if token.terminal == self.grammar.eof && !self.state.accepted().is_empty() {
                return Ok(());
            }
            if let Err(ParseError::NoActiveStacks { .. }) = self.shift_parse_token(column, &token) {
                return Err(ParseError::NoActiveStacks { entry: token.entry });
            }
            if token.terminal == self.grammar.eof {
                let next_column = self.state.current_column();
                self.reduce_until_stable(next_column, token.terminal)?;
            }
        }
        Ok(())
    }
}

impl<Root: LexerRoot + Clone, Lower> Parser<Root, Lower> {
    pub(crate) async fn parse_delta(
        &mut self,
        working: &mut ParserSnapshotState,
        delta: Delta<Span, usize>,
        ctx: &Context,
    ) -> Result<LayerDeltas<Lower>, ParseError>
    where
        Lower: NonTopLayer<_Key = super::ParsePath, _Value = super::ParseForest>,
    {
        let key = *delta.key();
        let uri = key.uri;
        let roots_before = working.roots.get(&uri).cloned().unwrap_or_default();
        let eof = self.grammar.eof;

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
        };
        let restart = key.range.start().min(session_ctx.state.current_column());

        let old_columns = session_ctx.state.columns_from(restart + 1);
        let old_active: Vec<Vec<usize>> = old_columns
            .iter()
            .map(|col| {
                let mut states: Vec<usize> = col
                    .active_nodes()
                    .filter_map(|n| session_ctx.gss_node_state(n))
                    .collect();
                states.sort_unstable();
                states.dedup();
                states
            })
            .collect();

        let old_column_products: Vec<Vec<ProductId>> = session_ctx
            .state
            .columns[restart..]
            .iter()
            .map(|c| c.products.clone())
            .collect();

        let columns_before = session_ctx.state.columns.len();
        let repair_start = Instant::now();

        session_ctx.state.truncate_to_column(restart);

        let span = Span {
            uri,
            range: RangeOrPoint::Range(restart, usize::MAX),
        };
        let tokens: Vec<TokenData> = ctx
            .post::<Lexer<Root, Self>, super::GetParseTokens>(super::GetParseTokens(span))
            .await
            .map_err(|_| ParseError::Build(BuildError::MissingProduct(0)))?;

        let mut converged_at_old = None;
        for data in &tokens {
            let terminal = data.terminal.unwrap_or(eof);
            let token = ParseToken {
                entry: data.id,
                terminal,
                length: data.length,
            };
            let column = session_ctx.state.current_column();
            session_ctx.reduce_until_stable(column, token.terminal)?;
            if token.terminal == eof && !session_ctx.state.accepted().is_empty() {
                if let Some(old_idx) = old_active
                    .iter()
                    .position(|o| *o == session_ctx.active_lr_states())
                {
                    converged_at_old = Some(old_idx);
                }
                break;
            }
            if let Err(ParseError::NoActiveStacks { .. }) =
                session_ctx.shift_parse_token(column, &token)
            {
                return Err(ParseError::NoActiveStacks { entry: token.entry });
            }
            if token.terminal == eof {
                let next_column = session_ctx.state.current_column();
                session_ctx.reduce_until_stable(next_column, token.terminal)?;
            }
            if let Some(old_idx) = old_active
                .iter()
                .position(|o| *o == session_ctx.active_lr_states())
            {
                converged_at_old = Some(old_idx);
                break;
            }
        }

        if let Some(old_idx) = converged_at_old {
            session_ctx
                .state
                .append_reused_columns(old_columns.into_iter().skip(old_idx + 1));
        }

        let roots_after = session_ctx.state.accepted().to_vec();
        working.roots.insert(uri, roots_after.clone());

        let reparsed = if converged_at_old.is_some() {
            converged_at_old.unwrap() + 1
        } else {
            session_ctx.state.current_column().saturating_sub(restart)
        };
        drop(session_ctx);

        let lower_deltas = if roots_before != roots_after {
            if roots_before.is_empty() {
                vec![Delta::Insert {
                    key: super::ParsePath {
                        uri,
                        path: Vec::new(),
                        range: RangeOrPoint::Point(0),
                    },
                    value: super::ParseForest {
                        roots: roots_after.clone(),
                    },
                }]
            } else if !roots_after.is_empty() {
                let raw = super::diff::diff_trees(
                    &arenas.products,
                    &arenas.trees,
                    &roots_before,
                    &roots_after,
                    uri,
                );
                super::diff::compact(raw)
            } else {
                vec![Delta::Delete {
                    key: super::ParsePath {
                        uri,
                        path: Vec::new(),
                        range: RangeOrPoint::from_range(0, roots_before.len()),
                    },
                }]
            }
        } else {
            Vec::new()
        };

        let elapsed = repair_start.elapsed();
        let conv_flag = if converged_at_old.is_some() { "conv" } else { "full" };
        if columns_before > 1 {
            let total_suffix = columns_before.saturating_sub(restart);
            log::debug!(
                target: "Measure",
                "{} reparsed={}/{} ({:.0}%) converged={} in {:?}",
                uri,
                reparsed,
                total_suffix,
                if total_suffix > 0 { reparsed as f64 * 100.0 / total_suffix as f64 } else { 0.0 },
                conv_flag,
                elapsed,
            );
        } else {
            log::debug!(
                target: "Measure",
                "{} initial={} cols converged={} in {:?}",
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
