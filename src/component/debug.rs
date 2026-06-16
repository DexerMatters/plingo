use std::{convert::Infallible, marker::PhantomData, pin::Pin};

use plingo_macros::layer;

use crate::scheme::{BottomLayer, Context, LayerChange, LayerChanges, MiddleLayer, NonTopLayer};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[layer]
pub struct DebugSink<C>
where
    C: LayerChange,
{
    _marker: PhantomData<fn() -> C>,
    consume_fn: Box<
        dyn for<'a> Fn(
                &'a Context,
                LayerChanges<Self>,
            )
                -> Pin<Box<dyn Future<Output = Result<(), Infallible>> + Send + 'a>>
            + Send
            + Sync,
    >,
}

impl<C> DebugSink<C>
where
    C: LayerChange,
{
    pub fn new<ConsumeFn>(consume_fn: ConsumeFn) -> Self
    where
        ConsumeFn: for<'a> Fn(&'a Context, LayerChanges<Self>) -> BoxFuture<'a, Result<(), Infallible>>
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
impl<C> BottomLayer for DebugSink<C>
where
    C: LayerChange,
{
    type Error = Infallible;
    type Change = C;

    fn consume(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        (self.consume_fn)(ctx, changes)
    }
}

#[layer]
pub struct DebugRelay<C, Lower>
where
    C: LayerChange,
    Lower: NonTopLayer<Change = C> + Send + Sync + 'static,
{
    _marker: PhantomData<fn() -> (C, Lower)>,
    investigate_fn: Box<dyn for<'a> Fn(&'a Context, &'a LayerChanges<Self>) + Send + Sync>,
}

impl<C, Lower> DebugRelay<C, Lower>
where
    C: LayerChange,
    Lower: NonTopLayer<Change = C> + Send + Sync + 'static,
{
    pub fn new<InvestigateFn>(investigate_fn: InvestigateFn) -> Self
    where
        InvestigateFn: for<'a> Fn(&'a Context, &'a LayerChanges<Self>) + Send + Sync + 'static,
    {
        Self {
            _marker: PhantomData,
            investigate_fn: Box::new(investigate_fn),
        }
    }
}

#[layer(middle)]
impl<C, Lower> MiddleLayer for DebugRelay<C, Lower>
where
    C: LayerChange,
    Lower: NonTopLayer<Change = C> + Send + Sync + 'static,
{
    type Lower = Lower;
    type Error = Infallible;
    type Change = C;

    async fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> Result<LayerChanges<Self::Lower>, Self::Error> {
        (self.investigate_fn)(ctx, &changes);
        Ok(changes)
    }
}
