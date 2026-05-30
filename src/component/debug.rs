use std::{convert::Infallible, marker::PhantomData, pin::Pin};

use plingo_macros::layer;

use crate::scheme::{BottomLayer, Context, LayerDeltas, MiddleLayer, NonTopLayer};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[layer]
pub struct DebugSink<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    _marker: PhantomData<fn() -> (K, V)>,
    consume_fn: Box<
        dyn for<'a> Fn(
                &'a Context,
                LayerDeltas<Self>,
            )
                -> Pin<Box<dyn Future<Output = Result<(), Infallible>> + Send + 'a>>
            + Send
            + Sync,
    >,
}

impl<K, V> DebugSink<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn new<ConsumeFn>(consume_fn: ConsumeFn) -> Self
    where
        ConsumeFn: for<'a> Fn(&'a Context, LayerDeltas<Self>) -> BoxFuture<'a, Result<(), Infallible>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            _marker: PhantomData,
            consume_fn: Box::new(consume_fn),
        }
    }
}

#[layer(bottom)]
impl<K, V> BottomLayer for DebugSink<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    type Error = Infallible;
    type Key = K;
    type Value = V;

    fn consume(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        (self.consume_fn)(ctx, deltas)
    }
}

#[layer]
pub struct DebugRelay<K, V, Lower>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
    Lower: NonTopLayer<_Key = K, _Value = V> + Send + Sync + 'static,
{
    _marker: PhantomData<fn() -> (K, V, Lower)>,
    investigate_fn: Box<dyn for<'a> Fn(&'a Context, &'a LayerDeltas<Self>) + Send + Sync>,
}

impl<K, V, Lower> DebugRelay<K, V, Lower>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
    Lower: NonTopLayer<_Key = K, _Value = V> + Send + Sync + 'static,
{
    pub fn new<InvestigateFn>(investigate_fn: InvestigateFn) -> Self
    where
        InvestigateFn: for<'a> Fn(&'a Context, &'a LayerDeltas<Self>) + Send + Sync + 'static,
    {
        Self {
            _marker: PhantomData,
            investigate_fn: Box::new(investigate_fn),
        }
    }
}

#[layer(middle)]
impl<K, V, Lower> MiddleLayer for DebugRelay<K, V, Lower>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
    Lower: NonTopLayer<_Key = K, _Value = V> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Key = K;
    type Error = Infallible;
    type Value = V;

    async fn pass(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> Result<LayerDeltas<Self::Lower>, Self::Error> {
        (self.investigate_fn)(ctx, &deltas);
        Ok(deltas)
    }
}
