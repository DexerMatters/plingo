use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;

use crate::{
    component::{
        parse::{
            AstArtifact, AstKey, AstLocation, ParseRoots, ParsedAst, ParserNode, data::AstBox,
        },
        scope::{
            RelativeRegex, ResolutionPath, Scope, ScopeCatalogNode, ScopeDatum, ScopeDomain,
            ScopeEdge, ScopeError, ScopeHandle, ScopeOwner, ScopeReference,
        },
    },
    scheme::node::{DeriveCx, Node, NodeError, View},
    utils::Span,
};

use crate::component::scope::{
    ScopeDatums, ScopeEdges, ScopeReferences, SourceRequirements, resolve_indexed,
};

type AstOf<D> = <D as ScopeDomain>::Ast;

/// Failure to select exactly one scope-graph resolution.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("scope query resolved no matching datum")]
    Missing,
    #[error("scope query resolved {0} matching data; expected one")]
    Ambiguous(usize),
}

type Visitor<Pass, D> = dyn for<'here, 'transaction, 'nodes> Fn(
        &mut Here<'here, 'transaction, 'nodes, Pass, D>,
        &AstOf<D>,
    ) -> Result<(), ScopeError<D>>
    + Send
    + Sync;

/// Contextual task identity for one pass traversing an AST from an incoming
/// scope. The same AST can therefore be elaborated independently in distinct
/// lexical contexts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ElaborationFrameKey<D: ScopeDomain> {
    pub ast: AstKey,
    pub incoming: Scope<D>,
}

impl<D: ScopeDomain> ElaborationFrameKey<D> {
    pub const fn new(ast: AstKey, incoming: Scope<D>) -> Self {
        Self { ast, incoming }
    }
}

/// Root or frame task key for one elaboration pass.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ElaborationKey<D: ScopeDomain> {
    Root(Uri<&'static str>),
    Frame(ElaborationFrameKey<D>),
}

impl<D: ScopeDomain> ElaborationKey<D> {
    pub const fn root(uri: Uri<&'static str>) -> Self {
        Self::Root(uri)
    }

    pub const fn frame(frame: ElaborationFrameKey<D>) -> Self {
        Self::Frame(frame)
    }
}

/// Unit completion marker for a root or frame elaboration task.
pub struct ElaborationStamp<Pass, D: ScopeDomain>(PhantomData<fn() -> (Pass, D)>);

impl<Pass, D> View for ElaborationStamp<Pass, D>
where
    Pass: Send + Sync + 'static,
    D: ScopeDomain,
{
    type Key = ElaborationKey<D>;
    type Value = ();
}

/// One independently installed semantic pass.
///
/// `Pass` is a marker type that gives each installation a unique graph node
/// identity. Multiple passes over the same domain compose through typed scope
/// relations rather than callback ordering.
pub struct ElaboratorNode<Pass, D: ScopeDomain>
where
    Pass: Send + Sync + 'static,
{
    visitor: Arc<Visitor<Pass, D>>,
    _pass: PhantomData<fn() -> Pass>,
}

impl<Pass, D> ElaboratorNode<Pass, D>
where
    Pass: Send + Sync + 'static,
    D: ScopeDomain,
{
    pub fn new<F>(visitor: F) -> Self
    where
        F: for<'here, 'transaction, 'nodes> Fn(
                &mut Here<'here, 'transaction, 'nodes, Pass, D>,
                &AstOf<D>,
            ) -> Result<(), ScopeError<D>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            visitor: Arc::new(visitor),
            _pass: PhantomData,
        }
    }

    fn root(&self, cx: &mut DeriveCx<'_, '_>, uri: Uri<&'static str>) -> Result<(), NodeError> {
        // Materialize producer facts but depend only on the semantic root
        // manifest and catalog handle. Span-only parser publications therefore
        // do not rerun a pass root.
        cx.materialize::<ParserNode<D::Root, AstOf<D>>>(uri)?;
        let root_owner = ScopeOwner::<D>::document(uri);
        cx.materialize::<ScopeCatalogNode<D>>(root_owner.clone())?;
        let root_scope = cx.observe::<ScopeHandle<D>>(root_owner)?;
        let roots = cx.observe::<ParseRoots<D::Root, AstOf<D>>>(uri)?;
        for ast in roots.iter().cloned() {
            cx.require::<Self>(ElaborationKey::frame(ElaborationFrameKey::new(
                ast, root_scope,
            )))?;
        }
        Ok(())
    }

    fn frame(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        key: ElaborationFrameKey<D>,
    ) -> Result<(), NodeError> {
        let artifact = match cx.observe::<ParsedAst<D::Root, AstOf<D>>>(key.ast.clone()) {
            Ok(artifact) => artifact,
            // Parser deletion can schedule this frame before its parent root
            // replaces children. Empty output atomically retracts its facts.
            Err(NodeError::MissingView(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let owner = ScopeOwner::<D>::ast(key.ast.clone());
        cx.materialize::<ScopeCatalogNode<D>>(owner.clone())?;
        let scope = match cx.observe::<ScopeHandle<D>>(owner) {
            Ok(scope) => scope,
            Err(NodeError::MissingView(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let current = Arc::clone(&artifact.value);
        let mut here = Here {
            derive: cx,
            key,
            scope,
            node: artifact.ast_box,
            artifacts: HashMap::from([(artifact.ast_box.into_key(), artifact)]),
            edges: Vec::new(),
            datums: Vec::new(),
            references: Vec::new(),
            requests: Vec::new(),
            children: HashSet::new(),
            pending: Vec::new(),
            _pass: PhantomData,
        };
        (self.visitor)(&mut here, current.as_ref())
            .map_err(|error| NodeError::message(error.to_string()))?;
        let (edges, datums, references, requests, pending) = here.into_outputs();
        for edge in edges {
            cx.emit_relation::<ScopeEdges<D>>(edge)?;
        }
        for datum in datums {
            cx.emit_relation::<ScopeDatums<D>>(datum)?;
        }
        for reference in references {
            cx.emit_relation::<ScopeReferences<D>>(reference)?;
        }
        for request in requests {
            cx.emit_relation::<SourceRequirements<D>>(request)?;
        }
        for child in pending {
            cx.require::<Self>(ElaborationKey::frame(child))?;
        }
        Ok(())
    }
}

impl<Pass, D> Node for ElaboratorNode<Pass, D>
where
    Pass: Send + Sync + 'static,
    D: ScopeDomain,
{
    type Key = ElaborationKey<D>;
    type Output = ElaborationStamp<Pass, D>;

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        match key {
            ElaborationKey::Root(uri) => self.root(cx, uri),
            ElaborationKey::Frame(frame) => self.frame(cx, frame),
        }
    }
}

/// Rule-local context for one elaborator frame.
pub struct Here<'here, 'transaction, 'nodes, Pass, D>
where
    Pass: Send + Sync + 'static,
    D: ScopeDomain,
{
    derive: &'here mut DeriveCx<'transaction, 'nodes>,
    key: ElaborationFrameKey<D>,
    scope: Scope<D>,
    node: AstBox<AstOf<D>>,
    artifacts: HashMap<AstKey, AstArtifact<AstOf<D>>>,
    edges: Vec<ScopeEdge<D>>,
    datums: Vec<ScopeDatum<D>>,
    references: Vec<ScopeReference<D>>,
    requests: Vec<D::Request>,
    children: HashSet<ElaborationFrameKey<D>>,
    pending: Vec<ElaborationFrameKey<D>>,
    _pass: PhantomData<fn() -> Pass>,
}

impl<Pass, D> Here<'_, '_, '_, Pass, D>
where
    Pass: Send + Sync + 'static,
    D: ScopeDomain,
{
    fn artifact(
        &mut self,
        node: AstBox<AstOf<D>>,
    ) -> Result<&AstArtifact<AstOf<D>>, ScopeError<D>> {
        let key = node.into_key();
        if !self.artifacts.contains_key(&key) {
            let artifact = self
                .derive
                .observe::<ParsedAst<D::Root, AstOf<D>>>(key.clone())
                .map_err(|_| ScopeError::MissingAst(key.clone()))?;
            self.artifacts.insert(key.clone(), artifact);
        }
        self.artifacts.get(&key).ok_or(ScopeError::MissingAst(key))
    }

    /// Current AST identity. The callback receives its resolved value directly.
    pub fn node(&self) -> AstBox<AstOf<D>> {
        self.node
    }

    /// Resolves another semantic AST artifact and records a keyed dependency.
    pub fn ast(&mut self, node: AstBox<AstOf<D>>) -> Result<&AstOf<D>, ScopeError<D>> {
        Ok(self.artifact(node)?.value.as_ref())
    }

    /// Opt-in source-location dependency for diagnostics or source-sensitive
    /// analysis. Semantic passes should not call this unless they need spans.
    pub fn span(&mut self, node: AstBox<AstOf<D>>) -> Result<Span, ScopeError<D>> {
        let key = node.into_key();
        self.derive
            .observe::<AstLocation<D::Root, AstOf<D>>>(key.clone())
            .map_err(|_| ScopeError::MissingAst(key))
    }

    /// Scope allocated for the current AST owner.
    pub fn scope(&self) -> Scope<D> {
        self.scope
    }

    /// Scope from which the current AST was reached.
    pub fn incoming_scope(&self) -> Scope<D> {
        self.key.incoming
    }

    pub fn key(&self) -> &ElaborationFrameKey<D> {
        &self.key
    }

    /// Materializes and returns the stable scope allocated for an AST.
    pub fn scope_of(&mut self, node: AstBox<AstOf<D>>) -> Result<Scope<D>, ScopeError<D>> {
        let key = node.into_key();
        let owner = ScopeOwner::<D>::ast(key.clone());
        self.derive
            .materialize::<ScopeCatalogNode<D>>(owner.clone())
            .map_err(|error| ScopeError::Rule(error.to_string()))?;
        self.derive
            .observe::<ScopeHandle<D>>(owner)
            .map_err(|_| ScopeError::MissingAst(key))
    }

    /// Materializes and returns one application-owned external scope.
    pub fn external_scope(&mut self, anchor: D::Anchor) -> Result<Scope<D>, ScopeError<D>> {
        let owner = ScopeOwner::<D>::external(anchor);
        self.derive
            .materialize::<ScopeCatalogNode<D>>(owner.clone())
            .map_err(|error| ScopeError::Rule(error.to_string()))?;
        self.derive
            .observe::<ScopeHandle<D>>(owner)
            .map_err(|_| ScopeError::Rule("external scope was not materialized".into()))
    }

    /// Resolves data reachable from `start` along paths described by `regex`.
    /// Every visited edge and datum bucket becomes an exact graph dependency.
    pub fn resolve_from<F>(
        &mut self,
        start: Scope<D>,
        regex: RelativeRegex<<D as ScopeDomain>::Label>,
        accepts: F,
    ) -> HashSet<ResolutionPath<D>>
    where
        F: Fn(&<D as ScopeDomain>::Datum) -> bool,
    {
        resolve_indexed(start, regex.into_path(), accepts, |scope, needs_datums| {
            let edges = self.derive.relation_facts_at::<ScopeEdges<D>>(scope);
            let datums = if needs_datums {
                self.derive.relation_facts_at::<ScopeDatums<D>>(scope)
            } else {
                Vec::new()
            };
            (edges, datums)
        })
    }

    /// Resolves data relative to the current AST-owned scope.
    pub fn resolve<F>(
        &mut self,
        regex: RelativeRegex<<D as ScopeDomain>::Label>,
        accepts: F,
    ) -> HashSet<ResolutionPath<D>>
    where
        F: Fn(&<D as ScopeDomain>::Datum) -> bool,
    {
        self.resolve_from(self.scope, regex, accepts)
    }

    /// Resolves exactly one datum or reports a missing/ambiguous result.
    pub fn resolve_one<F>(
        &mut self,
        regex: RelativeRegex<<D as ScopeDomain>::Label>,
        accepts: F,
    ) -> Result<ResolutionPath<D>, ResolveError>
    where
        F: Fn(&<D as ScopeDomain>::Datum) -> bool,
    {
        let answers = self.resolve(regex, accepts);
        let count = answers.len();
        if count == 0 {
            return Err(ResolveError::Missing);
        }
        if count > 1 {
            return Err(ResolveError::Ambiguous(count));
        }
        match answers.into_iter().next() {
            Some(answer) => Ok(answer),
            None => Err(ResolveError::Missing),
        }
    }

    /// Reads one dependency-tracked edge bucket.
    pub fn edges_from(&mut self, scope: Scope<D>) -> Vec<ScopeEdge<D>> {
        self.derive.relation_facts_at::<ScopeEdges<D>>(scope)
    }

    /// Reads one dependency-tracked datum bucket.
    pub fn datums_at(&mut self, scope: Scope<D>) -> Vec<ScopeDatum<D>> {
        self.derive.relation_facts_at::<ScopeDatums<D>>(scope)
    }

    /// Reads one dependency-tracked reference bucket.
    pub fn references_at(&mut self, scope: Scope<D>) -> Vec<ScopeReference<D>> {
        self.derive.relation_facts_at::<ScopeReferences<D>>(scope)
    }

    pub fn edge(&mut self, edge: ScopeEdge<D>) {
        self.edges.push(edge);
    }

    pub fn datum(&mut self, datum: ScopeDatum<D>) {
        self.datums.push(datum);
    }

    pub fn reference(&mut self, reference: ScopeReference<D>) {
        self.references.push(reference);
    }

    /// Convenience form of [`Self::edge`] with the current scope as source.
    pub fn edge_to(
        &mut self,
        label: <D as ScopeDomain>::Label,
        target: Scope<D>,
        property: crate::component::scope::ScopeProperty,
    ) {
        self.edge(ScopeEdge {
            source: self.scope,
            label,
            target,
            property,
        });
    }

    /// Adds a datum at the current scope.
    pub fn datum_here(&mut self, datum: <D as ScopeDomain>::Datum) {
        self.datum(ScopeDatum {
            scope: self.scope,
            datum,
        });
    }

    /// Adds a reference at the current scope.
    pub fn reference_here(&mut self, reference: <D as ScopeDomain>::Reference) {
        self.reference(ScopeReference {
            scope: self.scope,
            reference,
        });
    }

    pub fn require_source(&mut self, request: D::Request) {
        if !self.requests.contains(&request) {
            self.requests.push(request);
        }
    }

    /// Traverses `node` from the current scope.
    pub fn descend(&mut self, node: AstBox<AstOf<D>>) -> Result<Scope<D>, ScopeError<D>> {
        self.visit(node, self.scope)
    }

    /// Schedules the same pass for `node` under `incoming`, returning the
    /// child AST's stable scope immediately.
    pub fn visit(
        &mut self,
        node: AstBox<AstOf<D>>,
        incoming: Scope<D>,
    ) -> Result<Scope<D>, ScopeError<D>> {
        let scope = self.scope_of(node)?;
        let child = ElaborationFrameKey::new(node.into_key(), incoming);
        if self.children.insert(child.clone()) {
            self.pending.push(child);
        }
        Ok(scope)
    }

    pub fn fail(&self, reason: impl Into<String>) -> ScopeError<D> {
        ScopeError::Rule(reason.into())
    }

    fn into_outputs(
        self,
    ) -> (
        Vec<ScopeEdge<D>>,
        Vec<ScopeDatum<D>>,
        Vec<ScopeReference<D>>,
        Vec<D::Request>,
        Vec<ElaborationFrameKey<D>>,
    ) {
        (
            self.edges,
            self.datums,
            self.references,
            self.requests,
            self.pending,
        )
    }
}

trait AstBoxKey<T> {
    fn into_key(self) -> AstKey;
}

impl<T> AstBoxKey<T> for AstBox<T> {
    fn into_key(self) -> AstKey {
        AstKey {
            uri: self.uri,
            id: self.id,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use plingo_macros::{NonTerminal, Terminal};

    use super::*;
    use crate::{
        component::{
            lex::{LexErrorInfo, LexerNode},
            parse::{AstToken, ParseRoots, grammar::Grammar},
            scope::{
                DatumSelector, PathExpr, RelativeRegex, ResolutionKey, ResolutionNode,
                ScopeCatalogNode, ScopeDatums, ScopeEdges, ScopeHandle, ScopeOwner, ScopeProperty,
                SourceRequirements,
            },
            source::{SourceEdit, SourceNode},
        },
        scheme::node::Graph,
        utils::Span,
    };

    #[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
    #[scopes(root { Whitespace, Number })]
    enum Tokens {
        #[regex(r"\s+")]
        #[skip]
        Whitespace,
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

    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Language;
    impl ScopeDomain for Language {
        type Root = Tokens;
        type Ast = Value;
        type Anchor = ();
        type Label = Label;
        type Datum = Datum;
        type Reference = ();
        type Request = ();
    }

    impl DatumSelector<Language> for AnyDatum {
        fn accepts(&self, _: &Datum) -> bool {
            true
        }
    }

    struct Bindings;
    struct Queries;

    #[test]
    fn elaborator_owns_incremental_scope_facts_without_scope_rules() {
        let uri = Span::new("test://elaborator", 0, 0).unwrap().uri;
        let parser = Grammar::from_spec::<Value>().build_lr1::<Tokens>();
        let mut graph = Graph::new();
        graph.install(LexerNode::<Tokens>::new().unwrap()).unwrap();
        graph
            .install(ParserNode::<Tokens, Value>::from_parser(parser))
            .unwrap();
        ScopeCatalogNode::<Language>::install(&mut graph).unwrap();
        let visits = Arc::new(AtomicUsize::new(0));
        let visitor_visits = Arc::clone(&visits);
        let queries = Arc::new(AtomicUsize::new(0));
        let query_visits = Arc::clone(&queries);
        graph
            .install(ElaboratorNode::<Bindings, Language>::new(
                move |here, _ast| {
                    visitor_visits.fetch_add(1, Ordering::Relaxed);
                    let scope = here.scope();
                    here.datum_here(Datum(here.node().id));
                    here.edge_to(
                        Label::Declares,
                        here.incoming_scope(),
                        ScopeProperty::Cyclic,
                    );
                    here.require_source(());
                    if scope != here.incoming_scope() {
                        here.descend(here.node())?;
                    }
                    Ok(())
                },
            ))
            .unwrap();
        graph
            .install(ElaboratorNode::<Queries, Language>::new(
                move |here, _ast| {
                    // A missing answer is normal while a producer frame is
                    // still deriving. Its queried bucket reruns this frame
                    // once the binding datum has been published.
                    if let Ok(answer) = here.resolve_one(RelativeRegex::here(), |_| true) {
                        assert_eq!(answer.scopes[0], here.scope());
                        query_visits.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                },
            ))
            .unwrap();
        graph
            .install(ResolutionNode::<Language, AnyDatum>::default())
            .unwrap();
        graph.command(SourceNode::load(uri)).unwrap();
        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "7".into(),
            }))
            .unwrap();

        let root = graph
            .request::<ElaboratorNode<Bindings, Language>>(ElaborationKey::root(uri))
            .unwrap();
        let query_root = graph
            .request::<ElaboratorNode<Queries, Language>>(ElaborationKey::root(uri))
            .unwrap();
        assert_eq!(queries.load(Ordering::Relaxed), 1);
        let root_scope = graph
            .read::<ScopeHandle<Language>>(ScopeOwner::document(uri))
            .unwrap();
        let datums = graph.facts::<ScopeDatums<Language>>();
        let scope = datums[0].scope;
        assert_ne!(scope, root_scope);
        assert_eq!(datums.len(), 1, "fact support is multi-owner");
        assert_eq!(graph.facts::<ScopeEdges<Language>>().len(), 2);
        assert_eq!(graph.facts::<SourceRequirements<Language>>(), vec![()]);
        let resolution = graph
            .request::<ResolutionNode<Language, AnyDatum>>(ResolutionKey {
                start: scope,
                path: PathExpr::Epsilon,
                selector: AnyDatum,
            })
            .unwrap();
        assert_eq!(resolution.len(), 1);

        let semantic_visits = visits.load(Ordering::Relaxed);
        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "\n".into(),
            }))
            .unwrap();
        assert_eq!(visits.load(Ordering::Relaxed), semantic_visits);

        let prior_root = graph.read::<ParseRoots<Tokens, Value>>(uri).unwrap()[0].clone();
        graph
            .command(SourceNode::apply_all(vec![
                SourceEdit::Delete {
                    key: Span::new_uri(uri, 1, 2).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::point_uri(uri, 1).unwrap(),
                    value: "8".into(),
                },
            ]))
            .unwrap();
        let replacement = graph.read::<ParseRoots<Tokens, Value>>(uri).unwrap()[0].clone();
        assert_ne!(replacement.id, prior_root.id);
        assert!(
            graph
                .facts::<ScopeDatums<Language>>()
                .iter()
                .all(|fact| fact.datum.0 == replacement.id)
        );

        drop(resolution);
        drop(query_root);
        drop(root);
        graph.collect_garbage().unwrap();
        assert!(graph.facts::<ScopeDatums<Language>>().is_empty());
        assert!(graph.facts::<ScopeEdges<Language>>().is_empty());
        assert!(graph.facts::<SourceRequirements<Language>>().is_empty());
    }
}
