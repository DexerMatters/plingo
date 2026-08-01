//! The rule-set builder: a root handler for documents, typed case handlers
//! for AST nodes, and a fallback, compiled into one elaborator closure.

use std::{marker::PhantomData, sync::Arc};

use crate::component::{parse::AstArtifact, scope::ScopeDomain};

use super::{Elaboration, ElaboratorCx, ElaboratorError, ElaboratorRole};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleBuildError {
    #[error("rule builder has no root handler")]
    MissingRoot,
    #[error("rule builder has no fallback handler")]
    MissingFallback,
    #[error("rule builder registered the root handler more than once")]
    DuplicateRoot,
}

pub type RuleResult<R> =
    Result<<R as ElaboratorRole>::Output, ElaboratorError<<R as ElaboratorRole>::Diagnostic>>;
type RootHandler<R> = Box<
    dyn for<'a, 't, 'n> Fn(
            &mut ElaboratorCx<'a, 't, 'n, R>,
            Arc<<<R as ElaboratorRole>::Domain as ScopeDomain>::Ast>,
        ) -> RuleResult<R>
        + Send
        + Sync,
>;
type FallbackHandler<R> =
    Box<dyn for<'a, 't, 'n> Fn(&mut ElaboratorCx<'a, 't, 'n, R>) -> RuleResult<R> + Send + Sync>;
enum CaseResult<R: ElaboratorRole> {
    NoMatch,
    Matched(RuleResult<R>),
}
type CaseHandler<R> = Box<
    dyn for<'a, 't, 'n> Fn(&mut ElaboratorCx<'a, 't, 'n, R>, &AstArtifact) -> CaseResult<R>
        + Send
        + Sync,
>;

pub struct RuleSet<R: ElaboratorRole> {
    root: Option<RootHandler<R>>,
    root_count: usize,
    cases: Vec<CaseHandler<R>>,
    fallback: Option<FallbackHandler<R>>,
    _role: PhantomData<fn() -> R>,
}
pub fn rules<R: ElaboratorRole>() -> RuleSet<R> {
    RuleSet {
        root: None,
        root_count: 0,
        cases: vec![],
        fallback: None,
        _role: PhantomData,
    }
}
impl<R: ElaboratorRole> RuleSet<R> {
    pub fn root<F>(mut self, handler: F) -> Self
    where
        F: for<'a, 't, 'n> Fn(
                &mut ElaboratorCx<'a, 't, 'n, R>,
                Arc<<R::Domain as ScopeDomain>::Ast>,
            ) -> RuleResult<R>
            + Send
            + Sync
            + 'static,
    {
        self.root_count += 1;
        self.root = Some(Box::new(handler));
        self
    }
    pub fn case<T, F>(mut self, handler: F) -> Self
    where
        T: Send + Sync + 'static,
        F: for<'a, 't, 'n> Fn(&mut ElaboratorCx<'a, 't, 'n, R>, Arc<T>) -> RuleResult<R>
            + Send
            + Sync
            + 'static,
    {
        self.cases
            .push(Box::new(move |cx, artifact| match artifact.deref::<T>() {
                Some(v) => CaseResult::Matched(handler(cx, v)),
                None => CaseResult::NoMatch,
            }));
        self
    }
    pub fn otherwise<F>(mut self, handler: F) -> Self
    where
        F: for<'a, 't, 'n> Fn(&mut ElaboratorCx<'a, 't, 'n, R>) -> RuleResult<R>
            + Send
            + Sync
            + 'static,
    {
        self.fallback = Some(Box::new(handler));
        self
    }
    pub fn build(
        self,
    ) -> Result<
        impl for<'a, 't, 'n> Fn(
            &mut ElaboratorCx<'a, 't, 'n, R>,
        )
            -> Result<Elaboration<R::Output>, ElaboratorError<R::Diagnostic>>
        + Send
        + Sync
        + 'static,
        RuleBuildError,
    > {
        if self.root_count == 0 {
            return Err(RuleBuildError::MissingRoot);
        }
        if self.root_count > 1 {
            return Err(RuleBuildError::DuplicateRoot);
        }
        if self.fallback.is_none() {
            return Err(RuleBuildError::MissingFallback);
        }
        let root = self.root.expect("root count");
        let fallback = self.fallback.expect("fallback");
        let cases = self.cases;
        Ok(move |cx: &mut ElaboratorCx<'_, '_, '_, R>| {
            let result = if cx.is_root() {
                let ast = cx.choose_unique_interpretation()?;
                (root)(cx, ast)
            } else {
                let artifact = cx.current_artifact()?;
                let mut matched = None;
                for case in &cases {
                    match case(cx, &artifact) {
                        CaseResult::NoMatch => {}
                        CaseResult::Matched(v) => {
                            matched = Some(v);
                            break;
                        }
                    }
                }
                match matched {
                    Some(v) => v,
                    None => (fallback)(cx),
                }
            };
            result.map(Elaboration::Complete)
        })
    }
}
