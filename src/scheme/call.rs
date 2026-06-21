use std::{
    any::{Any, TypeId, type_name},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
};

use crate::scheme::{
    change::LayerChanges,
    context::Context,
    layer::{FallibleLayer, NonTopLayer, TopLayer},
};

pub type LayerCallFuture<'a, L, O> = Pin<Box<dyn Future<Output = CallOutcome<L, O>> + Send + 'a>>;

pub type LayerMethod<L, Args, O> =
    for<'a> fn(&'a mut L, &'a Context, &'a Args) -> LayerCallFuture<'a, L, O>;

/// Opaque runtime-owned plan describing compensating work that should be
/// scheduled before retrying the original layer call.
#[derive(Clone)]
pub struct AwaitPlan {
    pub(crate) target_layer_type: TypeId,
    pub(crate) target_layer_name: &'static str,
    pub(crate) action: Arc<dyn Any + Send + Sync>,
    pub(crate) action_name: &'static str,
    pub(crate) dispatch: crate::scheme::__macro_private::RegisteredDispatchFn,
}

impl AwaitPlan {
    pub fn new<L, Args, O>(method: LayerMethod<L, Args, O>, args: Args) -> Self
    where
        L: FallibleLayer + 'static,
        Args: Send + Sync + 'static,
        O: Send + Sync + 'static,
    {
        Self {
            target_layer_type: TypeId::of::<L>(),
            target_layer_name: type_name::<L>(),
            action: Arc::new(crate::scheme::__macro_private::CallPayload::<L, Args, O> {
                method,
                args,
                _marker: PhantomData,
            }),
            action_name: type_name::<Args>(),
            dispatch: crate::scheme::__macro_private::dispatch_call::<L, Args, O>,
        }
    }
}

pub(crate) struct Continuation {
    pub(crate) effect: ContinuationEffect,
}

pub(crate) enum ContinuationEffect {
    Propagate {
        payload: Box<dyn Any + Send + Sync>,
        completion: PropagationCompletion,
    },
    Await(AwaitPlan),
}

pub(crate) enum PropagationCompletion {
    Retry,
    Resolve(Box<dyn Any + Send + Sync>),
}

impl Continuation {
    fn propagate<Payload>(payload: Payload) -> Self
    where
        Payload: Send + Sync + 'static,
    {
        Self {
            effect: ContinuationEffect::Propagate {
                payload: Box::new(payload),
                completion: PropagationCompletion::Retry,
            },
        }
    }

    fn propagate_resolved<Payload, Output>(payload: Payload, output: Output) -> Self
    where
        Payload: Send + Sync + 'static,
        Output: Send + Sync + 'static,
    {
        Self {
            effect: ContinuationEffect::Propagate {
                payload: Box::new(payload),
                completion: PropagationCompletion::Resolve(Box::new(output)),
            },
        }
    }

    fn await_plan(plan: AwaitPlan) -> Self {
        Self {
            effect: ContinuationEffect::Await(plan),
        }
    }

    fn await_call<Target, Args, O>(method: LayerMethod<Target, Args, O>, args: Args) -> Self
    where
        Target: FallibleLayer + 'static,
        Args: Send + Sync + 'static,
        O: Send + Sync + 'static,
    {
        Self::await_plan(AwaitPlan::new::<Target, Args, O>(method, args))
    }
}

pub struct CallOutcome<L: FallibleLayer, O>(pub(crate) CallOutcomeKind<L, O>);

pub(crate) enum CallOutcomeKind<L: FallibleLayer, O> {
    Resolved(O),
    Continue(Continuation),
    Failed(L::__Error),
}

impl<L: FallibleLayer, O> CallOutcome<L, O> {
    pub fn ok(value: O) -> Self {
        Self(CallOutcomeKind::Resolved(value))
    }

    pub fn fail(err: L::__Error) -> Self {
        Self(CallOutcomeKind::Failed(err))
    }
}

impl<L: NonTopLayer, O> CallOutcome<L, O> {
    pub fn update(changes: LayerChanges<L>) -> Self {
        Self(CallOutcomeKind::Continue(Continuation::propagate(changes)))
    }

    pub fn expect<Target, Args, Awaited>(
        method: LayerMethod<Target, Args, Awaited>,
        args: Args,
    ) -> Self
    where
        Target: FallibleLayer + 'static,
        Args: Send + Sync + 'static,
        Awaited: Send + Sync + 'static,
    {
        Self(CallOutcomeKind::Continue(Continuation::await_call::<
            Target,
            Args,
            Awaited,
        >(method, args)))
    }
}

impl<L: TopLayer> CallOutcome<L, ()> {
    pub fn emit(changes: LayerChanges<L::Lower>) -> Self {
        Self(CallOutcomeKind::Continue(Continuation::propagate_resolved(
            changes,
            (),
        )))
    }
}
