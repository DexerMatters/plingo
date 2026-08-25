//! Parser ownership and integrated immutable revision construction.

use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;
use indexmap::IndexMap;
use ropey::Rope;

use crate::{
    framework::{
        lex::LexerRoot,
        parse::{
            ParseError,
            build::{ActionSet, Conflict, LR1State, LRStateId},
            data::{ast::AstId, green::ParseErrorInfo},
            diagnostics,
            grammar::{BuildError, Grammar, Symbol},
            parsing::ReplayPlan,
            types::{
                AstSnapshot, AstSnapshotEntry, AstTokenSnapshotEntry, IncrementalParseStats,
                ParseSnapshotId, ParserConfig, ParserSnapshotState, ParserTokenDocument,
                SessionArenas, TokenData,
            },
        },
    },
    utils::Span,
};

#[derive(Clone)]
pub struct Parser<Root = ()> {
    pub(crate) grammar: Grammar,
    pub(crate) states: Vec<LR1State>,
    pub(crate) transitions: IndexMap<(LRStateId, Symbol), LRStateId>,
    pub(crate) conflicts: Vec<Conflict>,
    pub(crate) actions: Vec<ActionSet>,
    pub(crate) gotos: Vec<Option<LRStateId>>,
    pub(crate) session_arenas: HashMap<Uri<String>, SessionArenas>,
    pub(crate) config: ParserConfig,

    pub(crate) latest: Arc<ParserSnapshotState>,
    /// Snapshot IDs distinguish immutable publications while snapshots held by
    /// consumers remain isolated from later parser mutations.
    pub(crate) next_snapshot: ParseSnapshotId,
    pub(crate) _root: PhantomData<fn() -> Root>,
}

impl<Root> Parser<Root> {
    /// Replays the exact lexer-authored structural patch against persistent
    /// parser token roots. Ordinary replay decodes only tokens between the
    /// restart checkpoint and convergence; no document token vector is built.
    pub(crate) fn derive_token_delta(
        &mut self,
        uri: Uri<String>,
        delta: &crate::framework::lex::TokenPatch,
        new_tokens: Arc<ParserTokenDocument>,
    ) -> Result<(), ParseError>
    where
        Root: LexerRoot + Clone,
    {
        let old_tokens = self.latest.tokens.get(&uri).cloned();
        if delta.structure_unchanged() && old_tokens.is_some() {
            let changed_column = delta
                .updated
                .iter()
                .filter_map(|occurrence| new_tokens.rank_of_occurrence(occurrence.0 as usize))
                .min()
                .unwrap_or(0);
            let retained_columns = self
                .latest
                .sessions
                .get(&uri)
                .map(|session| session.columns.len())
                .unwrap_or_default();
            crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
                work.restart_columns += changed_column as u64;
                work.columns_replayed += u64::from(!delta.updated.is_empty());
                work.columns_reused += retained_columns.saturating_sub(1) as u64;
            });
            let mut latest = (*self.latest).clone();
            latest.tokens.insert(uri, new_tokens);
            self.latest = Arc::new(latest);
            return Ok(());
        }

        let plan = ReplayPlan::from_token_patch(old_tokens, Arc::clone(&new_tokens), delta);
        let mut working = (*self.latest).clone();
        match self.parse_delta_batch(&mut working, uri.clone(), plan) {
            Ok(()) => {
                self.latest = Arc::new(working);
                Ok(())
            }
            Err(ParseError::NoActiveStacks { .. }) => {
                self.rebuild_document_after_replay_failure(uri, new_tokens)
            }
            Err(error) => Err(error),
        }
    }

    fn rebuild_document_after_replay_failure(
        &mut self,
        uri: Uri<String>,
        new_tokens: Arc<ParserTokenDocument>,
    ) -> Result<(), ParseError>
    where
        Root: LexerRoot + Clone,
    {
        crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
            work.full_rebuild_fallbacks += 1;
        });
        self.session_arenas.remove(&uri);
        let mut working = (*self.latest).clone();
        working.sessions.remove(&uri);
        working.roots.remove(&uri);
        working.tokens.remove(&uri);
        working.incremental_stats.remove(&uri);
        let patch = crate::framework::lex::TokenPatch {
            old_structure: crate::framework::lex::TokenStructureRevisionId(0),
            new_structure: crate::framework::lex::TokenStructureRevisionId(1),
            order_splices: Arc::from([]),
            inserted: Arc::from([]),
            updated: Arc::from([]),
            removed: Arc::from([]),
        };
        let plan = ReplayPlan::from_token_patch(None, new_tokens, &patch);
        self.parse_delta_batch(&mut working, uri, plan)?;
        self.latest = Arc::new(working);
        Ok(())
    }

    /// Creates a lazy, historically stable AST facade. No AST record or token
    /// coordinate map is materialized at commit: each resolver follows the
    /// persistent arena and token roots captured by this revision.  The token
    /// document is supplied explicitly so a layout-only edit can rebind the
    /// snapshot to coordinates at the current layout without re-deriving the
    /// semantic parser state.
    pub(crate) fn commit_snapshot(
        &mut self,
        uri: Uri<String>,
        source: Arc<Rope>,
        token_document: Arc<ParserTokenDocument>,
    ) -> Result<Arc<AstSnapshot>, ParseError> {
        let arena = self
            .session_arenas
            .get(&uri)
            .map(|arenas| Arc::clone(&arenas.ast))
            .ok_or(BuildError::MissingAst(0))?;
        let live_records = self
            .latest
            .tree_facts
            .get(&uri)
            .map(|facts| Arc::clone(&facts.records))
            .unwrap_or_default();

        let id = self.next_snapshot;
        self.next_snapshot = self.next_snapshot.saturating_add(1);
        Ok(Arc::new(AstSnapshot::new(
            id,
            uri,
            source,
            arena,
            live_records,
            token_document,
        )))
    }

    pub fn incremental_stats(&self, uri: Uri<String>) -> Option<IncrementalParseStats> {
        self.latest.incremental_stats.get(&uri).copied()
    }

    pub fn latest_parse_diagnostics(&self, uri: Uri<String>) -> Vec<ParseErrorInfo> {
        let Some(state) = self.latest.sessions.get(&uri) else {
            return Vec::new();
        };
        let roots = self
            .latest
            .roots
            .get(&uri)
            .map(|roots| roots.as_slice())
            .unwrap_or(&[]);
        diagnostics::collect_parse_diagnostics(
            state,
            self.session_arenas.get(&uri).map(|arenas| diagnostics::DiagnosticArenas {
                trees: &arenas.trees,
                products: &arenas.products,
            }),
            roots,
        )
    }
}
