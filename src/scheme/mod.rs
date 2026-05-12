use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    error::Error,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::scheme::marker::*;
use crate::scheme::message::*;

pub mod marker;
mod message;

/// Context shared by all layers which can be used to resolve getters and access
/// the registry.
#[derive(Clone)]
pub struct Context {
    pub(crate) registry: Arc<LayerRegistry>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            registry: Arc::new(LayerRegistry::default()),
        }
    }
}

impl Context {
    /// Request the output of a getter `G` from the layer `L` that can resolve it.
    pub async fn get<L, G>(&self, getter: G) -> Result<<G as Getter<L>>::Output, GetterError>
    where
        L: AnyLayer + 'static,
        G: Getter<L> + Send + Sync + 'static,
        <G as Getter<L>>::Output: Send + Sync + 'static,
    {
        let getter_name = type_name::<G>().to_string();
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
            .ok_or_else(|| missing_getter_resource(&getter_name, layer_name))?;

        let (response_tx, response_rx) = oneshot::channel();
        let demand = Demand {
            getter: Arc::new(getter),
            getter_name: type_name::<G>(),
            expected_output_type: TypeId::of::<<G as Getter<L>>::Output>(),
            expected_output_name: type_name::<<G as Getter<L>>::Output>(),
            origin_layer_type: layer_type,
            remaining_retries: DEFAULT_DEMAND_RETRY_BUDGET,
            response_tx,
        };

        sender
            .send(WorkerMessage::Demand(demand))
            .await
            .map_err(|_| channel_closed(&getter_name, layer_name))?;

        let erased = response_rx
            .await
            .map_err(|_| channel_closed(&getter_name, layer_name))??;

        if erased.output_type != TypeId::of::<<G as Getter<L>>::Output>() {
            return Err(output_type_mismatch(
                &getter_name,
                layer_name,
                type_name::<<G as Getter<L>>::Output>(),
                erased.output_name,
            ));
        }

        let typed = erased
            .value
            .downcast::<<G as Getter<L>>::Output>()
            .map_err(|_| {
                output_type_mismatch(
                    &getter_name,
                    layer_name,
                    type_name::<<G as Getter<L>>::Output>(),
                    erased.output_name,
                )
            })?;

        Ok(*typed)
    }
}

/// Delta representing an insertion, deletion, or update of a key-value pair.
pub enum Delta<K, V> {
    Insert { key: K, value: V },
    Delete { key: K },
    Update { key: K, value: V },
}

/// A batch of deltas — the actual unit of transmission between layers.
pub type Deltas<K, V> = Vec<Delta<K, V>>;

/// Convenience type alias for a batch of deltas for a specific non-top layer `L`.
pub type LayerDeltas<L> =
    Deltas<<L as NonTopLayer>::Key, <<L as NonTopLayer>::Key as Getter<L>>::Output>;

/// Getter is a request for a specific resource that the targeted layer `L` can
/// provide. The output type is determined by the getter type and the layer's
/// implementation of `Getter<L>`.
///
/// For example, the layer for source text might implement
/// `Getter<SourceTextLayer>` for a getter type `GetSourceText { uri: String }`
/// with output type `String`, allowing other layers to request source text by
/// URI.
pub trait Getter<L: AnyLayer> {
    /// The type of the value produced by this getter when resolved by layer `L`.
    type Output;
}

/// The result of a top layer's attempt to resolve a getter.
pub enum TopOutcome<G: Getter<L>, L: TopLayer> {
    /// The layer resolved the getter directly.
    Resolved(G::Output),
    /// The top layer emitted deltas as a side effect of resolving the getter.
    ///
    /// Semantically this is equivalent to [`Outcome::Updated`], except the delta
    /// batch targets the layer directly below the top layer.
    Emitted(LayerDeltas<L::Lower>),
    /// The layer encountered an error.
    Failed(L::Error),
}

/// The result of a middle or bottom layer's attempt to resolve a getter.
pub enum Outcome<G: Getter<L>, L: NonTopLayer> {
    /// The layer resolved the demand directly.
    Resolved(G::Output),
    /// The layer produced side-effect deltas; retry demand after applying them.
    Updated(LayerDeltas<L>),
    /// Forward the demand upward.
    Forwarded,
    /// The layer encountered an error.
    Failed(L::Error),
}

/// An erased version of
/// [`Outcome`] or [`TopOutcome`] that can be sent across layers. undefined
///
/// It can be built from [`AnyGetter`]. undefined
pub struct ErasedOutcome<L: AnyLayer> {
    inner: ErasedOutcomeKind<L>,
}

pub(crate) enum ErasedOutcomeKind<L: AnyLayer> {
    Resolved(ErasedOutput),
    Updated(Box<dyn Any + Send + Sync>),
    Emitted(Box<dyn Any + Send + Sync>),
    Forwarded,
    Failed(L::Error),
}

impl<L: AnyLayer> ErasedOutcome<L> {
    pub(crate) fn from_kind(inner: ErasedOutcomeKind<L>) -> Self {
        Self { inner }
    }

    pub(crate) fn into_kind(self) -> ErasedOutcomeKind<L> {
        self.inner
    }

    pub(crate) fn forwarded() -> Self {
        Self::from_kind(ErasedOutcomeKind::Forwarded)
    }
}

#[doc(hidden)]
#[sealed::sealed]
pub trait IntoErasedOutcome<L: AnyLayer> {
    fn into_erased_outcome(self) -> ErasedOutcome<L>;
}

#[sealed::sealed]
impl<G, L> IntoErasedOutcome<L> for Outcome<G, L>
where
    G: Getter<L>,
    L: NonTopLayer,
    <G as Getter<L>>::Output: Send + Sync + 'static,
    <L::Key as Getter<L>>::Output: Send + Sync + 'static,
{
    fn into_erased_outcome(self) -> ErasedOutcome<L> {
        match self {
            Outcome::Resolved(value) => {
                ErasedOutcome::from_kind(ErasedOutcomeKind::Resolved(ErasedOutput {
                    value: Box::new(value),
                    output_type: TypeId::of::<<G as Getter<L>>::Output>(),
                    output_name: type_name::<<G as Getter<L>>::Output>(),
                }))
            }
            Outcome::Updated(deltas) => {
                ErasedOutcome::from_kind(ErasedOutcomeKind::Updated(Box::new(deltas)))
            }
            Outcome::Forwarded => ErasedOutcome::forwarded(),
            Outcome::Failed(err) => ErasedOutcome::from_kind(ErasedOutcomeKind::Failed(err)),
        }
    }
}

#[sealed::sealed]
impl<G, L> IntoErasedOutcome<L> for TopOutcome<G, L>
where
    G: Getter<L>,
    L: TopLayer,
    <G as Getter<L>>::Output: Send + Sync + 'static,
    <<L::Lower as NonTopLayer>::Key as Getter<L::Lower>>::Output: Send + Sync + 'static,
{
    fn into_erased_outcome(self) -> ErasedOutcome<L> {
        match self {
            TopOutcome::Resolved(value) => {
                ErasedOutcome::from_kind(ErasedOutcomeKind::Resolved(ErasedOutput {
                    value: Box::new(value),
                    output_type: TypeId::of::<<G as Getter<L>>::Output>(),
                    output_name: type_name::<<G as Getter<L>>::Output>(),
                }))
            }
            TopOutcome::Emitted(deltas) => {
                ErasedOutcome::from_kind(ErasedOutcomeKind::Emitted(Box::new(deltas)))
            }
            TopOutcome::Failed(err) => ErasedOutcome::from_kind(ErasedOutcomeKind::Failed(err)),
        }
    }
}

/// A type-erased getter that can be downcast to its concrete type for resolution.
pub struct AnyGetter<'a, L: AnyLayer> {
    getter: &'a (dyn Any + Send + Sync),
    matched: Option<Pin<Box<dyn Future<Output = ErasedOutcome<L>> + Send + 'a>>>,
    _layer: PhantomData<fn() -> L>,
}

impl<'a, L: AnyLayer> AnyGetter<'a, L> {
    pub(crate) fn new(getter: &'a (dyn Any + Send + Sync)) -> Self {
        Self {
            getter,
            matched: None,
            _layer: PhantomData,
        }
    }

    /// Attempt to match this [`AnyGetter`] against a specific getter type `G` for
    /// layer `L`, and if it matches, resolve the getter using the provided
    /// async function `f`.
    pub fn case<G, F, Fut, O>(mut self, f: F) -> Self
    where
        G: Getter<L> + Send + Sync + 'static,
        F: FnOnce(&G) -> Fut + Send + 'a,
        Fut: Future<Output = O> + Send + 'a,
        O: IntoErasedOutcome<L> + 'a,
    {
        if self.matched.is_some() {
            return self;
        }

        let Some(typed_getter) = self.getter.downcast_ref::<G>() else {
            return self;
        };

        self.matched = Some(Box::pin(async move {
            let outcome = f(typed_getter).await;
            IntoErasedOutcome::into_erased_outcome(outcome)
        }));

        self
    }

    /// Finalize the matching process by providing a default async function `f`
    /// to resolve the getter if no cases matched.
    pub fn finally<F, Fut, O>(
        self,
        f: F,
    ) -> Pin<Box<dyn Future<Output = ErasedOutcome<L>> + Send + 'a>>
    where
        F: FnOnce() -> Fut + Send + 'a,
        Fut: Future<Output = O> + Send + 'a,
        O: IntoErasedOutcome<L> + 'a,
    {
        match self.matched {
            Some(fut) => fut,
            None => Box::pin(async move {
                let outcome = f().await;
                IntoErasedOutcome::into_erased_outcome(outcome)
            }),
        }
    }
}

/// A trait representing a layer in the pipeline. Each layer defines a key type
/// and an error type, and implements methods to resolve getters and process
/// deltas.
///
/// This trait is sealed. Implement one of the subtraits `TopLayer`,
/// `MiddleLayer`, or `BottomLayer` depending on the layer's role in the
/// pipeline.
#[sealed::sealed]
pub trait AnyLayer: Sized + Send + Sync + 'static {
    /// The type of errors that this layer can produce when resolving getters or
    /// processing deltas.
    type Error: Error + Send + Sync + 'static;

    /// Resolve a getter dynamically for this layer's resource.
    ///
    /// The outcome can be established by matching on the getter's concrete type
    /// using the provided `AnyGetter` API.
    ///
    /// By default, this method returns an erased forwarded outcome, meaning the
    /// demand will be forwarded upward to the next layer when possible.
    fn resolve<'a>(
        &'a self,
        _ctx: &'a Context,
        _getter: AnyGetter<'a, Self>,
    ) -> impl Future<Output = ErasedOutcome<Self>> + Send + 'a {
        async { ErasedOutcome::forwarded() }
    }

    /// A human-readable name for this layer, used in error messages. By
    /// default, this is the Rust type name of the layer.
    fn display() -> String {
        type_name::<Self>().to_string()
    }
}

/// A trait representing a top layer, which produces deltas from an external
/// source.
///
/// Only a top layer can emit deltas without receiving any input deltas, so it
/// is responsible for producing the initial data that flows through the
/// pipeline. For example, a top layer might read from a file, a network socket,
/// or another external source.
///
/// The building of the pipeline must start with a top layer, and there can only
/// be one top layer in the pipeline.
pub trait TopLayer: AnyLayer {
    /// The layer directly below this one.
    type Lower: NonTopLayer;

    /// Produce the next batch of deltas from an external source, expressed as
    /// deltas for the layer directly below. Returns `None` when the source is
    /// exhausted / closed.
    fn emit(
        &mut self,
        ctx: &Context,
    ) -> impl Future<Output = Result<Option<LayerDeltas<Self::Lower>>, Self::Error>> + Send;
}

/// Marker trait for layers that may appear below another layer in the pipeline.
/// Top layers must not implement this trait.
///
/// This trait is sealed.
#[sealed::sealed]
pub trait NonTopLayer: AnyLayer {
    /// The key getter that defines this layer's delta shape.
    type Key: Getter<Self> + Send + Sync + 'static;
}

/// A trait representing a middle layer, which transforms an incoming delta to
/// an outgoing delta.
///
/// A middle layer receives deltas from the layer above it, processes them, and
/// passes them down to the layer below it. For example, a middle layer might
/// take source text deltas from a source text layer and produce syntax tree
/// deltas for a syntax tree layer.
pub trait MiddleLayer: NonTopLayer {
    /// The layer directly below this one.
    type Lower: NonTopLayer;

    /// Process an incoming batch of deltas from the upper layer and produce an
    /// outgoing batch for the lower layer.
    fn pass(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<LayerDeltas<Self::Lower>, Self::Error>> + Send;
}

/// A trait representing a bottom layer, which consumes deltas without producing
/// any output deltas.
///
/// A bottom layer receives deltas from the layer above it and processes them
/// without passing anything further down the pipeline.
pub trait BottomLayer: NonTopLayer {
    fn consume(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// A enum representing errors that can occur while resolving getters in layers.
#[derive(Debug, Error)]
pub enum GetterError {
    #[error("Missing resource for getter {getter} while resolving in layer {layer}")]
    MissingResource { getter: String, layer: String },
    #[error("Layer {layer} failed while resolving getter {getter}: {reason}")]
    ErrorFromLayer {
        getter: String,
        layer: String,
        reason: String,
    },
    #[error(
        "Output type mismatch for getter {getter} at layer {layer}: expected {expected}, got {actual}"
    )]
    OutputTypeMismatch {
        getter: String,
        layer: String,
        expected: String,
        actual: String,
    },
    #[error("Layer channel closed while resolving getter {getter} in layer {layer}")]
    ChannelClosed { getter: String, layer: String },
    #[error("Retry limit reached while resolving getter {getter} in layer {layer}")]
    RetryLimitReached { getter: String, layer: String },
}

/// A enum representing errors that can occur while building the runtime.
#[derive(Debug, Error)]
pub enum RuntimeBuildError {
    #[error("Runtime is already running")]
    AlreadyRunning,
}

/// A enum representing errors that can occur while processing deltas in layers.
#[derive(Debug, Error)]
pub(crate) enum DeltaFlowError {
    #[error("Top layer {layer} failed while emitting delta: {reason}")]
    TopEmitFailed { layer: String, reason: String },
    #[error("Missing lower sender while propagating delta to layer {layer}")]
    MissingLowerSender { layer: String },
    #[error("Lower sender closed while propagating delta to layer {layer}")]
    LowerSenderClosed { layer: String },
    #[error("Layer {layer} failed while processing delta: {reason}")]
    MiddlePassFailed { layer: String, reason: String },
    #[error("Bottom layer {layer} failed while consuming delta: {reason}")]
    BottomConsumeFailed { layer: String, reason: String },
}

#[derive(Clone, Default)]
pub(crate) struct LayerRegistry {
    senders: HashMap<TypeId, mpsc::Sender<WorkerMessage>>,
    lower_by_upper: HashMap<TypeId, TypeId>,
    upper_by_lower: HashMap<TypeId, TypeId>,
    layer_names: HashMap<TypeId, &'static str>,
}

struct RuntimeInner {
    specs: HashMap<TypeId, LayerSpec>,
    lower_by_upper: HashMap<TypeId, TypeId>,
    upper_by_lower: HashMap<TypeId, TypeId>,
    layer_names: HashMap<TypeId, &'static str>,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl RuntimeInner {
    fn new() -> Self {
        Self {
            specs: HashMap::new(),
            lower_by_upper: HashMap::new(),
            upper_by_lower: HashMap::new(),
            layer_names: HashMap::new(),
            workers: Vec::new(),
        }
    }
}

/// This is the main entry point for building and running a plingo pipeline. It
/// maintains the registry of layers and their communication channels, and
/// provides the API for constructing the pipeline by registering layers in
/// order.
///
/// [`Runtime::run`] is only exposed on [`Runtime<Sealed>`] to ensure that the
/// pipeline is fully constructed with at least one top layer and a bottom layer
/// before it can be run.
pub struct Runtime<S = NeedsTop> {
    inner: RuntimeInner,
    context: Context,
    _state: PhantomData<fn() -> S>,
}

impl Runtime<NeedsTop> {
    /// Create a new [`Runtime`] in the initial state, with no layers registered.
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
        <<T::Lower as NonTopLayer>::Key as Getter<T::Lower>>::Output: Send + Sync + 'static,
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

impl<Upper: AnyLayer, Edge: AnyLayer> Runtime<Linked<Upper, Edge>> {
    /// Attach a middle layer to the runtime.
    pub fn with(mut self, layer: Edge) -> Runtime<Linked<Edge, Edge::Lower>>
    where
        Edge: MiddleLayer,
        <Edge::Key as Getter<Edge>>::Output: Send + Sync + 'static,
        <<Edge::Lower as NonTopLayer>::Key as Getter<Edge::Lower>>::Output: Send + Sync + 'static,
    {
        let upper_type = TypeId::of::<Upper>();
        let layer_type = TypeId::of::<Edge>();
        let layer_name = type_name::<Edge>();

        self.inner.layer_names.insert(layer_type, layer_name);
        self.inner.lower_by_upper.insert(upper_type, layer_type);
        self.inner.upper_by_lower.insert(layer_type, upper_type);

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
        <Edge::Key as Getter<Edge>>::Output: Send + Sync + 'static,
    {
        let upper_type = TypeId::of::<Upper>();
        let layer_type = TypeId::of::<Edge>();
        let layer_name = type_name::<Edge>();

        self.inner.layer_names.insert(layer_type, layer_name);
        self.inner.lower_by_upper.insert(upper_type, layer_type);
        self.inner.upper_by_lower.insert(layer_type, upper_type);

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
    /// Run the runtime.
    pub async fn run(mut self) -> Result<Self, RuntimeBuildError> {
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
            upper_by_lower: self.inner.upper_by_lower.clone(),
            layer_names: self.inner.layer_names.clone(),
        });
        self.context = Context { registry };

        let specs = std::mem::take(&mut self.inner.specs);
        for (layer_type, spec) in specs {
            let receiver = receivers
                .remove(&layer_type)
                .expect("receiver must exist for every registered layer");
            let worker = (spec.start_worker)(self.context.clone(), receiver);
            self.inner.workers.push(worker);
        }

        Ok(self)
    }

    /// Get a reference to the runtime's shared context, which can be used to
    /// resolve getters.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Shutdown the runtime.
    pub async fn shutdown(mut self) {
        self.context = Context::default();
        for worker in self.inner.workers.drain(..) {
            worker.abort();
            let _ = worker.await;
        }
    }
}
