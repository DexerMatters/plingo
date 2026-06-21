use std::any::Any;

use crate::scheme::{
    call::{CallOutcome, CallOutcomeKind, Continuation},
    context::Context,
    layer::FallibleLayer,
};

pub type DispatchFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = RegisteredDispatchOutcome> + Send + 'a>>;

pub type RegisteredDispatchFn = for<'a> fn(
    &'a mut (dyn Any + Send + Sync),
    &'a Context,
    &'a (dyn Any + Send + Sync),
) -> DispatchFuture<'a>;

pub struct CallPayload<L: FallibleLayer, Args, O> {
    pub method: crate::scheme::call::LayerMethod<L, Args, O>,
    pub args: Args,
    pub _marker: std::marker::PhantomData<fn() -> (L, O)>,
}

pub fn dispatch_call<'a, L, Args, O>(
    layer: &'a mut (dyn Any + Send + Sync),
    ctx: &'a Context,
    payload: &'a (dyn Any + Send + Sync),
) -> DispatchFuture<'a>
where
    L: FallibleLayer + 'static,
    Args: Send + Sync + 'static,
    O: Send + Sync + 'static,
{
    let Some(layer) = layer.downcast_mut::<L>() else {
        unreachable!(
            "layer call type mismatch: layer={}, args={}",
            std::any::type_name::<L>(),
            std::any::type_name::<Args>(),
        );
    };
    let Some(payload) = payload.downcast_ref::<CallPayload<L, Args, O>>() else {
        unreachable!(
            "layer call registration type mismatch: layer={}, args={}",
            std::any::type_name::<L>(),
            std::any::type_name::<Args>(),
        );
    };
    Box::pin(async move {
        into_registered_call_outcome((payload.method)(layer, ctx, &payload.args).await)
    })
}

pub struct RegisteredDispatchOutcome(pub(super) RegisteredDispatchOutcomeKind);

pub(super) enum RegisteredDispatchOutcomeKind {
    Resolved(Box<dyn Any + Send + Sync>),
    Continue(Continuation),
    Failed(String),
}

pub fn into_registered_call_outcome<L, O>(outcome: CallOutcome<L, O>) -> RegisteredDispatchOutcome
where
    L: FallibleLayer,
    O: Send + Sync + 'static,
{
    match outcome.0 {
        CallOutcomeKind::Resolved(value) => {
            RegisteredDispatchOutcome(RegisteredDispatchOutcomeKind::Resolved(Box::new(value)))
        }
        CallOutcomeKind::Continue(continuation) => {
            RegisteredDispatchOutcome(RegisteredDispatchOutcomeKind::Continue(continuation))
        }
        CallOutcomeKind::Failed(err) => {
            RegisteredDispatchOutcome(RegisteredDispatchOutcomeKind::Failed(err.to_string()))
        }
    }
}
