//! Plain-function parser pipeline.
//!
//! Parser state remains private inside one mutex-protected machine. The
//! authored reactive boundary is the per-document `run` call: token-domain
//! changes schedule only the affected parser invocation, and omitted writes
//! retract closed documents.

use std::{collections::BTreeSet, marker::PhantomData, sync::Arc};

use crate::framework::parse::delta::RemovedRecord;

use crate::framework::lex::LexerRoot;
use fluent_uri::Uri;
use parking_lot::Mutex;

use crate::framework::parse::data::ast::AstBox;
use crate::framework::parse::{
    DocumentSnapshot, IncrementalParseStats, ParseErrorInfo, ParseStatus, Parser,
};
use crate::reactive::kind::{
    List, Map, TreeKey, TreePatch, TreeView, ViewKind, emit_patch, emit_view, observe_view,
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
            root: self.root.clone(),
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
        self.root == other.root && self.status == other.status && self.revision == other.revision
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

/// Stable parser payloads keyed by generated syntax-node identity.
///
/// The parser keeps its arena and lineage machinery private; this map is the
/// public payload projection consumed by parser clients and lowering passes.
#[view]
pub struct ParserTreePayloads<A: crate::framework::parse::AbstractTreeFamily>(
    Map<crate::reactive::view::Node<A::View>, A::Node>,
);

/// Stable parent/child edges keyed by the two generated syntax-node
/// identities. Edge membership is independent of payload and order facts.
#[view]
pub struct ParserTreeEdges<A: crate::framework::parse::AbstractTreeFamily>(
    Map<
        (
            crate::reactive::view::Node<A::View>,
            crate::reactive::view::Node<A::View>,
        ),
        (),
    >,
);
/// Direct parent facts keyed by child syntax-node identity. Root nodes carry
/// an explicit `None` parent row; non-roots carry their stable parent node.
#[view]
pub struct ParserTreeParents<A: crate::framework::parse::AbstractTreeFamily>(
    Map<
        crate::reactive::view::Node<A::View>,
        Option<crate::reactive::view::Node<A::View>>,
    >,
);


/// One accepted parser root per document URI.
#[view]
pub struct ParserTreeRoots<A: crate::framework::parse::AbstractTreeFamily>(
    Map<String, crate::reactive::view::Node<A::View>>,
);

/// Ordered child identities for each parser node.
#[view]
pub struct ParserTreeOrders<A: crate::framework::parse::AbstractTreeFamily>(
    Map<
        crate::reactive::view::Node<A::View>,
        Arc<[crate::reactive::view::Node<A::View>]>,
    >,
);
/// Descriptive alias for the ordered child-fact projection.
pub type ParserTreeChildOrders<A> = ParserTreeOrders<A>;

/// Descriptive alias for the parent/child edge projection.
pub type ParserTreeFieldEdges<A> = ParserTreeEdges<A>;

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

/// Exact parser-private syntax dimensions. These views are not part of the
/// public tree facade; generated projection components consume them.
#[view]
pub(crate) struct ParserNestedSyntaxNodes(Map<ParserSyntaxKey, ()>);

#[view]
pub(crate) struct ParserSyntaxPayloads<A: crate::framework::parse::AbstractTreeFamily>(
    Map<ParserSyntaxKey, A::Node>,
);

#[view]
pub(crate) struct ParserSyntaxParents(Map<ParserSyntaxKey, ParserSyntaxKey>);

#[view]
pub(crate) struct ParserSyntaxFieldEdges(Map<ParserFieldEdgeKey, ()>);

#[view]
pub(crate) struct ParserSyntaxFirstInField(Map<ParserFieldKey, ParserSyntaxKey>);

#[view]
pub(crate) struct ParserSyntaxLastInField(Map<ParserFieldKey, ParserSyntaxKey>);

#[view]
pub(crate) struct ParserSyntaxNextInField(Map<ParserSyntaxKey, ParserSyntaxKey>);

#[view]
pub(crate) struct ParserSyntaxRoots(Map<ParserRootKey, ParserSyntaxKey>);

#[view]
pub(crate) struct ParserSyntaxOrders(
    Map<ParserSyntaxKey, Arc<[ParserSyntaxKey]>>,
);

#[view]
pub(crate) struct ProjectedSyntaxNodes<A: crate::framework::parse::AbstractTreeFamily>(
    Map<ParserSyntaxKey, crate::reactive::view::Node<A::View>>,
);

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
        Arc<std::collections::HashMap<u64, crate::framework::parse::types::PublishedChildOrder>>,
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
            previous_child_orders: Arc::new(std::collections::HashMap::new()),
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
        session_lineage.or_else(|| {
            self.delta
                .inserted_records
                .iter()
                .chain(self.delta.updated_records.iter())
                .find_map(|(lineage, candidate)| {
                    (*candidate == record).then_some(lineage.0)
                })
        })
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
    A: crate::framework::parse::AbstractTreeFamily,
{
    let mut parent = ast.parent_of(record as usize);
    while let Some(parent_record) = parent {
        if A::__tree_member_kind_of(ast, parent_record as u64).is_some()
            && let Some(lineage) = publication.lineage_of(parent_record as u64)
        {
            return Some(ParserSyntaxKey::new(&publication.uri, lineage));
        }
        parent = ast.parent_of(parent_record);
    }
    None
}
fn close_parser_dimensions<A>(uri: &str) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let nested_keys = observe_view::<ParserNestedSyntaxNodes>()?.keys()?;
    let nested = emit_view::<ParserNestedSyntaxNodes>()?;
    let payloads = emit_view::<ParserSyntaxPayloads<A>>()?;
    let parents = emit_view::<ParserSyntaxParents>()?;
    let projected = emit_view::<ProjectedSyntaxNodes<A>>()?;
    for key in nested_keys.into_iter().filter(|key| key.uri == uri) {
        nested.remove(key.clone())?;
        payloads.remove(key.clone())?;
        parents.remove(key.clone())?;
        projected.remove(key)?;
    }

    let edges = emit_view::<ParserSyntaxFieldEdges>()?;
    for key in observe_view::<ParserSyntaxFieldEdges>()?
        .keys()?
        .into_iter()
        .filter(|key| key.parent.uri == uri)
    {
        edges.remove(key)?;
    }
    let first = emit_view::<ParserSyntaxFirstInField>()?;
    let last = emit_view::<ParserSyntaxLastInField>()?;
    for key in observe_view::<ParserSyntaxFirstInField>()?
        .keys()?
        .into_iter()
        .filter(|key| key.parent.uri == uri)
    {
        first.remove(key.clone())?;
        last.remove(key)?;
    }
    let next = emit_view::<ParserSyntaxNextInField>()?;
    for key in observe_view::<ParserSyntaxNextInField>()?
        .keys()?
        .into_iter()
        .filter(|key| key.uri == uri)
    {
        next.remove(key)?;
    }
    let orders = emit_view::<ParserSyntaxOrders>()?;
    for key in observe_view::<ParserSyntaxOrders>()?
        .keys()?
        .into_iter()
        .filter(|key| key.uri == uri)
    {
        orders.remove(key)?;
    }

    emit_view::<ParserSyntaxRoots>()?.remove(ParserRootKey::new(uri))?;
    Ok(())
}

/// Projects the authoritative parser delta into framework-private exact
/// dimensions. Status and diagnostics deliberately do not call this helper.
fn publish_parser_dimensions<A>(
    publication: &DeltaPublication<'_, A>,
    ast: &crate::framework::parse::data::AstArena,
    resolver: &dyn Fn(u64) -> Option<u64>,
) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
    A::View: crate::reactive::kind::TreeView,
{
    if publication.arenas.is_none() {
        return close_parser_dimensions::<A>(&publication.uri);
    }

    let delta = &publication.delta;
    let syntax_changed = !delta.syntax_payloads.is_empty()
        || !delta.child_splices.is_empty()
        || !delta.roots.inserted.is_empty()
        || !delta.roots.removed.is_empty()
        || !delta.inserted_records.is_empty()
        || !delta.removed_records.is_empty()
        || !delta.updated_records.is_empty();
    if !syntax_changed {
        return Ok(());
    }

    let nested = emit_patch::<ParserNestedSyntaxNodes>()?;
    let payloads = emit_patch::<ParserSyntaxPayloads<A>>()?;
    let parents = emit_patch::<ParserSyntaxParents>()?;
    let edges = emit_patch::<ParserSyntaxFieldEdges>()?;
    let first = emit_patch::<ParserSyntaxFirstInField>()?;
    let last = emit_patch::<ParserSyntaxLastInField>()?;
    let next = emit_patch::<ParserSyntaxNextInField>()?;
    let roots = emit_patch::<ParserSyntaxRoots>()?;
    let projected = emit_patch::<ProjectedSyntaxNodes<A>>()?;

    // The first publication is the one allowed initialization walk. Its
    // delta is a frontier in recovery cases, so seed every live record once.
    // Steady-state publications touch only inserted records and splice
    // middles; they never rediscover a retained parent's full child list.
    let mut dimension_records = Vec::new();
    let mut seen_records = BTreeSet::new();
    for &(lineage, record) in delta
        .inserted_records
        .iter()
        .chain(delta.updated_records.iter())
    {
        if seen_records.insert(record) {
            dimension_records.push((lineage.0, record));
        }
    }

    let mut dimension_member_keys = BTreeSet::new();
    for &(lineage, record) in &dimension_records {
        let key = ParserSyntaxKey::new(&publication.uri, lineage);
        let Some(payload) = A::__tree_payload_for_record(ast, record) else {
            continue;
        };
        dimension_member_keys.insert(key.clone());
        payloads.upsert(key.clone(), payload)?;
        if let Some(node) = A::__tree_plain_node_for_record(
            &publication.uri,
            ast,
            record,
            publication.current_root_record == Some(record),
            resolver,
        ) {
            projected.upsert(key.clone(), node)?;
        }
        if publication.current_root_record == Some(record) {
            nested.remove(key.clone())?;
            parents.remove(key.clone())?;
            roots.upsert(ParserRootKey::new(&publication.uri), key.clone())?;
        } else {
            nested.upsert(key.clone(), ())?;
            match nearest_tree_parent(publication, ast, record) {
                Some(parent) => parents.upsert(key.clone(), parent)?,
                None => parents.remove(key.clone())?,
            }
        }
    }
    let mut removed_next = BTreeSet::new();

    for removed in delta.removed_records.iter() {
        if publication.bearer_of(removed.lineage.0).is_some() {
            continue;
        }
        let key = ParserSyntaxKey::new(&publication.uri, removed.lineage.0);
        nested.remove(key.clone())?;
        payloads.remove(key.clone())?;
        parents.remove(key.clone())?;
        projected.remove(key.clone())?;
        if let Some(order) = publication.previous_child_orders.get(&removed.lineage.0) {
            for &(child_lineage, _) in order.children.iter() {
                edges.remove(ParserFieldEdgeKey {
                    parent: key.clone(),
                    child: ParserSyntaxKey::new(&publication.uri, child_lineage),
                })?;
                // The old parent is gone. Clear the removed child's outgoing
                // adjacency unconditionally; if the child survives under a
                // new parent, that parent's local order pass overwrites the
                // entry with its actual successor.
                removed_next.insert(child_lineage);
            }
            let field = ParserFieldKey {
                parent: key,
                field: 0,
            };
            first.remove(field.clone())?;
            last.remove(field)?;
        }
    }
    if publication.current_root_record.is_none() {
        roots.remove(ParserRootKey::new(&publication.uri))?;
    }

    // Newly inserted parents need their complete local field facts. A
    // retained parent's changed order is handled by the bounded splice path
    // below. If an old order is absent, fall back to this local initialization
    // path rather than scanning the document.
    let mut full_parent_records = std::collections::BTreeMap::<u64, u64>::new();
    for &(lineage, record) in delta.inserted_records.iter() {
        full_parent_records.insert(lineage.0, record);
    }
    for splice in delta.child_splices.iter() {
        if !publication
            .previous_child_orders
            .contains_key(&splice.parent.0)
            && let Some(record) = publication.bearer_of(splice.parent.0)
        {
            full_parent_records
                .entry(splice.parent.0)
                .or_insert(record);
        }
    }

    let mut next_ops = std::collections::BTreeMap::<u64, Option<u64>>::new();
    for child_lineage in removed_next {
        next_ops.insert(child_lineage, None);
    }
    for (&lineage, &record) in full_parent_records.iter() {
        let parent = ParserSyntaxKey::new(&publication.uri, lineage);
        let child_records = A::__tree_plain_child_records(ast, record);
        let mut child_keys = Vec::with_capacity(child_records.len());
        for child_record in child_records {
            if A::__tree_member_kind_of(ast, child_record).is_none() {
                continue;
            }
            let Some(child_lineage) = publication.lineage_of(child_record) else {
                continue;
            };
            let child = ParserSyntaxKey::new(&publication.uri, child_lineage);
            child_keys.push(child.clone());
            if dimension_member_keys.insert(child.clone()) {
                parents.upsert(child.clone(), parent.clone())?;
                nested.upsert(child, ())?;
            }
        }

        let previous = publication.previous_child_orders.get(&lineage);
        let old_children: BTreeSet<u64> = previous
            .map(|order| order.children.iter().map(|(child, _)| *child).collect())
            .unwrap_or_default();
        let current_children: BTreeSet<u64> =
            child_keys.iter().map(ParserSyntaxKey::lineage).collect();
        if let Some(previous) = previous {
            for &(old_child, _) in previous.children.iter() {
                if !current_children.contains(&old_child) {
                    edges.remove(ParserFieldEdgeKey {
                        parent: parent.clone(),
                        child: ParserSyntaxKey::new(&publication.uri, old_child),
                    })?;
                }
                next_ops.insert(old_child, None);
            }
        }
        for child in &child_keys {
            if !old_children.contains(&child.lineage()) {
                edges.upsert(
                    ParserFieldEdgeKey {
                        parent: parent.clone(),
                        child: child.clone(),
                    },
                    (),
                )?;
            }
        }
        for pair in child_keys.windows(2) {
            next_ops.insert(
                pair[0].lineage(),
                Some(pair[1].lineage()),
            );
        }
        let field = ParserFieldKey {
            parent,
            field: 0,
        };
        match (child_keys.first(), child_keys.last()) {
            (Some(first_child), Some(last_child)) => {
                first.upsert(field.clone(), first_child.clone())?;
                last.upsert(field, last_child.clone())?;
            }
            (None, None) => {
                first.remove(field.clone())?;
                last.remove(field)?;
            }
            _ => unreachable!("first and last share one child order"),
        }
    }

    // Apply retained-parent splices without reading or reconstructing the old
    // order. The previous published order provides exact identity anchors.
    for splice in delta.child_splices.iter() {
        if full_parent_records.contains_key(&splice.parent.0) {
            continue;
        }
        let Some(previous) = publication.previous_child_orders.get(&splice.parent.0) else {
            continue;
        };
        let parent = ParserSyntaxKey::new(&publication.uri, splice.parent.0);
        for child in splice.delta.removed.iter() {
            edges.remove(ParserFieldEdgeKey {
                parent: parent.clone(),
                child: ParserSyntaxKey::new(&publication.uri, child.0),
            })?;
            next_ops.insert(child.0, None);
        }
        for &(child, _) in splice.inserted_children.iter() {
            let child_key = ParserSyntaxKey::new(&publication.uri, child.0);
            edges.upsert(
                ParserFieldEdgeKey {
                    parent: parent.clone(),
                    child: child_key.clone(),
                },
                (),
            )?;
            // A retained child can move between stable parents while its
            // syntax identity stays unchanged. The bounded splice updates
            // adjacency, but the private parent dimension is independent.
            if !dimension_member_keys.contains(&child_key) {
                parents.upsert(child_key, parent.clone())?;
            }
        }

        let inserted: Vec<u64> = splice.delta.inserted.iter().map(|id| id.0).collect();
        if let Some(before) = splice.delta.before {
            let target = inserted.first().copied().or_else(|| splice.delta.after.map(|id| id.0));
            next_ops.insert(before.0, target);
        }
        for pair in inserted.windows(2) {
            next_ops.insert(pair[0], Some(pair[1]));
        }
        if let Some(last) = inserted.last().copied() {
            next_ops.insert(last, splice.delta.after.map(|id| id.0));
        }

        let old_first = previous.children.first().map(|(child, _)| *child);
        let old_last = previous.children.last().map(|(child, _)| *child);
        let new_first = if splice.delta.before.is_none() {
            inserted
                .first()
                .copied()
                .or_else(|| splice.delta.after.map(|id| id.0))
        } else {
            old_first
        };
        let new_last = if splice.delta.after.is_none() {
            inserted
                .last()
                .copied()
                .or_else(|| splice.delta.before.map(|id| id.0))
        } else {
            old_last
        };
        let field = ParserFieldKey {
            parent,
            field: 0,
        };
        match new_first {
            Some(child) => first.upsert(
                field.clone(),
                ParserSyntaxKey::new(&publication.uri, child),
            )?,
            None => first.remove(field.clone())?,
        }
        match new_last {
            Some(child) => last.upsert(
                field,
                ParserSyntaxKey::new(&publication.uri, child),
            )?,
            None => last.remove(field)?,
        }
    }

    for (child, target) in next_ops {
        let key = ParserSyntaxKey::new(&publication.uri, child);
        if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
            eprintln!("next-op uri={} child={} target={target:?}", publication.uri, child);
        }
        match target {
            Some(next_child) => next.upsert(
                key,
                ParserSyntaxKey::new(&publication.uri, next_child),
            )?,
            None => next.remove(key)?,
        }
    }
    Ok(())
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
    P: FnOnce(
        &DeltaPublication<'_, A>,
    ) -> Result<
        Option<
            Arc<
                std::collections::HashMap<u64, crate::framework::parse::types::PublishedChildOrder>,
            >,
        >,
    >,
{
    use crate::framework::parse::delta::ParsedStatus;

    let machine_arc = Arc::clone(&machine);
    let lifecycle = state_cell::<u8>();
    let was_active = lifecycle.with(|value| value.is_some())?;
    let semantic_observe = observe_view::<crate::framework::lex::LexedDocuments<R>>()?;
    let Some(lexed) = semantic_observe.get(&uri)? else {
        crate::framework::workspace::record_parser_work(&uri, |work| {
            work.component_runs += 1;
        });
        let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
        let mut machine_guard = machine.lock();
        let publication = DeltaPublication::<A>::empty(uri.clone());
        let result = (|| {
            emit_view::<ParseStatusDocuments>()?.remove(uri.clone())?;
            emit_view::<ParseDiagnosticsDocuments>()?.clear(&uri)?;
            publish(&publication).map(|_| ())
        })();
        drop(machine_guard);
        // Normal close releases private parser document state (plan §7.7
        // and hard invariant 15): sessions, roots, token roots, stats,
        // tree facts/deltas, semantic revisions, diagnostics, and the
        // session arenas all drop together.
        let mut machine_guard = machine.lock();
        machine_guard.parser.forget_document(&static_uri);
        return result;
    };
    let mut machine = machine.lock();
    let static_uri: Uri<String> = uri.parse().expect("workspace uris are valid");
    if !was_active {
        machine.parser.forget_document(&static_uri);
    }
    // The root record THIS command replaces, captured pre-replay for
    // document-stable-root suppression (plan §11).
    let previous_root_record = machine
        .parser
        .latest
        .tree_facts
        .get(&static_uri)
        .and_then(|facts| facts.root);
    let previous_child_orders = machine
        .parser
        .latest
        .tree_facts
        .get(&static_uri)
        .map(|facts| Arc::clone(&facts.published_child_orders))
        .unwrap_or_else(|| Arc::new(std::collections::HashMap::new()));
    crate::framework::workspace::record_parser_work(&uri, |work| {
        work.component_runs += 1;
    });
    let token_document = Arc::new(
        crate::framework::parse::types::ParserTokenDocument::from_lexical(&lexed.document),
    );
    // Cut B participant bridge (plan section 20.1): stage this document's
    // parser session for the command. The previous session root is one Arc
    // clone; on any failure the participant reinstates it after fact
    // rollback, so lineage/recovery state can never wedge across commands.
    {
        let previous_session = machine.parser.latest.sessions.get(&static_uri).cloned();
        let machine_for_undo = Arc::clone(&machine_arc);
        let undo_uri = static_uri.clone();
        crate::reactive::plain::with_txn_pub(move |txn| {
            txn.private_undos.push(Box::new(move || {
                let mut machine_guard = machine_for_undo.lock();
                let latest = Arc::make_mut(&mut machine_guard.parser.latest);
                match previous_session {
                    Some(session) => {
                        latest.sessions.insert(undo_uri.clone(), session);
                    }
                    None => {
                        latest.sessions.remove(&undo_uri);
                    }
                }
            }));
        });
    }
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
        previous_child_orders,
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
    if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
        eprintln!(
            "publication uri={} rev={} root={:?} current_root_record={:?} previous_root_record={:?} prev_orders={:?} inserted={:?} updated={:?} removed={:?} roots+={:?} roots-={:?} payloads={} splices={:?}",
            publication.uri,
            publication.revision,
            publication.root,
            publication.current_root_record,
            publication.previous_root_record,
            publication.previous_child_orders.keys().collect::<Vec<_>>(),
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
                        splice.delta.removed.iter().map(|id| id.0).collect::<Vec<_>>(),
                        splice.delta.inserted.iter().map(|id| id.0).collect::<Vec<_>>(),
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
        let latest = Arc::make_mut(&mut machine.parser.latest);
        if let Some(facts) = latest.tree_facts.get_mut(&static_uri) {
            Arc::make_mut(facts).published_child_orders = next;
        }
    }
    Ok(())
}

fn capture_published_node<V: 'static>(
    node: &crate::reactive::view::Node<V>,
) -> Option<crate::framework::parse::types::PublishedNodeIdentity> {
    Some(
        crate::framework::parse::types::PublishedNodeIdentity {
            raw: node.raw_id(),
            identity: node.syntax_identity()?,
        },
    )
}

fn rehydrate_published_node<V: 'static>(
    published: &crate::framework::parse::types::PublishedNodeIdentity,
) -> Option<crate::reactive::view::Node<V>> {
    if published.identity.view != std::any::TypeId::of::<V>() {
        return None;
    }
    Some(crate::reactive::view::Node::from_syntax(
        published.raw,
        published.identity.uri.as_ref(),
        published.identity.lineage,
        published.identity.member,
        published.identity.root,
    ))
}

fn build_published_child_orders<A>(
    publication: &DeltaPublication<'_, A>,
    ast: &crate::framework::parse::data::AstArena,
    resolver: &dyn Fn(u64) -> Option<u64>,
) -> Arc<std::collections::HashMap<u64, crate::framework::parse::types::PublishedChildOrder>>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let Some(_arenas) = publication.arenas else {
        return Arc::new(std::collections::HashMap::new());
    };
    let node_for_record = |record: u64, root: bool| {
        A::__tree_plain_node_for_record(
            publication.uri.as_str(),
            ast,
            record,
            root,
            resolver,
        )
        .or_else(|| {
            let member = A::__tree_member_kind_of(ast, record)?;
            if root {
                Some(A::__root_node(publication.uri.as_str(), member))
            } else {
                let lineage = publication.lineage_of(record)?;
                Some(A::__node_from_parts(
                    publication.uri.as_str(),
                    lineage,
                    member,
                ))
            }
        })
    };
    let mut next = (*publication.previous_child_orders).clone();
    for removed in publication.delta.removed_records.iter() {
        next.remove(&removed.lineage.0);
    }

    let mut refresh_record = |lineage: u64, record: u64, root: bool| {
        let Some(parent_node) = node_for_record(record, root)
            .and_then(|node| capture_published_node(&node))
        else {
            return;
        };
        let child_records = A::__tree_plain_child_records(ast, record);
        let mut children = Vec::with_capacity(child_records.len());
        for child_record in child_records {
            let Some(child_lineage) = publication.lineage_of(child_record as u64) else {
                return;
            };
            let Some(child_node) = node_for_record(child_record as u64, false)
                .and_then(|node| capture_published_node(&node))
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
        let Some(node) = node_for_record(
            record,
            publication.current_root_record == Some(record),
        )
        .and_then(|node| capture_published_node(&node))
        else {
            continue;
        };
        if let Some(order) = next.get_mut(&lineage.0) {
            order.parent_node = node;
        }
    }

    for splice in publication.delta.child_splices.iter() {
        let Some(previous) = publication.previous_child_orders.get(&splice.parent.0) else {
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
            let Some(node) = capture_published_node(&node) else {
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
fn publish_parser_orders<A>(
    publication: &DeltaPublication<'_, A>,
    next: &std::collections::HashMap<
        u64,
        crate::framework::parse::types::PublishedChildOrder,
    >,
) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let mut changed = BTreeSet::new();
    changed.extend(
        publication
            .delta
            .inserted_records
            .iter()
            .map(|(lineage, _)| lineage.0),
    );
    changed.extend(
        publication
            .delta
            .updated_records
            .iter()
            .map(|(lineage, _)| lineage.0),
    );
    changed.extend(
        publication
            .delta
            .removed_records
            .iter()
            .map(|removed| removed.lineage.0),
    );
    changed.extend(
        publication
            .delta
            .child_splices
            .iter()
            .map(|splice| splice.parent.0),
    );

    let orders = emit_patch::<ParserSyntaxOrders>()?;
    for lineage in changed {
        let key = ParserSyntaxKey::new(&publication.uri, lineage);
        match next.get(&lineage) {
            Some(order) => {
                let children: Vec<_> = order
                    .children
                    .iter()
                    .map(|(child, _)| ParserSyntaxKey::new(&publication.uri, *child))
                    .collect();
                orders.upsert(key, Arc::from(children))?;
            }
            None => orders.remove(key)?,
        }
    }
    Ok(())
}

fn project_parser_parent<A>(key: ParserSyntaxKey) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let Some(parent_key) = observe_view::<ParserSyntaxParents>()?.get(&key)? else {
        return Ok(());
    };
    let Some(child) = observe_view::<ProjectedSyntaxNodes<A>>()?.get(&key)? else {
        return Ok(());
    };
    let Some(parent) = observe_view::<ProjectedSyntaxNodes<A>>()?
        .get(&parent_key)?
    else {
        return Ok(());
    };
    emit_view::<ParserTreeParents<A>>()?.insert(
        child.as_ref().clone(),
        parent.as_ref().clone().into(),
    )
}


fn project_parser_payload<A>(key: ParserSyntaxKey) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let Some(payload) = observe_view::<ParserSyntaxPayloads<A>>()?.get(&key)? else {
        return Ok(());
    };
    let Some(node) = observe_view::<ProjectedSyntaxNodes<A>>()?.get(&key)? else {
        return Ok(());
    };
    emit_view::<ParserTreePayloads<A>>()?.insert(node.as_ref().clone(), (*payload).clone())
}

fn project_parser_edge<A>(key: ParserFieldEdgeKey) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    if observe_view::<ParserSyntaxFieldEdges>()?.get(&key)?.is_none() {
        return Ok(());
    }
    let Some(parent) = observe_view::<ProjectedSyntaxNodes<A>>()?.get(&key.parent)? else {
        return Ok(());
    };
    let Some(child) = observe_view::<ProjectedSyntaxNodes<A>>()?.get(&key.child)? else {
        return Ok(());
    };
    emit_view::<ParserTreeEdges<A>>()?.insert(
        (parent.as_ref().clone(), child.as_ref().clone()),
        (),
    )
}

fn project_parser_root<A>(key: ParserRootKey) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let Some(source_root) = observe_view::<ParserSyntaxRoots>()?.get(&key)? else {
        return Ok(());
    };
    let Some(root) = observe_view::<ProjectedSyntaxNodes<A>>()?
        .get(&source_root)?
    else {
        return Ok(());
    };
    emit_view::<ParserTreeRoots<A>>()?.insert(key.uri.clone(), root.as_ref().clone())?;
    emit_view::<ParserTreeParents<A>>()?.insert(root.as_ref().clone(), None)
}

fn project_parser_order<A>(key: ParserSyntaxKey) -> Result<()>
where
    A: crate::framework::parse::AbstractTreeFamily,
{
    let Some(order) = observe_view::<ParserSyntaxOrders>()?.get(&key)? else {
        return Ok(());
    };
    let Some(parent) = observe_view::<ProjectedSyntaxNodes<A>>()?.get(&key)? else {
        return Ok(());
    };
    let mut children = Vec::with_capacity(order.len());
    for child_key in order.iter() {
        let Some(child) = observe_view::<ProjectedSyntaxNodes<A>>()?
            .get(child_key)?
        else {
            return Ok(());
        };
        children.push(child.as_ref().clone());
    }
    emit_view::<ParserTreeOrders<A>>()?.insert(parent.as_ref().clone(), Arc::from(children))
}

fn stage_published_removed_nodes<V>(
    previous: &std::collections::HashMap<u64, crate::framework::parse::types::PublishedChildOrder>,
    removed: &[RemovedRecord],
    preserve_root_record: Option<u64>,
    retractions: &mut Vec<crate::reactive::kind::TreeKey<String, crate::reactive::view::Node<V>>>,
) -> crate::reactive::Result<()>
where
    V: crate::reactive::kind::TreeView,
{
    for removed_record in removed {
        if preserve_root_record == Some(removed_record.record) {
            continue;
        }
        if std::env::var_os("PLINGO_TRACE_REMOVALS").is_some() {
            eprintln!(
                "removed-stage lineage={} record={} parent={:?} previous={:?}",
                removed_record.lineage.0,
                removed_record.record,
                removed_record.parent_lineage,
                previous
                    .get(&removed_record.lineage.0)
                    .map(|entry| (entry.parent_node.clone(), entry.children.clone()))
            );
        }
        let node_identity = previous
            .get(&removed_record.lineage.0)
            .map(|entry| entry.parent_node.clone())
            .or_else(|| {
                removed_record.parent_lineage.and_then(|parent| {
                    previous.get(&parent).and_then(|entry| {
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
        let Some(node) = rehydrate_published_node::<V>(&node_identity) else {
            continue;
        };
        retractions.push(crate::reactive::kind::TreeKey::Payload(node.clone()));
        retractions.push(crate::reactive::kind::TreeKey::Parent(node.clone()));
        retractions.push(crate::reactive::kind::TreeKey::ChildOrder(node.clone()));
        if let Some(order) = previous.get(&removed_record.lineage.0) {
            for (_, child_identity) in order.children.iter() {
                let Some(child) = rehydrate_published_node::<V>(child_identity) else {
                    continue;
                };
                retractions.push(crate::reactive::kind::TreeKey::ChildLink(
                    node.clone(),
                    child,
                ));
            }
        }
    }
    Ok(())
}

fn parser_document_tree<R, A>(machine: Arc<Mutex<ParserMachine<R, A>>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: crate::framework::parse::AbstractTreeFamily + 'static,
    A::View: TreeView<Key = String>
        + ViewKind<Observe = crate::reactive::kind::TreeObserve<A::View>, Patch = TreePatch<A::View>>,
{
    parser_document_with(machine, uri, |publication: &DeltaPublication<'_, A>| {
        let patch = emit_patch::<A::View>()?;
        let Some(arenas) = publication.arenas else {
            emit_view::<TreeParseUnits<A>>()?.remove(publication.uri.to_string())?;
            emit_view::<ParserTreeStatuses>()?.remove(publication.uri.to_string())?;
            close_parser_dimensions::<A>(&publication.uri)?;
            return Ok(None);
        };
        let ast = arenas.ast.as_ref();
        let uri = publication.uri.as_str();
        let current_root = publication.current_root_record;
        let resolver = move |record: u64| -> Option<u64> { publication.lineage_of(record) };
        publish_parser_dimensions(publication, ast, &resolver)?;

        // Removed nodes are retracted solely from the publication-time order
        // map. No dead arena record is consulted after lineage settlement.
        let mut retractions: Vec<<A::View as crate::reactive::View>::Input> = Vec::new();
        let mut seen_retractions: std::collections::HashSet<
            <A::View as crate::reactive::View>::Input,
        > = std::collections::HashSet::new();
        stage_published_removed_nodes::<A::View>(
            &publication.previous_child_orders,
            publication.delta.removed_records.as_ref(),
            publication
                .previous_root_record
                .filter(|_| publication.current_root_record.is_some()),
            &mut retractions,
        )?;
        if std::env::var_os("PLINGO_TRACE_REMOVALS").is_some() {
            eprintln!("removed-stage keys={retractions:?}");
        }
        retractions.retain(|key| seen_retractions.insert(key.clone()));
        for key in retractions {
            patch.remove(key)?;
        }

        // New records carry complete local facts in the authoritative delta.
        // Retained records are refreshed below and changed child order is
        // applied only by the bounded splice loop.
        for &(_, record) in publication.delta.inserted_records.iter() {
            let is_root = current_root == Some(record);
            A::__tree_plain_emit_record(uri, ast, record, is_root, &resolver)?;
        }

        // Retained updates change payload and parent dimensions only. Their
        // child order is changed, if at all, by the bounded splice loop.
        for &(_, record) in publication.delta.updated_records.iter() {
            let is_root = current_root == Some(record);
            A::__tree_refresh_payload(uri, ast, record, is_root, &resolver)?;
            let Some(id) = A::__tree_plain_node_for_record(uri, ast, record, is_root, &resolver)
            else {
                continue;
            };
            let parent = if is_root {
                None
            } else {
                ast.parent_of(record as usize).and_then(|parent_record| {
                    A::__tree_plain_node_for_record(
                        uri,
                        ast,
                        parent_record as u64,
                        false,
                        &resolver,
                    )
                })
            };
            patch.upsert(
                crate::reactive::kind::TreeKey::Parent(id),
                crate::reactive::kind::TreeFact::Parent(parent),
            )?;
        }

        // Replace each changed retained-parent order from complete published
        // syntax identities. The patch operation retracts dropped links and
        // writes only inserted links plus the new order fact.
        for splice in publication.delta.child_splices.iter() {
            let Some(previous) = publication.previous_child_orders.get(&splice.parent.0) else {
                continue;
            };
            let Some(parent) =
                rehydrate_published_node::<A::View>(&previous.parent_node)
            else {
                continue;
            };
            let before = splice.delta.before.and_then(|lineage| {
                previous
                    .children
                    .iter()
                    .find(|(child, _)| *child == lineage.0)
                    .and_then(|(_, identity)| {
                        rehydrate_published_node::<A::View>(identity)
                    })
            });
            let after = splice.delta.after.and_then(|lineage| {
                previous
                    .children
                    .iter()
                    .find(|(child, _)| *child == lineage.0)
                    .and_then(|(_, identity)| {
                        rehydrate_published_node::<A::View>(identity)
                    })
            });
            let mut removed_nodes = Vec::with_capacity(splice.delta.removed.len());
            let mut complete = true;
            for lineage in splice.delta.removed.iter() {
                let Some(identity) = previous
                    .children
                    .iter()
                    .find(|(child, _)| *child == lineage.0)
                    .map(|(_, identity)| identity)
                else {
                    complete = false;
                    break;
                };
                let Some(node) = rehydrate_published_node::<A::View>(identity) else {
                    complete = false;
                    break;
                };
                removed_nodes.push(node);
            }
            if !complete {
                continue;
            }
            let mut inserted_nodes = Vec::with_capacity(splice.inserted_children.len());
            for &(lineage, record) in splice.inserted_children.iter() {
                let Some(node) = A::__tree_plain_node_for_record(uri, ast, record, false, &resolver)
                else {
                    complete = false;
                    break;
                };
                debug_assert_eq!(lineage, splice.delta.inserted[inserted_nodes.len()]);
                inserted_nodes.push(node);
            }
            if !complete || inserted_nodes.len() != splice.delta.inserted.len() {
                continue;
            }
            patch.splice_children(
                parent.clone(),
                before,
                &removed_nodes,
                &inserted_nodes,
                after,
            )?;
            // A retained node can move from a rebuilt parent back into its
            // original parent without changing its stable identity. The
            // child splice updates links and order, but parent is an
            // independent tree fact and must follow that move explicitly.
            for (&(_, record), child) in splice.inserted_children.iter().zip(inserted_nodes) {
                let already_emitted = publication
                    .delta
                    .inserted_records
                    .iter()
                    .any(|&(_, emitted)| emitted == record)
                    || publication
                        .delta
                        .updated_records
                        .iter()
                        .any(|&(_, emitted)| emitted == record);
                if !already_emitted {
                    patch.upsert(
                        crate::reactive::kind::TreeKey::Parent(child),
                        crate::reactive::kind::TreeFact::Parent(Some(parent.clone())),
                    )?;
                }
            }
        }

        let root_changed = !publication.delta.roots.inserted.is_empty()
            || !publication.delta.roots.removed.is_empty();
        if current_root.is_none() {
            if let Some(previous_root_lineage) =
                publication.delta.roots.removed.first().map(|root| root.0)
            {
                if let Some(previous_root) =
                    publication.previous_child_orders.get(&previous_root_lineage)
                {
                    if let Some(previous_root_node) =
                        rehydrate_published_node::<A::View>(&previous_root.parent_node)
                    {
                        patch.remove(crate::reactive::kind::TreeKey::RootLink(
                            uri.to_string(),
                            previous_root_node,
                        ))?;
                    }
            }
        }
        }
        let root_id = current_root
            .and_then(|record| A::__tree_plain_node_for_record(uri, ast, record, true, &resolver));
        if root_changed {
            A::__tree_plain_emit_roots(uri, root_id.clone().into_iter().collect())?;
        }
        match root_id {
            Some(root_id) => {
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
        emit_view::<ParserTreeStatuses>()?
            .insert(publication.uri.clone(), publication.status.clone())?;


        // Diagnostics/status exact keys ride in delta.diagnostics and
        // delta.status for oracle proofs; their slots publish above via
        // ParseDiagnostics/ParseUnits replacement.
        let next_orders = build_published_child_orders(publication, ast, &resolver);
        publish_parser_orders(publication, &next_orders)?;
        Ok(Some(next_orders))
    })
}

fn parser_document<R, A>(machine: Arc<Mutex<ParserMachine<R, A>>>, uri: String) -> Result<()>
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
fn parser_layout_snapshot<R, A>(machine: Arc<Mutex<ParserMachine<R, A>>>, uri: String) -> Result<()>
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
            .map(|session| session.column_count())
            .unwrap_or_default();
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
    let machine = Arc::new(Mutex::new(ParserMachine {
        parser,
        _ast: PhantomData,
    }));
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
        + crate::framework::parse::AbstractTreeFamily
        + 'static,
    A::View: TreeView<Key = String>
        + ViewKind<Observe = crate::reactive::kind::TreeObserve<A::View>, Patch = TreePatch<A::View>>,
{
    use crate::framework::lex::{LexedDocuments, TokenLayoutDocuments};
    let mut parser = crate::framework::parse::grammar::Grammar::from_spec::<A>().build_lr1::<R>();
    parser.tree_member_kind = Some(A::__tree_member_kind_of);
    parser.tree_child_records = Some(A::__tree_plain_child_records);
    let machine = Arc::new(Mutex::new(ParserMachine {
        parser,
        _ast: PhantomData,
    }));
    // Document-scoped first-class components (see install_parser).
    let semantic_machine = Arc::clone(&machine);
    engine.install_component_each_key::<ParserTreeSemanticDefinition<R, A>, LexedDocuments<R>, _>(
        move |uri| parser_document_tree::<R, A>(Arc::clone(&semantic_machine), uri),
    )?;
    let layout_machine = Arc::clone(&machine);
    engine.install_component_each_key::<ParserLayoutDefinition<R, A>, TokenLayoutDocuments<R>, _>(
        move |uri| parser_layout_snapshot::<R, A>(Arc::clone(&layout_machine), uri),
    )?;

    // Public parser views are projections of the exact private dimensions.
    // Their components read only the relevant payload, edge, root, or order
    // key plus the generated node-identity join.
    engine.install_component_each_key::<
        ParserPayloadProjectionDefinition<R, A>,
        ParserSyntaxPayloads<A>,
        _,
    >(move |key| project_parser_payload::<A>(key))?;
    engine.install_component_each_key::<
        ParserParentProjectionDefinition<R, A>,
        ParserSyntaxParents,
        _,
    >(move |key| project_parser_parent::<A>(key))?;

    engine.install_component_each_key::<
        ParserEdgeProjectionDefinition<R, A>,
        ParserSyntaxFieldEdges,
        _,
    >(move |key| project_parser_edge::<A>(key))?;
    engine.install_component_each_key::<
        ParserRootProjectionDefinition<R, A>,
        ParserSyntaxRoots,
        _,
    >(move |key| project_parser_root::<A>(key))?;
    engine.install_component_each_key::<
        ParserOrderProjectionDefinition<R, A>,
        ParserSyntaxOrders,
        _,
    >(move |key| project_parser_order::<A>(key))?;
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

#[doc(hidden)]
pub struct ParserPayloadProjectionDefinition<R, A>(PhantomData<fn() -> (R, A)>);
#[doc(hidden)]
pub struct ParserParentProjectionDefinition<R, A>(PhantomData<fn() -> (R, A)>);
#[doc(hidden)]
pub struct ParserEdgeProjectionDefinition<R, A>(PhantomData<fn() -> (R, A)>);
#[doc(hidden)]
pub struct ParserRootProjectionDefinition<R, A>(PhantomData<fn() -> (R, A)>);
#[doc(hidden)]
pub struct ParserOrderProjectionDefinition<R, A>(PhantomData<fn() -> (R, A)>);

impl<R, A> crate::reactive::component::ComponentDefinition
    for ParserPayloadProjectionDefinition<R, A>
{
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::tree_payload_projection"
    }
}

impl<R, A> crate::reactive::component::ComponentDefinition
    for ParserParentProjectionDefinition<R, A>
{
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::tree_parent_projection"
    }
}

impl<R, A> crate::reactive::component::ComponentDefinition
    for ParserEdgeProjectionDefinition<R, A>
{
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::tree_edge_projection"
    }
}

impl<R, A> crate::reactive::component::ComponentDefinition
    for ParserRootProjectionDefinition<R, A>
{
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::tree_root_projection"
    }
}

impl<R, A> crate::reactive::component::ComponentDefinition
    for ParserOrderProjectionDefinition<R, A>
{
    fn __descriptor() -> &'static str {
        "plingo::framework::parse::tree_order_projection"
    }
}
