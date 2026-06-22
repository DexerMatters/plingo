use std::{any::TypeId, collections::HashMap, fmt, marker::PhantomData, time::Duration};

use fluent_uri::Uri;
use indexmap::{IndexMap, IndexSet};

use crate::component::{
    lex::{LexerRoot, TokenChange},
    parse::{
        build::{ActionSet, Conflict, LR1State, LRStateId},
        data::{
            ast::{AstArena, AstBox, TokenEntryId},
            green::{GreenTree, TreeArena},
            gss::GssArena,
            product::{Product, ProductArena, ProductData, ProductId},
        },
        grammar::{Grammar, Symbol, TerminalId},
        parsing::{ParserSessionState, SessionContext},
    },
};
use crate::layer;
use crate::scheme::{
    change::{LayerChanges, ReplacementChange},
    context::Context,
    layer::{MiddleLayer, NonTopLayer, SnapshotLayer},
};
use crate::utils::RangeOrPoint;

pub(crate) mod analyze;
pub(crate) mod build;
pub(crate) mod checkpoint;
pub mod data;
pub(crate) mod diff;
pub mod generator;
pub mod grammar;
pub(crate) mod identity;
pub mod interface;
pub(crate) mod parsing;
pub(crate) mod recovery;

pub use data::ast::AstToken;
pub use data::green::{ErrorKind, ParseErrorInfo};
pub use identity::TokenFingerprint;
pub use parsing::ParseError;

#[doc(hidden)]
pub mod __macro_private;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergenceLevel {
    /// Compare only LR state sets at each column.
    LRState,
    /// Also require matching accepted product counts.
    AcceptedProducts,
    /// Deep: compare GreenId of accepted products (most robust).
    GreenIds,
}

impl Default for ConvergenceLevel {
    fn default() -> Self {
        Self::GreenIds
    }
}

#[derive(Debug, Clone)]
pub struct ParserConfig {
    pub convergence_level: ConvergenceLevel,
    pub error_recovery: bool,
    pub error_recovery_timeout: Duration,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            convergence_level: ConvergenceLevel::default(),
            error_recovery: true,
            error_recovery_timeout: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsePath {
    pub uri: Uri<&'static str>,
    pub path: Vec<usize>,
    pub range: RangeOrPoint<usize>,
}

impl fmt::Display for ParsePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.uri)?;
        for child in &self.path {
            write!(f, "/{child}")?;
        }
        write!(f, "@{}", self.range)
    }
}

#[derive(Clone, Debug)]
pub struct ParseForest {
    pub roots: Vec<ProductId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParseAddress {
    pub uri: Uri<&'static str>,
    pub parent_path: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseUnit {
    pub product: ProductId,
}

pub type ParseChange = ReplacementChange<ParseAddress, ParseUnit>;

pub type TokenOccurrenceId = usize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncrementalParseStats {
    pub restart_boundary: usize,
    pub reconverged_new_boundary: Option<usize>,
    pub reconverged_old_boundary: Option<usize>,
    pub reparsed: usize,
    pub reused: usize,
    pub recovery_columns: usize,
    pub frontier_converged: bool,
    pub semantic_reused: bool,
    pub converged: bool,
}

impl fmt::Display for ParseForest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} parse roots", self.roots.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenData {
    pub id: TokenEntryId,
    pub terminal: Option<TerminalId>,
    pub start: usize,
    pub length: usize,
    pub column: TokenOccurrenceId,
    pub fingerprint: TokenFingerprint,
}

#[derive(Clone, Default)]
pub struct ParserSnapshotState {
    pub sessions: HashMap<Uri<&'static str>, ParserSessionState>,
    pub roots: HashMap<Uri<&'static str>, Vec<ProductId>>,
}

pub(crate) struct SessionArenas {
    pub trees: TreeArena,
    pub products: ProductArena,
    pub ast: AstArena,
    pub gss: GssArena,
}

#[layer]
pub struct Parser<Root = (), Lower = ()> {
    pub grammar: Grammar,
    pub states: Vec<LR1State>,
    pub transitions: IndexMap<(LRStateId, Symbol), LRStateId>,
    pub conflicts: Vec<Conflict>,
    pub actions: Vec<ActionSet>,
    pub gotos: Vec<Option<LRStateId>>,
    pub(crate) session_arenas: HashMap<Uri<&'static str>, SessionArenas>,
    pub config: ParserConfig,

    #[snapshot]
    pub latest: ParserSnapshotState,
    pub(crate) latest_incremental_stats: HashMap<Uri<&'static str>, IncrementalParseStats>,
    pub(crate) _lower: PhantomData<(Root, Lower)>,
}

impl<Root, Lower> Parser<Root, Lower> {
    pub fn parse_tokens_at(
        &mut self,
        uri: fluent_uri::Uri<&'static str>,
        tokens: &[TokenData],
    ) -> Result<(), ParseError> {
        let arenas = self
            .session_arenas
            .entry(uri)
            .or_insert_with(|| SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: AstArena::new(uri),
                gss: GssArena::new(),
            });
        let state = self.latest.sessions.entry(uri).or_default();
        if state.columns.is_empty() {
            let start = arenas.gss.node(0, 0, 0);
            state.columns = vec![parsing::ParseColumn::new(None, IndexSet::from([start]))];
        }
        let mut ctx = SessionContext {
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
        ctx.parse_tokens(tokens)
    }

    pub fn truncate_session(&mut self, uri: fluent_uri::Uri<&'static str>, column: usize) {
        if let Some(state) = self.latest.sessions.get_mut(&uri) {
            state.truncate_to_column(column);
        }
    }

    pub fn session_state(&self, uri: fluent_uri::Uri<&'static str>) -> Option<&ParserSessionState> {
        self.latest.sessions.get(&uri)
    }

    pub fn incremental_stats(
        &self,
        uri: fluent_uri::Uri<&'static str>,
    ) -> Option<IncrementalParseStats> {
        self.latest_incremental_stats.get(&uri).copied()
    }

    pub fn latest_parse_diagnostics(
        &self,
        uri: fluent_uri::Uri<&'static str>,
    ) -> Vec<ParseErrorInfo> {
        let Some(state) = self.latest.sessions.get(&uri) else {
            return Vec::new();
        };
        let roots = self
            .latest
            .roots
            .get(&uri)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        interface::collect_parse_diagnostics(state, self.session_arenas.get(&uri), roots)
    }

    pub fn session_product(
        &self,
        uri: fluent_uri::Uri<&'static str>,
        id: ProductId,
    ) -> Option<&Product> {
        self.session_arenas.get(&uri)?.products.get(id)
    }

    pub fn session_green(
        &self,
        uri: fluent_uri::Uri<&'static str>,
        id: usize,
    ) -> Option<&GreenTree> {
        self.session_arenas.get(&uri)?.trees.get(id)
    }

    pub(crate) fn products_at_path(
        &self,
        state: &ParserSnapshotState,
        path: &ParsePath,
    ) -> Vec<ProductId> {
        let Some(roots) = state.roots.get(&path.uri) else {
            return Vec::new();
        };
        if path.path.is_empty() {
            return roots.clone();
        }
        let Some(arenas) = self.session_arenas.get(&path.uri) else {
            return Vec::new();
        };

        let mut current = roots.clone();

        for &child_idx in &path.path {
            let mut next = Vec::new();
            for pid in current {
                if let Some(Product {
                    data: ProductData::Node { children, .. },
                    ..
                }) = arenas.products.get(pid)
                {
                    if let Some(&child) = children.get(child_idx) {
                        next.push(child);
                    }
                }
            }
            current = next;
        }
        current
    }

    pub(crate) fn ast_boxes_at_path<T: 'static>(
        &self,
        state: &ParserSnapshotState,
        path: &ParsePath,
    ) -> Vec<AstBox<T>> {
        let products = self.products_at_path(state, path);
        let Some(arenas) = self.session_arenas.get(&path.uri) else {
            return Vec::new();
        };
        let target = TypeId::of::<T>();
        products
            .iter()
            .filter_map(|&pid| match arenas.products.get(pid)?.data {
                ProductData::Node { ast, ty, .. } if ty == target => {
                    Some(AstBox::new(ast, path.uri))
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn ast_tokens_at_path<T: 'static>(
        &self,
        state: &ParserSnapshotState,
        path: &ParsePath,
    ) -> Vec<AstToken<T>> {
        let products = self.products_at_path(state, path);
        let Some(arenas) = self.session_arenas.get(&path.uri) else {
            return Vec::new();
        };
        let target = TypeId::of::<T>();
        products
            .iter()
            .filter_map(|&pid| match arenas.products.get(pid)?.data {
                ProductData::Token { entry, ty, .. } if ty == target => Some(AstToken::new(entry)),
                _ => None,
            })
            .collect()
    }
}

#[layer(middle)]
impl<Root, Lower> MiddleLayer for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<Change = ParseChange> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Error = ParseError;
    type Change = TokenChange;

    fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move {
            let mut working = self.latest.clone();
            let mut lower_changes = Vec::new();

            for change in changes {
                lower_changes.extend(self.parse_delta_batch(&mut working, change).await?);
            }

            self.latest = working;
            if let Some(snapshot) = ctx.snapshot() {
                self.push_state(snapshot);
            }
            Ok(lower_changes)
        }
    }
}
