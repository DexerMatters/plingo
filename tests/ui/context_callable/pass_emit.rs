use std::{fmt, future::Future};

use plingo::{
    context_callable,
    scheme::{
        call::CallOutcome,
        change::{LayerChanges, Revision},
        context::Context,
        layer::{FallibleLayer, NonTopLayer, SnapshotLayer, TopLayer},
        snapshot::SnapshotRetention,
    },
};

#[derive(Debug, Clone)]
struct DummyError;

impl fmt::Display for DummyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dummy error")
    }
}

struct Lower;

impl FallibleLayer for Lower {
    type __Error = DummyError;
}

impl NonTopLayer for Lower {
    type _Error = DummyError;
    type Address = ();
    type Unit = ();
}

struct Top;

impl FallibleLayer for Top {
    type __Error = DummyError;
}

impl SnapshotLayer for Top {
    type State = ();

    fn initialize_snapshots(&mut self) {}
    fn push_state(&mut self, _: u64) {}
    fn rollback_state(&mut self, _: Revision) -> bool { true }
    fn state(&self, _: Option<u64>) -> Option<&Self::State> { Some(&()) }
    fn latest_state(&self) -> &Self::State { &() }
    fn latest_state_mut(&mut self) -> &mut Self::State { panic!("fixture has no state") }
    fn set_snapshot_retention(&mut self, _: SnapshotRetention) {}
    fn snapshot_retention(&self) -> SnapshotRetention { SnapshotRetention::default() }
}

impl TopLayer for Top {
    type Error = DummyError;
    type Lower = Lower;

    fn emit<'a>(
        &'a mut self,
        _ctx: &'a Context,
    ) -> impl Future<Output = Result<Option<LayerChanges<Self::Lower>>, Self::Error>> + Send + 'a
    {
        async { Ok(None) }
    }
}

impl Top {
    #[context_callable]
    pub async fn propagate<'a>(
        &'a mut self,
        _ctx: &'a Context,
        _value: &'a usize,
    ) -> CallOutcome<Self, ()> {
        CallOutcome::emit(LayerChanges::<Lower>::empty(Revision { base: 0, target: 1 }))
    }
}

fn main() {}
