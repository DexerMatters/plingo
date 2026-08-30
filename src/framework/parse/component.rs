//! Plain-function parser pipeline.
//!
//! Parser state remains private inside one mutex-protected machine. The
//! authored reactive boundary is the per-document `run` call: token-domain
//! changes schedule only the affected parser invocation, and omitted writes
//! retract closed documents.

use std::{collections::BTreeSet, collections::HashMap, marker::PhantomData, sync::Arc};

use crate::framework::parse::delta::RemovedRecord;

use crate::framework::lex::LexerRoot;
use fluent_uri::Uri;

use crate::framework::parse::data::ast::AstBox;
use crate::framework::parse::parsing::ParserSessionState;
use crate::framework::parse::types::SessionArenas;
use crate::framework::parse::{
    DocumentSnapshot, IncrementalParseStats, ParseErrorInfo, ParseStatus, Parser,
};
use crate::reactive::kind::{
    List, Map, TreePatch, TreeView, ViewKind, emit_patch, emit_view, observe_view,
};
use crate::reactive::{Engine, Error, Result, state_cell};
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
/// Per-document parse status for tree-flavor parser clients.
#[view]
pub struct ParserTreeStatuses(Map<String, ParseStatus>);

/// Framework-private syntax lineage key. The URI is part of the key because
/// parser serials are document-local.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ParserSyntaxKey {
    uri: String,
    lineage: crate::framework::parse::delta::SyntaxNodeId,
}

impl ParserSyntaxKey {
    fn new(uri: &str, lineage: u64) -> Self {
        Self {
            uri: uri.to_owned(),
            lineage: crate::framework::parse::delta::SyntaxNodeId(lineage),
        }
    }

    fn lineage(&self) -> u64 {
        self.lineage.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ParserFieldKey {
    parent: ParserSyntaxKey,
    field: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ParserFieldEdgeKey {
    parent: ParserSyntaxKey,
    child: ParserSyntaxKey,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ParserRootKey {
    uri: String,
}

impl ParserRootKey {
    fn new(uri: &str) -> Self {
        Self {
            uri: uri.to_owned(),
        }
    }
}

#[view]
pub(crate) struct ParseStatusDocuments(Map<String, ParseStatus>);

#[view]
pub(crate) struct ParseDiagnosticsDocuments(List<String, ParseErrorInfo>);

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
pub struct AstSnapshots<A: 'static>(Map<String, crate::framework::parse::DocumentSnapshot<A>>);

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
    pub(crate) previous_child_orders:
        Arc<crate::reactive::store::RadixMap<crate::framework::parse::types::PublishedChildOrder>>,
    /// `record -> lineage` for every record this command inserted or
    /// updated. Prebuilt once per publication so the per-record fact loop
    /// resolves identities in O(1) instead of scanning the delta arrays
    /// (a cold parse made that O(n²)).
    pub(crate) changed_lineages: HashMap<u64, u64>,
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
            previous_child_orders: Arc::new(crate::reactive::store::RadixMap::default()),
            changed_lineages: HashMap::new(),
            status: ParseStatus::Unrecoverable { diagnostics: 0 },
            revision: 0,
            stats: IncrementalParseStats::default(),
        }
    }

    pub(crate) fn lineage_of(&self, record: u64) -> Option<u64> {
        let session_lineage = self.session.as_ref().and_then(|session| {
            session
                .lineage
                .lineage_of(record as usize)
                .or_else(|| session.lineage.died_lineage_of(record))
        });
        session_lineage.or_else(|| self.changed_lineages.get(&record).copied())
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
fn nearest_tree_parent<A>(
    publication: &DeltaPublication<'_, A>,
    ast: &crate::framework::parse::data::AstArena,
    record: u64,
) -> Option<ParserSyntaxKey>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    let mut parent = ast.parent_of(record as usize);
    while let Some(parent_record) = parent {
        if syntax_member_of::<A>(ast, parent_record as u64).is_some()
            && let Some(lineage) = publication.lineage_of(parent_record as u64)
        {
            return Some(ParserSyntaxKey::new(&publication.uri, lineage));
        }
        parent = ast.parent_of(parent_record);
    }
    None
}
fn close_parser_dimensions<A>(uri: &str) -> Result<()> {
    let _ = uri;
    Ok(())
}

fn parser_document_with<R, A, P>(
    machine: Arc<ParserMachine<R, A>>,
    uri: String,
    publish: P,
) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
    P: FnOnce(
        &DeltaPublication<'_, A>,
    ) -> Result<
        Option<
            Arc<
                crate::reactive::store::RadixMap<
                    crate::framework::parse::types::PublishedChildOrder,
                >,
            >,
        >,
    >,
{
    let machine_arc = Arc::clone(&machine);
    let lifecycle = state_cell::<u8>();
    let was_active = lifecycle.with(|value| value.is_some())?;
    let semantic_observe = observe_view::<crate::framework::lex::LexedDocuments<R>>()?;
    let Some(lexed) = semantic_observe.get(&uri)? else {
        crate::framework::workspace::record_parser_work(&uri, |work| {
            work.component_runs += 1;
            work.semantic_runs += 1;
        });
        let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
        let publication = DeltaPublication::<A>::empty(uri.clone());
        let result = (|| {
            emit_view::<ParseStatusDocuments>()?.remove(uri.clone())?;
            emit_view::<ParseDiagnosticsDocuments>()?.clear(&uri)?;
            publish(&publication).map(|_| ())
        })();
        // Normal close releases private parser document state (plan §7.7
        // and hard invariant 15): sessions, roots, token roots, stats,
        // tree facts/deltas, semantic revisions, diagnostics, and the
        // session arenas all drop together.
        machine.parser.forget_document(&static_uri);
        return result;
    };
    let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
    if !was_active {
        machine.parser.forget_document(&static_uri);
    }
    // The root record THIS command replaces, captured pre-replay for
    // document-stable-root suppression (plan §11).
    let previous_document = machine.parser.document_root(&static_uri);
    let previous_root_record = previous_document
        .as_ref()
        .and_then(|root| root.tree_facts.root);
    let previous_child_orders = previous_document.as_ref().map_or_else(
        || Arc::new(crate::reactive::store::RadixMap::default()),
        |root| Arc::clone(&root.tree_facts.published_child_orders),
    );
    crate::framework::workspace::record_parser_work(&uri, |work| {
        work.component_runs += 1;
        work.semantic_runs += 1;
    });
    let token_document = Arc::new(
        crate::framework::parse::types::ParserTokenDocument::from_lexical(&lexed.document),
    );
    // Cut B participant bridge (plan section 20.1): stage this document's
    // parser session for the command. The previous session root is one Arc
    // clone; on any failure the participant reinstates it after fact
    // rollback, so lineage/recovery state can never wedge across commands.
    {
        let previous_root = machine.parser.document_root(&static_uri);
        let rollback_uri = static_uri.clone();
        let machine_for_undo = Arc::clone(&machine_arc);
        crate::reactive::plain::with_txn_pub(move |txn| {
            txn.private_undos.push(Box::new(move || {
                machine_for_undo
                    .parser
                    .restore_document_root(rollback_uri, previous_root);
            }));
        });
    }
    machine
        .parser
        .derive_token_delta(static_uri.clone(), &lexed.patch, token_document)
        .map_err(|error| Error::Internal(error.to_string().into()))?;
    let document_root = machine.parser.document_root(&static_uri);
    let accepted = document_root
        .as_ref()
        .map_or_else(Vec::new, |root| root.roots.as_ref().clone());
    let delta = document_root.as_ref().map_or_else(
        || Arc::new(crate::framework::parse::delta::ParseDelta::default()),
        |root| Arc::clone(&root.tree_delta),
    );
    let facts_root = document_root.as_ref().and_then(|root| root.tree_facts.root);
    let parse_diagnostics = machine.parser.latest_parse_diagnostics(static_uri.clone());
    let stats = document_root
        .as_ref()
        .map_or_else(IncrementalParseStats::default, |root| {
            root.incremental_stats
        });
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
        document_root
            .as_ref()?
            .arenas
            .ast
            .expect::<A>(record as usize)
    });
    let arenas = document_root.as_ref().map(|root| root.arenas.as_ref());
    let session = document_root.as_ref().map(|root| Arc::clone(&root.session));
    let changed_lineages: HashMap<u64, u64> = document_root
        .as_ref()
        .map(|root| {
            let delta = &root.tree_delta;
            delta
                .inserted_records
                .iter()
                .chain(delta.updated_records.iter())
                .map(|(lineage, record)| (*record, lineage.0))
                .collect()
        })
        .unwrap_or_default();
    let publication = DeltaPublication {
        uri: uri.clone(),
        arenas,
        root,
        current_root_record: facts_root,
        previous_root_record,
        delta,
        session,
        previous_child_orders,
        changed_lineages,
        status,
        revision: document_root
            .as_ref()
            .map_or(0, |root| root.semantic_revision),
        stats,
    };
    if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
        eprintln!(
            "publication uri={} rev={} root={:?} current_root_record={:?} previous_root_record={:?} prev_orders={:?} inserted={:?} updated={:?} removed={:?} roots+={:?} roots-={:?} payloads={} splices={:?}",
            publication.uri,
            publication.revision,
            publication.root,
            publication.current_root_record,
            publication.previous_root_record,
            publication
                .previous_child_orders
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            publication.delta.inserted_records,
            publication.delta.updated_records,
            publication.delta.removed_records,
            publication.delta.roots.inserted,
            publication.delta.roots.removed,
            publication.delta.syntax_payloads.len(),
            publication
                .delta
                .child_splices
                .iter()
                .map(|splice| {
                    (
                        splice.parent.0,
                        splice.delta.before.map(|id| id.0),
                        splice
                            .delta
                            .removed
                            .iter()
                            .map(|id| id.0)
                            .collect::<Vec<_>>(),
                        splice
                            .delta
                            .inserted
                            .iter()
                            .map(|id| id.0)
                            .collect::<Vec<_>>(),
                        splice.delta.after.map(|id| id.0),
                    )
                })
                .collect::<Vec<_>>()
        );
    }
    emit_view::<ParseUnits<A>>()?.insert(
        uri.clone(),
        ParseUnit::new(
            publication.root,
            publication.status.clone(),
            publication.stats,
        ),
    )?;
    emit_view::<ParseStatusDocuments>()?.insert(uri.clone(), publication.status.clone())?;
    emit_view::<ParseDiagnosticsDocuments>()?.replace(&uri, parse_diagnostics.clone())?;
    emit_view::<ParseDiagnostics>()?.replace(&uri, parse_diagnostics)?;
    let published_child_orders = publish(&publication)?;
    lifecycle.set(1)?;
    if let Some(next) = published_child_orders {
        machine
            .parser
            .set_published_child_orders(static_uri.clone(), next);
    }
    Ok(())
}

fn capture_published_node<A>(
    node: &crate::reactive::abstract_tree::AstBox<()>,
) -> Option<crate::framework::parse::types::PublishedNodeIdentity>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    let identity = node.identity_syntax()?;
    if identity.view != std::any::TypeId::of::<A>() {
        return None;
    }
    Some(crate::framework::parse::types::PublishedNodeIdentity { raw: 0, identity })
}

fn build_published_child_orders<A>(
    publication: &DeltaPublication<'_, A>,
    ast: &crate::framework::parse::data::AstArena,
    resolver: &dyn Fn(u64) -> Option<u64>,
) -> Arc<crate::reactive::store::RadixMap<crate::framework::parse::types::PublishedChildOrder>>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    let Some(_arenas) = publication.arenas else {
        return Arc::new(crate::reactive::store::RadixMap::default());
    };
    let node_for_record = |record: u64, root: bool| {
        published_node_for_record::<A>(publication, record, root, resolver)
    };
    let mut next = (*publication.previous_child_orders).clone();
    for removed in publication.delta.removed_records.iter() {
        next.remove(removed.lineage.0);
    }

    let mut refresh_record = |lineage: u64, record: u64, root: bool| {
        let Some(parent_node) =
            node_for_record(record, root).and_then(|node| capture_published_node::<A>(&node))
        else {
            return;
        };
        let child_records = syntax_child_records::<A>(ast, record);
        let mut children = Vec::with_capacity(child_records.len());
        for child_record in child_records {
            let Some(child_lineage) = publication.lineage_of(child_record as u64) else {
                return;
            };
            let Some(child_node) = node_for_record(child_record as u64, false)
                .and_then(|node| capture_published_node::<A>(&node))
            else {
                return;
            };
            children.push((child_lineage, child_node));
        }
        next.insert(
            lineage,
            crate::framework::parse::types::PublishedChildOrder {
                parent_node,
                children: children.into(),
            },
        );
    };

    for &(lineage, record) in publication.delta.inserted_records.iter() {
        refresh_record(
            lineage.0,
            record,
            publication.current_root_record == Some(record),
        );
    }

    // A retained record's payload update keeps its child order. Refresh only
    // its stable node bearer; any changed child order is applied below as a
    // bounded splice.
    for &(lineage, record) in publication.delta.updated_records.iter() {
        let Some(node) = node_for_record(record, publication.current_root_record == Some(record))
            .and_then(|node| capture_published_node::<A>(&node))
        else {
            continue;
        };
        if let Some(mut order) = next.get(lineage.0).cloned() {
            order.parent_node = node;
            next.insert(lineage.0, order);
        }
    }

    for splice in publication.delta.child_splices.iter() {
        let Some(previous) = publication.previous_child_orders.get(splice.parent.0) else {
            continue;
        };
        let start = match splice.delta.before {
            Some(before) => {
                let Some(index) = previous
                    .children
                    .iter()
                    .position(|(lineage, _)| *lineage == before.0)
                else {
                    continue;
                };
                index + 1
            }
            None => 0,
        };
        let end = match splice.delta.after {
            Some(after) => {
                let Some(index) = previous
                    .children
                    .iter()
                    .position(|(lineage, _)| *lineage == after.0)
                else {
                    continue;
                };
                index
            }
            None => previous.children.len(),
        };
        if end < start {
            continue;
        }
        let mut inserted = Vec::with_capacity(splice.inserted_children.len());
        let mut complete = true;
        for &(lineage, record) in splice.inserted_children.iter() {
            let Some(node) = node_for_record(record, false) else {
                complete = false;
                break;
            };
            let Some(node) = capture_published_node::<A>(&node) else {
                complete = false;
                break;
            };
            inserted.push((lineage.0, node));
        }
        if !complete
            || inserted.len() != splice.delta.inserted.len()
            || inserted
                .iter()
                .zip(splice.delta.inserted.iter())
                .any(|((lineage, _), expected)| *lineage != expected.0)
        {
            continue;
        }
        let mut children = previous.children.to_vec();
        children.splice(start..end, inserted);
        next.insert(
            splice.parent.0,
            crate::framework::parse::types::PublishedChildOrder {
                parent_node: previous.parent_node.clone(),
                children: children.into(),
            },
        );
    }
    Arc::new(next)
}
/// Syntax-family runtime capability supplied by generated `#[abstract_tree]`
/// syntax families: the arena member probe plus the publication dispatcher.
/// Decodes one parser arena record through the generated family adapter.
fn syntax_value<'a>(
    arena: &'a crate::framework::parse::data::AstArena,
    record: u64,
) -> Option<&'a (dyn std::any::Any + Send + Sync)> {
    arena.erased(usize::try_from(record).ok()?)
}

fn syntax_member_of<A>(
    arena: &crate::framework::parse::data::AstArena,
    record: u64,
) -> Option<&'static str>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    syntax_value(arena, record).and_then(
        <A as crate::reactive::abstract_tree::SyntaxFamilyPublication>::__syntax_member_of,
    )
}

fn syntax_child_records<A>(arena: &crate::framework::parse::data::AstArena, record: u64) -> Vec<u64>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    syntax_value(arena, record).map_or_else(Vec::new, |value| {
        <A as crate::reactive::abstract_tree::SyntaxFamilyPublication>::__syntax_child_records(
            value,
        )
    })
}

fn syntax_member_kind<A>(arena: &crate::framework::parse::data::AstArena, record: u64) -> Option<u8>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    syntax_value(arena, record).and_then(
        <A as crate::reactive::abstract_tree::SyntaxFamilyPublication>::__syntax_member_kind,
    )
}

/// Builds the published identity for one arena record under the general
/// tree schema. The root keeps the document-stable lineage 0 identity.
fn published_node_for_record<A>(
    publication: &DeltaPublication<'_, A>,
    record: u64,
    root: bool,
    resolver: &dyn Fn(u64) -> Option<u64>,
) -> Option<crate::reactive::abstract_tree::AstBox<()>>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    let member = syntax_member_of::<A>(publication.arenas?.ast.as_ref(), record)?;
    let lineage = if root { 0 } else { resolver(record)? };
    Some(crate::reactive::abstract_tree::__published_syntax_box::<
        <A as crate::reactive::abstract_tree::AbstractTreeNode>::Family,
    >(publication.uri.as_str(), lineage, member, root))
}

/// Stages the exact retractions of one removed published record: parent,
/// and surviving links, from delta-carried topology (plan §12).
fn stage_published_removed_nodes<A>(
    previous: &crate::reactive::store::RadixMap<
        crate::framework::parse::types::PublishedChildOrder,
    >,
    removed: &[RemovedRecord],
    preserve_root_record: Option<u64>,
    uri: &str,
    retractions: &mut Vec<crate::reactive::abstract_tree::TreeKey<<<A as crate::reactive::abstract_tree::AbstractTreeNode>::Family as crate::reactive::abstract_tree::AbstractTreeFamily>::Domain>>,
) -> crate::reactive::Result<()>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    for removed_record in removed {
        if preserve_root_record == Some(removed_record.record) {
            continue;
        }
        let node_identity = previous
            .get(removed_record.lineage.0)
            .map(|entry| entry.parent_node.clone())
            .or_else(|| {
                removed_record.parent_lineage.and_then(|parent| {
                    previous.get(parent).and_then(|entry| {
                        entry
                            .children
                            .iter()
                            .find(|(lineage, _)| *lineage == removed_record.lineage.0)
                            .map(|(_, identity)| identity.clone())
                    })
                })
            });
        let Some(node_identity) = node_identity else {
            continue;
        };
        let Some(node) = published_node_from_identity::<A>(uri, &node_identity.identity) else {
            continue;
        };
        retractions.push(crate::reactive::abstract_tree::TreeKey::Parent(
            node.clone(),
        ));
    }
    Ok(())
}

fn published_node_from_identity<A>(
    uri: &str,
    identity: &crate::reactive::view::SyntaxNodeIdentity,
) -> Option<crate::reactive::abstract_tree::AstBox<()>>
where
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication,
{
    if identity.view != std::any::TypeId::of::<A>() || identity.uri.as_ref() != uri {
        return None;
    }
    Some(crate::reactive::abstract_tree::__published_syntax_box::<
        <A as crate::reactive::abstract_tree::AbstractTreeNode>::Family,
    >(
        identity.uri.as_ref(),
        identity.lineage,
        identity.member,
        identity.root,
    ))
}

fn parser_document_tree<R, A>(machine: Arc<ParserMachine<R, A>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication
        + 'static,
{
    parser_document_with(machine, uri, |publication: &DeltaPublication<'_, A>| {
        let Some(arenas) = publication.arenas else {
            emit_view::<ParserTreeStatuses>()?.remove(publication.uri.to_string())?;
            close_parser_dimensions::<A>(&publication.uri)?;
            return Ok(None);
        };
        let ast = arenas.ast.as_ref();
        let resolver = move |record: u64| -> Option<u64> { publication.lineage_of(record) };
        type DomainOf<A> = <<A as crate::reactive::abstract_tree::AbstractTreeNode>::Family as crate::reactive::abstract_tree::AbstractTreeFamily>::Domain;
        let mut facts: Vec<(
            crate::reactive::abstract_tree::TreeKey<DomainOf<A>>,
            crate::reactive::abstract_tree::TreeFact,
        )> = Vec::new();
        let project = |record: u64| -> Option<crate::reactive::abstract_tree::AstBox<()>> {
            published_node_for_record::<A>(publication, record, false, &resolver)
        };
        for (record, ()) in publication.delta.live_records.iter() {
            let is_root = publication.current_root_record == Some(record);
            let Some(node) =
                published_node_for_record::<A>(publication, record, is_root, &resolver)
            else {
                continue;
            };
            let Some(value) = ast.erased(usize::try_from(record).ok().unwrap_or(usize::MAX)) else {
                continue;
            };
            let _ =
                <A as crate::reactive::abstract_tree::SyntaxFamilyPublication>::__syntax_publish(
                    node, value, is_root, &project, &mut facts,
                )?;
        }
        crate::reactive::abstract_tree::__emit_facts::<
            <A as crate::reactive::abstract_tree::AbstractTreeNode>::Family,
        >(facts)?;

        let mut root_facts: Vec<(
            crate::reactive::abstract_tree::TreeKey<DomainOf<A>>,
            crate::reactive::abstract_tree::TreeFact,
        )> = Vec::new();
        if let Some(root_node) = publication
            .current_root_record
            .and_then(|record| published_node_for_record::<A>(publication, record, true, &resolver))
        {
            let domain =
                <A as crate::reactive::abstract_tree::SyntaxFamilyPublication>::__domain_from_uri(
                    publication.uri.as_str(),
                );
            root_facts.push((
                crate::reactive::abstract_tree::TreeKey::RootOrder(domain.clone()),
                crate::reactive::abstract_tree::TreeFact::RootOrder(
                    std::iter::once(root_node.clone()).collect(),
                ),
            ));
            root_facts.push((
                crate::reactive::abstract_tree::TreeKey::RootLink(domain, root_node.clone()),
                crate::reactive::abstract_tree::TreeFact::RootLink(root_node),
            ));
        }
        crate::reactive::abstract_tree::__emit_facts::<
            <A as crate::reactive::abstract_tree::AbstractTreeNode>::Family,
        >(root_facts)?;
        emit_view::<ParserTreeStatuses>()?
            .insert(publication.uri.clone(), publication.status.clone())?;

        let next_orders = build_published_child_orders(publication, ast, &resolver);
        Ok(Some(next_orders))
    })
}

fn parser_document<R, A>(machine: Arc<ParserMachine<R, A>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    parser_document_with(machine, uri, |_publication| Ok(None))
}
/// Refreshes editor-facing AST coordinates after layout-sensitive changes.
///
/// The semantic parser is keyed by `LexedDocuments`, whose equality is
/// semantic-revision-only. This companion child is keyed by `Tokens` and
/// publishes the immutable editor facade without changing parser/tree facts.
fn parser_layout_snapshot<R, A>(machine: Arc<ParserMachine<R, A>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    crate::framework::workspace::record_parser_work(&uri, |work| {
        work.layout_projection_runs += 1;
    });
    let tokens = observe_view::<crate::framework::lex::TokenLayoutDocuments<R>>()?.get(&uri)?;
    let Some(tokens) = tokens else {
        emit_view::<AstSnapshots<A>>()?.remove(uri);
        return Ok(());
    };
    let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
    let current = machine
        .parser
        .document_root(&static_uri)
        .and_then(|root| root.token.clone());
    let Some(current) = current else {
        return Ok(());
    };
    // The semantic parser is keyed by structural revisions only, so a
    // value-only or layout-only edit leaves the stored token document at an
    // older layout revision. Rebuild the coordinate root for the editor
    // snapshot and record the reused-coordinate work without a semantic
    let token_document = Arc::new(
        crate::framework::parse::types::ParserTokenDocument::from_lexical(&tokens.document),
    );
    if current.layout_revision().0 < token_document.layout_revision().0 {
        let session_columns = machine
            .parser
            .document_root(&static_uri)
            .map_or(0, |root| root.session.column_count());
        crate::framework::workspace::record_parser_work(&uri, |work| {
            work.columns_reused += session_columns.saturating_sub(1) as u64;
            work.tokens_reused += current.semantic_len() as u64;
        });
    }
    let snapshot = machine
        .parser
        .commit_snapshot(
            static_uri,
            Arc::clone(&tokens.document.source),
            token_document,
        )
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
    let machine = Arc::new(ParserMachine {
        parser,
        _ast: PhantomData,
    });
    // Document-scoped first-class components (Cut C): one stable child
    // invocation per document, identity = definition + URI.
    let semantic_machine = Arc::clone(&machine);
    engine.install_component_each_key::<ParserSemanticDefinition<R, A>, LexedDocuments<R>, _>(
        move |uri| parser_document::<R, A>(Arc::clone(&semantic_machine), uri),
    )?;
    let layout_machine = Arc::clone(&machine);
    engine.install_component_each_key::<ParserLayoutDefinition<R, A>, TokenLayoutDocuments<R>, _>(
        move |uri| parser_layout_snapshot::<R, A>(Arc::clone(&layout_machine), uri),
    )?;
    Ok(())
}

/// Definition markers for the framework parser stages (Cut C).
#[doc(hidden)]
pub struct ParserSemanticDefinition<R, A>(PhantomData<fn() -> (R, A)>);
#[doc(hidden)]
pub struct ParserLayoutDefinition<R, A>(PhantomData<fn() -> (R, A)>);

impl<R, A> crate::reactive::component::ComponentDefinition for ParserSemanticDefinition<R, A> {
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::semantic"
    }
}
impl<R, A> crate::reactive::component::ComponentDefinition for ParserLayoutDefinition<R, A> {
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::layout"
    }
}

/// Installs the parser plus generated abstract-tree facts as ordinary roots.
pub fn install_parser_tree<R, A>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: crate::framework::parse::__macro_private::NonTerminalSpec
        + crate::reactive::abstract_tree::AbstractTreeNode
        + crate::reactive::abstract_tree::SyntaxFamilyPublication
        + 'static,
{
    use crate::framework::lex::{LexedDocuments, TokenLayoutDocuments};
    let mut parser = crate::framework::parse::grammar::Grammar::from_spec::<A>().build_lr1::<R>();
    parser.tree_member_kind = Some(syntax_member_kind::<A>);
    parser.tree_child_records = Some(syntax_child_records::<A>);
    let machine = Arc::new(ParserMachine {
        parser,
        _ast: PhantomData,
    });
    // Document-scoped first-class components (see install_parser).
    let semantic_machine = Arc::clone(&machine);
    engine.install_component_each_key::<ParserTreeSemanticDefinition<R, A>, LexedDocuments<R>, _>(
        move |uri| parser_document_tree::<R, A>(Arc::clone(&semantic_machine), uri),
    )?;
    let layout_machine = Arc::clone(&machine);
    engine.install_component_each_key::<ParserLayoutDefinition<R, A>, TokenLayoutDocuments<R>, _>(
        move |uri| parser_layout_snapshot::<R, A>(Arc::clone(&layout_machine), uri),
    )?;

    Ok(())
}

/// Tree-flavor semantic stage marker (distinct descriptor from the
/// tree-less variant so both may coexist in one engine).
#[doc(hidden)]
pub struct ParserTreeSemanticDefinition<R, A>(PhantomData<fn() -> (R, A)>);

impl<R, A> crate::reactive::component::ComponentDefinition for ParserTreeSemanticDefinition<R, A> {
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::tree_semantic"
    }
}
