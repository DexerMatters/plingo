use std::{
    any::{Any, TypeId},
    sync::Arc,
};

use tokio::sync::{mpsc, oneshot};

use crate::scheme::{
    __macro_private,
    change::{ChangeSet, FlowUnit, Revision},
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
    pub payload: Box<dyn ErasedChanges>,
    pub completion: Option<oneshot::Sender<Result<(), crate::scheme::error::DeltaFlowError>>>,
}

impl DeltaEnvelope {
    pub(crate) fn new(payload: Box<dyn ErasedChanges>) -> Self {
        Self {
            payload,
            completion: None,
        }
    }
}

pub(crate) trait ErasedChanges: Any + Send + Sync {
    fn revision(&self) -> Revision;
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync>;
}

impl<Address, Unit> ErasedChanges for ChangeSet<Address, Unit>
where
    Address: Send + Sync + 'static,
    Unit: FlowUnit,
{
    fn revision(&self) -> Revision {
        self.revision
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync> {
        self
    }
}

pub(crate) struct Demand {
    pub action: Arc<dyn Any + Send + Sync>,
    pub action_name: &'static str,
    pub requester_layer_type: TypeId,
    pub snapshot: Option<SnapshotId>,
    pub remaining_retries: u8,
    /// Read-only demands may be serviced while a worker forwards a delta.
    pub read_only: bool,
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
