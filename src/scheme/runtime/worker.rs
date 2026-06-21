use std::any::{TypeId, type_name};

use tokio::sync::mpsc;

use crate::scheme::{
    call::Continuation,
    change::{EmittedChanges, LayerChanges},
    context::Context,
    error::{ActionError, DeltaFlowError},
    layer::{BottomLayer, FallibleLayer, MiddleLayer, NonTopLayer, TopLayer},
    runtime::{
        dispatch::{ErasedOutcome, ErasedOutcomeKind, dispatch_registered_action},
        message::{DeltaEnvelope, Demand, WorkerMessage},
        pending::{
            ContinuationTransition, complete_propagated_demand, forward_delta_down,
            forward_delta_down_to, handle_barrier, transition_continuation,
        },
    },
};

enum PostDemand {
    Done,
    Continue {
        continuation: Continuation,
        demand: Demand,
    },
}

pub(crate) fn spawn_top_worker<T>(
    context: Context,
    mut receiver: mpsc::Receiver<WorkerMessage>,
    layer_type: TypeId,
    layer_name: &'static str,
    mut layer: T,
) -> tokio::task::JoinHandle<()>
where
    T: TopLayer,
{
    tokio::spawn(async move {
        enum TopEvent<TLower: NonTopLayer, TError> {
            Emit(Result<Option<EmittedChanges<TLower>>, TError>),
            Message(Option<WorkerMessage>),
        }

        loop {
            let mut channel_closed = false;
            loop {
                match receiver.try_recv() {
                    Ok(message) => {
                        handle_any_message_top::<T>(
                            layer_type, layer_name, &context, &mut layer, message,
                        )
                        .await;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        channel_closed = true;
                        break;
                    }
                }
            }
            if channel_closed {
                break;
            }

            let event = {
                let emit_ctx = context.with_current_layer(layer_type);
                let next_emit = layer.emit(&emit_ctx);
                tokio::pin!(next_emit);
                tokio::select! {
                    biased;
                    msg = receiver.recv() => TopEvent::Message(msg),
                    maybe_delta = &mut next_emit => TopEvent::Emit(maybe_delta),
                }
            };

            match event {
                TopEvent::Emit(Ok(Some(emitted))) => {
                    if let Err(err) = forward_delta_down(
                        layer_type,
                        layer_name,
                        &context,
                        DeltaEnvelope {
                            snapshot: emitted.snapshot,
                            payload: Box::new(emitted.changes),
                        },
                    )
                    .await
                    {
                        eprintln!("{err}");
                    }
                }
                TopEvent::Emit(Err(err)) => {
                    eprintln!(
                        "{}",
                        DeltaFlowError::TopEmitFailed {
                            layer: layer_name.to_string(),
                            reason: err.to_string(),
                        }
                    );
                    break;
                }
                TopEvent::Emit(Ok(None)) => break,
                TopEvent::Message(Some(message)) => {
                    handle_any_message_top::<T>(
                        layer_type, layer_name, &context, &mut layer, message,
                    )
                    .await;
                }
                TopEvent::Message(None) => break,
            }
        }
    })
}

async fn handle_any_message_top<T>(
    layer_type: TypeId,
    layer_name: &'static str,
    context: &Context,
    layer: &mut T,
    message: WorkerMessage,
) where
    T: TopLayer,
{
    match message {
        WorkerMessage::Demand(demand) => {
            let resolve_ctx = context
                .with_snapshot(demand.snapshot)
                .with_current_layer(layer_type)
                .with_call_stack(demand.call_stack.clone());
            let outcome = dispatch_registered_action(
                layer,
                &resolve_ctx,
                demand.action_name,
                demand.action.as_ref(),
                demand.dispatch,
            )
            .await;
            match finalize_demand_outcome(demand, outcome).await {
                PostDemand::Done => {}
                PostDemand::Continue {
                    continuation,
                    demand,
                } => match transition_continuation(context, demand, continuation).await {
                    ContinuationTransition::Done => {}
                    ContinuationTransition::Propagate {
                        envelope,
                        demand,
                        completion,
                    } => {
                        if let Err(err) =
                            forward_delta_down(layer_type, layer_name, context, envelope).await
                        {
                            eprintln!("{err}");
                            let _ = demand.response_tx.send(Err(ActionError::ErrorFromLayer {
                                action: demand.action_name.to_string(),
                                layer: layer_name.to_string(),
                                reason: err.to_string(),
                            }));
                            return;
                        }
                        complete_propagated_demand(context, demand, completion).await;
                    }
                },
            }
        }
        WorkerMessage::Delta(_) => {
            eprintln!(
                "{}",
                DeltaFlowError::UnexpectedTopDelta {
                    layer: layer_name.to_string()
                }
            );
        }
        WorkerMessage::Barrier(_) => {
            eprintln!("top layer {layer_name} received an unexpected barrier");
        }
    }
}

fn downcast_layer_changes<L>(layer_name: &'static str, delta: DeltaEnvelope) -> LayerChanges<L>
where
    L: NonTopLayer,
{
    delta
        .payload
        .downcast::<LayerChanges<L>>()
        .map(|typed| *typed)
        .unwrap_or_else(|_| {
            unreachable!(
                "layer change downcast must match pipeline wiring: layer={}, expected={}",
                layer_name,
                type_name::<LayerChanges<L>>()
            )
        })
}

pub(crate) fn spawn_middle_worker<M>(
    context: Context,
    mut receiver: mpsc::Receiver<WorkerMessage>,
    layer_type: TypeId,
    layer_name: &'static str,
    mut layer: M,
) -> tokio::task::JoinHandle<()>
where
    M: MiddleLayer,
{
    tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            match message {
                WorkerMessage::Demand(demand) => {
                    let resolve_ctx = context
                        .with_snapshot(demand.snapshot)
                        .with_current_layer(layer_type)
                        .with_call_stack(demand.call_stack.clone());
                    let outcome = dispatch_registered_action(
                        &mut layer,
                        &resolve_ctx,
                        demand.action_name,
                        demand.action.as_ref(),
                        demand.dispatch,
                    )
                    .await;
                    match finalize_demand_outcome(demand, outcome).await {
                        PostDemand::Done => {}
                        PostDemand::Continue {
                            continuation,
                            demand,
                        } => match transition_continuation(&context, demand, continuation).await {
                            ContinuationTransition::Done => {}
                            ContinuationTransition::Propagate {
                                envelope,
                                demand,
                                completion,
                            } => {
                                if let Err(err) = apply_middle_delta::<M>(
                                    layer_type, layer_name, &context, &mut layer, envelope,
                                )
                                .await
                                {
                                    eprintln!("{err}");
                                    let _ =
                                        demand.response_tx.send(Err(ActionError::ErrorFromLayer {
                                            action: demand.action_name.to_string(),
                                            layer: layer_name.to_string(),
                                            reason: err.to_string(),
                                        }));
                                    continue;
                                }
                                complete_propagated_demand(&context, demand, completion).await;
                            }
                        },
                    }
                }
                WorkerMessage::Delta(delta_box) => {
                    if let Err(err) = apply_middle_delta::<M>(
                        layer_type, layer_name, &context, &mut layer, delta_box,
                    )
                    .await
                    {
                        eprintln!("{err}");
                    }
                }
                WorkerMessage::Barrier(barrier) => {
                    handle_barrier(layer_type, layer_name, &context, barrier).await;
                }
            }
        }
    })
}

async fn apply_middle_delta<M>(
    layer_type: TypeId,
    layer_name: &'static str,
    context: &Context,
    layer: &mut M,
    delta: DeltaEnvelope,
) -> Result<(), DeltaFlowError>
where
    M: MiddleLayer,
{
    let snapshot = delta.snapshot;
    let typed = downcast_layer_changes::<M>(layer_name, delta);
    let delta_ctx = context
        .with_snapshot(Some(snapshot))
        .with_current_layer(layer_type);

    let out =
        layer
            .pass(&delta_ctx, typed)
            .await
            .map_err(|err| DeltaFlowError::MiddlePassFailed {
                layer: layer_name.to_string(),
                reason: err.to_string(),
            })?;

    let lower_type = context
        .registry
        .lower_by_upper
        .get(&layer_type)
        .copied()
        .ok_or_else(|| DeltaFlowError::MissingLowerSender {
            layer: layer_name.to_string(),
        })?;

    forward_delta_down_to(
        lower_type,
        layer_name,
        context,
        DeltaEnvelope {
            snapshot,
            payload: Box::new(out),
        },
    )
    .await?;

    Ok(())
}

pub(crate) fn spawn_bottom_worker<B>(
    context: Context,
    mut receiver: mpsc::Receiver<WorkerMessage>,
    _layer_type: TypeId,
    layer_name: &'static str,
    mut layer: B,
) -> tokio::task::JoinHandle<()>
where
    B: BottomLayer,
{
    tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            match message {
                WorkerMessage::Demand(demand) => {
                    let resolve_ctx = context
                        .with_snapshot(demand.snapshot)
                        .with_current_layer(TypeId::of::<B>())
                        .with_call_stack(demand.call_stack.clone());
                    let outcome = dispatch_registered_action(
                        &mut layer,
                        &resolve_ctx,
                        demand.action_name,
                        demand.action.as_ref(),
                        demand.dispatch,
                    )
                    .await;
                    match finalize_demand_outcome(demand, outcome).await {
                        PostDemand::Done => {}
                        PostDemand::Continue {
                            continuation,
                            demand,
                        } => match transition_continuation(&context, demand, continuation).await {
                            ContinuationTransition::Done => {}
                            ContinuationTransition::Propagate {
                                envelope,
                                demand,
                                completion,
                            } => {
                                let snapshot = envelope.snapshot;
                                let typed = downcast_layer_changes::<B>(layer_name, envelope);
                                let delta_ctx = context
                                    .with_snapshot(Some(snapshot))
                                    .with_current_layer(TypeId::of::<B>());
                                if let Err(err) = layer.consume(&delta_ctx, typed).await {
                                    eprintln!(
                                        "{}",
                                        DeltaFlowError::BottomConsumeFailed {
                                            layer: layer_name.to_string(),
                                            reason: err.to_string(),
                                        }
                                    );
                                    let _ =
                                        demand.response_tx.send(Err(ActionError::ErrorFromLayer {
                                            action: demand.action_name.to_string(),
                                            layer: layer_name.to_string(),
                                            reason: err.to_string(),
                                        }));
                                    continue;
                                }
                                complete_propagated_demand(&context, demand, completion).await;
                            }
                        },
                    }
                }
                WorkerMessage::Delta(delta_box) => {
                    let snapshot = delta_box.snapshot;
                    let typed = downcast_layer_changes::<B>(layer_name, delta_box);
                    let delta_ctx = context
                        .with_snapshot(Some(snapshot))
                        .with_current_layer(TypeId::of::<B>());
                    if let Err(err) = layer.consume(&delta_ctx, typed).await {
                        eprintln!(
                            "{}",
                            DeltaFlowError::BottomConsumeFailed {
                                layer: layer_name.to_string(),
                                reason: err.to_string(),
                            }
                        );
                    }
                }
                WorkerMessage::Barrier(barrier) => {
                    handle_barrier(TypeId::of::<B>(), layer_name, &context, barrier).await;
                }
            }
        }
    })
}

async fn finalize_demand_outcome<L>(demand: Demand, outcome: ErasedOutcome<L>) -> PostDemand
where
    L: FallibleLayer,
{
    let result = match outcome.inner {
        ErasedOutcomeKind::Resolved(output) => Ok(output),
        ErasedOutcomeKind::Continue(continuation) => {
            return PostDemand::Continue {
                continuation,
                demand,
            };
        }
        ErasedOutcomeKind::Failed(err) => Err(err),
    };

    let _ = demand.response_tx.send(result);
    PostDemand::Done
}
