use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;
use plingo_macros::layer;
use thiserror::Error;

use crate::{
    component::{
        lex::LexerRoot,
        parse::{AstView, ParseAddress, ParseUnit, Parser, data::AstBox},
    },
    context_callable,
    scheme::{
        call::CallOutcome,
        change::{AddressChange, ChangeSet, LayerChanges, Revision, Splice},
        context::{Context, SnapshotId},
        error::ActionError,
        layer::{MiddleLayer, NonTopLayer, SnapshotLayer},
    },
};

use super::{
    ScopeProperty,
    data::{
        AstOwner, FrameDraft, FrameKey, PatchBuilder, Scope, ScopeDatum, ScopeEdge, ScopeError,
        ScopePatch, ScopeReference, ScopeSnapshot,
    },
    query::{QueryConfirmation, RecordedQuery, ResolutionPath, ScopeQuery},
};

#[derive(Debug, Error)]
pub enum ScopeLayerError {
    #[error(transparent)]
    Rule(#[from] ScopeError),
    #[error("could not read parser snapshot: {0}")]
    ParserRead(ActionError),
    #[error("scope snapshot {0} is unavailable")]
    MissingSnapshot(SnapshotId),
}

trait ScopeVisitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>:
    Send + Sync
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn visit(
        &self,
        cx: &mut ScopeCx<'_, Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>,
        node: AstBox<Ast>,
        incoming: Scope,
    ) -> Result<(), ScopeError>;
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower, F>
    ScopeVisitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower> for F
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
    F: for<'a> Fn(
            &mut ScopeCx<'a, Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>,
            AstBox<Ast>,
            Scope,
        ) -> Result<(), ScopeError>
        + Send
        + Sync,
{
    fn visit(
        &self,
        cx: &mut ScopeCx<'_, Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>,
        node: AstBox<Ast>,
        incoming: Scope,
    ) -> Result<(), ScopeError> {
        self(cx, node, incoming)
    }
}

type State<Anchor, Label, Datum, Reference, Request> =
    ScopeSnapshot<Anchor, Label, Datum, Reference, Request>;
type Patch<Label, Datum, Reference, Request> =
    ScopePatch<Label, Datum, Reference, Request>;
type PatchDraft<Label, Datum, Reference, Request> =
    PatchBuilder<Label, Datum, Reference, Request>;
type Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower> =
    dyn ScopeVisitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>;
type SourceLoader<Request> =
    dyn Fn(&Context, &Request) -> Result<(), ActionError> + Send + Sync;

/// Parser-delta-driven, closure-configured incremental scope construction.
///
/// It is a middle layer. The application provides its traversal rule and all
/// graph semantics; this layer provides only AST-owned scopes, external
/// anchors, exact fact ownership, graph patches, and parser-delta updates.
#[layer]
pub struct ScopeLayer<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower = ()>
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    visitor: Arc<Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>>,
    #[snapshot]
    latest: Arc<State<Anchor, Label, Datum, Reference, Request>>,
    source_loader: Option<Arc<SourceLoader<Request>>>,
    pending_source_requests: HashMap<SnapshotId, Arc<[Request]>>,
    _lower: PhantomData<fn() -> Lower>,
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>
    ScopeLayer<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    pub fn new<F>(visitor: F) -> Self
    where
        F: for<'a> Fn(
                &mut ScopeCx<'a, Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>,
                AstBox<Ast>,
                Scope,
            ) -> Result<(), ScopeError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            visitor: Arc::new(visitor),
            latest: Arc::new(State::default()),
            source_loader: None,
            pending_source_requests: HashMap::new(),
            _lower: PhantomData,
            _snapshot: Default::default(),
        }
    }

    /// Installs the application-owned mapping from a generic request to a
    /// deferred `Source::load` action. It runs only after the transaction has
    /// committed through every lower layer.
    pub fn on_source_request<F>(mut self, loader: F) -> Self
    where
        F: Fn(&Context, &Request) -> Result<(), ActionError> + Send + Sync + 'static,
    {
        self.source_loader = Some(Arc::new(loader));
        self
    }

    fn ast_owner(view: &AstView<Ast>, node: AstBox<Ast>) -> Result<AstOwner, ScopeError> {
        let product = view
            .owner(node)
            .ok_or(ScopeError::MissingAstProduct(usize::MAX))?;
        Ok(AstOwner {
            uri: node.uri,
            product,
        })
    }

    fn evaluate(
        state: &mut State<Anchor, Label, Datum, Reference, Request>,
        patch: &mut PatchDraft<Label, Datum, Reference, Request>,
        view: &AstView<Ast>,
        visitor: Arc<Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>>,
        node: AstBox<Ast>,
        incoming: Scope,
        force: bool,
    ) -> Result<FrameKey, ScopeError> {
        let key = FrameKey {
            owner: Self::ast_owner(view, node)?,
            incoming,
        };
        if !force && state.frames.contains_key(&key) {
            return Ok(key);
        }

        let mut draft = FrameDraft::default();
        {
            let mut cx = ScopeCx {
                state,
                patch,
                view,
                visitor: Arc::clone(&visitor),
                key: key.clone(),
                draft: &mut draft,
            };
            visitor.visit(&mut cx, node, incoming)?;
        }
        state.replace_frame(key.clone(), draft, patch)?;
        Ok(key)
    }

    fn refresh_view(
        state: &mut State<Anchor, Label, Datum, Reference, Request>,
        patch: &mut PatchDraft<Label, Datum, Reference, Request>,
        view: &AstView<Ast>,
        visitor: Arc<Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>>,
    ) -> Result<(), ScopeError> {
        let root_scope = state.root_scope(view.uri(), patch);
        let mut roots = HashSet::new();
        for root in view.roots().iter().copied() {
            roots.insert(Self::evaluate(
                state,
                patch,
                view,
                Arc::clone(&visitor),
                root,
                root_scope,
                true,
            )?);
        }
        state.replace_roots(view.uri(), roots, patch);
        Ok(())
    }

    #[context_callable]
    pub async fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        query: &'a ScopeQuery<Label, Datum>,
    ) -> CallOutcome<Self, Vec<ResolutionPath<Label, Datum>>>
    where
        Lower: NonTopLayer<Address = (), Unit = Patch<Label, Datum, Reference, Request>>
            + Send
            + Sync
            + 'static,
    {
        let snapshot = match self.state(ctx.snapshot()) {
            Some(snapshot) => snapshot,
            None => {
                return CallOutcome::fail(ScopeLayerError::MissingSnapshot(
                    ctx.snapshot().unwrap_or_default(),
                ));
            }
        };
        CallOutcome::ok(snapshot.resolve_query(query))
    }

    #[context_callable]
    pub async fn confirm<'a>(
        &'a mut self,
        ctx: &'a Context,
        recorded: &'a RecordedQuery<Label, Datum>,
    ) -> CallOutcome<Self, QueryConfirmation<Label, Datum>>
    where
        Lower: NonTopLayer<Address = (), Unit = Patch<Label, Datum, Reference, Request>>
            + Send
            + Sync
            + 'static,
    {
        let snapshot = match self.state(ctx.snapshot()) {
            Some(snapshot) => snapshot,
            None => {
                return CallOutcome::fail(ScopeLayerError::MissingSnapshot(
                    ctx.snapshot().unwrap_or_default(),
                ));
            }
        };
        CallOutcome::ok(snapshot.confirm_query(recorded))
    }
}

/// Generic rule-local mutation surface.
///
/// The layer supplies no declaration, type, lexical, import, or other
/// language relation. Rules create ordinary datum scopes and ordinary labelled
/// edges, then choose which scopes are passed to recursive visits.
pub struct ScopeCx<'a, Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    state: &'a mut State<Anchor, Label, Datum, Reference, Request>,
    patch: &'a mut PatchDraft<Label, Datum, Reference, Request>,
    view: &'a AstView<Ast>,
    visitor: Arc<Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>>,
    key: FrameKey,
    draft: &'a mut FrameDraft<Label, Datum, Reference, Request>,
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>
    ScopeCx<'_, Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn owner_of(&self, node: AstBox<Ast>) -> Result<AstOwner, ScopeError> {
        ScopeLayer::<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>::ast_owner(
            self.view, node,
        )
    }

    pub fn ast(&self, node: AstBox<Ast>) -> Result<&Ast, ScopeError> {
        self.view.get(node).ok_or_else(|| {
            ScopeError::MissingAstValue(self.view.owner(node).unwrap_or(usize::MAX))
        })
    }

    /// The graph scope owned by the AST node currently being evaluated.
    pub fn scope(&mut self) -> Scope {
        self.state.ast_scope(&self.key.owner, self.patch)
    }

    /// The graph scope owned by another AST node in this parser snapshot.
    pub fn scope_of(&mut self, node: AstBox<Ast>) -> Result<Scope, ScopeError> {
        let owner = self.owner_of(node)?;
        Ok(self.state.ast_scope(&owner, self.patch))
    }

    /// A stable application-owned scope with no AST owner. Its meaning is
    /// entirely defined by the application-provided `Anchor` value.
    pub fn external_scope(&mut self, anchor: Anchor) -> Scope {
        self.state.external_scope(anchor, self.patch)
    }

    pub fn edge(
        &mut self,
        source: Scope,
        label: Label,
        target: Scope,
        property: ScopeProperty,
    ) {
        self.draft.edges.push(ScopeEdge {
            source,
            label,
            target,
            property,
        });
    }

    pub fn datum(&mut self, scope: Scope, datum: Datum) {
        self.draft.datums.push(ScopeDatum { scope, datum });
    }

    pub fn reference(&mut self, scope: Scope, reference: Reference) {
        self.draft
            .references
            .push(ScopeReference { scope, reference });
    }

    /// Recurses through an application-chosen incoming graph scope and records
    /// the child-frame dependency automatically.
    pub fn visit(&mut self, node: AstBox<Ast>, incoming: Scope) -> Result<(), ScopeError> {
        let child = ScopeLayer::<
            Root,
            Ast,
            Anchor,
            Label,
            Datum,
            Reference,
            Request,
            Lower,
        >::evaluate(
            self.state,
            self.patch,
            self.view,
            Arc::clone(&self.visitor),
            node,
            incoming,
            false,
        )?;
        self.draft.children.insert(child);
        Ok(())
    }

    /// Stages an application-defined effect for post-commit delivery.
    pub fn require_source(&mut self, request: Request) {
        if !self.draft.requests.contains(&request) {
            self.draft.requests.push(request);
        }
    }

    pub fn fail(&self, reason: impl Into<String>) -> ScopeError {
        ScopeError::Rule(reason.into())
    }
}

#[layer(middle)]
impl<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower> MiddleLayer
    for ScopeLayer<Root, Ast, Anchor, Label, Datum, Reference, Request, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
    Lower: NonTopLayer<Address = (), Unit = Patch<Label, Datum, Reference, Request>>
        + Send
        + Sync
        + 'static,
{
    type Lower = Lower;
    type Error = ScopeLayerError;
    type Address = ParseAddress;
    type Unit = ParseUnit;

    async fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> Result<LayerChanges<Self::Lower>, Self::Error> {
        let revision = changes.revision;
        let target = ctx.with_snapshot(Some(revision.target));
        let changed_uris = changes
            .changes
            .iter()
            .map(|change| change.address.uri)
            .collect::<HashSet<Uri<&'static str>>>();
        if changed_uris.is_empty() {
            self.push_state(revision.target);
            return Ok(ChangeSet::empty(revision));
        }

        let mut working = (*self.latest).clone();
        let mut patch = PatchDraft::default();
        for uri in changed_uris {
            let view = target
                .read(Parser::<Root, Self>::get_ast_view::<Ast>, uri)
                .await
                .map_err(ScopeLayerError::ParserRead)?;
            Self::refresh_view(
                &mut working,
                &mut patch,
                &view,
                Arc::clone(&self.visitor),
            )?;
        }

        let patch = patch.finish();
        if self.source_loader.is_some() && !patch.required_sources.is_empty() {
            self.pending_source_requests
                .insert(revision.target, Arc::clone(&patch.required_sources));
        }
        self.latest = Arc::new(working);
        self.push_state(revision.target);
        if patch.is_empty() {
            return Ok(ChangeSet::empty(revision));
        }
        Ok(ChangeSet {
            revision,
            changes: vec![AddressChange {
                address: (),
                old_extent: 0,
                new_extent: 1,
                splices: vec![Splice {
                    old_range: 0..0,
                    new_range: 0..1,
                    removed: Arc::from([]),
                    inserted: Arc::from([patch]),
                }],
            }],
        })
    }

    fn commit_transaction(&mut self, ctx: &Context, revision: Revision) {
        let Some(loader) = self.source_loader.as_ref() else {
            return;
        };
        let Some(requests) = self.pending_source_requests.remove(&revision.target) else {
            return;
        };
        for request in requests.iter() {
            if let Err(error) = loader(ctx, request) {
                eprintln!("could not queue committed scope source request: {error}");
            }
        }
    }

    fn rollback_transaction(&mut self, revision: Revision) -> bool {
        self.pending_source_requests.remove(&revision.target);
        self.rollback_state(revision)
    }
}
