use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::Arc,
};

use fluent_uri::Uri;
use thiserror::Error;

use crate::{component::parse::data::product::ProductId, scheme::change::FlowUnit};

/// Opaque identity of a scope in a committed scope snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Scope(pub(crate) u64);

/// Cycle policy of one user-defined relationship.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScopeProperty {
    #[default]
    Cyclic,
    Acyclic,
}

/// A datum attached to a user-selected scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeDatum<Datum> {
    pub scope: Scope,
    pub datum: Datum,
}

/// A user-defined labelled relationship in the URI-free graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeEdge<Label> {
    pub source: Scope,
    pub label: Label,
    pub target: Scope,
    pub property: ScopeProperty,
}

/// A user-defined analysis fact attached to a scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeReference<Reference> {
    pub scope: Scope,
    pub reference: Reference,
}

/// Exact committed graph and effect changes produced by one parser revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopePatch<Label, Datum, Reference, Request> {
    pub added_scopes: Arc<[Scope]>,
    pub removed_scopes: Arc<[Scope]>,
    pub added_datums: Arc<[ScopeDatum<Datum>]>,
    pub removed_datums: Arc<[ScopeDatum<Datum>]>,
    pub added_edges: Arc<[ScopeEdge<Label>]>,
    pub removed_edges: Arc<[ScopeEdge<Label>]>,
    pub added_references: Arc<[ScopeReference<Reference>]>,
    pub removed_references: Arc<[ScopeReference<Reference>]>,
    /// Effects whose first live owner was committed by this revision.
    pub required_sources: Arc<[Request]>,
    /// Effects whose final live owner was removed by this revision.
    pub released_sources: Arc<[Request]>,
    pub rebuilt_frames: usize,
    pub removed_frames: usize,
}

impl<Label, Datum, Reference, Request> Default for ScopePatch<Label, Datum, Reference, Request> {
    fn default() -> Self {
        Self {
            added_scopes: Arc::from([]),
            removed_scopes: Arc::from([]),
            added_datums: Arc::from([]),
            removed_datums: Arc::from([]),
            added_edges: Arc::from([]),
            removed_edges: Arc::from([]),
            added_references: Arc::from([]),
            removed_references: Arc::from([]),
            required_sources: Arc::from([]),
            released_sources: Arc::from([]),
            rebuilt_frames: 0,
            removed_frames: 0,
        }
    }
}

impl<Label, Datum, Reference, Request> ScopePatch<Label, Datum, Reference, Request> {
    pub fn is_empty(&self) -> bool {
        self.added_scopes.is_empty()
            && self.removed_scopes.is_empty()
            && self.added_datums.is_empty()
            && self.removed_datums.is_empty()
            && self.added_edges.is_empty()
            && self.removed_edges.is_empty()
            && self.added_references.is_empty()
            && self.removed_references.is_empty()
            && self.required_sources.is_empty()
            && self.released_sources.is_empty()
            && self.rebuilt_frames == 0
            && self.removed_frames == 0
    }
}

impl<Label, Datum, Reference, Request> FlowUnit for ScopePatch<Label, Datum, Reference, Request>
where
    Label: Clone + Send + Sync + 'static,
    Datum: Clone + Send + Sync + 'static,
    Reference: Clone + Send + Sync + 'static,
    Request: Clone + Send + Sync + 'static,
{
    fn extent(&self) -> usize {
        1
    }
}

#[derive(Debug, Error)]
pub enum ScopeError {
    #[error("an acyclic relationship from {from:?} to {to:?} closes a cycle")]
    Cycle { from: Scope, to: Scope },
    #[error("parser product {0} has no typed AST value")]
    MissingAstProduct(ProductId),
    #[error("typed AST value for parser product {0} is unavailable")]
    MissingAstValue(ProductId),
    #[error("unknown scope {0:?}")]
    MissingScope(Scope),
    #[error("scope {0:?} has multiple datums")]
    DuplicateDatum(Scope),
    #[error("scope rule failed: {0}")]
    Rule(String),
}

/// Internal, URI-qualified parser identity. It never appears in graph facts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AstOwner {
    pub uri: Uri<&'static str>,
    pub product: ProductId,
}

/// Contextual analysis identity. A graph scope is owned by [`AstOwner`], while
/// a frame tracks facts emitted when that AST is visited from `incoming`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FrameKey {
    pub owner: AstOwner,
    pub incoming: Scope,
}

#[derive(Clone)]
pub(crate) struct FrameDraft<Label, Datum, Reference, Request> {
    pub children: HashSet<FrameKey>,
    pub edges: Vec<ScopeEdge<Label>>,
    pub datums: Vec<ScopeDatum<Datum>>,
    pub references: Vec<ScopeReference<Reference>>,
    pub requests: Vec<Request>,
}

impl<Label, Datum, Reference, Request> Default for FrameDraft<Label, Datum, Reference, Request> {
    fn default() -> Self {
        Self {
            children: HashSet::new(),
            edges: Vec::new(),
            datums: Vec::new(),
            references: Vec::new(),
            requests: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct FrameRecord<Label, Datum, Reference, Request> {
    pub children: HashSet<FrameKey>,
    pub edges: Vec<ScopeEdge<Label>>,
    pub datums: Vec<ScopeDatum<Datum>>,
    pub references: Vec<(u64, ScopeReference<Reference>)>,
    pub requests: Vec<Request>,
}

#[derive(Clone)]
pub(crate) struct GraphState<Label, Datum> {
    pub scopes: HashSet<Scope>,
    /// Datum plus exact owner count for shared scopes.
    pub datums: HashMap<Scope, (Datum, usize)>,
    pub edges: HashMap<ScopeEdge<Label>, usize>,
}

impl<Label, Datum> Default for GraphState<Label, Datum> {
    fn default() -> Self {
        Self {
            scopes: HashSet::new(),
            datums: HashMap::new(),
            edges: HashMap::new(),
        }
    }
}

/// Opaque snapshot state. It is public only because the layer macro exposes it
/// as an associated state type; all mutation remains inside the scope module.
#[doc(hidden)]
#[derive(Clone)]
pub struct ScopeSnapshot<Anchor, Label, Datum, Reference, Request> {
    pub(crate) next_scope: u64,
    pub(crate) next_fact: u64,
    pub(crate) graph: GraphState<Label, Datum>,
    pub(crate) roots: HashMap<Uri<&'static str>, HashSet<FrameKey>>,
    pub(crate) root_scopes: HashMap<Uri<&'static str>, Scope>,
    /// One graph scope per parser-owned AST identity; no positional slots.
    pub(crate) ast_scopes: HashMap<AstOwner, Scope>,
    /// Stable application-owned scopes for values with no AST identity.
    pub(crate) external_scopes: HashMap<Anchor, Scope>,
    pub(crate) frames: HashMap<FrameKey, FrameRecord<Label, Datum, Reference, Request>>,
    pub(crate) parents: HashMap<FrameKey, HashSet<FrameKey>>,
    pub(crate) references: HashMap<u64, ScopeReference<Reference>>,
    pub(crate) request_counts: HashMap<Request, usize>,
}

impl<Anchor, Label, Datum, Reference, Request> Default
    for ScopeSnapshot<Anchor, Label, Datum, Reference, Request>
{
    fn default() -> Self {
        Self {
            next_scope: 0,
            next_fact: 0,
            graph: GraphState::default(),
            roots: HashMap::new(),
            root_scopes: HashMap::new(),
            ast_scopes: HashMap::new(),
            external_scopes: HashMap::new(),
            frames: HashMap::new(),
            parents: HashMap::new(),
            references: HashMap::new(),
            request_counts: HashMap::new(),
        }
    }
}

pub(crate) struct PatchBuilder<Label, Datum, Reference, Request>
where
    Label: Eq + Hash,
    Datum: Eq + Hash,
    Reference: Eq + Hash,
    Request: Eq + Hash,
{
    pub added_scopes: HashSet<Scope>,
    pub removed_scopes: HashSet<Scope>,
    pub added_datums: HashSet<ScopeDatum<Datum>>,
    pub removed_datums: HashSet<ScopeDatum<Datum>>,
    pub added_edges: HashSet<ScopeEdge<Label>>,
    pub removed_edges: HashSet<ScopeEdge<Label>>,
    pub added_references: HashSet<ScopeReference<Reference>>,
    pub removed_references: HashSet<ScopeReference<Reference>>,
    pub required_sources: HashSet<Request>,
    pub released_sources: HashSet<Request>,
    pub rebuilt_frames: usize,
    pub removed_frames: usize,
}

impl<Label, Datum, Reference, Request> Default for PatchBuilder<Label, Datum, Reference, Request>
where
    Label: Eq + Hash,
    Datum: Eq + Hash,
    Reference: Eq + Hash,
    Request: Eq + Hash,
{
    fn default() -> Self {
        Self {
            added_scopes: HashSet::new(),
            removed_scopes: HashSet::new(),
            added_datums: HashSet::new(),
            removed_datums: HashSet::new(),
            added_edges: HashSet::new(),
            removed_edges: HashSet::new(),
            added_references: HashSet::new(),
            removed_references: HashSet::new(),
            required_sources: HashSet::new(),
            released_sources: HashSet::new(),
            rebuilt_frames: 0,
            removed_frames: 0,
        }
    }
}

impl<Label, Datum, Reference, Request> PatchBuilder<Label, Datum, Reference, Request>
where
    Label: Clone + Eq + Hash,
    Datum: Clone + Eq + Hash,
    Reference: Clone + Eq + Hash,
    Request: Clone + Eq + Hash,
{
    pub fn add_scope(&mut self, scope: Scope) {
        if !self.removed_scopes.remove(&scope) {
            self.added_scopes.insert(scope);
        }
    }

    pub fn remove_scope(&mut self, scope: Scope) {
        if !self.added_scopes.remove(&scope) {
            self.removed_scopes.insert(scope);
        }
    }

    pub fn add_datum(&mut self, datum: ScopeDatum<Datum>) {
        if !self.removed_datums.remove(&datum) {
            self.added_datums.insert(datum);
        }
    }

    pub fn remove_datum(&mut self, datum: ScopeDatum<Datum>) {
        if !self.added_datums.remove(&datum) {
            self.removed_datums.insert(datum);
        }
    }

    pub fn add_edge(&mut self, edge: ScopeEdge<Label>) {
        if !self.removed_edges.remove(&edge) {
            self.added_edges.insert(edge);
        }
    }

    pub fn remove_edge(&mut self, edge: ScopeEdge<Label>) {
        if !self.added_edges.remove(&edge) {
            self.removed_edges.insert(edge);
        }
    }

    pub fn add_reference(&mut self, reference: ScopeReference<Reference>) {
        if !self.removed_references.remove(&reference) {
            self.added_references.insert(reference);
        }
    }

    pub fn remove_reference(&mut self, reference: ScopeReference<Reference>) {
        if !self.added_references.remove(&reference) {
            self.removed_references.insert(reference);
        }
    }

    pub fn require_source(&mut self, request: Request) {
        if !self.released_sources.remove(&request) {
            self.required_sources.insert(request);
        }
    }

    pub fn release_source(&mut self, request: Request) {
        if !self.required_sources.remove(&request) {
            self.released_sources.insert(request);
        }
    }

    pub fn finish(self) -> ScopePatch<Label, Datum, Reference, Request> {
        ScopePatch {
            added_scopes: self.added_scopes.into_iter().collect::<Vec<_>>().into(),
            removed_scopes: self.removed_scopes.into_iter().collect::<Vec<_>>().into(),
            added_datums: self.added_datums.into_iter().collect::<Vec<_>>().into(),
            removed_datums: self.removed_datums.into_iter().collect::<Vec<_>>().into(),
            added_edges: self.added_edges.into_iter().collect::<Vec<_>>().into(),
            removed_edges: self.removed_edges.into_iter().collect::<Vec<_>>().into(),
            added_references: self.added_references.into_iter().collect::<Vec<_>>().into(),
            removed_references: self
                .removed_references
                .into_iter()
                .collect::<Vec<_>>()
                .into(),
            required_sources: self.required_sources.into_iter().collect::<Vec<_>>().into(),
            released_sources: self.released_sources.into_iter().collect::<Vec<_>>().into(),
            rebuilt_frames: self.rebuilt_frames,
            removed_frames: self.removed_frames,
        }
    }
}
