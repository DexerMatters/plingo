use std::{
    any::TypeId,
    convert::Infallible,
    future::{Future, poll_fn},
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{mpsc, oneshot};

use crate::scheme::{
    __macro_private,
    call::{CallOutcome, LayerCallFuture},
    change::EmittedChanges,
    context::Context,
    layer::{FallibleLayer, TopLayer},
    runtime::{
        LayerRegistry,
        message::{DEFAULT_DEMAND_RETRY_BUDGET, Demand, WorkerMessage},
        worker::spawn_top_worker,
    },
};

struct ProbeTop {
    emit_polled: Arc<AtomicBool>,
    entered_tx: Option<oneshot::Sender<()>>,
    resume_rx: Option<oneshot::Receiver<()>>,
    _marker: PhantomData<fn() -> ()>,
}

impl FallibleLayer for ProbeTop {
    type __Error = Infallible;
}

impl TopLayer for ProbeTop {
    type Error = Infallible;
    type Lower = ();

    fn emit<'a>(
        &'a mut self,
        _ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<EmittedChanges<Self::Lower>>, Self::Error>> + Send + 'a
    {
        let emit_polled = Arc::clone(&self.emit_polled);
        async move {
            poll_fn(|_| {
                emit_polled.store(true, Ordering::SeqCst);
                std::task::Poll::Pending::<()>
            })
            .await;
            Ok(None)
        }
    }
}

impl ProbeTop {
    fn ping<'a>(&'a mut self, _ctx: &'a Context, _args: &'a ()) -> LayerCallFuture<'a, Self, ()> {
        Box::pin(async move {
            if let Some(tx) = self.entered_tx.take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.resume_rx.take() {
                let _ = rx.await;
            }
            CallOutcome::ok(())
        })
    }
}

#[tokio::test]
async fn top_worker_services_queued_messages_before_polling_emit() {
    let emit_polled = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let (sender, receiver) = mpsc::channel(4);
    let (response_tx, response_rx) = oneshot::channel();

    let layer_type = TypeId::of::<ProbeTop>();
    let layer_name = std::any::type_name::<ProbeTop>();

    let demand = Demand {
        action: Arc::new(__macro_private::CallPayload::<ProbeTop, (), ()> {
            method: ProbeTop::ping,
            args: (),
            _marker: PhantomData,
        }),
        action_name: std::any::type_name::<()>(),
        requester_layer_type: layer_type,
        snapshot: None,
        remaining_retries: DEFAULT_DEMAND_RETRY_BUDGET,
        dispatch: __macro_private::dispatch_call::<ProbeTop, (), ()>,
        call_stack: Vec::new(),
        response_tx,
    };

    sender.send(WorkerMessage::Demand(demand)).await.unwrap();
    drop(sender);

    let registry = Arc::new(LayerRegistry::default());
    let context = Context {
        registry,
        snapshot: None,
        current_layer_type: None,
        call_stack: Vec::new(),
    };

    let worker = spawn_top_worker(
        context,
        receiver,
        layer_type,
        layer_name,
        ProbeTop {
            emit_polled: Arc::clone(&emit_polled),
            entered_tx: Some(entered_tx),
            resume_rx: Some(resume_rx),
            _marker: PhantomData,
        },
    );

    entered_rx.await.unwrap();
    assert!(
        !emit_polled.load(Ordering::SeqCst),
        "top worker polled emit before handling a queued message"
    );

    let _ = resume_tx.send(());
    response_rx.await.unwrap().unwrap();
    worker.abort();
    let _ = worker.await;
}
