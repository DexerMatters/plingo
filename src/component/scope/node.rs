//! Scope-graph derivations for the node runtime.
//!
//! Scope work is split into root ownership tasks and independently demandable
//! AST frames.  Frames own only the facts they emit; the graph runtime reclaims
//! them when their root or parent task no longer requires them.

use std::{collections::HashMap, hash::Hash, marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::LexerRoot,
        parse::{AstArtifact, AstKey, ParseRoots, ParsedAst, ParserNode, data::AstBox},
        source::LoadSourceText,
    },
    scheme::node::{
        ComponentState, DeriveCx, Graph, IndexedRelation, Node, NodeError, NodeKey, ReclaimCx,
        Relation, View,
    },
};

use super::{
    ScopeProperty,
    data::{
        AstOwner, FrameDraft, PatchBuilder, Scope, ScopeDatum, ScopeEdge, ScopeError,
        ScopeFrameKey, ScopeReference, ScopeSnapshot,
    },
    query::ResolutionPath,
};

type State<Anchor, Label, Datum, Reference, Request> =
    ScopeSnapshot<Anchor, Label, Datum, Reference, Request>;
type PatchDraft<Label, Datum, Reference, Request> = PatchBuilder<Label, Datum, Reference, Request>;

type Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request> =
    dyn ScopeVisitor<Root, Ast, Anchor, Label, Datum, Reference, Request>;

trait ScopeVisitor<Root, Ast, Anchor, Label, Datum, Reference, Request>: Send + Sync
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn visit<'scope, 'transaction, 'nodes>(
        &self,
        cx: &mut ScopeCx<
            'scope,
            'transaction,
            'nodes,
            Root,
            Ast,
            Anchor,
            Label,
            Datum,
            Reference,
            Request,
        >,
        node: AstBox<Ast>,
        incoming: Scope,
    ) -> Result<(), ScopeError>;
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request, F>
    ScopeVisitor<Root, Ast, Anchor, Label, Datum, Reference, Request> for F
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
    F: for<'scope, 'transaction, 'nodes> Fn(
            &mut ScopeCx<
                'scope,
                'transaction,
                'nodes,
                Root,
                Ast,
                Anchor,
                Label,
                Datum,
                Reference,
                Request,
            >,
            AstBox<Ast>,
            Scope,
        ) -> Result<(), ScopeError>
        + Send
        + Sync,
{
    fn visit<'scope, 'transaction, 'nodes>(
        &self,
        cx: &mut ScopeCx<
            'scope,
            'transaction,
            'nodes,
            Root,
            Ast,
            Anchor,
            Label,
            Datum,
            Reference,
            Request,
        >,
        node: AstBox<Ast>,
        incoming: Scope,
    ) -> Result<(), ScopeError> {
        self(cx, node, incoming)
    }
}

/// Key of one scope-task invocation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeKey {
    Root(Uri<&'static str>),
    Frame(ScopeFrameKey),
}

impl ScopeKey {
    pub const fn root(uri: Uri<&'static str>) -> Self {
        Self::Root(uri)
    }

    pub const fn frame(frame: ScopeFrameKey) -> Self {
        Self::Frame(frame)
    }

    pub const fn frame_for(ast: AstKey, incoming: Scope) -> Self {
        Self::Frame(ScopeFrameKey::new(ast, incoming))
    }
}

/// Backwards-neutral spelling for callers that want to emphasize task keys.
pub type ScopeTaskKey = ScopeKey;

/// Unit completion marker for one root or frame scope task.
pub struct ScopeStamp<Root, Ast, Anchor, Label, Datum, Reference, Request>(
    PhantomData<fn() -> (Root, Ast, Anchor, Label, Datum, Reference, Request)>,
);

impl<Root, Ast, Anchor, Label, Datum, Reference, Request> View
    for ScopeStamp<Root, Ast, Anchor, Label, Datum, Reference, Request>
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Key = ScopeKey;
    type Value = ();
}

/// Materialized scope identity for one root or contextual frame task.
///
/// Root keys resolve to the document root scope; frame keys resolve to the
/// AST-owned scope for that frame's AST artifact. This lets clients construct
/// resolution requests without discovering scopes indirectly through facts.
pub struct ScopeHandle<Root, Ast, Anchor, Label, Datum, Reference, Request>(
    PhantomData<fn() -> (Root, Ast, Anchor, Label, Datum, Reference, Request)>,
);

impl<Root, Ast, Anchor, Label, Datum, Reference, Request> View
    for ScopeHandle<Root, Ast, Anchor, Label, Datum, Reference, Request>
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Key = ScopeKey;
    type Value = Scope;
}

/// Public relation of URI-free graph edges.
pub struct ScopeEdges<Label>(PhantomData<fn() -> Label>);
impl<Label> Relation for ScopeEdges<Label>
where
    Label: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Fact = ScopeEdge<Label>;
}

impl<Label> IndexedRelation for ScopeEdges<Label>
where
    Label: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Index = Scope;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.source
    }
}

/// Public relation of URI-free graph datums.
pub struct ScopeDatums<Datum>(PhantomData<fn() -> Datum>);
impl<Datum> Relation for ScopeDatums<Datum>
where
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Fact = ScopeDatum<Datum>;
}

impl<Datum> IndexedRelation for ScopeDatums<Datum>
where
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Index = Scope;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.scope
    }
}

/// Public relation of URI-free graph references.
pub struct ScopeReferences<Reference>(PhantomData<fn() -> Reference>);
impl<Reference> Relation for ScopeReferences<Reference>
where
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Fact = ScopeReference<Reference>;
}

impl<Reference> IndexedRelation for ScopeReferences<Reference>
where
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Index = Scope;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.scope
    }
}

/// Post-commit source requirements emitted by individual scope frames.
pub struct SourceRequirements<Request>(PhantomData<fn() -> Request>);
impl<Request> Relation for SourceRequirements<Request>
where
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Fact = Request;
}

impl<Request> IndexedRelation for SourceRequirements<Request>
where
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Index = Request;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.clone()
    }
}

/// A cacheable application-defined datum selector for resolution.
pub trait DatumSelector<Datum>: NodeKey {
    fn accepts(&self, datum: &Datum) -> bool;
}

/// Stable key for one materialized scope resolution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResolutionKey<Label, Selector> {
    pub start: Scope,
    pub path: super::PathExpr<Label>,
    pub selector: Selector,
}

/// Resolution result view keyed by [`ResolutionKey`].
pub struct ScopeResolution<Label, Datum, Selector>(PhantomData<fn() -> (Label, Datum, Selector)>);

impl<Label, Datum, Selector> View for ScopeResolution<Label, Datum, Selector>
where
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Selector: DatumSelector<Datum>,
{
    type Key = ResolutionKey<Label, Selector>;
    /// Resolution answers are unordered witnesses; relation insertion order
    /// must not affect cache equality or subscription publication.
    type Value = std::collections::HashSet<ResolutionPath<Label, Datum>>;
}

/// Generic node that resolves a query from materialized edge and datum facts.
pub struct ResolutionNode<Label, Datum, Selector>(PhantomData<fn() -> (Label, Datum, Selector)>);

impl<Label, Datum, Selector> Default for ResolutionNode<Label, Datum, Selector> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<Label, Datum, Selector> Node for ResolutionNode<Label, Datum, Selector>
where
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Selector: DatumSelector<Datum>,
{
    type Key = ResolutionKey<Label, Selector>;
    type Output = ScopeResolution<Label, Datum, Selector>;

    fn derive(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        key: Self::Key,
    ) -> Result<std::collections::HashSet<ResolutionPath<Label, Datum>>, NodeError> {
        let selector = key.selector.clone();
        Ok(super::query::resolve_indexed(
            key.start,
            key.path,
            move |datum| selector.accepts(datum),
            |scope, needs_datums| {
                let edges = cx.relation_facts_at::<ScopeEdges<Label>>(scope);
                let datums = if needs_datums {
                    cx.relation_facts_at::<ScopeDatums<Datum>>(scope)
                } else {
                    Default::default()
                };
                (edges, datums)
            },
        ))
    }
}

/// Generic scope rule node with root and frame task keys.
pub struct ScopeNode<Root, Ast, Anchor, Label, Datum, Reference, Request>
where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    visitor: Arc<Visitor<Root, Ast, Anchor, Label, Datum, Reference, Request>>,
    state: ComponentState<State<Anchor, Label, Datum, Reference, Request>>,
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request>
    ScopeNode<Root, Ast, Anchor, Label, Datum, Reference, Request>
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
        F: for<'scope, 'transaction, 'nodes> Fn(
                &mut ScopeCx<
                    'scope,
                    'transaction,
                    'nodes,
                    Root,
                    Ast,
                    Anchor,
                    Label,
                    Datum,
                    Reference,
                    Request,
                >,
                AstBox<Ast>,
                Scope,
            ) -> Result<(), ScopeError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            visitor: Arc::new(visitor),
            state: ComponentState::new(State::default()),
        }
    }

    /// Installs an idempotent post-commit loader for URI requirements emitted
    /// by this scope node. The loader is invoked only on a requirement's first
    /// live support and its text is applied in a follow-up transaction.
    pub fn install_uri_source_loader(
        graph: &mut Graph,
        loader: impl Fn(Uri<&'static str>) -> Result<Arc<str>, String> + Send + Sync + 'static,
    ) where
        Request: std::convert::Into<Uri<&'static str>> + std::fmt::Debug,
    {
        graph.on_relation_added_command::<SourceRequirements<Request>, LoadSourceText>(
            move |_, request| {
                let uri = request.into();
                loader(uri).map(|text| LoadSourceText { uri, text })
            },
        );
    }

    fn root(&self, cx: &mut DeriveCx<'_, '_>, uri: Uri<&'static str>) -> Result<(), NodeError> {
        cx.require::<ParserNode<Root, Ast>>(uri)?;
        let roots = cx.observe::<ParseRoots<Root, Ast>>(uri)?;
        for ast in roots.iter().cloned() {
            cx.observe::<ParsedAst<Root, Ast>>(ast)?;
        }

        let (root_scope, frames) = {
            let state = cx.state_mut(&self.state)?;
            let mut staged = state.clone();
            let mut patch = PatchDraft::default();
            let root_scope = staged.root_scope(uri, &mut patch);
            let frames: std::collections::HashSet<ScopeFrameKey> = roots
                .iter()
                .cloned()
                .map(|ast| ScopeFrameKey::new(ast, root_scope))
                .collect();
            staged.replace_roots(uri, frames.clone(), &mut patch);
            *state = staged;
            (root_scope, frames)
        };
        cx.emit::<ScopeHandle<Root, Ast, Anchor, Label, Datum, Reference, Request>>(
            ScopeKey::root(uri),
            root_scope,
        )?;
        for frame in frames {
            cx.require::<Self>(ScopeKey::frame(frame))?;
        }
        Ok(())
    }

    fn frame(&self, cx: &mut DeriveCx<'_, '_>, key: ScopeFrameKey) -> Result<(), NodeError> {
        cx.require::<ParserNode<Root, Ast>>(key.ast.uri)?;
        let artifact = match cx.observe::<ParsedAst<Root, Ast>>(key.ast.clone()) {
            Ok(artifact) => artifact,
            // A parser revision can invalidate an orphaned frame before its
            // root task has reclaimed it. Forget private frame accounting now;
            // the empty task output retracts runtime-owned facts transactionally.
            Err(NodeError::MissingView(_)) => {
                let mut staged = cx.state_mut(&self.state)?.clone();
                let mut patch = PatchDraft::default();
                staged.forget_frame(&key, &mut patch);
                *cx.state_mut(&self.state)? = staged;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let owner = AstOwner {
            uri: artifact.ast_box.uri,
            product: artifact.product,
        };
        let node = artifact.ast_box;
        let mut staged = cx.state_mut(&self.state)?.clone();
        let mut patch = PatchDraft::default();
        // Every materialized frame exposes its AST-owned scope, even when a
        // particular rule does not itself emit graph facts from that scope.
        let frame_scope = staged.ast_scope(&owner, &mut patch);
        let mut draft = FrameDraft::default();
        {
            let mut scope_cx: ScopeCx<
                '_,
                '_,
                '_,
                Root,
                Ast,
                Anchor,
                Label,
                Datum,
                Reference,
                Request,
            > = ScopeCx {
                state: &mut staged,
                patch: &mut patch,
                derive: cx,
                owner: owner.clone(),
                key: key.clone(),
                draft: &mut draft,
                asts: HashMap::from([(key.ast.clone(), artifact)]),
                _root: PhantomData,
            };
            self.visitor
                .visit(&mut scope_cx, node, key.incoming)
                .map_err(|error| NodeError::message(error.to_string()))?;
        }
        let edges = draft.edges.clone();
        let datums = draft.datums.clone();
        let references = draft.references.clone();
        let requests = draft.requests.clone();
        let pending = draft.pending.clone();
        staged
            .replace_frame(key.clone(), owner, draft, &mut patch)
            .map_err(|error| NodeError::message(error.to_string()))?;
        *cx.state_mut(&self.state)? = staged;

        cx.emit::<ScopeHandle<Root, Ast, Anchor, Label, Datum, Reference, Request>>(
            ScopeKey::frame(key.clone()),
            frame_scope,
        )?;
        for edge in edges {
            cx.emit_relation::<ScopeEdges<Label>>(edge)?;
        }
        for datum in datums {
            cx.emit_relation::<ScopeDatums<Datum>>(datum)?;
        }
        for reference in references {
            cx.emit_relation::<ScopeReferences<Reference>>(reference)?;
        }
        for request in requests {
            cx.emit_relation::<SourceRequirements<Request>>(request)?;
        }
        for child in pending {
            cx.require::<Self>(ScopeKey::frame(child))?;
        }
        Ok(())
    }
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request> Node
    for ScopeNode<Root, Ast, Anchor, Label, Datum, Reference, Request>
where
    Root: LexerRoot + Clone + 'static,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    type Key = ScopeKey;
    type Output = ScopeStamp<Root, Ast, Anchor, Label, Datum, Reference, Request>;

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        match key {
            ScopeKey::Root(uri) => self.root(cx, uri),
            ScopeKey::Frame(key) => self.frame(cx, key),
        }
    }

    fn reclaim(&self, cx: &mut ReclaimCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let mut staged = cx.state_mut(&self.state)?.clone();
        let mut patch = PatchDraft::default();
        match key {
            ScopeKey::Root(uri) => staged.forget_root(
                uri,
                |frame| cx.is_live::<Self>(ScopeKey::frame(frame.clone())),
                &mut patch,
            ),
            ScopeKey::Frame(frame) => staged.forget_frame(&frame, &mut patch),
        }

        // Graph task ownership is authoritative. Once the final root/frame
        // task disappears, discard every private allocation/cache record so a
        // later demand cannot inherit stale frames, fact counts, or requests.
        if !cx.has_materialized::<Self>() {
            staged = State::default();
        }
        *cx.state_mut(&self.state)? = staged;
        Ok(())
    }
}

/// Rule-local mutation surface for one [`ScopeNode`] frame.
pub struct ScopeCx<
    'scope,
    'transaction,
    'nodes,
    Root,
    Ast,
    Anchor,
    Label,
    Datum,
    Reference,
    Request,
> where
    Root: LexerRoot + Clone,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    state: &'scope mut State<Anchor, Label, Datum, Reference, Request>,
    patch: &'scope mut PatchDraft<Label, Datum, Reference, Request>,
    derive: &'scope mut DeriveCx<'transaction, 'nodes>,
    owner: AstOwner,
    key: ScopeFrameKey,
    draft: &'scope mut FrameDraft<Label, Datum, Reference, Request>,
    asts: HashMap<AstKey, AstArtifact<Ast>>,
    _root: PhantomData<fn() -> Root>,
}

impl<Root, Ast, Anchor, Label, Datum, Reference, Request>
    ScopeCx<'_, '_, '_, Root, Ast, Anchor, Label, Datum, Reference, Request>
where
    Root: LexerRoot + Clone + 'static,
    Ast: Clone + Send + Sync + 'static,
    Anchor: Clone + Eq + Hash + Send + Sync + 'static,
    Label: Clone + Eq + Hash + Send + Sync + 'static,
    Datum: Clone + Eq + Hash + Send + Sync + 'static,
    Reference: Clone + Eq + Hash + Send + Sync + 'static,
    Request: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn artifact(&mut self, node: AstBox<Ast>) -> Result<&AstArtifact<Ast>, ScopeError> {
        let key = AstKey {
            uri: node.uri,
            id: node.id,
        };
        if !self.asts.contains_key(&key) {
            let artifact = self
                .derive
                .observe::<ParsedAst<Root, Ast>>(key.clone())
                .map_err(|_| ScopeError::MissingAst(key.clone()))?;
            self.asts.insert(key.clone(), artifact);
        }
        Ok(self.asts.get(&key).expect("AST artifact was inserted"))
    }

    fn owner_of(&mut self, node: AstBox<Ast>) -> Result<AstOwner, ScopeError> {
        let artifact = self.artifact(node)?;
        Ok(AstOwner {
            uri: artifact.ast_box.uri,
            product: artifact.product,
        })
    }

    /// Reads and caches the individual parser artifact that owns `node`.
    pub fn ast(&mut self, node: AstBox<Ast>) -> Result<&Ast, ScopeError> {
        Ok(self.artifact(node)?.value.as_ref())
    }

    pub fn scope(&mut self) -> Scope {
        self.state.ast_scope(&self.owner, self.patch)
    }

    pub fn scope_of(&mut self, node: AstBox<Ast>) -> Result<Scope, ScopeError> {
        let owner = self.owner_of(node)?;
        Ok(self.state.ast_scope(&owner, self.patch))
    }

    pub fn external_scope(&mut self, anchor: Anchor) -> Scope {
        self.state.external_scope(anchor, self.patch)
    }

    pub fn edge(&mut self, source: Scope, label: Label, target: Scope, property: ScopeProperty) {
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

    /// Schedules a child frame. It is required only after this frame commits.
    pub fn visit(&mut self, node: AstBox<Ast>, incoming: Scope) -> Result<(), ScopeError> {
        self.owner_of(node)?;
        let child = ScopeFrameKey::new(
            AstKey {
                uri: node.uri,
                id: node.id,
            },
            incoming,
        );
        if self.draft.children.insert(child.clone()) {
            self.draft.pending.push(child);
        }
        Ok(())
    }

    pub fn require_source(&mut self, request: Request) {
        if !self.draft.requests.contains(&request) {
            self.draft.requests.push(request);
        }
    }

    pub fn fail(&self, reason: impl Into<String>) -> ScopeError {
        ScopeError::Rule(reason.into())
    }

    /// The task key currently being evaluated.
    pub fn key(&self) -> &ScopeFrameKey {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use plingo_macros::{NonTerminal, Terminal};

    use super::*;
    use crate::{
        component::{
            lex::{LexErrorInfo, LexerNode},
            parse::{AstToken, ParserNode, grammar::Grammar},
            source::{SourceEdit, SourceNode},
        },
        scheme::node::Graph,
        utils::Span,
    };

    #[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
    #[scopes(root { Number })]
    enum Tokens {
        #[regex(r"[0-9]+")]
        Number(usize),
        #[error]
        Error(LexErrorInfo),
    }

    impl fmt::Display for Tokens {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{self:?}")
        }
    }

    #[allow(dead_code)]
    #[derive(NonTerminal, Debug, Clone)]
    enum Value {
        #[rule(Tokens::Number)]
        Number(#[from(0)] AstToken<Tokens>),
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    enum Label {
        Declares,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Datum(usize);

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct AnyDatum;

    impl DatumSelector<Datum> for AnyDatum {
        fn accepts(&self, _: &Datum) -> bool {
            true
        }
    }

    #[test]
    fn scope_frames_publish_nested_uri_free_facts_and_retract_stale_roots() {
        let uri = Span::new("test://node-scope", 0, 0).unwrap().uri;
        let parser = Grammar::from_spec::<Value>().build_lr1::<Tokens>();
        let mut graph = Graph::new();
        graph.install(LexerNode::<Tokens>::new().unwrap()).unwrap();
        graph
            .install(ParserNode::<Tokens, Value>::from_parser(parser))
            .unwrap();
        graph
            .install(ScopeNode::<Tokens, Value, (), Label, Datum, (), ()>::new(
                |cx, node, incoming| {
                    let current = cx.scope();
                    cx.datum(current, Datum(node.id));
                    cx.edge(current, Label::Declares, incoming, ScopeProperty::Cyclic);
                    cx.require_source(());
                    cx.ast(node)?;
                    if current != incoming {
                        cx.visit(node, current)?;
                    }
                    Ok(())
                },
            ))
            .unwrap();
        graph
            .install(ResolutionNode::<Label, Datum, AnyDatum>::default())
            .unwrap();
        graph.command(SourceNode::load(uri)).unwrap();
        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "7".into(),
            }))
            .unwrap();

        let _root = graph
            .request::<ScopeNode<Tokens, Value, (), Label, Datum, (), ()>>(ScopeKey::root(uri))
            .unwrap();
        let root_scope = graph
            .read::<ScopeHandle<Tokens, Value, (), Label, Datum, (), ()>>(ScopeKey::root(uri))
            .expect("the root task must materialize its root scope handle");
        let datums = graph.facts::<ScopeDatums<Datum>>();
        assert_ne!(root_scope, datums[0].scope);
        let edges = graph.facts::<ScopeEdges<Label>>();
        assert_eq!(datums.len(), 1);
        assert_eq!(
            edges.len(),
            2,
            "the queued child frame publishes its own edge"
        );
        assert_eq!(graph.facts::<SourceRequirements<()>>(), vec![()]);
        let scope = datums[0].scope;
        let root_id = datums[0].datum.0;
        let _resolution = graph
            .request::<ResolutionNode<Label, Datum, AnyDatum>>(ResolutionKey {
                start: scope,
                path: super::super::PathExpr::Epsilon,
                selector: AnyDatum,
            })
            .unwrap();
        assert_eq!(
            _resolution.len(),
            1,
            "resolution remains requestable through indexed relations",
        );

        assert_eq!(graph.facts::<ScopeDatums<Datum>>()[0].scope, scope);

        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 1).unwrap(),
                value: "8".into(),
            }))
            .unwrap();
        graph
            .command(SourceNode::apply(SourceEdit::Delete {
                key: Span::new("test://node-scope", 0, 1).unwrap(),
            }))
            .unwrap();
        let roots = graph.read::<ParseRoots<Tokens, Value>>(uri).unwrap();
        assert_ne!(
            roots[0].id, root_id,
            "the parser produced a replacement root"
        );
        let datums = graph.facts::<ScopeDatums<Datum>>();
        assert!(datums.iter().all(|datum| datum.datum.0 == roots[0].id));
        assert!(
            graph
                .facts::<ScopeEdges<Label>>()
                .iter()
                .all(|edge| edge.source == datums[0].scope)
        );
        assert_eq!(graph.facts::<SourceRequirements<()>>(), vec![()]);

        drop(_resolution);
        drop(_root);
        graph.collect_garbage().unwrap();
        assert!(graph.facts::<ScopeDatums<Datum>>().is_empty());
        assert!(graph.facts::<ScopeEdges<Label>>().is_empty());
        assert!(
            graph
                .read::<ScopeHandle<Tokens, Value, (), Label, Datum, (), ()>>(ScopeKey::root(uri))
                .is_none()
        );

        let _rematerialized = graph
            .request::<ScopeNode<Tokens, Value, (), Label, Datum, (), ()>>(ScopeKey::root(uri))
            .unwrap();
        assert_eq!(graph.facts::<ScopeDatums<Datum>>().len(), 1);
    }
}
