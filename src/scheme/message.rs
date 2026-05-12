use std::{
    any::{Any, TypeId, type_name},
    sync::Arc,
};

use tokio::sync::{mpsc, oneshot};

use crate::scheme::*;

pub(super) const DEFAULT_DEMAND_RETRY_BUDGET: u8 = 8;

// ---------------------------------------------------------------------------
// Wire types shared between mod.rs and this module
// ---------------------------------------------------------------------------

pub(super) enum WorkerMessage {
    Demand(Demand),
    Delta(Box<dyn Any + Send + Sync>),
}

pub(super) struct Demand {
    pub getter: Arc<dyn Any + Send + Sync>,
    pub getter_name: &'static str,
    pub expected_output_type: TypeId,
    pub expected_output_name: &'static str,
    /// Layer where the demand originated (for retry after `Handled` delta propagation).
    pub origin_layer_type: TypeId,
    pub remaining_retries: u8,
    pub response_tx: oneshot::Sender<Result<ErasedOutput, GetterError>>,
}

pub(crate) struct ErasedOutput {
    pub value: Box<dyn Any + Send + Sync>,
    pub output_type: TypeId,
    pub output_name: &'static str,
}

type WorkerStarter =
    Box<dyn FnOnce(Context, mpsc::Receiver<WorkerMessage>) -> tokio::task::JoinHandle<()> + Send>;

pub(super) struct LayerSpec {
    pub start_worker: WorkerStarter,
}

enum PostDemand {
    Done,
    Handled {
        deltas: Box<dyn Any + Send + Sync>,
        demand: Demand,
    },
}

// ---------------------------------------------------------------------------
// Top-layer worker: polls emit() for external signals, also handles demands
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
    <<T::Lower as NonTopLayer>::Key as Getter<T::Lower>>::Output: Send + Sync + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // External signal from the top layer's source.
                maybe_delta = layer.emit(&context) => {
                    match maybe_delta {
                        Ok(None) => break, // source exhausted
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
                // Incoming demand or externally-injected delta.
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
    <<T::Lower as NonTopLayer>::Key as Getter<T::Lower>>::Output: Send + Sync + 'static,
{
    match message {
        WorkerMessage::Demand(demand) => {
            match handle_any_demand::<T>(layer_type, layer_name, context, layer, demand).await {
                PostDemand::Done => {}
                PostDemand::Handled { deltas, demand } => {
                    // Top layer produced side-effect deltas; forward the batch downward then retry.
                    if let Err(err) =
                        forward_delta_down(layer_type, layer_name, context, deltas).await
                    {
                        eprintln!("{err}");
                    }
                    retry_demand_at_origin(context, demand).await;
                }
            }
        }
        // TopLayer receives no incoming deltas from above (it is the top).
        WorkerMessage::Delta(_) => {
            debug_assert!(false, "TopLayer received a Delta message -- this is a bug");
        }
    }
}

// ---------------------------------------------------------------------------
// Middle-layer worker: receives Delta<M>, calls pass(), forwards Delta<M::Lower>
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
    <M::Key as Getter<M>>::Output: Send + Sync + 'static,
    <<M::Lower as NonTopLayer>::Key as Getter<M::Lower>>::Output: Send + Sync + 'static,
{
    tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            match message {
                WorkerMessage::Demand(demand) => {
                    match handle_any_demand::<M>(layer_type, layer_name, &context, &layer, demand)
                        .await
                    {
                        PostDemand::Done => {}
                        PostDemand::Handled { deltas, demand } => {
                            if let Err(err) = apply_middle_delta::<M>(
                                layer_type, layer_name, &context, &mut layer, deltas,
                            )
                            .await
                            {
                                eprintln!("{err}");
                            }
                            retry_demand_at_origin(&context, demand).await;
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
    <M::Key as Getter<M>>::Output: Send + Sync + 'static,
    <<M::Lower as NonTopLayer>::Key as Getter<M::Lower>>::Output: Send + Sync + 'static,
{
    let Ok(typed) = delta.downcast::<LayerDeltas<M>>() else {
        debug_assert!(
            false,
            "delta cast mismatch in middle layer {layer_name}: expected {}",
            type_name::<LayerDeltas<M>>()
        );
        return Ok(());
    };

    let out =
        layer
            .pass(context, *typed)
            .await
            .map_err(|err| DeltaFlowError::MiddlePassFailed {
                layer: layer_name.to_string(),
                reason: err.to_string(),
            })?;

    let lower_type = context.registry.lower_by_upper.get(&layer_type).copied();

    if let Some(lower_type) = lower_type {
        forward_delta_down_to(lower_type, layer_name, context, Box::new(out)).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Bottom-layer worker: receives Delta<B>, calls consume(), nothing forwarded
// ---------------------------------------------------------------------------

pub(super) fn spawn_bottom_worker<B>(
    context: Context,
    mut receiver: mpsc::Receiver<WorkerMessage>,
    layer_type: TypeId,
    layer_name: &'static str,
    mut layer: B,
) -> tokio::task::JoinHandle<()>
where
    B: BottomLayer,
    <B::Key as Getter<B>>::Output: Send + Sync + 'static,
{
    tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            match message {
                WorkerMessage::Demand(demand) => {
                    match handle_any_demand::<B>(layer_type, layer_name, &context, &layer, demand)
                        .await
                    {
                        PostDemand::Done => {}
                        PostDemand::Handled { deltas, demand } => {
                            if let Ok(typed) = deltas.downcast::<LayerDeltas<B>>() {
                                if let Err(err) = layer.consume(&context, *typed).await {
                                    eprintln!(
                                        "{}",
                                        DeltaFlowError::BottomConsumeFailed {
                                            layer: layer_name.to_string(),
                                            reason: err.to_string(),
                                        }
                                    );
                                }
                            }
                            retry_demand_at_origin(&context, demand).await;
                        }
                    }
                }
                WorkerMessage::Delta(delta_box) => {
                    if let Ok(typed) = delta_box.downcast::<LayerDeltas<B>>() {
                        if let Err(err) = layer.consume(&context, *typed).await {
                            eprintln!(
                                "{}",
                                DeltaFlowError::BottomConsumeFailed {
                                    layer: layer_name.to_string(),
                                    reason: err.to_string(),
                                }
                            );
                        }
                    } else {
                        debug_assert!(
                            false,
                            "delta cast mismatch in bottom layer {layer_name}: expected {}",
                            type_name::<LayerDeltas<B>>()
                        );
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Shared demand handling (all three roles)
// ---------------------------------------------------------------------------

async fn handle_any_demand<L>(
    layer_type: TypeId,
    layer_name: &'static str,
    context: &Context,
    layer: &L,
    demand: Demand,
) -> PostDemand
where
    L: AnyLayer,
{
    let outcome = layer
        .resolve(context, AnyGetter::new(demand.getter.as_ref()))
        .await;

    let result = match outcome.into_kind() {
        ErasedOutcomeKind::Resolved(output) => validate_output(layer_name, &demand, output),
        ErasedOutcomeKind::Forwarded => {
            forward_to_upper(layer_type, layer_name, context, &demand).await
        }
        ErasedOutcomeKind::Failed(err) => Err(GetterError::ErrorFromLayer {
            getter: demand.getter_name.to_string(),
            layer: layer_name.to_string(),
            reason: err.to_string(),
        }),
        ErasedOutcomeKind::Updated(deltas) | ErasedOutcomeKind::Emitted(deltas) => {
            return PostDemand::Handled { deltas, demand };
        }
    };

    let _ = demand.response_tx.send(result);
    PostDemand::Done
}

// ---------------------------------------------------------------------------
// Demand / delta routing helpers
// ---------------------------------------------------------------------------

async fn forward_to_upper(
    current_layer_type: TypeId,
    current_layer_name: &'static str,
    context: &Context,
    demand: &Demand,
) -> Result<ErasedOutput, GetterError> {
    let Some(upper_type) = context
        .registry
        .upper_by_lower
        .get(&current_layer_type)
        .copied()
    else {
        return Err(GetterError::MissingResource {
            getter: demand.getter_name.to_string(),
            layer: current_layer_name.to_string(),
        });
    };

    let upper_name = context
        .registry
        .layer_names
        .get(&upper_type)
        .copied()
        .unwrap_or("unknown");
    let upper_sender = context
        .registry
        .senders
        .get(&upper_type)
        .cloned()
        .ok_or_else(|| GetterError::MissingResource {
            getter: demand.getter_name.to_string(),
            layer: upper_name.to_string(),
        })?;

    let (tx, rx) = oneshot::channel();
    let forwarded = Demand {
        getter: Arc::clone(&demand.getter),
        getter_name: demand.getter_name,
        expected_output_type: demand.expected_output_type,
        expected_output_name: demand.expected_output_name,
        origin_layer_type: demand.origin_layer_type,
        remaining_retries: demand.remaining_retries,
        response_tx: tx,
    };

    upper_sender
        .send(WorkerMessage::Demand(forwarded))
        .await
        .map_err(|_| channel_closed(demand.getter_name, upper_name))?;

    let result = rx
        .await
        .map_err(|_| channel_closed(demand.getter_name, upper_name))?;

    result.and_then(|output| validate_output(upper_name, demand, output))
}

/// Forward a delta one step downward using the `lower_by_upper` map.
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

fn validate_output(
    layer_name: &'static str,
    demand: &Demand,
    output: ErasedOutput,
) -> Result<ErasedOutput, GetterError> {
    if output.output_type != demand.expected_output_type {
        return Err(GetterError::OutputTypeMismatch {
            getter: demand.getter_name.to_string(),
            layer: layer_name.to_string(),
            expected: demand.expected_output_name.to_string(),
            actual: output.output_name.to_string(),
        });
    }
    Ok(output)
}

async fn retry_demand_at_origin(context: &Context, demand: Demand) {
    if demand.remaining_retries == 0 {
        let origin_name = context
            .registry
            .layer_names
            .get(&demand.origin_layer_type)
            .copied()
            .unwrap_or("unknown");
        let _ = demand.response_tx.send(Err(GetterError::RetryLimitReached {
            getter: demand.getter_name.to_string(),
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
        let _ = demand.response_tx.send(Err(GetterError::MissingResource {
            getter: demand.getter_name.to_string(),
            layer: origin_name.to_string(),
        }));
        return;
    };

    let (retry_tx, retry_rx) = oneshot::channel();
    let retry_demand = Demand {
        getter: Arc::clone(&demand.getter),
        getter_name: demand.getter_name,
        expected_output_type: demand.expected_output_type,
        expected_output_name: demand.expected_output_name,
        origin_layer_type: origin_type,
        remaining_retries,
        response_tx: retry_tx,
    };

    if sender
        .send(WorkerMessage::Demand(retry_demand))
        .await
        .is_err()
    {
        let _ = demand.response_tx.send(Err(GetterError::ChannelClosed {
            getter: demand.getter_name.to_string(),
            layer: origin_name.to_string(),
        }));
        return;
    }

    match retry_rx.await {
        Ok(result) => {
            let _ = demand.response_tx.send(result);
        }
        Err(_) => {
            let _ = demand.response_tx.send(Err(GetterError::ChannelClosed {
                getter: demand.getter_name.to_string(),
                layer: origin_name.to_string(),
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Error helper constructors (used by mod.rs via pub(super))
// ---------------------------------------------------------------------------

pub(super) fn missing_getter_resource(getter: &str, layer: &str) -> GetterError {
    GetterError::MissingResource {
        getter: getter.to_string(),
        layer: layer.to_string(),
    }
}

pub(super) fn channel_closed(getter: &str, layer: &str) -> GetterError {
    GetterError::ChannelClosed {
        getter: getter.to_string(),
        layer: layer.to_string(),
    }
}

pub(super) fn output_type_mismatch(
    getter: &str,
    layer: &str,
    expected: &str,
    actual: &str,
) -> GetterError {
    GetterError::OutputTypeMismatch {
        getter: getter.to_string(),
        layer: layer.to_string(),
        expected: expected.to_string(),
        actual: actual.to_string(),
    }
}
