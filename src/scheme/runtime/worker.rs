use std::{
    any::{TypeId, type_name},
    collections::VecDeque,
};

use tokio::sync::mpsc;

use crate::scheme::{
    call::Continuation,
    change::LayerChanges,
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
        layer.initialize_snapshots();
        enum TopEvent<TLower: NonTopLayer, TError> {
            Emit(Result<Option<LayerChanges<TLower>>, TError>),
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

            let event: TopEvent<T::Lower, T::Error> = {
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
                    if let Err(err) = emitted.validate() {
                        layer.rollback_transaction(emitted.revision);
                        eprintln!("invalid top-layer transaction from {layer_name}: {err}");
                        continue;
                    }
                    let revision = emitted.revision;
                    if let Err(err) = forward_delta_down(
                        layer_type,
                        layer_name,
                        &context,
                        DeltaEnvelope::new(Box::new(emitted)),
                    )
                    .await
                    {
                        layer.rollback_transaction(revision);
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
                        let revision = envelope.payload.revision();
                        if let Err(err) =
                            forward_delta_down(layer_type, layer_name, context, envelope).await
                        {
                            layer.rollback_transaction(revision);
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

fn downcast_layer_changes<L>(
    layer_name: &'static str,
    payload: Box<dyn crate::scheme::runtime::message::ErasedChanges>,
) -> LayerChanges<L>
where
    L: NonTopLayer,
{
    payload
        .into_any()
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
        layer.initialize_snapshots();
        let mut revision = 0;
        let mut queued = VecDeque::new();

        loop {
            let message = match queued.pop_front() {
                Some(message) => Some(message),
                None => receiver.recv().await,
            };
            let Some(message) = message else {
                break;
            };

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
                                    layer_type,
                                    layer_name,
                                    &context,
                                    &mut layer,
                                    &mut revision,
                                    &mut receiver,
                                    &mut queued,
                                    envelope.payload,
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
                    let completion = delta_box.completion;
                    if let Err(err) = apply_middle_delta::<M>(
                        layer_type,
                        layer_name,
                        &context,
                        &mut layer,
                        &mut revision,
                        &mut receiver,
                        &mut queued,
                        delta_box.payload,
                    )
                    .await
                    {
                        eprintln!("{err}");
                        let _ = completion.map(|completion| completion.send(Err(err)));
                    } else if let Some(completion) = completion {
                        let _ = completion.send(Ok(()));
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
    current_revision: &mut u64,
    receiver: &mut mpsc::Receiver<WorkerMessage>,
    queued: &mut VecDeque<WorkerMessage>,
    payload: Box<dyn crate::scheme::runtime::message::ErasedChanges>,
) -> Result<(), DeltaFlowError>
where
    M: MiddleLayer,
{
    let input_revision = payload.revision();
    if input_revision.base != *current_revision {
        return Err(DeltaFlowError::RevisionMismatch {
            layer: layer_name.to_string(),
            expected: *current_revision,
            base: input_revision.base,
            target: input_revision.target,
        });
    }
    let typed = downcast_layer_changes::<M>(layer_name, payload);
    typed
        .validate()
        .map_err(|err| DeltaFlowError::InvalidTransaction {
            layer: layer_name.to_string(),
            reason: err.to_string(),
        })?;
    let delta_ctx = context
        // A pass prepares target state from its committed base; target is not visible yet.
        .with_snapshot(Some(input_revision.base))
        .with_current_layer(layer_type);

    let out = match layer.pass(&delta_ctx, typed).await {
        Ok(out) => out,
        Err(err) => {
            layer.rollback_transaction(input_revision);
            return Err(DeltaFlowError::MiddlePassFailed {
                layer: layer_name.to_string(),
                reason: err.to_string(),
            });
        }
    };
    if out.revision != input_revision {
        layer.rollback_transaction(input_revision);
        return Err(DeltaFlowError::RevisionChanged {
            layer: layer_name.to_string(),
        });
    }
    if let Err(err) = out.validate() {
        layer.rollback_transaction(input_revision);
        return Err(DeltaFlowError::InvalidTransaction {
            layer: layer_name.to_string(),
            reason: err.to_string(),
        });
    }
    let lower_type = match context.registry.lower_by_upper.get(&layer_type).copied() {
        Some(lower_type) => lower_type,
        None => {
            layer.rollback_transaction(input_revision);
            return Err(DeltaFlowError::MissingLowerSender {
                layer: layer_name.to_string(),
            });
        }
    };

    if let Err(err) = forward_middle_delta(
        lower_type,
        layer_name,
        context,
        layer_type,
        layer,
        receiver,
        queued,
        DeltaEnvelope::new(Box::new(out)),
    )
    .await
    {
        layer.rollback_transaction(input_revision);
        return Err(err);
    }

    let commit_ctx = context
        .with_snapshot(Some(input_revision.target))
        .with_current_layer(layer_type);
    layer.commit_transaction(&commit_ctx, input_revision);
    *current_revision = input_revision.target;

    Ok(())
}

async fn forward_middle_delta<M>(
    lower_type: TypeId,
    layer_name: &'static str,
    context: &Context,
    layer_type: TypeId,
    layer: &mut M,
    receiver: &mut mpsc::Receiver<WorkerMessage>,
    queued: &mut VecDeque<WorkerMessage>,
    delta: DeltaEnvelope,
) -> Result<(), DeltaFlowError>
where
    M: MiddleLayer,
{
    let forward = forward_delta_down_to(lower_type, layer_name, context, delta);
    tokio::pin!(forward);
    let mut receiver_open = true;

    loop {
        tokio::select! {
            result = &mut forward => return result,
            message = receiver.recv(), if receiver_open => match message {
                Some(WorkerMessage::Demand(demand)) if demand.read_only => {
                    serve_read_only_demand(layer_type, context, layer, demand).await;
                }
                Some(message) => queued.push_back(message),
                None => receiver_open = false,
            },
        }
    }
}

async fn serve_read_only_demand<M>(
    layer_type: TypeId,
    context: &Context,
    layer: &mut M,
    demand: Demand,
) where
    M: MiddleLayer,
{
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

    let action = demand.action_name.to_string();
    let result = match outcome.inner {
        ErasedOutcomeKind::Resolved(output) => Ok(output),
        ErasedOutcomeKind::Failed(error) => Err(error),
        ErasedOutcomeKind::Continue(_) => Err(ActionError::ReadOnlyActionContinued {
            action,
            layer: M::display(),
        }),
    };
    let _ = demand.response_tx.send(result);
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
        let mut revision = 0;
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
                                let incoming = envelope.payload.revision();
                                if incoming.base != revision {
                                    eprintln!(
                                        "{}",
                                        DeltaFlowError::RevisionMismatch {
                                            layer: layer_name.to_string(),
                                            expected: revision,
                                            base: incoming.base,
                                            target: incoming.target,
                                        }
                                    );
                                    continue;
                                }
                                let typed =
                                    downcast_layer_changes::<B>(layer_name, envelope.payload);
                                if let Err(err) = typed.validate() {
                                    eprintln!(
                                        "{}",
                                        DeltaFlowError::InvalidTransaction {
                                            layer: layer_name.to_string(),
                                            reason: err.to_string(),
                                        }
                                    );
                                    continue;
                                }
                                let delta_ctx = context
                                    .with_snapshot(Some(incoming.target))
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
                                revision = incoming.target;
                                complete_propagated_demand(&context, demand, completion).await;
                            }
                        },
                    }
                }
                WorkerMessage::Delta(delta_box) => {
                    let DeltaEnvelope {
                        payload,
                        completion,
                    } = delta_box;
                    let incoming = payload.revision();
                    if incoming.base != revision {
                        let err = DeltaFlowError::RevisionMismatch {
                            layer: layer_name.to_string(),
                            expected: revision,
                            base: incoming.base,
                            target: incoming.target,
                        };
                        eprintln!("{err}");
                        let _ = completion.map(|completion| completion.send(Err(err)));
                        continue;
                    }
                    let typed = downcast_layer_changes::<B>(layer_name, payload);
                    if let Err(err) = typed.validate() {
                        let err = DeltaFlowError::InvalidTransaction {
                            layer: layer_name.to_string(),
                            reason: err.to_string(),
                        };
                        eprintln!("{err}");
                        let _ = completion.map(|completion| completion.send(Err(err)));
                        continue;
                    }
                    let delta_ctx = context
                        .with_snapshot(Some(incoming.target))
                        .with_current_layer(TypeId::of::<B>());
                    if let Err(err) = layer.consume(&delta_ctx, typed).await {
                        let err = DeltaFlowError::BottomConsumeFailed {
                            layer: layer_name.to_string(),
                            reason: err.to_string(),
                        };
                        eprintln!("{err}");
                        let _ = completion.map(|completion| completion.send(Err(err)));
                    } else {
                        revision = incoming.target;
                        if let Some(completion) = completion {
                            let _ = completion.send(Ok(()));
                        }
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
            if demand.read_only {
                Err(ActionError::ReadOnlyActionContinued {
                    action: demand.action_name.to_string(),
                    layer: L::display(),
                })
            } else {
                return PostDemand::Continue {
                    continuation,
                    demand,
                };
            }
        }
        ErasedOutcomeKind::Failed(err) => Err(err),
    };

    let _ = demand.response_tx.send(result);
    PostDemand::Done
}
