//! Parser ownership and integrated immutable revision construction.

use std::{
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
            data::{ast::AstId, green::ParseErrorInfo},
            diagnostics,
            grammar::{BuildError, Grammar, Symbol},
            types::{
                AstSnapshot, AstSnapshotEntry, AstTokenSnapshotEntry, IncrementalParseStats,
                ParseSnapshotId, ParserConfig, ParserSnapshotState, SessionArenas, TokenData,
            },
        },
    },
    scheme::change::AddressChange,
    utils::Span,
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
    /// Snapshot IDs distinguish immutable publications while snapshots held by
    /// consumers remain isolated from later parser mutations.
    pub(crate) next_snapshot: ParseSnapshotId,
    pub(crate) _root: PhantomData<fn() -> Root>,
}

impl<Root> Parser<Root> {
    /// Replays lexer-authored token changes exactly. Snapshot construction is
    /// deliberately separate from replay only in time: all AST values, their
    /// anchored extents, and product membership were already created by shifts
    /// and reductions before this returns.
    pub(crate) fn derive_changes(
        &mut self,
        uri: Uri<&'static str>,
        changes: &[AddressChange<Uri<&'static str>, TokenData>],
    ) -> Result<(), ParseError>
    where
        Root: LexerRoot + Clone,
    {
        if changes.is_empty() {
            return Ok(());
        }

        let mut working = (*self.latest).clone();
        for change in changes {
            if change.address != uri {
                return Err(ParseError::NoActiveStacks { column: None });
            }
            self.parse_delta_batch(&mut working, change.clone())?;
        }
        self.latest = Arc::new(working);
        Ok(())
    }

    /// Publishes one immutable snapshot from parser-built product metadata.
    /// It never recursively walks products: root products carry complete AST
    /// membership and each AST record already carries its anchored extent.
    pub(crate) fn commit_snapshot(
        &mut self,
        uri: Uri<&'static str>,
        source: Arc<str>,
        token_coordinates: &[TokenData],
    ) -> Result<Arc<AstSnapshot>, ParseError> {
        let roots = self
            .latest
            .roots
            .get(&uri)
            .map(|roots| roots.as_ref().clone())
            .unwrap_or_default();
        let arenas = self.session_arenas.get(&uri);

        let live_ids = arenas
            .map(|arenas| {
                roots
                    .iter()
                    .map(|root| {
                        arenas
                            .products
                            .get(*root)
                            .ok_or(BuildError::MissingProduct(*root))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|products| {
                        products
                            .into_iter()
                            .flat_map(|product| product.ast_ids.iter().copied())
                            .collect::<HashSet<AstId>>()
                    })
            })
            .transpose()?
            .unwrap_or_default();

        let coordinate = token_coordinates
            .iter()
            .map(|token| (token.column, *token))
            .collect::<HashMap<_, _>>();
        let mut entries = HashMap::with_capacity(live_ids.len());
        let mut values = HashMap::with_capacity(live_ids.len());
        if let Some(arenas) = arenas {
            for id in live_ids.iter().copied() {
                let extent = arenas.ast.extent_of(id).ok_or(BuildError::MissingAst(id))?;
                let start = coordinate
                    .get(&extent.start)
                    .map(|token| token.start)
                    .unwrap_or(source.len());
                let end = coordinate
                    .get(&extent.end)
                    .map(|token| {
                        if extent.end_at_token_end {
                            token.start + token.length
                        } else {
                            token.start
                        }
                    })
                    .unwrap_or(source.len());
                let span = Span::new_uri(uri, start.min(end), start.max(end))
                    .expect("parser token coordinates are UTF-8 source boundaries");
                entries.insert(
                    id,
                    AstSnapshotEntry {
                        product: arenas
                            .ast
                            .product_of(id)
                            .ok_or(BuildError::MissingAst(id))?,
                        type_id: arenas.ast.type_of(id).ok_or(BuildError::MissingAst(id))?,
                        span,
                    },
                );
                values.insert(
                    id,
                    arenas
                        .ast
                        .cloned_erased(id)
                        .ok_or(BuildError::MissingAst(id))?,
                );
            }
        }

        let tokens = token_coordinates
            .iter()
            .map(|token| {
                let end = token.start.saturating_add(token.length).min(source.len());
                let span = Span::new_uri(uri, token.start.min(end), token.start.max(end))
                    .expect("parser token coordinates are UTF-8 source boundaries");
                (
                    token.id,
                    AstTokenSnapshotEntry {
                        terminal: token.terminal,
                        span,
                    },
                )
            })
            .collect();
        let id = self.next_snapshot;
        self.next_snapshot = self.next_snapshot.saturating_add(1);
        Ok(Arc::new(AstSnapshot::new(
            id, uri, source, entries, values, tokens,
        )))
    }

    pub fn incremental_stats(&self, uri: Uri<&'static str>) -> Option<IncrementalParseStats> {
        self.latest.incremental_stats.get(&uri).copied()
    }

    pub(crate) fn forget_document(&mut self, uri: Uri<&'static str>) {
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

    pub fn latest_parse_diagnostics(&self, uri: Uri<&'static str>) -> Vec<ParseErrorInfo> {
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
}
