use std::{any::TypeId, sync::Arc};

use tokio::sync::oneshot;

use crate::scheme::{
    call::{AwaitPlan, Continuation, ContinuationEffect, PropagationCompletion},
    context::{Context, SnapshotId},
    error::{ActionError, DeltaFlowError},
    runtime::message::{AwaitBarrier, DeltaEnvelope, Demand, ErasedOutput, WorkerMessage},
};

pub(super) enum ContinuationTransition {
    Done,
    Propagate {
        envelope: DeltaEnvelope,
        demand: Demand,
        completion: PropagationCompletion,
    },
}

pub(super) async fn transition_continuation(
    context: &Context,
    demand: Demand,
    continuation: Continuation,
) -> ContinuationTransition {
    match continuation.effect {
        ContinuationEffect::Propagate {
            envelope,
            completion,
        } => {
            let snapshot = envelope.payload.revision().target;
            let demand = Demand {
                snapshot: Some(snapshot),
                ..demand
            };
            ContinuationTransition::Propagate {
                envelope,
                demand,
                completion,
            }
        }
        ContinuationEffect::Await(plan) => {
            let context = context.clone();
            tokio::spawn(async move {
                match execute_await_plan(
                    &context,
                    demand.requester_layer_type,
                    demand.snapshot,
                    demand.remaining_retries,
                    demand.call_stack.clone(),
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

async fn execute_await_plan(
    context: &Context,
    requester_layer_type: TypeId,
    snapshot: Option<SnapshotId>,
    remaining_retries: u8,
    call_stack: Vec<TypeId>,
    plan: AwaitPlan,
) -> Result<(), ActionError> {
    let AwaitPlan {
        target_layer_type,
        target_layer_name,
        action,
        action_name,
        dispatch,
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
    let mut awaited_call_stack = call_stack;
    awaited_call_stack.push(requester_layer_type);
    let awaited_demand = Demand {
        action,
        action_name,
        requester_layer_type,
        snapshot,
        remaining_retries,
        read_only: false,
        dispatch,
        call_stack: awaited_call_stack,
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
        requester_layer_type,
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

pub(super) async fn handle_barrier(
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

pub(super) async fn forward_delta_down(
    upper_type: TypeId,
    upper_name: &str,
    context: &Context,
    delta: DeltaEnvelope,
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

pub(super) async fn forward_delta_down_to(
    lower_type: TypeId,
    upper_name: &str,
    context: &Context,
    mut delta: DeltaEnvelope,
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
    let (completion, received) = oneshot::channel();
    delta.completion = Some(completion);
    lower_sender
        .send(WorkerMessage::Delta(delta))
        .await
        .map_err(|_| DeltaFlowError::LowerSenderClosed {
            layer: lower_name.to_string(),
        })?;
    received
        .await
        .map_err(|_| DeltaFlowError::LowerSenderClosed {
            layer: lower_name.to_string(),
        })?
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

pub(super) async fn complete_propagated_demand(
    context: &Context,
    demand: Demand,
    completion: PropagationCompletion,
) {
    match completion {
        PropagationCompletion::Retry => retry_demand_at_origin(context, demand).await,
        PropagationCompletion::Resolve(value) => {
            let _ = demand.response_tx.send(Ok(ErasedOutput { value }));
        }
    }
}

pub(super) async fn retry_demand_at_origin(context: &Context, demand: Demand) {
    if demand.remaining_retries == 0 {
        let origin_name = context
            .registry
            .layer_names
            .get(&demand.requester_layer_type)
            .copied()
            .unwrap_or("unknown");
        let _ = demand.response_tx.send(Err(ActionError::RetryLimitReached {
            action: demand.action_name.to_string(),
            layer: origin_name.to_string(),
        }));
        return;
    }

    let origin_type = demand.requester_layer_type;
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
        requester_layer_type: origin_type,
        snapshot: demand.snapshot,
        remaining_retries,
        read_only: demand.read_only,
        dispatch: demand.dispatch,
        call_stack: demand.call_stack.clone(),
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
