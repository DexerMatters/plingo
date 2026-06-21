use std::{convert::Infallible, fmt::Display, future::Future};

use crate::{
    marker::{HasAddress, HasError},
    scheme::{
        change::{EmittedChanges, LayerChange, LayerChanges},
        context::{Context, SnapshotId},
    },
};

/// A trait representing a layer in the pipeline.
///
/// Use `#[layer(top)]`, `#[layer(middle)]`, or `#[layer(bottom)]` to
/// auto-generate this impl.
pub trait FallibleLayer: Sized + Send + Sync + 'static {
    /// The type of errors that this layer can produce when resolving actions or
    /// processing deltas.
    type __Error: Display + Send + Sync + 'static;

    fn display() -> String {
        std::any::type_name::<Self>().to_string()
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
    ) -> impl Future<Output = Result<Option<EmittedChanges<Self::Lower>>, Self::Error>> + Send + 'a;
}

/// Marker trait for layers that may appear below another layer in the pipeline.
pub trait NonTopLayer: FallibleLayer<__Error = Self::_Error> {
    type _Error: Display + Send + Sync + 'static;
    type Change: LayerChange;
}

impl FallibleLayer for () {
    type __Error = Infallible;
}

impl NonTopLayer for () {
    type _Error = Infallible;
    type Change = ();
}

impl<A, L> HasAddress<A> for L
where
    L: NonTopLayer,
    L::Change: LayerChange<Address = A>,
{
    type Address = A;
}

pub trait SnapshotLayer {
    type State: Clone;

    fn push_state(&mut self, snapshot: SnapshotId);
    fn state(&self, snapshot: Option<SnapshotId>) -> Option<&Self::State>;
    fn latest_state(&self) -> &Self::State;
    fn latest_state_mut(&mut self) -> &mut Self::State;
}

/// A trait representing a middle layer.
pub trait MiddleLayer: NonTopLayer<_Error = Self::Error> {
    type Lower: NonTopLayer;
    type Error: Display + Send + Sync + 'static;
    type Change: LayerChange;

    fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send;
}

/// A trait representing a bottom layer.
pub trait BottomLayer: NonTopLayer<_Error = Self::Error> {
    type Error: Display + Send + Sync + 'static;
    type Change: LayerChange;

    fn consume(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
