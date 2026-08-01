//! Task identity and task-published views.

use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    sync::Arc,
};

use crate::{
    component::{parse::AstKey, scope::ScopeId},
    scheme::node::View,
};

use super::{Elaboration, ElaboratorDiagnostic, ElaboratorRole};

pub struct ElaboratorTask<R: ElaboratorRole> {
    pub ast: AstKey,
    pub incoming: ScopeId<R::Domain>,
    pub input: R::Input,
}

impl<R: ElaboratorRole> ElaboratorTask<R> {
    pub fn new(ast: AstKey, incoming: ScopeId<R::Domain>, input: R::Input) -> Self {
        Self {
            ast,
            incoming,
            input,
        }
    }
}

impl<R: ElaboratorRole> Clone for ElaboratorTask<R> {
    fn clone(&self) -> Self {
        Self::new(self.ast.clone(), self.incoming, self.input.clone())
    }
}
impl<R: ElaboratorRole> PartialEq for ElaboratorTask<R> {
    fn eq(&self, other: &Self) -> bool {
        self.ast == other.ast && self.incoming == other.incoming && self.input == other.input
    }
}
impl<R: ElaboratorRole> Eq for ElaboratorTask<R> {}
impl<R: ElaboratorRole> Hash for ElaboratorTask<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.ast.hash(state);
        self.incoming.hash(state);
        self.input.hash(state);
    }
}
impl<R: ElaboratorRole> fmt::Debug for ElaboratorTask<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ElaboratorTask")
            .field("ast", &self.ast)
            .field("incoming", &self.incoming)
            .field("input", &self.input)
            .finish()
    }
}

pub struct ElaboratorOutput<R: ElaboratorRole>(PhantomData<fn() -> R>);
impl<R: ElaboratorRole> View for ElaboratorOutput<R> {
    type Key = ElaboratorTask<R>;
    type Value = Elaboration<R::Output>;
}

pub struct ElaboratorDiagnostics<R: ElaboratorRole>(PhantomData<fn() -> R>);
impl<R: ElaboratorRole> View for ElaboratorDiagnostics<R> {
    type Key = ElaboratorTask<R>;
    type Value = Arc<[ElaboratorDiagnostic<R::Diagnostic>]>;
}

pub struct Child<R: ElaboratorRole> {
    pub(crate) task: ElaboratorTask<R>,
}
impl<R: ElaboratorRole> Clone for Child<R> {
    fn clone(&self) -> Self {
        Self {
            task: self.task.clone(),
        }
    }
}
impl<R: ElaboratorRole> Child<R> {
    pub fn task(&self) -> &ElaboratorTask<R> {
        &self.task
    }
}
