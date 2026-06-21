use std::{ops::Range, sync::OnceLock};

use crate::scheme::layer::NonTopLayer;

pub trait LayerChange: Send + Sync + 'static {
    type Address: Send + Sync + 'static;
    type Unit: Send + Sync + 'static;

    fn address(&self) -> &Self::Address;

    fn batch(&self) -> &ReplacementBatch<Self::Unit>;

    fn is_changed(&self) -> bool {
        self.batch().is_changed()
    }
}

impl LayerChange for () {
    type Address = ();
    type Unit = ();

    fn address(&self) -> &Self::Address {
        self
    }

    fn batch(&self) -> &ReplacementBatch<Self::Unit> {
        static BATCH: OnceLock<ReplacementBatch<()>> = OnceLock::new();
        BATCH.get_or_init(|| ReplacementBatch {
            old_units: Vec::new(),
            new_units: Vec::new(),
            prefix_len: 0,
            suffix_len: 0,
            old_changed_range: 0..0,
            new_changed_range: 0..0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementBatch<Unit> {
    pub old_units: Vec<Unit>,
    pub new_units: Vec<Unit>,
    pub prefix_len: usize,
    pub suffix_len: usize,
    pub old_changed_range: Range<usize>,
    pub new_changed_range: Range<usize>,
}

impl<Unit> ReplacementBatch<Unit> {
    pub fn is_changed(&self) -> bool {
        self.old_changed_range.start != self.old_changed_range.end
            || self.new_changed_range.start != self.new_changed_range.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementChange<Address, Unit> {
    pub address: Address,
    pub batch: ReplacementBatch<Unit>,
}

impl<Address, Unit> ReplacementChange<Address, Unit> {
    pub fn new(address: Address, batch: ReplacementBatch<Unit>) -> Self {
        Self { address, batch }
    }
}

impl<Address, Unit> LayerChange for ReplacementChange<Address, Unit>
where
    Address: Send + Sync + 'static,
    Unit: Send + Sync + 'static,
{
    type Address = Address;
    type Unit = Unit;

    fn address(&self) -> &Self::Address {
        &self.address
    }

    fn batch(&self) -> &ReplacementBatch<Self::Unit> {
        &self.batch
    }
}

pub type LayerChanges<L> = Vec<<L as NonTopLayer>::Change>;

pub struct EmittedChanges<L: NonTopLayer> {
    pub snapshot: crate::scheme::context::SnapshotId,
    pub changes: LayerChanges<L>,
}

impl<L: NonTopLayer> EmittedChanges<L> {
    pub fn new(snapshot: crate::scheme::context::SnapshotId, changes: LayerChanges<L>) -> Self {
        Self { snapshot, changes }
    }
}
