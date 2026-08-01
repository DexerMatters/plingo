//! Elaborator role contract and installation registry.

use std::{fmt, marker::PhantomData};

use crate::{
    component::{
        parse::ParserNode,
        scope::{ScopeCatalogNode, ScopeDomain},
    },
    scheme::node::{Graph, NodeError, NodeKey, NodeValue},
};

use super::{ElaboratorCx, ElaboratorError, ElaboratorKey, ElaboratorNode};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeAccess {
    Build,
    Extend,
    Query,
}

pub trait ElaboratorRole: Send + Sync + 'static {
    type Domain: ScopeDomain;
    type Input: NodeKey + fmt::Debug + Default;
    type Output: NodeValue + fmt::Debug;
    type Diagnostic: NodeKey + fmt::Debug;
    const SCOPE_ACCESS: ScopeAccess = ScopeAccess::Build;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Elaboration<T> {
    Complete(T),
    Awaiting,
}
impl<T> Elaboration<T> {
    pub const fn complete(value: T) -> Self {
        Self::Complete(value)
    }
    pub const fn awaiting() -> Self {
        Self::Awaiting
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NoDiagnostic;

pub struct Elaborators<D: ScopeDomain> {
    installers: Vec<Box<dyn FnOnce(&mut Graph) -> Result<(), NodeError> + Send>>,
    _domain: PhantomData<fn() -> D>,
}
impl<D: ScopeDomain> Default for Elaborators<D> {
    fn default() -> Self {
        Self::new()
    }
}
impl<D: ScopeDomain> Elaborators<D> {
    pub fn new() -> Self {
        Self {
            installers: vec![],
            _domain: PhantomData,
        }
    }
    pub fn rule<R, F>(mut self, rules: F) -> Self
    where
        R: ElaboratorRole<Domain = D>,
        F: for<'a, 't, 'n> Fn(
                &mut ElaboratorCx<'a, 't, 'n, R>,
            )
                -> Result<Elaboration<R::Output>, ElaboratorError<R::Diagnostic>>
            + Send
            + Sync
            + 'static,
    {
        self.installers.push(Box::new(move |graph| {
            graph.install(ElaboratorNode::<R>::new(rules))?;
            graph.connect::<ParserNode<D::Root, D::Ast>, ElaboratorNode<R>>(|key| {
                ElaboratorKey::AttachedRoot(key)
            })?;
            Ok(())
        }));
        self
    }
    pub fn install(self, graph: &mut Graph) -> Result<(), NodeError> {
        graph.install(ScopeCatalogNode::<D>::new())?;
        for install in self.installers {
            install(graph)?;
        }
        Ok(())
    }
}
pub fn elaborators<D: ScopeDomain>() -> Elaborators<D> {
    Elaborators::new()
}
