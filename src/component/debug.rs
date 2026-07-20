use std::{convert::Infallible, hash::Hash, marker::PhantomData, pin::Pin};

use plingo_macros::layer;

use crate::scheme::{
    change::{FlowUnit, LayerChanges},
    context::Context,
    layer::BottomLayer,
};

#[layer]
pub struct DebugSink<Address, Unit>
where
    Address: Eq + Hash + Send + Sync + 'static,
    Unit: FlowUnit,
{
    _marker: PhantomData<fn() -> (Address, Unit)>,
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

impl<Address, Unit> DebugSink<Address, Unit>
where
    Address: Eq + Hash + Send + Sync + 'static,
    Unit: FlowUnit,
{
    pub fn new<ConsumeFn>(consume_fn: ConsumeFn) -> Self
    where
        ConsumeFn: for<'a> Fn(
                &'a Context,
                LayerChanges<Self>,
            )
                -> Pin<Box<dyn Future<Output = Result<(), Infallible>> + Send + 'a>>
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
impl<Address, Unit> BottomLayer for DebugSink<Address, Unit>
where
    Address: Eq + Hash + Send + Sync + 'static,
    Unit: FlowUnit,
{
    type Error = Infallible;
    type Address = Address;
    type Unit = Unit;

    fn consume(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        (self.consume_fn)(ctx, changes)
    }
}
