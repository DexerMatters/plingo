use std::{
    any::{Any, TypeId},
    sync::Arc,
};

use tokio::sync::{mpsc, oneshot};

use crate::scheme::{
    __macro_private,
    context::{Context, SnapshotId},
    error::ActionError,
};

pub(crate) const DEFAULT_DEMAND_RETRY_BUDGET: u8 = 8;

pub(crate) enum WorkerMessage {
    Demand(Demand),
    Delta(DeltaEnvelope),
    Barrier(AwaitBarrier),
}

pub(crate) struct DeltaEnvelope {
    pub snapshot: SnapshotId,
    pub payload: Box<dyn Any + Send + Sync>,
}

pub(crate) struct Demand {
    pub action: Arc<dyn Any + Send + Sync>,
    pub action_name: &'static str,
    pub requester_layer_type: TypeId,
    pub snapshot: Option<SnapshotId>,
    pub remaining_retries: u8,
    pub dispatch: __macro_private::RegisteredDispatchFn,
    pub call_stack: Vec<TypeId>,
    pub response_tx: oneshot::Sender<Result<ErasedOutput, ActionError>>,
}

pub(crate) struct ErasedOutput {
    pub value: Box<dyn Any + Send + Sync>,
}

pub(crate) struct AwaitBarrier {
    pub destination_layer_type: TypeId,
    pub response_tx: oneshot::Sender<()>,
}

type WorkerStarter =
    Box<dyn FnOnce(Context, mpsc::Receiver<WorkerMessage>) -> tokio::task::JoinHandle<()> + Send>;

pub(crate) struct LayerSpec {
    pub start_worker: WorkerStarter,
}
