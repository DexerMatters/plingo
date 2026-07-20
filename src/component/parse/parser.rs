//! Parser ownership, direct parsing, and runtime layer integration.

use std::{any::TypeId, collections::HashMap, future::Future, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;
use indexmap::{IndexMap, IndexSet};

use crate::{
    component::{
        lex::LexerRoot,
        parse::{
            ParseError,
            build::{ActionSet, Conflict, LR1State, LRStateId},
            data::{
                ast::{AstArena, AstBox, AstToken},
                green::{GreenTree, ParseErrorInfo, TreeArena},
                gss::GssArena,
                product::{Product, ProductArena, ProductData, ProductId},
            },
            diagnostics,
            grammar::{Grammar, Symbol},
            parsing::{self, ParserSessionState, SessionContext},
            types::{
                IncrementalParseStats, ParseAddress, ParsePath, ParseUnit, ParserConfig,
                ParserSnapshotState, SessionArenas, TokenData,
            },
        },
    },
    layer,
    scheme::{
        change::{ChangeSet, LayerChanges},
        context::Context,
        layer::{MiddleLayer, NonTopLayer, SnapshotLayer},
    },
};

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
    pub latest: Arc<ParserSnapshotState>,
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
        let latest = Arc::make_mut(&mut self.latest);
        let state = Arc::make_mut(latest.sessions.entry(uri).or_default());
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
        ctx.parse_tokens(tokens)?;
        latest.tokens.insert(uri, Arc::new(tokens.to_vec()));
        Ok(())
    }

    pub fn truncate_session(&mut self, uri: fluent_uri::Uri<&'static str>, column: usize) {
        if let Some(state) = Arc::make_mut(&mut self.latest).sessions.get_mut(&uri) {
            Arc::make_mut(state).truncate_to_column(column);
        }
    }

    pub fn session_state(&self, uri: fluent_uri::Uri<&'static str>) -> Option<&ParserSessionState> {
        self.latest.sessions.get(&uri).map(Arc::as_ref)
    }

    pub fn incremental_stats(
        &self,
        uri: fluent_uri::Uri<&'static str>,
    ) -> Option<IncrementalParseStats> {
        self.latest.incremental_stats.get(&uri).copied()
    }

    pub(crate) fn snapshot_state(
        &self,
        snapshot: Option<crate::scheme::context::SnapshotId>,
    ) -> Result<&ParserSnapshotState, ParseError> {
        match snapshot {
            Some(snapshot) => self
                .state(Some(snapshot))
                .ok_or(ParseError::MissingSnapshot(snapshot)),
            None => Ok(self.latest_state()),
        }
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
            .map(|roots| roots.as_slice())
            .unwrap_or(&[]);
        diagnostics::collect_parse_diagnostics(state, self.session_arenas.get(&uri), roots)
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
            return roots.as_ref().clone();
        }
        let Some(arenas) = self.session_arenas.get(&path.uri) else {
            return Vec::new();
        };

        let mut current = roots.as_ref().clone();

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
    Lower: NonTopLayer<Address = ParseAddress, Unit = ParseUnit> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Error = ParseError;
    type Address = Uri<&'static str>;
    type Unit = TokenData;

    fn pass(
        &mut self,
        _ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move {
            let revision = changes.revision;
            if changes.changes.is_empty() {
                self.push_state(revision.target);
                return Ok(ChangeSet::empty(revision));
            }
            let mut working = (*self.latest).clone();
            let mut lower_changes = Vec::new();

            for change in changes.changes {
                lower_changes.extend(self.parse_delta_batch(&mut working, change).await?);
            }

            self.latest = Arc::new(working);
            self.push_state(revision.target);
            Ok(ChangeSet {
                revision,
                changes: lower_changes,
            })
        }
    }
}
