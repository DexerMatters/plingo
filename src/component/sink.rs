use std::{convert::Infallible, marker::PhantomData, pin::Pin};

use plingo_macros::layer;

use crate::scheme::{BottomLayer, Context, LayerDeltas};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[layer]
pub struct DebugSink<K, V>
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    _marker: PhantomData<K>,
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
