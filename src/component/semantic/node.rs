//! Graph node which runs one elaborator root or task.

use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use fluent_uri::Uri;

use crate::{
    component::{
        parse::{ParseCandidate, ParseCandidates, ParsedAst, data::AstBox},
        scope::{ScopeData, ScopeDomain, ScopeEdges, ScopeLifecycles, SourceRequirements},
    },
    scheme::node::{DeriveCx, NodeError, NodeProvider, NodeSchema, ReadGraph},
};

use super::{
    Elaboration, ElaboratorCx, ElaboratorDiagnostics, ElaboratorError, ElaboratorOutput,
    ElaboratorRole, ElaboratorTask, FrameworkDiagnostic,
};

#[doc(hidden)]
pub enum ElaboratorKey<R: ElaboratorRole> {
    AttachedRoot(Uri<&'static str>),
    Task(ElaboratorTask<R>),
}
impl<R: ElaboratorRole> Clone for ElaboratorKey<R> {
    fn clone(&self) -> Self {
        match self {
            Self::AttachedRoot(uri) => Self::AttachedRoot(*uri),
            Self::Task(task) => Self::Task(task.clone()),
        }
    }
}
impl<R: ElaboratorRole> PartialEq for ElaboratorKey<R> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::AttachedRoot(a), Self::AttachedRoot(b)) => a == b,
            (Self::Task(a), Self::Task(b)) => a == b,
            _ => false,
        }
    }
}
impl<R: ElaboratorRole> Eq for ElaboratorKey<R> {}
impl<R: ElaboratorRole> Hash for ElaboratorKey<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::AttachedRoot(uri) => {
                0u8.hash(state);
                uri.hash(state);
            }
            Self::Task(task) => {
                1u8.hash(state);
                task.hash(state);
            }
        }
    }
}
impl<R: ElaboratorRole> fmt::Debug for ElaboratorKey<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttachedRoot(uri) => f.debug_tuple("AttachedRoot").field(uri).finish(),
            Self::Task(task) => f.debug_tuple("Task").field(task).finish(),
        }
    }
}

pub struct ElaboratorNode<R: ElaboratorRole> {
    rules: Box<
        dyn for<'a, 't, 'n> Fn(
                &mut ElaboratorCx<'a, 't, 'n, R>,
            )
                -> Result<Elaboration<R::Output>, ElaboratorError<R::Diagnostic>>
            + Send
            + Sync,
    >,
    _role: PhantomData<fn() -> R>,
}
impl<R: ElaboratorRole> ElaboratorNode<R> {
    pub fn new<F>(rules: F) -> Self
    where
        F: for<'a, 't, 'n> Fn(
                &mut ElaboratorCx<'a, 't, 'n, R>,
            )
                -> Result<Elaboration<R::Output>, ElaboratorError<R::Diagnostic>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            rules: Box::new(rules),
            _role: PhantomData,
        }
    }
    fn root(&self, derive: &mut DeriveCx<'_, '_>, uri: Uri<&'static str>) -> Result<(), NodeError> {
        let Some(candidates) = derive.get::<ParseCandidates<<R::Domain as ScopeDomain>::Root, <R::Domain as ScopeDomain>::Ast>>(uri) else { return Ok(()); };
        let mut cx = ElaboratorCx::root(derive, candidates, R::Input::default());
        let output =
            (self.rules)(&mut cx).map_err(|error| NodeError::message(error.to_string()))?;
        match cx.task.clone() {
            Some(task) => cx.publish(task, output),
            None if cx.awaiting => Ok(()),
            None => Err(NodeError::message(
                FrameworkDiagnostic::InterpretationNotSelected.to_string(),
            )),
        }
    }
    fn task(
        &self,
        derive: &mut DeriveCx<'_, '_>,
        task: ElaboratorTask<R>,
    ) -> Result<(), NodeError> {
        let Some(artifact) =
            derive.get::<ParsedAst<<R::Domain as ScopeDomain>::Root>>(task.ast.clone())
        else {
            return Ok(());
        };
        let interpretation = artifact
            .deref::<<R::Domain as ScopeDomain>::Ast>()
            .map(|value| ParseCandidate {
                ast_box: AstBox::new(task.ast.id, task.ast.uri),
                product: artifact.product,
                value,
            });
        let mut cx = ElaboratorCx::task(derive, task.clone(), interpretation);
        let output =
            (self.rules)(&mut cx).map_err(|error| NodeError::message(error.to_string()))?;
        cx.publish(task, output)
    }
}
impl<R: ElaboratorRole> NodeProvider for ElaboratorNode<R> {
    type Key = ElaboratorKey<R>;
    fn schema() -> NodeSchema {
        use crate::scheme::node::PortDeclaration;
        NodeSchema::new(
            std::any::type_name::<Self>(),
            vec![
                PortDeclaration::map::<ElaboratorOutput<R>>(),
                PortDeclaration::map::<ElaboratorDiagnostics<R>>(),
                PortDeclaration::map::<ScopeData<R::Domain>>(),
                PortDeclaration::indexed_set::<ScopeEdges<R::Domain>>(),
                PortDeclaration::indexed_set::<ScopeLifecycles<R::Domain>>(),
                PortDeclaration::indexed_set::<SourceRequirements<R::Domain>>(),
            ],
        )
    }
    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        match key {
            ElaboratorKey::AttachedRoot(uri) => self.root(cx, uri),
            ElaboratorKey::Task(task) => self.task(cx, task),
        }
    }
}
