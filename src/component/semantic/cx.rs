//! Transactional context for one keyed elaborator task.

use std::{collections::HashSet, fmt, sync::Arc};

use crate::{
    component::{
        lex::{TokenEntryKey, TokenLexeme},
        parse::{
            AstArtifact, AstKey, AstLocation, AstToken, ParseCandidate, ParsedAst,
            data::{AstBox, ProductId},
        },
        scope::{
            RelativeRegex, ResolutionPath, ScopeAllocations, ScopeCatalogNode, ScopeData,
            ScopeDomain, ScopeEdge, ScopeEdges, ScopeHandle, ScopeId, ScopeLifecycle,
            ScopeLifecycles, ScopeProperty, SourceRequirements, resolve_indexed,
        },
    },
    scheme::node::{DeriveCx, NodeError, ReadGraph},
    utils::Span,
};

use super::{
    AstWalk, Child, Elaboration, ElaboratorDiagnostic, ElaboratorDiagnostics, ElaboratorError,
    ElaboratorKey, ElaboratorOutput, ElaboratorRole, ElaboratorTask, FrameworkDiagnostic,
    ScopeAccess, node::ElaboratorNode,
};

pub struct ElaboratorCx<'task, 'transaction, 'nodes, P: ElaboratorRole> {
    pub(crate) derive: &'task mut DeriveCx<'transaction, 'nodes>,
    pub(crate) task: Option<ElaboratorTask<P>>,
    pub(crate) incoming: Option<ScopeId<P::Domain>>,
    pub(crate) input: P::Input,
    pub(crate) node: Option<AstKey>,
    pub(crate) interpretations: Arc<[ParseCandidate<<P::Domain as ScopeDomain>::Ast>]>,
    data: Vec<(ScopeId<P::Domain>, <P::Domain as ScopeDomain>::ScopeData)>,
    edges: Vec<ScopeEdge<P::Domain>>,
    lifecycles: Vec<ScopeLifecycle<P::Domain>>,
    declared: HashSet<ScopeId<P::Domain>>,
    sealed: HashSet<ScopeId<P::Domain>>,
    requests: Vec<<P::Domain as ScopeDomain>::Request>,
    pub(crate) diagnostics: Vec<ElaboratorDiagnostic<P::Diagnostic>>,
    pub(crate) children: HashSet<ElaboratorTask<P>>,
    pub(crate) pending: Vec<ElaboratorTask<P>>,
    pub(crate) awaiting: bool,
}

impl<'task, 'transaction, 'nodes, P: ElaboratorRole> ElaboratorCx<'task, 'transaction, 'nodes, P> {
    pub(crate) fn root(
        derive: &'task mut DeriveCx<'transaction, 'nodes>,
        interpretations: Arc<[ParseCandidate<<P::Domain as ScopeDomain>::Ast>]>,
        input: P::Input,
    ) -> Self {
        Self {
            derive,
            task: None,
            incoming: None,
            input,
            node: None,
            interpretations,
            data: Vec::new(),
            edges: Vec::new(),
            lifecycles: Vec::new(),
            declared: HashSet::new(),
            sealed: HashSet::new(),
            requests: Vec::new(),
            diagnostics: Vec::new(),
            children: HashSet::new(),
            pending: Vec::new(),
            awaiting: false,
        }
    }

    pub(crate) fn task(
        derive: &'task mut DeriveCx<'transaction, 'nodes>,
        task: ElaboratorTask<P>,
        interpretation: Option<ParseCandidate<<P::Domain as ScopeDomain>::Ast>>,
    ) -> Self {
        Self {
            derive,
            incoming: Some(task.incoming),
            input: task.input.clone(),
            node: Some(task.ast.clone()),
            task: Some(task),
            interpretations: interpretation
                .map(|value| Arc::from([value]))
                .unwrap_or_default(),
            data: Vec::new(),
            edges: Vec::new(),
            lifecycles: Vec::new(),
            declared: HashSet::new(),
            sealed: HashSet::new(),
            requests: Vec::new(),
            diagnostics: Vec::new(),
            children: HashSet::new(),
            pending: Vec::new(),
            awaiting: false,
        }
    }

    pub(crate) fn publish(
        self,
        task: ElaboratorTask<P>,
        result: Elaboration<P::Output>,
    ) -> Result<(), NodeError> {
        if self.declared != self.sealed {
            return Err(NodeError::message(
                "an elaborator published an unsealed scope",
            ));
        }
        let output = if self.awaiting {
            Elaboration::Awaiting
        } else {
            result
        };
        self.derive
            .emit::<ElaboratorOutput<P>>(task.clone(), output)?;
        self.derive
            .emit::<ElaboratorDiagnostics<P>>(task, self.diagnostics.into())?;
        for (scope, data) in self.data {
            self.derive.emit::<ScopeData<P::Domain>>(scope, data)?;
        }
        for edge in self.edges {
            self.derive.emit_relation::<ScopeEdges<P::Domain>>(edge)?;
        }
        for lifecycle in self.lifecycles {
            self.derive
                .emit_relation::<ScopeLifecycles<P::Domain>>(lifecycle)?;
        }
        for request in self.requests {
            self.derive
                .emit_relation::<SourceRequirements<P::Domain>>(request)?;
        }
        for child in self.pending {
            self.derive
                .defer::<ElaboratorNode<P>>(ElaboratorKey::Task(child));
        }
        Ok(())
    }

    pub fn input(&self) -> &P::Input {
        &self.input
    }
    pub fn is_root(&self) -> bool {
        self.task.is_none()
    }
    pub fn interpretations(&self) -> &[ParseCandidate<<P::Domain as ScopeDomain>::Ast>] {
        &self.interpretations
    }

    pub fn choose_interpretation(
        &mut self,
        product: ProductId,
    ) -> Result<Arc<<P::Domain as ScopeDomain>::Ast>, ElaboratorError<P::Diagnostic>> {
        let Some((node, value)) = self
            .interpretations
            .iter()
            .find(|candidate| candidate.product == product)
            .map(|candidate| (candidate.ast_box, Arc::clone(&candidate.value)))
        else {
            return Err(FrameworkDiagnostic::UnknownInterpretation(product).into());
        };
        if let Some(selected) = &self.node {
            if selected != &node.key() {
                return Err(FrameworkDiagnostic::InterpretationAlreadyChosen.into());
            }
            return Ok(value);
        }
        self.node = Some(node.key());
        if let Some(incoming) = self.incoming {
            self.task = Some(ElaboratorTask::new(
                node.key(),
                incoming,
                self.input.clone(),
            ));
        }
        Ok(value)
    }

    pub fn choose_unique_interpretation(
        &mut self,
    ) -> Result<Arc<<P::Domain as ScopeDomain>::Ast>, ElaboratorError<P::Diagnostic>> {
        let product = match self.interpretations.as_ref() {
            [] => return Err(FrameworkDiagnostic::MissingInterpretation.into()),
            [candidate] => candidate.product,
            candidates => {
                return Err(FrameworkDiagnostic::AmbiguousInterpretation(candidates.len()).into());
            }
        };
        self.choose_interpretation(product)
    }

    pub fn try_interpretations<T, E, I, F>(
        &mut self,
        products: I,
        mut attempt: F,
    ) -> Result<T, ElaboratorError<P::Diagnostic>>
    where
        E: fmt::Display,
        I: IntoIterator<Item = ProductId>,
        F: FnMut(&mut Self, &<P::Domain as ScopeDomain>::Ast) -> Result<T, E>,
    {
        if self.node.is_some() {
            return Err(FrameworkDiagnostic::InterpretationAlreadyChosen.into());
        }
        let mut failures = Vec::new();
        let mut attempted = false;
        for product in products {
            attempted = true;
            let checkpoint = OutputCheckpoint::new(self);
            let value = self.choose_interpretation(product)?;
            match attempt(self, value.as_ref()) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    failures.push(format!("product {product}: {error}"));
                    checkpoint.restore(self);
                }
            }
        }
        if !attempted {
            return Err(FrameworkDiagnostic::MissingInterpretation.into());
        }
        Err(FrameworkDiagnostic::Message(format!(
            "every requested interpretation was rejected: {}",
            failures.join("; ")
        ))
        .into())
    }

    pub fn ast_key(&self) -> AstKey {
        self.node
            .clone()
            .expect("choose an interpretation before requesting its AST identity")
    }

    pub(crate) fn current_artifact(
        &mut self,
    ) -> Result<AstArtifact, ElaboratorError<P::Diagnostic>> {
        let key = self.ast_key();
        self.derive
            .get::<ParsedAst<<P::Domain as ScopeDomain>::Root>>(key.clone())
            .ok_or(FrameworkDiagnostic::MissingAst(key).into())
    }

    pub fn current_ast<T>(&mut self) -> Result<Option<Arc<T>>, ElaboratorError<P::Diagnostic>>
    where
        T: Send + Sync + 'static,
    {
        Ok(self.current_artifact()?.deref::<T>())
    }

    pub fn ast<T>(&mut self, node: AstBox<T>) -> Result<Arc<T>, ElaboratorError<P::Diagnostic>>
    where
        T: Send + Sync + 'static,
    {
        let key = node.key();
        self.derive
            .get::<ParsedAst<<P::Domain as ScopeDomain>::Root>>(key.clone())
            .ok_or_else(|| FrameworkDiagnostic::MissingAst(key.clone()))?
            .deref::<T>()
            .ok_or_else(|| FrameworkDiagnostic::MissingAst(key).into())
    }

    pub fn span<T>(&mut self, node: AstBox<T>) -> Result<Span, ElaboratorError<P::Diagnostic>> {
        let key = node.key();
        self.derive
            .get::<AstLocation<<P::Domain as ScopeDomain>::Root>>(key.clone())
            .ok_or_else(|| FrameworkDiagnostic::MissingAst(key).into())
    }

    pub fn text(
        &mut self,
        token: AstToken<<P::Domain as ScopeDomain>::Root>,
    ) -> Result<Arc<str>, ElaboratorError<P::Diagnostic>> {
        let uri = self.ast_key().uri;
        self.derive
            .get::<TokenLexeme<<P::Domain as ScopeDomain>::Root>>(TokenEntryKey {
                uri,
                id: token.id,
            })
            .ok_or_else(|| FrameworkDiagnostic::MissingToken(token.id).into())
    }

    fn catalog(
        &mut self,
        key: <P::Domain as ScopeDomain>::ScopeKey,
    ) -> Result<ScopeId<P::Domain>, ElaboratorError<P::Diagnostic>> {
        self.derive
            .materialize::<ScopeCatalogNode<P::Domain>>(key.clone())
            .map_err(|error| FrameworkDiagnostic::Message(error.to_string()))?;
        self.derive
            .get::<ScopeHandle<P::Domain>>(key)
            .ok_or_else(|| {
                FrameworkDiagnostic::Message("scope catalog did not publish an identity".into())
                    .into()
            })
    }

    /// Declares exactly one semantic scope and its one datum.
    pub fn declare(
        &mut self,
        key: <P::Domain as ScopeDomain>::ScopeKey,
        data: <P::Domain as ScopeDomain>::ScopeData,
    ) -> Result<ScopeId<P::Domain>, ElaboratorError<P::Diagnostic>> {
        if P::SCOPE_ACCESS == ScopeAccess::Query {
            return Err(FrameworkDiagnostic::Message(
                "this elaborator role cannot declare semantic scopes".into(),
            )
            .into());
        }
        let scope = self.catalog(key)?;
        if !self.declared.insert(scope) {
            return Err(FrameworkDiagnostic::Message(
                "this task declared one semantic scope twice".into(),
            )
            .into());
        }
        self.data.push((scope, data));
        Ok(scope)
    }

    /// Declares and attaches the semantic scope of a root task.
    pub fn declare_root(
        &mut self,
        key: <P::Domain as ScopeDomain>::ScopeKey,
        data: <P::Domain as ScopeDomain>::ScopeData,
    ) -> Result<ScopeId<P::Domain>, ElaboratorError<P::Diagnostic>> {
        if self.incoming.is_some() {
            return Err(FrameworkDiagnostic::Message(
                "root semantic scope was attached twice".into(),
            )
            .into());
        }
        let scope = self.declare(key, data)?;
        self.incoming = Some(scope);
        let ast = self.ast_key();
        self.task = Some(ElaboratorTask::new(ast, scope, self.input.clone()));
        Ok(scope)
    }

    /// Attaches a root or task to an existing catalog scope without defining it.
    pub fn attach_root(
        &mut self,
        key: <P::Domain as ScopeDomain>::ScopeKey,
    ) -> Result<ScopeId<P::Domain>, ElaboratorError<P::Diagnostic>> {
        let scope = self.catalog(key)?;
        if let Some(incoming) = self.incoming {
            if incoming != scope {
                return Err(FrameworkDiagnostic::Message(
                    "root semantic scope was attached twice".into(),
                )
                .into());
            }
            return Ok(scope);
        }
        self.incoming = Some(scope);
        let ast = self.ast_key();
        self.task = Some(ElaboratorTask::new(ast, scope, self.input.clone()));
        Ok(scope)
    }

    /// Resolves an explicitly named scope key to its stable graph identity.
    pub fn scope(
        &mut self,
        key: <P::Domain as ScopeDomain>::ScopeKey,
    ) -> Result<ScopeId<P::Domain>, ElaboratorError<P::Diagnostic>> {
        self.catalog(key)
    }

    /// Finds an already allocated scope without creating one.
    pub fn find_scope(
        &self,
        key: <P::Domain as ScopeDomain>::ScopeKey,
    ) -> Option<ScopeId<P::Domain>> {
        self.derive
            .scan::<ScopeAllocations<P::Domain>>(key)
            .into_iter()
            .next()
            .map(|fact| fact.scope)
    }

    pub fn data(&self, scope: ScopeId<P::Domain>) -> Option<<P::Domain as ScopeDomain>::ScopeData> {
        self.derive.get::<ScopeData<P::Domain>>(scope)
    }

    pub fn incoming_scope(&self) -> ScopeId<P::Domain> {
        self.incoming
            .expect("attach the root semantic scope before using the incoming scope")
    }

    pub fn edge(
        &mut self,
        source: ScopeId<P::Domain>,
        label: <P::Domain as ScopeDomain>::Label,
        target: ScopeId<P::Domain>,
        property: ScopeProperty,
    ) -> Result<(), ElaboratorError<P::Diagnostic>> {
        if P::SCOPE_ACCESS == ScopeAccess::Query {
            return Err(FrameworkDiagnostic::Message(
                "this elaborator role cannot construct binding edges".into(),
            )
            .into());
        }
        if source == target && property == ScopeProperty::Acyclic {
            return Err(FrameworkDiagnostic::Message(
                "an acyclic scope edge cannot be self-referential".into(),
            )
            .into());
        }
        self.edges.push(ScopeEdge {
            source,
            label,
            target,
            property,
        });
        Ok(())
    }

    /// Closes a scope contribution after its datum and all edges are staged.
    pub fn seal(
        &mut self,
        scope: ScopeId<P::Domain>,
    ) -> Result<(), ElaboratorError<P::Diagnostic>> {
        if !self.declared.contains(&scope) {
            return Err(FrameworkDiagnostic::Message(
                "this task cannot close a scope it did not declare".into(),
            )
            .into());
        }
        if !self.sealed.insert(scope) {
            return Err(
                FrameworkDiagnostic::Message("scope completeness was closed twice".into()).into(),
            );
        }
        self.lifecycles.push(ScopeLifecycle::closed(scope));
        Ok(())
    }

    pub fn require_source(&mut self, request: <P::Domain as ScopeDomain>::Request) {
        self.requests.push(request);
    }

    pub fn schedule<T>(
        &mut self,
        node: AstBox<T>,
        incoming: ScopeId<P::Domain>,
        input: P::Input,
    ) -> Result<Child<P>, ElaboratorError<P::Diagnostic>> {
        self.schedule_key(node.key(), incoming, input)
    }

    fn schedule_key(
        &mut self,
        ast: AstKey,
        incoming: ScopeId<P::Domain>,
        input: P::Input,
    ) -> Result<Child<P>, ElaboratorError<P::Diagnostic>> {
        let task = ElaboratorTask::new(ast, incoming, input);
        if self.children.insert(task.clone()) {
            self.pending.push(task.clone());
        }
        Ok(Child { task })
    }

    pub fn schedule_children<T: AstWalk>(
        &mut self,
        ast: &T,
        incoming: ScopeId<P::Domain>,
        input: P::Input,
    ) -> Result<Vec<Child<P>>, ElaboratorError<P::Diagnostic>> {
        let mut keys = Vec::new();
        ast.direct_children(&mut |key| keys.push(key));
        keys.into_iter()
            .map(|key| self.schedule_key(key, incoming, input.clone()))
            .collect()
    }

    pub fn observe(
        &mut self,
        child: &Child<P>,
    ) -> Result<Option<P::Output>, ElaboratorError<P::Diagnostic>> {
        match self.derive.get::<ElaboratorOutput<P>>(child.task.clone()) {
            Some(Elaboration::Complete(value)) => Ok(Some(value)),
            Some(Elaboration::Awaiting) | None => {
                self.awaiting = true;
                Ok(None)
            }
        }
    }

    pub fn child_diagnostics(
        &mut self,
        child: &Child<P>,
    ) -> Result<Option<Arc<[ElaboratorDiagnostic<P::Diagnostic>]>>, ElaboratorError<P::Diagnostic>>
    {
        Ok(self
            .derive
            .get::<ElaboratorDiagnostics<P>>(child.task.clone()))
    }

    pub fn report(&mut self, diagnostic: P::Diagnostic) {
        self.diagnostics
            .push(ElaboratorDiagnostic::Rule(diagnostic));
    }
    pub fn report_framework(&mut self, diagnostic: FrameworkDiagnostic) {
        self.diagnostics
            .push(ElaboratorDiagnostic::Framework(diagnostic));
    }

    pub fn resolve_from<F>(
        &mut self,
        start: ScopeId<P::Domain>,
        regex: RelativeRegex<<P::Domain as ScopeDomain>::Label>,
        accepts: F,
    ) -> HashSet<ResolutionPath<P::Domain>>
    where
        F: Fn(&<P::Domain as ScopeDomain>::ScopeData) -> bool,
    {
        resolve_indexed(start, regex.into_path(), accepts, |scope, needs| {
            if self
                .derive
                .scan::<ScopeLifecycles<P::Domain>>(scope)
                .is_empty()
            {
                self.awaiting = true;
            }
            let edges = self.derive.scan::<ScopeEdges<P::Domain>>(scope);
            let data = needs
                .then(|| self.derive.get::<ScopeData<P::Domain>>(scope))
                .flatten();
            (edges, data)
        })
    }
}

struct OutputCheckpoint<R: ElaboratorRole> {
    data: usize,
    edges: usize,
    lifecycles: usize,
    declared: HashSet<ScopeId<R::Domain>>,
    sealed: HashSet<ScopeId<R::Domain>>,
    requests: usize,
    diagnostics: usize,
    children: HashSet<ElaboratorTask<R>>,
    pending: usize,
    task: Option<ElaboratorTask<R>>,
    incoming: Option<ScopeId<R::Domain>>,
    node: Option<AstKey>,
}

impl<R: ElaboratorRole> OutputCheckpoint<R> {
    fn new(cx: &ElaboratorCx<'_, '_, '_, R>) -> Self {
        Self {
            data: cx.data.len(),
            edges: cx.edges.len(),
            lifecycles: cx.lifecycles.len(),
            declared: cx.declared.clone(),
            sealed: cx.sealed.clone(),
            requests: cx.requests.len(),
            diagnostics: cx.diagnostics.len(),
            children: cx.children.clone(),
            pending: cx.pending.len(),
            task: cx.task.clone(),
            incoming: cx.incoming,
            node: cx.node.clone(),
        }
    }
    fn restore(self, cx: &mut ElaboratorCx<'_, '_, '_, R>) {
        cx.data.truncate(self.data);
        cx.edges.truncate(self.edges);
        cx.lifecycles.truncate(self.lifecycles);
        cx.declared = self.declared;
        cx.sealed = self.sealed;
        cx.requests.truncate(self.requests);
        cx.diagnostics.truncate(self.diagnostics);
        cx.children = self.children;
        cx.pending.truncate(self.pending);
        cx.task = self.task;
        cx.incoming = self.incoming;
        cx.node = self.node;
    }
}
