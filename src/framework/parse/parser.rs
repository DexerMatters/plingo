//! Parser ownership and integrated immutable revision construction.

use std::{marker::PhantomData, sync::Arc};

use fluent_uri::Uri;
use indexmap::IndexMap;
use ropey::Rope;

use crate::framework::{
    lex::LexerRoot,
    parse::{
        ParseError,
        build::{ActionSet, Conflict, LR1State, LRStateId},
        data::green::ParseErrorInfo,
        diagnostics,
        grammar::{BuildError, Grammar, Symbol},
        parsing::ReplayPlan,
        types::{
            AstSnapshot, IncrementalParseStats, ParserConfig, ParserSnapshotState,
            ParserTokenDocument,
        },
    },
};

pub struct Parser<Root = ()> {
    pub(crate) grammar: Grammar,
    pub(crate) states: Vec<LR1State>,
    pub(crate) transitions: IndexMap<(LRStateId, Symbol), LRStateId>,
    pub(crate) conflicts: Vec<Conflict>,
    pub(crate) actions: Vec<ActionSet>,
    pub(crate) gotos: Vec<Option<LRStateId>>,
    /// Tree publication supplies the generated abstract-family member
    /// classifier so parser deltas exclude grammar-internal/token records
    /// from child topology.
    pub(crate) tree_member_kind:
        Option<fn(&crate::framework::parse::data::ast::AstArena, u64) -> Option<u8>>,
    /// Tree publication supplies generated AST child-field topology. This
    /// includes optional/list fields that product reachability may omit.
    pub(crate) tree_child_records:
        Option<fn(&crate::framework::parse::data::ast::AstArena, u64) -> Vec<u64>>,
    pub(crate) config: ParserConfig,
    /// Per-document roots. Publication swaps the persistent URI map under a
    /// short write guard while a parse runs against its own root copy, so
    /// distinct documents never contend on a parser-global mutation lock.
    /// Snapshot IDs distinguish immutable publications while snapshots held
    /// by consumers remain isolated from later parser mutations.
    pub(crate) latest: Arc<parking_lot::RwLock<Arc<ParserSnapshotState>>>,
    pub(crate) next_snapshot: std::sync::atomic::AtomicU64,
    pub(crate) _root: PhantomData<fn() -> Root>,
}

impl<Root> Parser<Root> {
    /// Current committed snapshot map (cheap Arc clone; readers never block
    /// a writer for another document).
    pub(crate) fn snapshot(&self) -> Arc<ParserSnapshotState> {
        Arc::clone(&self.latest.read())
    }

    /// Committed root of one document, if any.
    pub(crate) fn document_root(
        &self,
        uri: &Uri<String>,
    ) -> Option<Arc<crate::framework::parse::types::ParserDocumentRoot>> {
        self.snapshot().documents.get(uri).cloned()
    }

    /// Publishes one document root under the short map write guard. The next
    /// map is rebuilt from the currently committed map, so concurrent
    /// distinct-document publications never lose each other's entries; the
    /// parse itself ran against a private root copy.
    fn publish_document_root(
        &self,
        uri: &Uri<String>,
        root: Option<Arc<crate::framework::parse::types::ParserDocumentRoot>>,
    ) {
        let mut guard = self.latest.write();
        let mut working = (**guard).clone();
        match root {
            Some(root) => working.replace_document(uri.clone(), root),
            None => working.remove_document(uri),
        }
        *guard = Arc::new(working);
    }

    pub(crate) fn replace_document_root(
        &self,
        uri: Uri<String>,
        root: Arc<crate::framework::parse::types::ParserDocumentRoot>,
    ) {
        self.publish_document_root(&uri, Some(root));
    }

    pub(crate) fn restore_document_root(
        &self,
        uri: Uri<String>,
        root: Option<Arc<crate::framework::parse::types::ParserDocumentRoot>>,
    ) {
        self.publish_document_root(&uri, root);
    }

    pub(crate) fn set_published_child_orders(
        &self,
        uri: Uri<String>,
        orders: Arc<
            crate::reactive::store::RadixMap<crate::framework::parse::types::PublishedChildOrder>,
        >,
    ) {
        let Some(previous) = self.document_root(&uri) else {
            return;
        };
        let mut root = (*previous).clone();
        let mut facts = (*root.tree_facts).clone();
        facts.published_child_orders = orders;
        root.tree_facts = Arc::new(facts);
        self.replace_document_root(uri, Arc::new(root));
    }

    /// Drops all private state for a document whose reactive semantic
    /// component was retired between publications.
    ///
    /// Component ownership retracts the published tree facts on retirement,
    /// but the parser machine is shared by the component definition and does
    /// not receive a final body call. A later equal-text reopen therefore
    /// must start from an empty replay state rather than reusing the old
    /// document identity.
    pub(crate) fn forget_document(&self, uri: &Uri<String>) {
        self.publish_document_root(uri, None);
    }

    /// Replays the exact lexer-authored structural patch against persistent
    /// parser token roots. Ordinary replay decodes only tokens between the
    /// restart checkpoint and convergence; no document token vector is built.
    pub(crate) fn derive_token_delta(
        &self,
        uri: Uri<String>,
        delta: &crate::framework::lex::TokenPatch,
        new_tokens: Arc<ParserTokenDocument>,
    ) -> Result<(), ParseError>
    where
        Root: LexerRoot + Clone,
    {
        let old_root = self.document_root(&uri);
        let old_tokens = old_root.as_ref().and_then(|root| root.token.clone());
        if delta.structure_unchanged() && old_tokens.is_some() {
            let changed_column = delta
                .updated
                .iter()
                .filter_map(|occurrence| new_tokens.rank_of_occurrence(*occurrence))
                .min()
                .unwrap_or(0);
            let retained_columns = old_root
                .as_ref()
                .map_or(0, |root| root.session.column_count());
            crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
                work.restart_columns += changed_column as u64;
                work.columns_replayed += u64::from(!delta.updated.is_empty());
                work.columns_reused += retained_columns.saturating_sub(1) as u64;
            });
            let root = (**old_root
                .as_ref()
                .expect("document root exists for a structural-unchanged patch"))
            .clone();
            self.publish_document_root(&uri, Some(Arc::new(root)));
            return Ok(());
        }

        let plan = ReplayPlan::from_token_patch(old_tokens, Arc::clone(&new_tokens), delta);
        match self.parse_delta_batch(old_root, uri.clone(), plan) {
            Ok(root) => {
                self.publish_document_root(&uri, Some(root));
                Ok(())
            }
            Err(ParseError::NoActiveStacks { .. }) => {
                self.rebuild_document_after_replay_failure(&uri, new_tokens)
            }
            Err(error) => Err(error),
        }
    }

    fn rebuild_document_after_replay_failure(
        &self,
        uri: &Uri<String>,
        new_tokens: Arc<ParserTokenDocument>,
    ) -> Result<(), ParseError>
    where
        Root: LexerRoot + Clone,
    {
        crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
            work.full_rebuild_fallbacks += 1;
        });
        self.publish_document_root(uri, None);
        let patch = crate::framework::lex::TokenPatch {
            old_structure: crate::framework::lex::TokenStructureRevisionId(0),
            new_structure: crate::framework::lex::TokenStructureRevisionId(1),
            order_splices: Arc::from([]),
            inserted: Arc::from([]),
            updated: Arc::from([]),
            removed: Arc::from([]),
        };
        let plan = ReplayPlan::from_token_patch(None, new_tokens, &patch);
        let root = self.parse_delta_batch(None, uri.clone(), plan)?;
        self.publish_document_root(uri, Some(root));
        Ok(())
    }

    /// Creates a lazy, historically stable AST facade. No AST record or token
    /// coordinate map is materialized at commit: each resolver follows the
    /// persistent arena and token roots captured by this revision.  The token
    /// document is supplied explicitly so a layout-only edit can rebind the
    /// snapshot to coordinates at the current layout without re-deriving the
    /// semantic parser state.
    pub(crate) fn commit_snapshot(
        &self,
        uri: Uri<String>,
        source: Arc<Rope>,
        token_document: Arc<ParserTokenDocument>,
    ) -> Result<Arc<AstSnapshot>, ParseError> {
        let root = self.document_root(&uri).ok_or(BuildError::MissingAst(0))?;
        let arena = Arc::clone(&root.arenas.ast);
        let live_records = Arc::clone(&root.tree_facts.records);
        let id = self
            .next_snapshot
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        self.document_root(&uri).map(|root| root.incremental_stats)
    }

    pub fn latest_parse_diagnostics(&self, uri: Uri<String>) -> Vec<ParseErrorInfo> {
        let Some(root) = self.document_root(&uri) else {
            return Vec::new();
        };
        diagnostics::collect_parse_diagnostics(
            &root.session,
            Some(diagnostics::DiagnosticArenas {
                trees: &root.arenas.trees,
                products: &root.arenas.products,
            }),
            root.roots.as_slice(),
        )
    }
}
