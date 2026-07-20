use std::{convert::Infallible, fmt::Display, future::Future, hash::Hash};

use crate::{
    marker::{HasAddress, HasError},
    scheme::{
        change::{FlowUnit, LayerChanges, Revision},
        context::{Context, SnapshotId},
        snapshot::SnapshotRetention,
    },
};

pub trait FallibleLayer: Sized + Send + Sync + 'static {
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

pub trait TopLayer: FallibleLayer<__Error = Self::Error> + SnapshotLayer {
    type Error: Display + Send + Sync + 'static;
    type Lower: NonTopLayer;

    fn emit<'a>(
        &'a mut self,
        ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<LayerChanges<Self::Lower>>, Self::Error>> + Send + 'a;

    fn rollback_transaction(&mut self, revision: Revision) {
        let _ = self.rollback_state(revision);
    }
}

pub trait NonTopLayer: FallibleLayer<__Error = Self::_Error> {
    type _Error: Display + Send + Sync + 'static;
    type Address: Eq + Hash + Send + Sync + 'static;
    type Unit: FlowUnit;
}

impl FallibleLayer for () {
    type __Error = Infallible;
}

impl NonTopLayer for () {
    type _Error = Infallible;
    type Address = ();
    type Unit = ();
}

impl<A, L> HasAddress<A> for L
where
    L: NonTopLayer<Address = A>,
{
    type Address = A;
}

pub trait SnapshotLayer {
    type State: Clone;

    fn initialize_snapshots(&mut self);
    fn push_state(&mut self, snapshot: SnapshotId);
    fn rollback_state(&mut self, revision: Revision) -> bool;
    fn state(&self, snapshot: Option<SnapshotId>) -> Option<&Self::State>;
    fn latest_state(&self) -> &Self::State;
    fn latest_state_mut(&mut self) -> &mut Self::State;
    fn set_snapshot_retention(&mut self, retention: SnapshotRetention);
    fn snapshot_retention(&self) -> SnapshotRetention;
}

pub trait MiddleLayer: NonTopLayer<_Error = Self::Error> + SnapshotLayer {
    type Lower: NonTopLayer;
    type Error: Display + Send + Sync + 'static;
    type Address: Eq + Hash + Send + Sync + 'static;
    type Unit: FlowUnit;

    fn pass(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<LayerChanges<Self::Lower>, Self::Error>> + Send;
}

pub trait BottomLayer: NonTopLayer<_Error = Self::Error> {
    type Error: Display + Send + Sync + 'static;
    type Address: Eq + Hash + Send + Sync + 'static;
    type Unit: FlowUnit;

    fn consume(
        &mut self,
        ctx: &Context,
        changes: LayerChanges<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
