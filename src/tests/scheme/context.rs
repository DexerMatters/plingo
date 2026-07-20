use std::{any::TypeId, convert::Infallible, sync::Arc};

use tokio::sync::mpsc;

use crate::scheme::{
    call::LayerCallFuture, context::Context, error::ActionError, layer::FallibleLayer,
    runtime::LayerRegistry,
};

struct CycleA;
struct CycleB;

impl FallibleLayer for CycleA {
    type __Error = Infallible;
}

impl FallibleLayer for CycleB {
    type __Error = Infallible;
}

fn stub_method<'a>(
    _layer: &'a mut CycleB,
    _ctx: &'a Context,
    _args: &'a (),
) -> LayerCallFuture<'a, CycleB, ()> {
    Box::pin(async { unreachable!("cycle detection should fire before dispatch") })
}

#[tokio::test]
async fn context_call_rejects_recursive_layer_cycles() {
    let (tx, _rx) = mpsc::channel(1);
    let mut registry = LayerRegistry::default();
    registry.senders.insert(TypeId::of::<CycleB>(), tx);
    registry
        .layer_names
        .insert(TypeId::of::<CycleA>(), "CycleA");
    registry
        .layer_names
        .insert(TypeId::of::<CycleB>(), "CycleB");

    let ctx = Context {
        registry: Arc::new(registry),
        snapshot: None,
        current_layer_type: None,
        call_stack: Vec::new(),
    };

    let ctx = ctx
        .with_current_layer(TypeId::of::<CycleA>())
        .with_call_stack(vec![TypeId::of::<CycleB>()]);

    let err = ctx.call(stub_method, ()).await.unwrap_err();
    assert!(matches!(err, ActionError::LayerCallCycle { .. }));
}

#[test]
fn last_snapshot_uses_transaction_parent_not_numeric_predecessor() {
    let ctx = Context::default();
    let target = ctx.allocate_snapshot(7);
    assert_eq!(
        ctx.with_snapshot(Some(target)).last_snapshot().snapshot(),
        Some(7)
    );
}
