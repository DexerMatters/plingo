use std::{any::Any, marker::PhantomData};

use crate::scheme::{
    __macro_private, call::CallOutcomeKind, context::Context, error::ActionError,
    layer::FallibleLayer, runtime::message::ErasedOutput,
};

pub(super) struct ErasedOutcome<L: FallibleLayer> {
    pub(super) inner: ErasedOutcomeKind,
    _marker: PhantomData<fn() -> L>,
}

pub(super) enum ErasedOutcomeKind {
    Resolved(ErasedOutput),
    Continue(crate::scheme::call::Continuation),
    Failed(ActionError),
}

pub(super) fn registered_outcome_to_erased<L: FallibleLayer>(
    action_name: &'static str,
    outcome: __macro_private::RegisteredDispatchOutcome,
) -> ErasedOutcome<L> {
    match outcome.0 {
        __macro_private::RegisteredDispatchOutcomeKind::Resolved(value) => ErasedOutcome {
            inner: ErasedOutcomeKind::Resolved(ErasedOutput { value }),
            _marker: PhantomData,
        },
        __macro_private::RegisteredDispatchOutcomeKind::Continue(continuation) => ErasedOutcome {
            inner: ErasedOutcomeKind::Continue(continuation),
            _marker: PhantomData,
        },
        __macro_private::RegisteredDispatchOutcomeKind::Failed(reason) => ErasedOutcome {
            inner: ErasedOutcomeKind::Failed(ActionError::ErrorFromLayer {
                action: action_name.to_string(),
                layer: L::display(),
                reason,
            }),
            _marker: PhantomData,
        },
    }
}

pub(super) async fn dispatch_registered_action<L>(
    layer: &mut L,
    ctx: &Context,
    action_name: &'static str,
    action: &(dyn Any + Send + Sync),
    dispatch: __macro_private::RegisteredDispatchFn,
) -> ErasedOutcome<L>
where
    L: FallibleLayer,
{
    let layer_any: &mut (dyn Any + Send + Sync) = layer;
    let out = dispatch(layer_any, ctx, action).await;
    registered_outcome_to_erased::<L>(action_name, out)
}

#[allow(dead_code)]
fn _assert_call_outcome_kind_visibility<L, O>(_outcome: CallOutcomeKind<L, O>)
where
    L: FallibleLayer,
{
}
