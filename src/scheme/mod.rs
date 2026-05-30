use core::fmt;
use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    convert::Infallible,
    fmt::Display,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::marker::*;
use crate::scheme::message::*;

mod message;

/// Context shared by all layers which can be used to resolve actions and access
/// the registry.
#[derive(Clone)]
pub struct Context {
    pub(crate) registry: Arc<LayerRegistry>,
    pub(crate) snapshot: Option<SnapshotId>,
}

pub type SnapshotId = u64;

impl Default for Context {
    fn default() -> Self {
        Self {
            registry: Arc::new(LayerRegistry::default()),
            snapshot: None,
        }
    }
}

impl Context {
    pub fn snapshot(&self) -> Option<SnapshotId> {
        self.snapshot
    }

    pub fn with_snapshot(&self, snapshot: Option<SnapshotId>) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            snapshot,
        }
    }

    pub fn last_snapshot(&self) -> Context {
        self.with_snapshot(self.snapshot.map(|n| n.saturating_sub(1)))
    }

    pub(crate) fn allocate_snapshot(&self) -> SnapshotId {
        self.registry.next_snapshot.fetch_add(1, Ordering::Relaxed)
    }

    /// Request the output of a action `G` from the layer `L` that can resolve it.
    pub async fn post<L, G>(&self, action: G) -> Result<<L as Receiver<G>>::_Output, ActionError>
    where
        L: FallibleLayer + Receiver<G> + Resolve<G> + 'static,
        <L as Receiver<G>>::_Output: Send + Sync + 'static,
        G: Send + Sync + 'static,
    {
        let action_name = type_name::<G>().to_string();
        let layer_type = TypeId::of::<L>();
        let layer_name = self
            .registry
            .layer_names
            .get(&layer_type)
            .copied()
            .unwrap_or(type_name::<L>());
        let sender = self
            .registry
            .senders
            .get(&layer_type)
            .cloned()
            .ok_or_else(|| ActionError::MissingResource {
                action: action_name.clone(),
                layer: layer_name.to_string(),
            })?;

        let (response_tx, response_rx) = oneshot::channel();
        let demand = Demand {
            action: Arc::new(action),
            action_name: type_name::<G>(),
            requester_layer_type: layer_type,
            snapshot: self.snapshot,
            remaining_retries: DEFAULT_DEMAND_RETRY_BUDGET,
            dispatch: __macro_private::dispatch_resolve::<L, G>,
            response_tx,
        };

        sender
            .send(WorkerMessage::Demand(demand))
            .await
            .map_err(|_| ActionError::ChannelClosed {
                action: action_name.clone(),
                layer: layer_name.to_string(),
            })?;

        let erased = response_rx.await.map_err(|_| ActionError::ChannelClosed {
            action: action_name.clone(),
            layer: layer_name.to_string(),
        })??;

        let typed = erased
            .value
            .downcast::<<L as Receiver<G>>::_Output>()
            .unwrap_or_else(|_| {
                unreachable!(
                    "receiver output downcast must match surface API: action={}, layer={}, expected={}",
                    action_name,
                    layer_name,
                    type_name::<<L as Receiver<G>>::_Output>(),
                )
            });

        Ok(*typed)
    }
}

/// Delta representing an insertion or deletion of a key-value pair.
#[derive(Debug, Clone)]
pub enum Delta<K, V> {
    Insert { key: K, value: V },
    Delete { key: K },
}

impl<K, V> fmt::Display for Delta<K, V>
where
    K: fmt::Display,
    V: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Delta::Insert { key, value } => {
                write!(f, "Insert {{ key: {}, value: {} }}", key, value)
            }
            Delta::Delete { key } => {
                write!(f, "Delete {{ key: {} }}", key)
            }
        }
    }
}

impl<K, V> Delta<K, V> {
    /// Get the key associated with this delta, regardless of the variant.
    pub fn key(&self) -> &K {
        match self {
            Delta::Insert { key, .. } | Delta::Delete { key, .. } => key,
        }
    }

    pub fn map_value<U, F: FnOnce(&V) -> U>(&self, f: F) -> Option<U> {
        match self {
            Delta::Insert { value, .. } => Some(f(value)),
            Delta::Delete { .. } => None,
        }
    }
}

/// A batch of deltas.
pub type Deltas<K, V> = Vec<Delta<K, V>>;

pub struct EmittedDeltas<L: NonTopLayer> {
    pub snapshot: SnapshotId,
    pub deltas: LayerDeltas<L>,
}

impl<L: NonTopLayer> EmittedDeltas<L> {
    pub fn new(snapshot: SnapshotId, deltas: LayerDeltas<L>) -> Self {
        Self { snapshot, deltas }
    }
}

/// Convenience type alias for a batch of deltas for a specific non-top layer `L`.
pub type LayerDeltas<L> = Deltas<<L as NonTopLayer>::_Key, <L as NonTopLayer>::_Value>;

#[doc(hidden)]
pub mod __macro_private;

fn registered_outcome_to_erased<L: FallibleLayer>(
    action_name: &'static str,
    outcome: __macro_private::RegisteredDispatchOutcome,
) -> ErasedOutcome<L> {
    match outcome.0 {
        __macro_private::RegisteredDispatchOutcomeKind::Resolved(value) => ErasedOutcome {
            inner: ErasedOutcomeKind::Resolved(ErasedOutput { value }),
            _marker: PhantomData,
        },
        __macro_private::RegisteredDispatchOutcomeKind::Continue(continuation) => ErasedOutcome {
            inner: ErasedOutcomeKind::Continue(continuation),
            _marker: PhantomData,
        },
        __macro_private::RegisteredDispatchOutcomeKind::Failed(reason) => ErasedOutcome {
            inner: ErasedOutcomeKind::Failed(ActionError::ErrorFromLayer {
                action: action_name.to_string(),
                layer: L::display(),
                reason,
            }),
            _marker: PhantomData,
        },
    }
}

async fn dispatch_registered_action<L>(
    layer: &mut L,
    ctx: &Context,
    action_name: &'static str,
    action: &(dyn Any + Send + Sync),
    dispatch: __macro_private::RegisteredDispatchFn,
) -> ErasedOutcome<L>
where
    L: FallibleLayer,
{
    let layer_any: &mut (dyn Any + Send + Sync) = layer;
    let out = dispatch(layer_any, ctx, action).await;
    registered_outcome_to_erased::<L>(action_name, out)
}

/// Static action resolution contract for all layers.
pub trait Resolve<G>: FallibleLayer + Receiver<G, _Output = Self::Output> {
    type Output: Send + Sync + 'static;
    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a G,
    ) -> impl Future<Output = Outcome<G, Self>> + Send + 'a;
}

/// Opaque runtime-owned plan describing compensating work that should be
/// scheduled before retrying the original resolve request.
#[derive(Clone)]
pub struct AwaitPlan {
    target_layer_type: TypeId,
    target_layer_name: &'static str,
    action: Arc<dyn Any + Send + Sync>,
    action_name: &'static str,
}

impl AwaitPlan {
    pub fn new<L, G>(action: G) -> Self
    where
        L: FallibleLayer + Receiver<G>,
        G: Send + Sync + 'static,
    {
        Self {
            target_layer_type: TypeId::of::<L>(),
            target_layer_name: type_name::<L>(),
            action: Arc::new(action),
            action_name: type_name::<G>(),
        }
    }
}

struct Continuation {
    effect: ContinuationEffect,
}

enum ContinuationEffect {
    Propagate(Box<dyn Any + Send + Sync>),
    Await(AwaitPlan),
}

impl Continuation {
    fn propagate<Payload>(payload: Payload) -> Self
    where
        Payload: Send + Sync + 'static,
    {
        Self {
            effect: ContinuationEffect::Propagate(Box::new(payload)),
        }
    }

    fn await_plan(plan: AwaitPlan) -> Self {
        Self {
            effect: ContinuationEffect::Await(plan),
        }
    }

    fn await_action<Target, Awaited>(action: Awaited) -> Self
    where
        Target: FallibleLayer + Receiver<Awaited>,
        Awaited: Send + Sync + 'static,
    {
        Self::await_plan(AwaitPlan::new::<Target, Awaited>(action))
    }
}

pub struct Outcome<G, L: FallibleLayer + Receiver<G>>(OutcomeKind<G, L>);

enum OutcomeKind<G, L: FallibleLayer + Receiver<G>> {
    Resolved(<L as Receiver<G>>::_Output),
    Continue(Continuation),
    Failed(L::__Error),
}

impl<G, L: FallibleLayer + Receiver<G>> Outcome<G, L> {
    pub fn ok(value: <L as Receiver<G>>::_Output) -> Self {
        Self(OutcomeKind::Resolved(value))
    }

    pub fn fail(err: L::__Error) -> Self {
        Self(OutcomeKind::Failed(err))
    }
}

impl<G, L: NonTopLayer + Receiver<G>> Outcome<G, L> {
    pub fn update(deltas: LayerDeltas<L>) -> Self {
        Self(OutcomeKind::Continue(Continuation::propagate(deltas)))
    }

    pub fn expect<Target, Awaited>(action: Awaited) -> Self
    where
        Target: FallibleLayer + Resolve<Awaited>,
        Awaited: Send + Sync + 'static,
    {
        Self(OutcomeKind::Continue(Continuation::await_action::<
            Target,
            Awaited,
        >(action)))
    }
}

impl<G, L: TopLayer + Receiver<G>> Outcome<G, L> {
    pub fn emit(deltas: LayerDeltas<L::Lower>) -> Self {
        Self(OutcomeKind::Continue(Continuation::propagate(deltas)))
    }
}

struct ErasedOutcome<L: FallibleLayer> {
    inner: ErasedOutcomeKind,
    _marker: PhantomData<fn() -> L>,
}

enum ErasedOutcomeKind {
    Resolved(ErasedOutput),
    Continue(Continuation),
    Failed(ActionError),
}

/// A trait representing a layer in the pipeline.
///
/// Use `#[layer(top)]`, `#[layer(middle)]`, or `#[layer(bottom)]` to
/// auto-generate this impl.
pub trait FallibleLayer: Sized + Send + Sync + 'static {
    /// The type of errors that this layer can produce when resolving actions or
    /// processing deltas.
    type __Error: Display + Send + Sync + 'static;

    fn display() -> String {
        type_name::<Self>().to_string()
    }
}

impl<E, L> HasError<E> for L
where
    L: FallibleLayer<__Error = E>,
{
    type Error = L::__Error;
}

/// A trait representing a top layer, which produces deltas from an external source.
pub trait TopLayer: FallibleLayer<__Error = Self::Error> {
    type Error: Display + Send + Sync + 'static;
    type Lower: NonTopLayer;

    fn emit<'a>(
        &'a mut self,
        ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<EmittedDeltas<Self::Lower>>, Self::Error>> + Send + 'a;
}

/// Marker trait for layers that may appear below another layer in the pipeline.
pub trait NonTopLayer: FallibleLayer<__Error = Self::_Error> {
    type _Error: Display + Send + Sync + 'static;
    type _Key: Send + Sync + 'static;
    type _Value: Send + Sync + 'static;
}

impl FallibleLayer for () {
    type __Error = Infallible;
}

impl NonTopLayer for () {
    type _Error = Infallible;
    type _Key = crate::utils::Span;
    type _Value = usize;
}

impl<K, L> HasKey<K> for L
where
    L: NonTopLayer<_Key = K>,
{
    type Key = L::_Key;
}

pub trait SnapshotLayer {
    type State: Clone;

    fn push_state(&mut self, snapshot: SnapshotId);
    fn state(&self, snapshot: Option<SnapshotId>) -> Option<&Self::State>;
    fn latest_state(&self) -> &Self::State;
    fn latest_state_mut(&mut self) -> &mut Self::State;
}

/// A trait representing a middle layer.
pub trait MiddleLayer:
    NonTopLayer<_Error = Self::Error, _Key = Self::Key, _Value = Self::Value>
{
    type Lower: NonTopLayer;
    type Error: Display + Send + Sync + 'static;
    type Key: Send + Sync + 'static;
    type Value: Send + Sync + 'static;

    fn pass(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<LayerDeltas<Self::Lower>, Self::Error>> + Send;
}

/// A trait representing a bottom layer.
pub trait BottomLayer:
    NonTopLayer<_Error = Self::Error, _Key = Self::Key, _Value = Self::Value>
{
    type Key: Send + Sync + 'static;
    type Error: Display + Send + Sync + 'static;
    type Value: Send + Sync + 'static;
    fn consume(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Errors that can occur while resolving actions in layers.
#[derive(Debug, Error, Clone)]
pub enum ActionError {
    #[error("Missing resource for action {action} while resolving in layer {layer}")]
    MissingResource { action: String, layer: String },
    #[error(
        "Await target layer {target} does not flow down to requester layer {layer} for action {action}"
    )]
    AwaitPathMissing {
        action: String,
        target: String,
        layer: String,
    },
    #[error("Layer {layer} failed while resolving action {action}: {reason}")]
    ErrorFromLayer {
        action: String,
        layer: String,
        reason: String,
    },
    #[error("Layer channel closed while resolving action {action} in layer {layer}")]
    ChannelClosed { action: String, layer: String },
    #[error("Retry limit reached while resolving action {action} in layer {layer}")]
    RetryLimitReached { action: String, layer: String },
}

/// Errors that can occur while building the runtime.
#[derive(Debug, Error)]
pub enum RuntimeBuildError {
    #[error("Runtime is already running")]
    AlreadyRunning,
}

/// Errors that can occur while processing deltas in layers.
#[derive(Debug, Error)]
pub(crate) enum DeltaFlowError {
    #[error("Top layer {layer} failed while emitting delta: {reason}")]
    TopEmitFailed { layer: String, reason: String },
    #[error("Top layer {layer} received an unexpected incoming delta")]
    UnexpectedTopDelta { layer: String },
    #[error("Missing lower sender while propagating delta to layer {layer}")]
    MissingLowerSender { layer: String },
    #[error("Lower sender closed while propagating delta to layer {layer}")]
    LowerSenderClosed { layer: String },
    #[error("Layer {layer} failed while processing delta: {reason}")]
    MiddlePassFailed { layer: String, reason: String },
    #[error("Bottom layer {layer} failed while consuming delta: {reason}")]
    BottomConsumeFailed { layer: String, reason: String },
}

#[derive(Default)]
pub(crate) struct LayerRegistry {
    senders: HashMap<TypeId, mpsc::Sender<WorkerMessage>>,
    lower_by_upper: HashMap<TypeId, TypeId>,
    layer_names: HashMap<TypeId, &'static str>,
    next_snapshot: AtomicU64,
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
        });
        self.context = Context {
            registry,
            snapshot: None,
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

    pub async fn shutdown(mut self) {
        self.context = Context::default();
        for worker in self.inner.workers.drain(..) {
            worker.abort();
            let _ = worker.await;
        }
    }
}
