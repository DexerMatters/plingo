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

use crate::{
    component::{
        debug::DebugSink,
        source::{Source, SourceEdit, TextChunk},
    },
    scheme::{
        __macro_private,
        call::{CallOutcome, LayerCallFuture},
        change::{ChangeSet, LayerChanges},
        context::Context,
        layer::{BottomLayer, FallibleLayer, MiddleLayer, SnapshotLayer, TopLayer},
        runtime::{
            LayerRegistry, Runtime,
            message::{DEFAULT_DEMAND_RETRY_BUDGET, Demand, WorkerMessage},
            worker::spawn_top_worker,
        },
        snapshot::SnapshotRetention,
    },
    utils::Span,
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

impl SnapshotLayer for ProbeTop {
    type State = ();

    fn initialize_snapshots(&mut self) {}
    fn push_state(&mut self, _: u64) {}
    fn rollback_state(&mut self, _: crate::scheme::change::Revision) -> bool {
        true
    }
    fn state(&self, _: Option<u64>) -> Option<&Self::State> {
        Some(&())
    }
    fn latest_state(&self) -> &Self::State {
        &()
    }
    fn latest_state_mut(&mut self) -> &mut Self::State {
        panic!("ProbeTop has no state")
    }
    fn set_snapshot_retention(&mut self, _: SnapshotRetention) {}
    fn snapshot_retention(&self) -> SnapshotRetention {
        SnapshotRetention::default()
    }
}

impl TopLayer for ProbeTop {
    type Error = Infallible;
    type Lower = ();

    fn emit<'a>(
        &'a mut self,
        _ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<LayerChanges<Self::Lower>>, Self::Error>> + Send + 'a
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
        read_only: false,
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

#[plingo_macros::layer]
struct ReentrantParent {
    #[snapshot]
    value: Arc<usize>,
}

impl ReentrantParent {
    #[crate::context_callable]
    async fn value_at<'a>(
        &'a mut self,
        ctx: &'a Context,
        _: &'a (),
    ) -> CallOutcome<Self, usize> {
        let Some(value) = self.state(ctx.snapshot()) else {
            unreachable!("the forwarding target snapshot is prepared before child pass")
        };
        CallOutcome::ok(*value)
    }
}

#[plingo_macros::layer]
struct ReentrantChild {
    #[snapshot]
    state: Arc<()>,
    observed: mpsc::UnboundedSender<usize>,
}

#[plingo_macros::layer(middle)]
impl MiddleLayer for ReentrantParent {
    type Lower = ReentrantChild;
    type Error = Infallible;
    type Address = fluent_uri::Uri<&'static str>;
    type Unit = TextChunk;

    fn pass(
        &mut self,
        _ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move {
            let revision = changes.revision;
            self.value = Arc::new(*self.value + 1);
            self.push_state(revision.target);
            Ok(ChangeSet {
                revision,
                changes: vec![crate::scheme::change::AddressChange {
                    address: (),
                    old_extent: 1,
                    new_extent: 1,
                    splices: vec![crate::scheme::change::Splice {
                        old_range: 0..1,
                        new_range: 0..1,
                        removed: Arc::from([()]),
                        inserted: Arc::from([()]),
                    }],
                }],
            })
        }
    }
}

#[plingo_macros::layer(middle)]
impl MiddleLayer for ReentrantChild {
    type Lower = DebugSink<(), ()>;
    type Error = crate::scheme::error::ActionError;
    type Address = ();
    type Unit = ();

    fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move {
            let target = ctx.with_snapshot(Some(changes.revision.target));
            let value = target.read(ReentrantParent::value_at, ()).await?;
            let _ = self.observed.send(value);
            self.push_state(changes.revision.target);
            Ok(ChangeSet::empty(changes.revision))
        }
    }
}

#[tokio::test]
async fn forwarding_middle_worker_services_reentrant_read_demands() {
    let (source_tx, source_rx) = mpsc::channel(1);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let sink = DebugSink::<(), ()>::new(|_ctx, _changes| Box::pin(async { Ok(()) }));
    let mut runtime = Runtime::new()
        .with(Source::<ReentrantParent>::new(source_rx))
        .with(ReentrantParent {
            value: Arc::new(0),
            _snapshot: Default::default(),
        })
        .with(ReentrantChild {
            state: Arc::new(()),
            observed: observed_tx,
            _snapshot: Default::default(),
        })
        .finish(sink);
    runtime.run().await.unwrap();

    let uri = Span::new("test://reentrant-read", 0, 0).unwrap().uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0).unwrap(),
            value: "x".into(),
        })
        .await
        .unwrap();

    for _ in 0..1_000 {
        if let Ok(value) = observed_rx.try_recv() {
            assert_eq!(value, 1);
            runtime.shutdown().await;
            return;
        }
        tokio::task::yield_now().await;
    }

    runtime.shutdown().await;
    panic!("reentrant read demand was not serviced");
}

#[plingo_macros::layer]
struct ExtraLayer {
    #[snapshot]
    state: Arc<()>,
}

struct RejectSink {
    observed: mpsc::Sender<()>,
}

#[plingo_macros::layer(bottom)]
impl BottomLayer for RejectSink {
    type Error = &'static str;
    type Address = fluent_uri::Uri<&'static str>;
    type Unit = TextChunk;

    fn consume(
        &mut self,
        _ctx: &Context,
        _changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move {
            let _ = self.observed.send(()).await;
            Err("intentional rejection")
        }
    }
}

#[plingo_macros::layer(middle)]
impl MiddleLayer for ExtraLayer {
    type Lower = DebugSink<(), ()>;
    type Error = Infallible;
    type Address = fluent_uri::Uri<&'static str>;
    type Unit = TextChunk;

    fn pass(
        &mut self,
        _ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send {
        async move { Ok(ChangeSet::empty(changes.revision)) }
    }
}

#[tokio::test]
async fn inserted_layer_preserves_empty_transaction_revisions() {
    let (source_tx, source_rx) = mpsc::channel(4);
    let (sink_tx, mut sink_rx) = mpsc::channel(4);
    let sink = DebugSink::<(), ()>::new(move |_ctx, changes| {
        let sink_tx = sink_tx.clone();
        Box::pin(async move {
            sink_tx.send(changes).await.unwrap();
            Ok(())
        })
    });
    let mut runtime = Runtime::new()
        .with(Source::<ExtraLayer>::new(source_rx))
        .with(ExtraLayer {
            state: Arc::new(()),
            _snapshot: Default::default(),
        })
        .finish(sink);
    runtime.run().await.unwrap();
    let uri = Span::new("test://extra-layer", 0, 0).unwrap().uri;

    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0).unwrap(),
            value: "first".into(),
        })
        .await
        .unwrap();
    let first = sink_rx.recv().await.unwrap();
    assert!(first.changes.is_empty());
    assert_eq!(first.revision.base, 0);

    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 5, 5).unwrap(),
            value: " second".into(),
        })
        .await
        .unwrap();
    let second = sink_rx.recv().await.unwrap();
    assert!(second.changes.is_empty());
    assert_eq!(second.revision.base, first.revision.target);
    runtime.shutdown().await;
}

#[tokio::test]
async fn rejected_downstream_transaction_restores_source_state() {
    let (source_tx, source_rx) = mpsc::channel(1);
    let (observed_tx, mut observed_rx) = mpsc::channel(1);
    let mut runtime = Runtime::new()
        .with(Source::<RejectSink>::new(source_rx))
        .finish(RejectSink {
            observed: observed_tx,
        });
    runtime.run().await.unwrap();
    let uri = Span::new("test://rollback", 0, 0).unwrap().uri;

    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0).unwrap(),
            value: "uncommitted".into(),
        })
        .await
        .unwrap();
    observed_rx.recv().await.unwrap();

    let source = runtime
        .context()
        .call(
            Source::<RejectSink>::read_span,
            Span::new_uri(uri, 0, 0).unwrap(),
        )
        .await
        .unwrap();
    assert!(source.to_string().is_empty());
    runtime.shutdown().await;
}
