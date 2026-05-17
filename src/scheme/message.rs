use std::{
    any::{Any, TypeId, type_name},
    sync::Arc,
};

use tokio::sync::{mpsc, oneshot};

use crate::scheme::*;

pub(super) const DEFAULT_DEMAND_RETRY_BUDGET: u8 = 8;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

pub(super) enum WorkerMessage {
    Demand(Demand),
    Delta(Box<dyn Any + Send + Sync>),
    Barrier(AwaitBarrier),
}

pub(super) struct Demand {
    pub action: Arc<dyn Any + Send + Sync>,
    pub action_name: &'static str,
    pub origin_layer_type: TypeId,
    pub remaining_retries: u8,
    pub response_tx: oneshot::Sender<Result<ErasedOutput, ActionError>>,
}

pub(crate) struct ErasedOutput {
    pub value: Box<dyn Any + Send + Sync>,
}

pub(super) struct AwaitBarrier {
    destination_layer_type: TypeId,
    response_tx: oneshot::Sender<()>,
}

type WorkerStarter =
    Box<dyn FnOnce(Context, mpsc::Receiver<WorkerMessage>) -> tokio::task::JoinHandle<()> + Send>;

pub(super) struct LayerSpec {
    pub start_worker: WorkerStarter,
}

enum PostDemand {
    Done,
    Continue {
        continuation: Continuation,
        demand: Demand,
    },
}

enum ContinuationTransition {
    Done,
    Propagate {
        payload: Box<dyn Any + Send + Sync>,
        demand: Demand,
    },
}

// ---------------------------------------------------------------------------
// Top-layer worker
// ---------------------------------------------------------------------------

pub(super) fn spawn_top_worker<T>(
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
        loop {
            tokio::select! {
                maybe_delta = layer.emit(&context) => {
                    match maybe_delta {
                        Ok(None) => break,
                        Ok(Some(deltas)) => {
                            if let Err(err) = forward_delta_down(
                                layer_type,
                                layer_name,
                                &context,
                                Box::new(deltas),
                            )
                            .await
                            {
                                eprintln!("{err}");
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "{}",
                                DeltaFlowError::TopEmitFailed {
                                    layer: layer_name.to_string(),
                                    reason: err.to_string(),
                                }
                            );
                            break;
                        }
                    }
                }
                msg = receiver.recv() => {
                    let Some(message) = msg else { break };
                    handle_any_message_top::<T>(
                        layer_type,
                        layer_name,
                        &context,
                        &mut layer,
                        message,
                    )
                    .await;
                }
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
            let outcome = super::dispatch_registered_action(
                layer,
                context,
                demand.action_name,
                demand.action.as_ref(),
            )
            .await;
            match finalize_demand_outcome(layer_name, demand, outcome).await {
                PostDemand::Done => {}
                PostDemand::Continue { continuation, demand } => {
                    match transition_continuation(context, demand, continuation).await {
                        ContinuationTransition::Done => {}
                        ContinuationTransition::Propagate { payload, demand } => {
                            if let Err(err) =
                                forward_delta_down(layer_type, layer_name, context, payload).await
                            {
                                eprintln!("{err}");
                                let _ = demand.response_tx.send(Err(ActionError::ErrorFromLayer {
                                    action: demand.action_name.to_string(),
                                    layer: layer_name.to_string(),
                                    reason: err.to_string(),
                                }));
                                return;
                            }
                            retry_demand_at_origin(context, demand).await;
                        }
                    }
                }
            }
        }
        WorkerMessage::Delta(_) => {
            eprintln!(
                "{}",
                DeltaFlowError::UnexpectedTopDelta { layer: layer_name.to_string() }
            );
        }
        WorkerMessage::Barrier(_) => {
            eprintln!("top layer {layer_name} received an unexpected barrier");
        }
    }
}

fn downcast_layer_deltas<L>(
    layer_name: &'static str,
    delta: Box<dyn Any + Send + Sync>,
) -> LayerDeltas<L>
where
    L: NonTopLayer,
{
    delta
        .downcast::<LayerDeltas<L>>()
        .map(|typed| *typed)
        .unwrap_or_else(|_| {
            unreachable!(
                "layer delta downcast must match pipeline wiring: layer={}, expected={}",
                layer_name,
                type_name::<LayerDeltas<L>>()
            )
        })
}

// ---------------------------------------------------------------------------
// Middle-layer worker
// ---------------------------------------------------------------------------

pub(super) fn spawn_middle_worker<M>(
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
                    let outcome = super::dispatch_registered_action(
                        &layer,
                        &context,
                        demand.action_name,
                        demand.action.as_ref(),
                    )
                    .await;
                    match finalize_demand_outcome(layer_name, demand, outcome).await {
                        PostDemand::Done => {}
                        PostDemand::Continue { continuation, demand } => {
                            match transition_continuation(&context, demand, continuation).await {
                                ContinuationTransition::Done => {}
                                ContinuationTransition::Propagate { payload, demand } => {
                                    if let Err(err) = apply_middle_delta::<M>(
                                        layer_type, layer_name, &context, &mut layer, payload,
                                    )
                                    .await
                                    {
                                        eprintln!("{err}");
                                        let _ = demand.response_tx.send(Err(
                                            ActionError::ErrorFromLayer {
                                                action: demand.action_name.to_string(),
                                                layer: layer_name.to_string(),
                                                reason: err.to_string(),
                                            },
                                        ));
                                        continue;
                                    }
                                    retry_demand_at_origin(&context, demand).await;
                                }
                            }
                        }
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
    delta: Box<dyn Any + Send + Sync>,
) -> Result<(), DeltaFlowError>
where
    M: MiddleLayer,
{
    let typed = downcast_layer_deltas::<M>(layer_name, delta);

    let out = layer
        .pass(context, typed)
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

    forward_delta_down_to(lower_type, layer_name, context, Box::new(out)).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Bottom-layer worker
// ---------------------------------------------------------------------------

pub(super) fn spawn_bottom_worker<B>(
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
                    let outcome = super::dispatch_registered_action(
                        &layer,
                        &context,
                        demand.action_name,
                        demand.action.as_ref(),
                    )
                    .await;
                    match finalize_demand_outcome(layer_name, demand, outcome).await {
                        PostDemand::Done => {}
                        PostDemand::Continue { continuation, demand } => {
                            match transition_continuation(&context, demand, continuation).await {
                                ContinuationTransition::Done => {}
                                ContinuationTransition::Propagate { payload, demand } => {
                                    let typed = downcast_layer_deltas::<B>(layer_name, payload);
                                    if let Err(err) = layer.consume(&context, typed).await {
                                        eprintln!(
                                            "{}",
                                            DeltaFlowError::BottomConsumeFailed {
                                                layer: layer_name.to_string(),
                                                reason: err.to_string(),
                                            }
                                        );
                                        let _ = demand.response_tx.send(Err(
                                            ActionError::ErrorFromLayer {
                                                action: demand.action_name.to_string(),
                                                layer: layer_name.to_string(),
                                                reason: err.to_string(),
                                            },
                                        ));
                                        continue;
                                    }
                                    retry_demand_at_origin(&context, demand).await;
                                }
                            }
                        }
                    }
                }
                WorkerMessage::Delta(delta_box) => {
                    let typed = downcast_layer_deltas::<B>(layer_name, delta_box);
                    if let Err(err) = layer.consume(&context, typed).await {
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

// ---------------------------------------------------------------------------
// Shared demand handling
// ---------------------------------------------------------------------------

async fn finalize_demand_outcome<L>(
    _layer_name: &'static str,
    demand: Demand,
    outcome: ErasedOutcome<L>,
) -> PostDemand
where
    L: FallibleLayer,
{
    let result = match outcome.inner {
        ErasedOutcomeKind::Resolved(output) => Ok(output),
        ErasedOutcomeKind::Continue(continuation) => {
            return PostDemand::Continue { continuation, demand };
        }
        ErasedOutcomeKind::Failed(err) => Err(err),
    };

    let _ = demand.response_tx.send(result);
    PostDemand::Done
}

async fn transition_continuation(
    context: &Context,
    demand: Demand,
    continuation: Continuation,
) -> ContinuationTransition {
    match continuation.effect {
        ContinuationEffect::Propagate(payload) => {
            ContinuationTransition::Propagate { payload, demand }
        }
        ContinuationEffect::Await(plan) => {
            let context = context.clone();
            tokio::spawn(async move {
                match execute_await_plan(
                    &context,
                    demand.origin_layer_type,
                    demand.remaining_retries,
                    plan,
                )
                .await
                {
                    Ok(()) => retry_demand_at_origin(&context, demand).await,
                    Err(err) => {
                        let _ = demand.response_tx.send(Err(err));
                    }
                }
            });
            ContinuationTransition::Done
        }
    }
}

// ---------------------------------------------------------------------------
// Demand / delta routing helpers
// ---------------------------------------------------------------------------

async fn execute_await_plan(
    context: &Context,
    origin_layer_type: TypeId,
    remaining_retries: u8,
    plan: AwaitPlan,
) -> Result<(), ActionError> {
    let AwaitPlan {
        target_layer_type,
        target_layer_name,
        action,
        action_name,
    } = plan;
    let target_name = context
        .registry
        .layer_names
        .get(&target_layer_type)
        .copied()
        .unwrap_or(target_layer_name);
    let target_sender = context
        .registry
        .senders
        .get(&target_layer_type)
        .cloned()
        .ok_or_else(|| ActionError::MissingResource {
            action: action_name.to_string(),
            layer: target_name.to_string(),
        })?;

    let (tx, rx) = oneshot::channel();
    let awaited_demand = Demand {
        action,
        action_name,
        origin_layer_type: target_layer_type,
        remaining_retries,
        response_tx: tx,
    };

    target_sender
        .send(WorkerMessage::Demand(awaited_demand))
        .await
        .map_err(|_| ActionError::ChannelClosed {
            action: action_name.to_string(),
            layer: target_name.to_string(),
        })?;

    rx.await.map_err(|_| ActionError::ChannelClosed {
        action: action_name.to_string(),
        layer: target_name.to_string(),
    })??;

    wait_for_downstream_drain(
        context,
        target_layer_type,
        target_name,
        origin_layer_type,
        action_name,
    )
    .await?;

    Ok(())
}

async fn wait_for_downstream_drain(
    context: &Context,
    target_layer_type: TypeId,
    target_layer_name: &'static str,
    origin_layer_type: TypeId,
    action_name: &'static str,
) -> Result<(), ActionError> {
    if target_layer_type == origin_layer_type {
        return Ok(());
    }

    let origin_name = context
        .registry
        .layer_names
        .get(&origin_layer_type)
        .copied()
        .unwrap_or("unknown");

    let mut current = target_layer_type;
    let mut first_hop = None;
    loop {
        let lower_type = context
            .registry
            .lower_by_upper
            .get(&current)
            .copied()
            .ok_or_else(|| ActionError::AwaitPathMissing {
                action: action_name.to_string(),
                target: target_layer_name.to_string(),
                layer: origin_name.to_string(),
            })?;
        first_hop.get_or_insert(lower_type);
        if lower_type == origin_layer_type {
            break;
        }
        current = lower_type;
    }

    let first_hop = first_hop.expect("await drain must have a first hop");
    let sender = context
        .registry
        .senders
        .get(&first_hop)
        .cloned()
        .ok_or_else(|| ActionError::ChannelClosed {
            action: action_name.to_string(),
            layer: origin_name.to_string(),
        })?;

    let (tx, rx) = oneshot::channel();
    sender
        .send(WorkerMessage::Barrier(AwaitBarrier {
            destination_layer_type: origin_layer_type,
            response_tx: tx,
        }))
        .await
        .map_err(|_| ActionError::ChannelClosed {
            action: action_name.to_string(),
            layer: origin_name.to_string(),
        })?;

    rx.await.map_err(|_| ActionError::ChannelClosed {
        action: action_name.to_string(),
        layer: origin_name.to_string(),
    })?;

    Ok(())
}

async fn handle_barrier(
    layer_type: TypeId,
    layer_name: &'static str,
    context: &Context,
    barrier: AwaitBarrier,
) {
    if layer_type == barrier.destination_layer_type {
        let _ = barrier.response_tx.send(());
        return;
    }

    let lower_type = match context.registry.lower_by_upper.get(&layer_type).copied() {
        Some(lower_type) => lower_type,
        None => {
            eprintln!(
                "barrier could not continue downward from layer {layer_name}; await path is incomplete"
            );
            return;
        }
    };

    if let Err(err) = forward_barrier_down_to(lower_type, layer_name, context, barrier).await {
        eprintln!("{err}");
    }
}

async fn forward_delta_down(
    upper_type: TypeId,
    upper_name: &str,
    context: &Context,
    delta: Box<dyn Any + Send + Sync>,
) -> Result<(), DeltaFlowError> {
    let lower_type = context
        .registry
        .lower_by_upper
        .get(&upper_type)
        .copied()
        .ok_or_else(|| DeltaFlowError::MissingLowerSender {
            layer: upper_name.to_string(),
        })?;
    forward_delta_down_to(lower_type, upper_name, context, delta).await
}

async fn forward_delta_down_to(
    lower_type: TypeId,
    upper_name: &str,
    context: &Context,
    delta: Box<dyn Any + Send + Sync>,
) -> Result<(), DeltaFlowError> {
    let lower_name = context
        .registry
        .layer_names
        .get(&lower_type)
        .copied()
        .unwrap_or("unknown");
    let lower_sender = context
        .registry
        .senders
        .get(&lower_type)
        .cloned()
        .ok_or_else(|| DeltaFlowError::MissingLowerSender {
            layer: upper_name.to_string(),
        })?;
    lower_sender
        .send(WorkerMessage::Delta(delta))
        .await
        .map_err(|_| DeltaFlowError::LowerSenderClosed {
            layer: lower_name.to_string(),
        })
}

async fn forward_barrier_down_to(
    lower_type: TypeId,
    upper_name: &str,
    context: &Context,
    barrier: AwaitBarrier,
) -> Result<(), DeltaFlowError> {
    let lower_name = context
        .registry
        .layer_names
        .get(&lower_type)
        .copied()
        .unwrap_or("unknown");
    let lower_sender = context
        .registry
        .senders
        .get(&lower_type)
        .cloned()
        .ok_or_else(|| DeltaFlowError::MissingLowerSender {
            layer: upper_name.to_string(),
        })?;
    lower_sender
        .send(WorkerMessage::Barrier(barrier))
        .await
        .map_err(|_| DeltaFlowError::LowerSenderClosed {
            layer: lower_name.to_string(),
        })
}

async fn retry_demand_at_origin(context: &Context, demand: Demand) {
    if demand.remaining_retries == 0 {
        let origin_name = context
            .registry
            .layer_names
            .get(&demand.origin_layer_type)
            .copied()
            .unwrap_or("unknown");
        let _ = demand.response_tx.send(Err(ActionError::RetryLimitReached {
            action: demand.action_name.to_string(),
            layer: origin_name.to_string(),
        }));
        return;
    }

    let origin_type = demand.origin_layer_type;
    let origin_name = context
        .registry
        .layer_names
        .get(&origin_type)
        .copied()
        .unwrap_or("unknown");

    let remaining_retries = demand.remaining_retries - 1;

    let Some(sender) = context.registry.senders.get(&origin_type).cloned() else {
        let _ = demand.response_tx.send(Err(ActionError::MissingResource {
            action: demand.action_name.to_string(),
            layer: origin_name.to_string(),
        }));
        return;
    };

    let (retry_tx, retry_rx) = oneshot::channel();
    let retry_demand = Demand {
        action: Arc::clone(&demand.action),
        action_name: demand.action_name,
        origin_layer_type: origin_type,
        remaining_retries,
        response_tx: retry_tx,
    };

    if sender
        .send(WorkerMessage::Demand(retry_demand))
        .await
        .is_err()
    {
        let _ = demand.response_tx.send(Err(ActionError::ChannelClosed {
            action: demand.action_name.to_string(),
            layer: origin_name.to_string(),
        }));
        return;
    }

    match retry_rx.await {
        Ok(result) => {
            let _ = demand.response_tx.send(result);
        }
        Err(_) => {
            let _ = demand.response_tx.send(Err(ActionError::ChannelClosed {
                action: demand.action_name.to_string(),
                layer: origin_name.to_string(),
            }));
        }
    }
}
