//! Parser ownership, direct parsing, and runtime layer integration.

use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;
use indexmap::IndexMap;

use crate::{
    component::{
        lex::LexerRoot,
        parse::{
            ParseError,
            build::{ActionSet, Conflict, LR1State, LRStateId},
            data::{
                ast::AstBox,
                green::{GreenTree, ParseErrorInfo},
                product::{Product, ProductData, ProductId},
            },
            diagnostics,
            grammar::{BuildError, Grammar, Symbol},
            parsing::ParserSessionState,
            types::{
                AstView, AstViewEntry, IncrementalParseStats, ParserConfig, ParserSnapshotState,
                SessionArenas, TokenData,
            },
        },
    },
    scheme::change::AddressChange,
};

#[derive(Clone)]
pub struct Parser<Root = ()> {
    pub grammar: Grammar,
    pub states: Vec<LR1State>,
    pub transitions: IndexMap<(LRStateId, Symbol), LRStateId>,
    pub conflicts: Vec<Conflict>,
    pub actions: Vec<ActionSet>,
    pub gotos: Vec<Option<LRStateId>>,
    pub(crate) session_arenas: HashMap<Uri<&'static str>, SessionArenas>,
    pub config: ParserConfig,

    pub latest: Arc<ParserSnapshotState>,
    pub(crate) _root: PhantomData<fn() -> Root>,
}

impl<Root> Parser<Root> {
    /// Replays the lexer-authored token changes in order. A parser revision
    /// never compares complete token manifests or synthesizes a replacement:
    /// every replay starts from the exact token checkpoint supplied by lexer
    /// convergence and runs until parser frontier convergence or EOF.
    pub(crate) fn derive_changes(
        &mut self,
        uri: fluent_uri::Uri<&'static str>,
        changes: &[AddressChange<Uri<&'static str>, TokenData>],
    ) -> Result<Vec<ProductId>, ParseError>
    where
        Root: LexerRoot + Clone,
    {
        if changes.is_empty() {
            return Ok(self
                .latest
                .roots
                .get(&uri)
                .map(|roots| roots.as_ref().clone())
                .unwrap_or_default());
        }

        let mut working = (*self.latest).clone();
        for change in changes {
            if change.address != uri {
                return Err(ParseError::NoActiveStacks { column: None });
            }
            self.parse_delta_batch(&mut working, change.clone())?;
        }
        let roots = working
            .roots
            .get(&uri)
            .map(|roots| roots.as_ref().clone())
            .unwrap_or_default();
        self.latest = Arc::new(working);
        Ok(roots)
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

    pub(crate) fn forget_document(&mut self, uri: fluent_uri::Uri<&'static str>) {
        let latest = Arc::make_mut(&mut self.latest);
        latest.sessions.remove(&uri);
        latest.roots.remove(&uri);
        latest.tokens.remove(&uri);
        latest.incremental_stats.remove(&uri);
        self.session_arenas.remove(&uri);
    }

    pub(crate) fn reset_documents(&mut self) {
        self.latest = Arc::new(ParserSnapshotState::default());
        self.session_arenas.clear();
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

    pub(crate) fn ast_view<T>(
        &self,
        state: &ParserSnapshotState,
        uri: Uri<&'static str>,
    ) -> Result<AstView<T>, ParseError>
    where
        T: Send + Sync + 'static,
    {
        let Some(roots) = state.roots.get(&uri) else {
            return Ok(AstView::empty(uri));
        };
        if roots.is_empty() {
            return Ok(AstView::empty(uri));
        }
        let Some(arenas) = self.session_arenas.get(&uri) else {
            return Err(BuildError::MissingProduct(roots[0]).into());
        };

        fn visit<T>(
            arenas: &SessionArenas,
            uri: Uri<&'static str>,
            product_id: ProductId,
            target: TypeId,
            visited: &mut HashSet<ProductId>,
            entries: &mut Vec<AstViewEntry<T>>,
        ) -> Result<(), ParseError>
        where
            T: Send + Sync + 'static,
        {
            if !visited.insert(product_id) {
                return Ok(());
            }
            let product = arenas
                .products
                .get(product_id)
                .ok_or(BuildError::MissingProduct(product_id))?;
            match &product.data {
                ProductData::Node { ast, ty, children } => {
                    if *ty == target {
                        let value = match arenas.ast.cloned_arc::<T>(*ast) {
                            Some(value) => value,
                            None if arenas.ast.contains(*ast) => {
                                return Err(BuildError::TypeMismatch {
                                    product: product_id,
                                }
                                .into());
                            }
                            None => return Err(BuildError::MissingAst(*ast).into()),
                        };
                        entries.push(AstViewEntry {
                            ast_box: AstBox::new(*ast, uri),
                            product: product_id,
                            value,
                        });
                    }
                    for &child in children {
                        visit(arenas, uri, child, target, visited, entries)?;
                    }
                }
                ProductData::Error { children } => {
                    for &child in children {
                        visit(arenas, uri, child, target, visited, entries)?;
                    }
                }
                ProductData::Token { .. } => {}
            }
            Ok(())
        }

        let target = TypeId::of::<T>();
        let mut visited = HashSet::new();
        let mut entries = Vec::new();
        for &root in roots.iter() {
            visit(arenas, uri, root, target, &mut visited, &mut entries)?;
        }
        let typed_roots = roots
            .iter()
            .filter_map(|root| {
                entries
                    .iter()
                    .find(|entry| entry.product == *root)
                    .map(|entry| entry.ast_box)
            })
            .collect();
        Ok(AstView::new(uri, typed_roots, entries))
    }
}
