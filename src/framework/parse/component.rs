//! Plain-function parser pipeline.
//!
//! Parser state remains private inside one mutex-protected machine. The
//! authored reactive boundary is the per-document `run` call: token-domain
//! changes schedule only the affected parser invocation, and omitted writes
//! retract closed documents.

use std::{collections::BTreeSet, marker::PhantomData, sync::Arc};

use crate::framework::lex::LexerRoot;
use fluent_uri::Uri;
use parking_lot::Mutex;

use crate::framework::parse::data::ast::AstBox;
use crate::framework::parse::{
    DocumentSnapshot, IncrementalParseStats, ParseErrorInfo, ParseStatus, Parser,
};
use crate::reactive::{Engine, Error, Result};
use crate::reactive::kind::{
    emit_patch, emit_view, observe_view, List, Map, TreeKey, TreePatch, TreeView, ViewKind,
};
use reactive_macros::view;

/// A tree-less parser publication. `root` is absent when no accepted AST
/// root exists; parser internals retain the typed arena value privately.
pub struct ParseUnit<A: 'static> {
    pub root: Option<AstBox<A>>,
    pub status: ParseStatus,
    pub stats: IncrementalParseStats,
}

impl<A: 'static> Clone for ParseUnit<A> {
    fn clone(&self) -> Self {
        Self {
            root: self.root,
            status: self.status.clone(),
            stats: self.stats,
        }
    }
}

impl<A: 'static> std::fmt::Debug for ParseUnit<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ParseUnit")
            .field("root", &self.root.map(|_| "AstBox"))
            .field("status", &self.status)
            .finish()
    }
}

impl<A: 'static> PartialEq for ParseUnit<A> {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.status == other.status
    }
}

impl<A: 'static> Eq for ParseUnit<A> {}

impl<A: 'static> ParseUnit<A> {
    fn new(root: Option<AstBox<A>>, status: ParseStatus, stats: IncrementalParseStats) -> Self {
        Self {
            root,
            status,
            stats,
        }
    }
}
/// A tree publication whose accepted root is a syntax-view identity.
/// `revision` is the document's semantic-revision ordinal: it advances
/// exactly when a non-empty [`ParseDelta`](crate::framework::parse::delta::
/// ParseDelta) publishes facts, giving consumers an O(1) change handle
/// whose equality never depends on work counters (plan §12 step 7).
pub struct TreeParseUnit<A: crate::framework::parse::AbstractTreeFamily> {
    pub root: Option<crate::reactive::view::Node<A::View>>,
    pub status: ParseStatus,
    pub revision: u64,
    pub stats: IncrementalParseStats,
    _marker: PhantomData<fn() -> A>,
}

impl<A: crate::framework::parse::AbstractTreeFamily> Clone for TreeParseUnit<A> {
    fn clone(&self) -> Self {
        Self {
            root: self.root,
            status: self.status.clone(),
            revision: self.revision,
            stats: self.stats,
            _marker: PhantomData,
        }
    }
}

impl<A: crate::framework::parse::AbstractTreeFamily> std::fmt::Debug for TreeParseUnit<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The revision is an internal wake handle, not semantic tree
        // content. Omitting it keeps debug snapshots comparable across a
        // warm edit and an equivalent cold build.
        formatter
            .debug_struct("TreeParseUnit")
            .field("root", &self.root)
            .field("status", &self.status)
            .finish()
    }
}

impl<A: crate::framework::parse::AbstractTreeFamily> PartialEq for TreeParseUnit<A> {
    fn eq(&self, other: &Self) -> bool {
        // Revision participates deliberately: it advances exactly when a
        // non-empty ParseDelta publishes, giving keyed children a reliable
        // wake even when root identity and status are retained (plan
        // §12 step 7).
        self.root == other.root
            && self.status == other.status
            && self.revision == other.revision
    }
}

impl<A: crate::framework::parse::AbstractTreeFamily> Eq for TreeParseUnit<A> {}

impl<A: crate::framework::parse::AbstractTreeFamily> TreeParseUnit<A> {
    fn new(
        root: Option<crate::reactive::view::Node<A::View>>,
        status: ParseStatus,
        revision: u64,
        stats: IncrementalParseStats,
    ) -> Self {
        Self {
            root,
            status,
            revision,
            stats,
            _marker: PhantomData,
        }
    }
}

/// Per-document accepted syntax-tree roots.
#[view]
pub struct TreeParseUnits<A: crate::framework::parse::AbstractTreeFamily>(
    Map<String, TreeParseUnit<A>>,
);

/// Per-document tree-less parse publications.
#[view]
pub struct ParseUnits<A: 'static>(Map<String, ParseUnit<A>>);

/// Per-document parser diagnostics.
/// Per-document parser diagnostics: one slot per diagnostic, replaced
/// wholesale by each document's parser invocation via `replace` diffs.
#[view]
pub struct ParseDiagnostics(List<String, ParseErrorInfo>);

/// Committed AST snapshots keyed by document URI.
///
/// Phase 0 characterization scaffold backing the canonical oracle
/// projection (plan §10.4); Phase 8 replaces this with the persistent
/// `ParsedDocuments` core.
#[view]
pub struct AstSnapshots<A: 'static>(
    Map<String, crate::framework::parse::DocumentSnapshot<A>>,
);


struct ParserMachine<R: LexerRoot + Clone + std::fmt::Debug, A: 'static> {
    parser: Parser<R>,
    _ast: PhantomData<fn() -> A>,
}



/// One publication's complete delta-facing context (plan §12): the tree
/// publisher consumes ONLY this canonical output — never live-record sets.
pub(crate) struct DeltaPublication<'a, A> {
    pub(crate) uri: String,
    /// `None` on the document-close path (everything retracts).
    pub(crate) arenas: Option<&'a SessionArenas>,
    pub(crate) root: Option<AstBox<A>>,
    pub(crate) current_root_record: Option<u64>,
    pub(crate) previous_root_record: Option<u64>,
    pub(crate) delta: Arc<crate::framework::parse::delta::ParseDelta>,
    pub(crate) session: Option<Arc<ParserSessionState>>,
    pub(crate) status: ParseStatus,
    pub(crate) revision: u64,
    pub(crate) stats: IncrementalParseStats,
}

impl<'a, A> DeltaPublication<'a, A> {
    fn empty(uri: String) -> Self {
        Self {
            uri,
            arenas: None,
            root: None,
            current_root_record: None,
            previous_root_record: None,
            delta: Default::default(),
            session: None,
            status: ParseStatus::Unrecoverable { diagnostics: 0 },
            revision: 0,
            stats: IncrementalParseStats::default(),
        }
    }

    /// Stable-lineage resolution for one arena record.
    pub(crate) fn lineage_of(&self, record: u64) -> Option<u64> {
        let session = self.session.as_ref()?;
        session
            .lineage
            .lineage_of(record as usize)
            .or_else(|| session.lineage.died_lineage_of(record))
    }

    /// The current bearer record of one stable syntax identity.
    pub(crate) fn bearer_of(&self, lineage: u64) -> Option<u64> {
        self.session
            .as_ref()?
            .lineage
            .holder_of(lineage)
            .map(|record| record as u64)
    }
}

use crate::framework::parse::parsing::ParserSessionState;
use crate::framework::parse::types::SessionArenas;

fn parser_document_with<R, A, P>(
    machine: Arc<Mutex<ParserMachine<R, A>>>,
    uri: String,
    publish: P,
) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
    P: FnOnce(&DeltaPublication<'_, A>) -> Result<()>,
{
    use crate::framework::parse::delta::ParsedStatus;

    let semantic_observe = observe_view::<crate::framework::lex::LexedDocuments<R>>()?;
    let Some(lexed) = semantic_observe.get(&uri)? else {
        crate::framework::workspace::record_parser_work(&uri, |work| {
            work.component_runs += 1;
        });
        let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
        let mut machine_guard = machine.lock();
        let publication = DeltaPublication::<A>::empty(uri.clone());
        let result = publish(&publication);
        drop(machine_guard);
        // Normal close releases private parser document state (plan §7.7
        // and hard invariant 15): sessions, roots, token roots, stats,
        // tree facts/deltas, and the session arenas all drop together.
        let mut machine_guard = machine.lock();
        let parser = &mut machine_guard.parser;
        parser.session_arenas.remove(&static_uri);
        let working = Arc::make_mut(&mut parser.latest);
        working.sessions.remove(&static_uri);
        working.roots.remove(&static_uri);
        working.tokens.remove(&static_uri);
        working.incremental_stats.remove(&static_uri);
        working.tree_facts.remove(&static_uri);
        working.tree_deltas.remove(&static_uri);
        working.published_status.remove(&static_uri);
        working.published_diagnostics.remove(&static_uri);
        return result;
    };
    let mut machine = machine.lock();
    let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
    // The root record THIS command replaces, captured pre-replay for
    // document-stable-root suppression (plan §11).
    let previous_root_record = machine
        .parser
        .latest
        .tree_facts
        .get(&static_uri)
        .and_then(|facts| facts.root);
    crate::framework::workspace::record_parser_work(&uri, |work| {
        work.component_runs += 1;
    });
    let token_document = Arc::new(
        crate::framework::parse::types::ParserTokenDocument::from_lexical(&lexed.document),
    );
    machine
        .parser
        .derive_token_delta(static_uri.clone(), &lexed.patch, token_document)
        .map_err(|error| Error::Internal(error.to_string().into()))?;
    let accepted = machine
        .parser
        .latest
        .roots
        .get(&static_uri)
        .map(|roots| roots.as_ref().clone())
        .unwrap_or_default();
    let delta = machine
        .parser
        .latest
        .tree_deltas
        .get(&static_uri)
        .cloned()
        .unwrap_or_default();
    let facts_root = machine
        .parser
        .latest
        .tree_facts
        .get(&static_uri)
        .and_then(|facts| facts.root);
    let parse_diagnostics = machine.parser.latest_parse_diagnostics(static_uri.clone());
    let stats = machine
        .parser
        .incremental_stats(static_uri.clone())
        .unwrap_or_default();
    let status = if parse_diagnostics.is_empty() {
        ParseStatus::Clean
    } else if accepted.is_empty() {
        ParseStatus::Unrecoverable {
            diagnostics: parse_diagnostics.len(),
        }
    } else {
        ParseStatus::Recovered {
            diagnostics: parse_diagnostics.len(),
        }
    };
    let root = facts_root.and_then(|record| {
        machine
            .parser
            .session_arenas
            .get(&static_uri)?
            .ast
            .expect::<A>(record as usize)
    });
    let arenas = machine.parser.session_arenas.get(&static_uri);
    let session = machine.parser.latest.sessions.get(&static_uri).cloned();
    let publication = DeltaPublication {
        uri: uri.clone(),
        arenas,
        root,
        current_root_record: facts_root,
        previous_root_record,
        delta,
        session,
        status,
        revision: machine
            .parser
            .latest
            .semantic_revisions
            .get(&static_uri)
            .copied()
            .unwrap_or(0),
        stats,
    };
    emit_view::<ParseUnits<A>>()?.insert(
        uri.clone(),
        ParseUnit::new(publication.root, publication.status.clone(), publication.stats),
    )?;
    emit_view::<ParseDiagnostics>()?.replace(&uri, parse_diagnostics)?;

    publish(&publication)
}

fn parser_document_tree<R, A>(machine: Arc<Mutex<ParserMachine<R, A>>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: crate::framework::parse::AbstractTreeFamily + 'static,
    A::View: TreeView<Key = String>
        + ViewKind<
            Observe = crate::reactive::kind::TreeObserve<A::View>,
            Patch = TreePatch<A::View>,
        >,
{
    parser_document_with(
        machine,
        uri,
        |publication: &DeltaPublication<'_, A>| {
            let patch = emit_patch::<A::View>()?;
            let Some(arenas) = publication.arenas else {
                emit_view::<TreeParseUnits<A>>()?.remove(publication.uri.to_string())?;
                return Ok(());
            };
            let ast = arenas.ast.as_ref();
            let uri = publication.uri.as_str();
            let current_root = publication.current_root_record;
            let resolver = move |record: u64| -> Option<u64> { publication.lineage_of(record) };

            // ---- retraction: exact removed identities from the delta ----
            // The syntax domains are already exclusive (insert/update/
            // removed disjoint) and carrier-suppressed: a dying record
            // whose identity a live inheritor carries does not retract,
            // avoiding remove+upsert collisions on the same node id.
            for &(lineage, record) in publication.delta.removed_records.iter() {
                let _ = lineage;
                // A replaced document root keeps its document-stable node;
                // its facts are refreshed, not retracted (plan §11).
                if Some(record) == publication.previous_root_record && current_root.is_some() {
                    continue;
                }
                A::__tree_plain_remove_record(uri, ast, record, &resolver)?;
            }

            // Root disappearance retracts the document-stable root facts.
            if let Some(previous_record) = publication.previous_root_record
                && current_root.is_none()
                && let Some(stable) =
                    A::__tree_plain_node_for_record(uri, ast, previous_record, true, &resolver)
            {
                patch.remove(crate::reactive::kind::TreeKey::Payload(stable))?;
                patch.remove(crate::reactive::kind::TreeKey::Parent(stable))?;
                patch.remove(crate::reactive::kind::TreeKey::ChildOrder(stable))?;
            }

            // ---- inserted identities: full fact sets --------------------
            // Fresh lineages need their complete fact set written; an
            // inherited identity with identical shape publishes nothing
            // (excluded upstream), and one with changed shape flows
            // through the update domain below.
            for &(lineage, record) in publication.delta.inserted_records.iter() {
                let _ = lineage;
                let is_root = current_root == Some(record);
                A::__tree_plain_emit_record(uri, ast, record, is_root, &resolver)?;
            }

            // A retained record whose green shape changed may also have a
            // different direct-child list. Emit the complete split fact set
            // so its payload, child order, and child links converge together.
            // Equal facts are filtered by the reactive patch commit, so this
            // remains observation-minimal while covering structural parents.
            for &(lineage, record) in publication.delta.updated_records.iter() {
                let _ = lineage;
                let is_root = current_root == Some(record);
                A::__tree_plain_emit_record(uri, ast, record, is_root, &resolver)?;
            }

            // ---- root splice + O(1) document handle ---------------------
            let root_changed = !publication.delta.roots.inserted.is_empty()
                || !publication.delta.roots.removed.is_empty();
            let root_id = current_root
                .and_then(|record| {
                    A::__tree_plain_node_for_record(uri, ast, record, true, &resolver)
                });
            match root_id {
                Some(root_id) => {
                    if root_changed {
                        A::__tree_plain_emit_roots(uri, vec![root_id])?;
                    }
                    emit_view::<TreeParseUnits<A>>()?.insert(
                        publication.uri.to_string(),
                        TreeParseUnit::new(
                            Some(root_id),
                            publication.status.clone(),
                            publication.revision,
                            publication.stats,
                        ),
                    )?;
                }
                None => {
                    emit_view::<TreeParseUnits<A>>()?.remove(publication.uri.to_string())?;
                }
            }

            // Diagnostics/status exact keys ride in delta.diagnostics and
            // delta.status for oracle proofs; their slots publish above via
            // ParseDiagnostics/ParseUnits replacement.
            Ok(())
        },
    )
}

fn parser_document<R, A>(machine: Arc<Mutex<ParserMachine<R, A>>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    parser_document_with(machine, uri, |_publication| Ok(()))
}
/// Refreshes editor-facing AST coordinates after layout-sensitive changes.
///
/// The semantic parser is keyed by `LexedDocuments`, whose equality is
/// semantic-revision-only. This companion child is keyed by `Tokens` and
/// publishes the immutable editor facade without changing parser/tree facts.
fn parser_layout_snapshot<R, A>(
    machine: Arc<Mutex<ParserMachine<R, A>>>,
    uri: String,
) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    let tokens = observe_view::<crate::framework::lex::TokenLayoutDocuments<R>>()?.get(&uri)?;
    let Some(tokens) = tokens else {
        emit_view::<AstSnapshots<A>>()?.remove(uri);
        return Ok(());
    };
    let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
    let mut machine = machine.lock();
    let current = machine.parser.latest.tokens.get(&static_uri).cloned();
    let Some(current) = current else {
        return Ok(());
    };
    // The semantic parser is keyed by structural revisions only, so a
    // value-only or layout-only edit leaves the stored token document at an
    // older layout revision. Rebuild the coordinate root for the editor
    // snapshot and record the reused-coordinate work without a semantic
    // component run (plan §7 revision domains, §10 counters).
    let token_document = Arc::new(
        crate::framework::parse::types::ParserTokenDocument::from_lexical(&tokens.document),
    );
    if current.layout_revision().0 < token_document.layout_revision().0 {
        let session_columns = machine
            .parser
            .latest
            .sessions
            .get(&static_uri)
            .map(|session| session.columns.len())
            .unwrap_or_default();
        crate::framework::workspace::record_parser_work(&uri, |work| {
            work.columns_reused += session_columns.saturating_sub(1) as u64;
            work.tokens_reused += current.semantic_len() as u64;
        });
    }
    let snapshot = machine
        .parser
        .commit_snapshot(static_uri, Arc::clone(&tokens.document.source), token_document)
        .map_err(|error| Error::Internal(error.to_string().into()))?;
    emit_view::<AstSnapshots<A>>()?.insert(uri, DocumentSnapshot::new(snapshot))?;
    Ok(())
}
/// Installs the tree-less parser as ordinary reactive roots.  Semantic parser
/// work keys off `LexedDocuments`; the coordinate-only editor projection is a
/// separate root over `Tokens`.
pub fn install_parser<R, A>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: crate::framework::parse::__macro_private::NonTerminalSpec + 'static,
{
    use crate::framework::lex::{LexedDocuments, TokenLayoutDocuments};
    let parser = crate::framework::parse::grammar::Grammar::from_spec::<A>().build_lr1::<R>();
    let machine = Arc::new(Mutex::new(ParserMachine {
        parser,
        _ast: PhantomData,
    }));
    // Document-scoped keyed families: one stable child invocation per
    // document, woken directly by that document's revision changes.
    let semantic_machine = Arc::clone(&machine);
    engine.install_keyed::<LexedDocuments<R>, _>(move |uri| {
        parser_document::<R, A>(Arc::clone(&semantic_machine), uri)
    })?;
    let layout_machine = Arc::clone(&machine);
    engine.install_keyed::<TokenLayoutDocuments<R>, _>(move |uri| {
        parser_layout_snapshot::<R, A>(Arc::clone(&layout_machine), uri)
    })?;
    Ok(())
}
/// Installs the parser plus generated abstract-tree facts as ordinary roots.
pub fn install_parser_tree<R, A>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: crate::framework::parse::__macro_private::NonTerminalSpec
        + crate::framework::parse::AbstractTreeFamily
        + 'static,
    A::View: TreeView<Key = String>
        + ViewKind<
            Observe = crate::reactive::kind::TreeObserve<A::View>,
            Patch = TreePatch<A::View>,
        >,
{
    use crate::framework::lex::{LexedDocuments, TokenLayoutDocuments};
    let parser = crate::framework::parse::grammar::Grammar::from_spec::<A>().build_lr1::<R>();
    let machine = Arc::new(Mutex::new(ParserMachine {
        parser,
        _ast: PhantomData,
    }));
    // Document-scoped keyed families (see install_parser).
    let semantic_machine = Arc::clone(&machine);
    engine.install_keyed::<LexedDocuments<R>, _>(move |uri| {
        parser_document_tree::<R, A>(Arc::clone(&semantic_machine), uri)
    })?;
    let layout_machine = Arc::clone(&machine);
    engine.install_keyed::<TokenLayoutDocuments<R>, _>(move |uri| {
        parser_layout_snapshot::<R, A>(Arc::clone(&layout_machine), uri)
    })?;
    Ok(())
}
