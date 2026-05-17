use super::*;
use std::any::{Any, TypeId};

pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = RegisteredDispatchOutcome> + Send + 'a>>;

pub struct ResolveActionEntry {
    layer_type: TypeId,
    action_type: TypeId,
    dispatch: for<'a> fn(
        &'a (dyn Any + Send + Sync),
        &'a Context,
        &'a (dyn Any + Send + Sync),
    ) -> DispatchFuture<'a>,
}

impl ResolveActionEntry {
    pub const fn new(
        layer_type: TypeId,
        action_type: TypeId,
        dispatch: for<'a> fn(
            &'a (dyn Any + Send + Sync),
            &'a Context,
            &'a (dyn Any + Send + Sync),
        ) -> DispatchFuture<'a>,
    ) -> Self {
        Self {
            layer_type,
            action_type,
            dispatch,
        }
    }

    pub(super) fn matches(&self, layer_type: TypeId, action_type: TypeId) -> bool {
        self.layer_type == layer_type && self.action_type == action_type
    }

    pub(super) fn call<'a>(
        &self,
        layer: &'a (dyn Any + Send + Sync),
        ctx: &'a Context,
        action: &'a (dyn Any + Send + Sync),
    ) -> DispatchFuture<'a> {
        (self.dispatch)(layer, ctx, action)
    }
}

pub struct RegisteredDispatchOutcome(pub(super) RegisteredDispatchOutcomeKind);

pub(super) enum RegisteredDispatchOutcomeKind {
    Resolved(Box<dyn Any + Send + Sync>),
    Continue(Continuation),
    Failed(String),
}

pub fn into_registered_dispatch_outcome<G, L>(outcome: Outcome<G, L>) -> RegisteredDispatchOutcome
where
    L: FallibleLayer + Receiver<G>,
{
    match outcome.0 {
        OutcomeKind::Resolved(value) => {
            RegisteredDispatchOutcome(RegisteredDispatchOutcomeKind::Resolved(Box::new(value)))
        }
        OutcomeKind::Continue(continuation) => {
            RegisteredDispatchOutcome(RegisteredDispatchOutcomeKind::Continue(continuation))
        }
        OutcomeKind::Failed(err) => {
            RegisteredDispatchOutcome(RegisteredDispatchOutcomeKind::Failed(err.to_string()))
        }
    }
}
