use std::{convert::Infallible, marker::PhantomData, pin::Pin};

use plingo_macros::{layer, resolve_action};

use crate::scheme::{BottomLayer, Context, LayerDeltas, Outcome, Resolve};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct DebugSink<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    _marker: PhantomData<K>,
    resolve_fn: Box<
        dyn for<'a> Fn(
                &'a Context,
                &'a K,
            ) -> Pin<Box<dyn Future<Output = Outcome<K, Self>> + Send + 'a>>
            + Send
            + Sync,
    >,
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
    pub fn new<ResolveFn, ConsumeFn>(resolve_fn: ResolveFn, consume_fn: ConsumeFn) -> Self
    where
        ResolveFn: for<'a> Fn(&'a Context, &'a K) -> BoxFuture<'a, Outcome<K, Self>>
            + Send
            + Sync
            + 'static,
        ConsumeFn: for<'a> Fn(&'a Context, LayerDeltas<Self>) -> BoxFuture<'a, Result<(), Infallible>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            _marker: PhantomData,
            resolve_fn: Box::new(resolve_fn),
            consume_fn: Box::new(consume_fn),
        }
    }
}

#[resolve_action]
impl<K, V> Resolve<K> for DebugSink<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    type Output = V;

    fn resolve<'a>(
        &'a self,
        ctx: &'a Context,
        action: &'a K,
    ) -> impl Future<Output = Outcome<K, Self>> + Send + 'a {
        (self.resolve_fn)(ctx, action)
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

    fn consume(
        &mut self,
        ctx: &Context,
        deltas: LayerDeltas<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        (self.consume_fn)(ctx, deltas)
    }
}
