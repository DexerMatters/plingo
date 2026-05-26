use super::*;
use std::any::Any;

pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = RegisteredDispatchOutcome> + Send + 'a>>;

pub type RegisteredDispatchFn = for<'a> fn(
    &'a mut (dyn Any + Send + Sync),
    &'a Context,
    &'a (dyn Any + Send + Sync),
) -> DispatchFuture<'a>;

pub fn dispatch_resolve<'a, L, G>(
    layer: &'a mut (dyn Any + Send + Sync),
    ctx: &'a Context,
    action: &'a (dyn Any + Send + Sync),
) -> DispatchFuture<'a>
where
    L: Resolve<G> + 'static,
    G: Send + Sync + 'static,
{
    let Some(layer) = layer.downcast_mut::<L>() else {
        unreachable!(
            "resolve action layer type mismatch: layer={}, action={}",
            std::any::type_name::<L>(),
            std::any::type_name::<G>(),
        );
    };
    let Some(action) = action.downcast_ref::<G>() else {
        unreachable!(
            "resolve action registration type mismatch: layer={}, action={}",
            std::any::type_name::<L>(),
            std::any::type_name::<G>(),
        );
    };
    Box::pin(async move { into_registered_dispatch_outcome(layer.resolve(ctx, action).await) })
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
