use std::{any::TypeId, collections::HashMap, fmt, marker::PhantomData};

use fluent_uri::Uri;
use indexmap::{IndexMap, IndexSet};

use crate::component::{
    lex::LexerRoot,
    parse::{
        build::{ActionSet, Conflict, LR1State, LRStateId},
        data::{
            AstArena, AstBox, GreenId, GreenTree, GssArena, Product, ProductArena, ProductData,
            ProductId, TokenEntryId, TreeArena,
        },
        grammar::{Grammar, Symbol, TerminalId},
        parsing::{ParserSessionState, SessionContext},
    },
};
use crate::layer;
use crate::scheme::{Context, Delta, LayerDeltas, MiddleLayer, NonTopLayer, SnapshotLayer};
use crate::utils::{RangeOrPoint, Span};

pub(crate) mod analyze;
pub(crate) mod build;
pub mod data;
pub(crate) mod diff;
pub mod grammar;
pub(crate) mod parsing;
pub mod policy;

pub use data::AstToken;
pub use parsing::ParseError;

#[doc(hidden)]
pub mod __macro_private;

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

#[derive(Clone)]
pub struct ParseForest {
    pub roots: Vec<ProductId>,
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
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetParseTokens(pub Span);

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

    #[snapshot]
    pub latest: ParserSnapshotState,
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
            state.columns = vec![parsing::ParseColumn::new(0, None, IndexSet::from([start]))];
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
        let products = &arenas.products.products;
        let trees = &arenas.trees;

        let mut current: Vec<GreenId> = roots
            .iter()
            .filter_map(|&pid| products.get(pid).map(|p| p.green))
            .collect();

        for &child_idx in &path.path {
            let mut next = Vec::new();
            for gid in current {
                if let Some(GreenTree {
                    data: data::TreeData::Node { children, .. },
                    ..
                }) = trees.get(gid)
                {
                    if let Some(&child) = children.get(child_idx) {
                        next.push(child);
                    }
                }
            }
            current = next;
        }

        let mut result = Vec::new();
        for (pid, product) in products.iter().enumerate() {
            if current.contains(&product.green) && !result.contains(&pid) {
                result.push(pid);
            }
        }
        result
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
                ProductData::Node { ast, ty } if ty == target => Some(AstBox::new(ast, path.uri)),
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
    Lower: NonTopLayer<_Key = ParsePath, _Value = ParseForest> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Key = Span;
    type Error = ParseError;
    type Value = usize;

    fn pass(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<LayerDeltas<Self::Lower>, Self::Error>> + Send {
        async move {
            let mut working = self.latest.clone();
            let mut lower_deltas = Vec::new();
            let mut deltas = deltas;

            while !deltas.is_empty() {
                let d = deltas.remove(0);
                if matches!(&d, Delta::Delete { .. })
                    && deltas.first().is_some_and(|next| {
                        matches!(next, Delta::Insert { .. })
                            && d.key().uri == next.key().uri
                            && d.key().range.start() == next.key().range.start()
                    })
                {
                    let ins = deltas.remove(0);
                    lower_deltas
                        .extend(self.parse_delta(&mut working, ins, ctx).await?);
                } else {
                    lower_deltas
                        .extend(self.parse_delta(&mut working, d, ctx).await?);
                }
            }

            self.latest = working;
            if let Some(snapshot) = ctx.snapshot() {
                self.push_state(snapshot);
            }
            Ok(lower_deltas)
        }
    }
}
