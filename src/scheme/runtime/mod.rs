use std::{
    any::{TypeId, type_name},
    collections::{BTreeMap, HashMap},
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use tokio::sync::mpsc;

use crate::{
    marker::{Linked, NeedsTop, Sealed},
    scheme::{
        context::Context,
        error::RuntimeBuildError,
        layer::{BottomLayer, FallibleLayer, MiddleLayer, TopLayer},
        runtime::{
            message::{LayerSpec, WorkerMessage},
            worker::{spawn_bottom_worker, spawn_middle_worker, spawn_top_worker},
        },
    },
};

pub(crate) mod dispatch;
pub(crate) mod message;
pub(crate) mod pending;
pub(crate) mod worker;

#[derive(Default)]
pub(crate) struct LayerRegistry {
    pub(crate) senders: HashMap<TypeId, mpsc::Sender<WorkerMessage>>,
    pub(crate) lower_by_upper: HashMap<TypeId, TypeId>,
    pub(crate) layer_names: HashMap<TypeId, &'static str>,
    pub(crate) next_snapshot: AtomicU64,
    pub(crate) snapshot_parents: Mutex<BTreeMap<u64, u64>>,
    pub(crate) panicked: AtomicBool,
}

struct RuntimeInner {
    specs: HashMap<TypeId, LayerSpec>,
    lower_by_upper: HashMap<TypeId, TypeId>,
    layer_names: HashMap<TypeId, &'static str>,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl RuntimeInner {
    fn new() -> Self {
        Self {
            specs: HashMap::new(),
            lower_by_upper: HashMap::new(),
            layer_names: HashMap::new(),
            workers: Vec::new(),
        }
    }
}

/// This is the main entry point for building and running a plingo pipeline.
pub struct Runtime<S = NeedsTop> {
    inner: RuntimeInner,
    context: Context,
    _state: PhantomData<fn() -> S>,
}

impl Runtime<NeedsTop> {
    pub fn new() -> Self {
        Self {
            inner: RuntimeInner::new(),
            context: Context::default(),
            _state: PhantomData,
        }
    }

    /// Attach a top layer to the runtime.
    pub fn with<T>(mut self, layer: T) -> Runtime<Linked<T, T::Lower>>
    where
        T: TopLayer,
    {
        let layer_type = TypeId::of::<T>();
        let layer_name = type_name::<T>();
        self.inner.layer_names.insert(layer_type, layer_name);
        self.inner.specs.insert(
            layer_type,
            LayerSpec {
                start_worker: Box::new(move |context, receiver| {
                    spawn_top_worker::<T>(context, receiver, layer_type, layer_name, layer)
                }),
            },
        );
        Runtime {
            inner: self.inner,
            context: self.context,
            _state: PhantomData,
        }
    }
}

impl<Upper: FallibleLayer, Edge: FallibleLayer> Runtime<Linked<Upper, Edge>> {
    /// Attach a middle layer to the runtime.
    pub fn with(mut self, layer: Edge) -> Runtime<Linked<Edge, Edge::Lower>>
    where
        Edge: MiddleLayer,
    {
        let upper_type = TypeId::of::<Upper>();
        let layer_type = TypeId::of::<Edge>();
        let layer_name = type_name::<Edge>();

        self.inner.layer_names.insert(layer_type, layer_name);
        self.inner.lower_by_upper.insert(upper_type, layer_type);

        self.inner.specs.insert(
            layer_type,
            LayerSpec {
                start_worker: Box::new(move |context, receiver| {
                    spawn_middle_worker::<Edge>(context, receiver, layer_type, layer_name, layer)
                }),
            },
        );
        Runtime {
            inner: self.inner,
            context: self.context,
            _state: PhantomData,
        }
    }

    /// Attach the bottom layer to the runtime.
    pub fn finish(mut self, layer: Edge) -> Runtime<Sealed>
    where
        Edge: BottomLayer,
    {
        let upper_type = TypeId::of::<Upper>();
        let layer_type = TypeId::of::<Edge>();
        let layer_name = type_name::<Edge>();

        self.inner.layer_names.insert(layer_type, layer_name);
        self.inner.lower_by_upper.insert(upper_type, layer_type);

        self.inner.specs.insert(
            layer_type,
            LayerSpec {
                start_worker: Box::new(move |context, receiver| {
                    spawn_bottom_worker::<Edge>(context, receiver, layer_type, layer_name, layer)
                }),
            },
        );
        Runtime {
            inner: self.inner,
            context: self.context,
            _state: PhantomData,
        }
    }
}

impl Runtime<Sealed> {
    pub async fn run(&mut self) -> Result<(), RuntimeBuildError> {
        if !self.inner.workers.is_empty() {
            return Err(RuntimeBuildError::AlreadyRunning);
        }

        let mut senders = HashMap::new();
        let mut receivers = HashMap::new();
        for &layer_type in self.inner.layer_names.keys() {
            let (tx, rx) = mpsc::channel(128);
            senders.insert(layer_type, tx);
            receivers.insert(layer_type, rx);
        }

        let registry = Arc::new(LayerRegistry {
            senders,
            lower_by_upper: self.inner.lower_by_upper.clone(),
            layer_names: self.inner.layer_names.clone(),
            next_snapshot: AtomicU64::new(1),
            snapshot_parents: Mutex::new(BTreeMap::new()),
            panicked: AtomicBool::new(false),
        });
        self.context = Context {
            registry,
            snapshot: None,
            current_layer_type: None,
            call_stack: Vec::new(),
        };

        let specs = std::mem::take(&mut self.inner.specs);
        for (layer_type, spec) in specs {
            let receiver = receivers
                .remove(&layer_type)
                .expect("receiver must exist for every registered layer");
            let worker = (spec.start_worker)(self.context.clone(), receiver);
            self.inner.workers.push(worker);
        }

        Ok(())
    }

    pub fn context(&self) -> Context {
        self.context.clone()
    }

    pub async fn shutdown(&mut self) {
        let registry = Arc::clone(&self.context.registry);
        self.context = Context::default();
        for worker in self.inner.workers.drain(..) {
            if worker.is_finished() {
                if let Err(e) = worker.await {
                    if e.is_panic() {
                        registry.panicked.store(true, Ordering::SeqCst);
                    }
                }
            } else {
                worker.abort();
                let _ = worker.await;
            }
        }
    }

    pub fn workers_panicked(&self) -> bool {
        self.context.registry.panicked.load(Ordering::SeqCst)
    }
}
