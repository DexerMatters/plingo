use std::{collections::HashSet, fmt, hash::Hash, ops::Range, sync::Arc};

use crate::scheme::{context::SnapshotId, layer::NonTopLayer};

pub trait FlowUnit: Send + Sync + 'static {
    fn extent(&self) -> usize;
}

impl FlowUnit for () {
    fn extent(&self) -> usize {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Revision {
    pub base: SnapshotId,
    pub target: SnapshotId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice<Unit> {
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    pub removed: Arc<[Unit]>,
    pub inserted: Arc<[Unit]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressChange<Address, Unit> {
    pub address: Address,
    pub old_extent: usize,
    pub new_extent: usize,
    pub splices: Vec<Splice<Unit>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet<Address, Unit> {
    pub revision: Revision,
    pub changes: Vec<AddressChange<Address, Unit>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidChangeSet(pub String);

impl fmt::Display for InvalidChangeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidChangeSet {}

impl<Address: Eq + Hash, Unit: FlowUnit> ChangeSet<Address, Unit> {
    pub fn empty(revision: Revision) -> Self {
        Self {
            revision,
            changes: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), InvalidChangeSet> {
        if self.revision.target <= self.revision.base {
            return Err(InvalidChangeSet(
                "transaction revision does not advance".into(),
            ));
        }
        let mut addresses = HashSet::with_capacity(self.changes.len());
        for change in &self.changes {
            if !addresses.insert(&change.address) {
                return Err(InvalidChangeSet("address appears more than once".into()));
            }
            let mut old_end = 0;
            let mut new_end = 0;
            for splice in &change.splices {
                if splice.old_range.start > splice.old_range.end
                    || splice.new_range.start > splice.new_range.end
                    || splice.old_range.end > change.old_extent
                    || splice.new_range.end > change.new_extent
                {
                    return Err(InvalidChangeSet("splice range exceeds its extent".into()));
                }
                if splice.old_range.is_empty() && splice.new_range.is_empty() {
                    return Err(InvalidChangeSet("splice is empty".into()));
                }
                if splice.old_range.start < old_end || splice.new_range.start < new_end {
                    return Err(InvalidChangeSet("splices overlap or are not sorted".into()));
                }
                if splice.old_range.start - old_end != splice.new_range.start - new_end {
                    return Err(InvalidChangeSet("unchanged gap extents differ".into()));
                }
                let removed = splice.removed.iter().map(FlowUnit::extent).sum::<usize>();
                let inserted = splice.inserted.iter().map(FlowUnit::extent).sum::<usize>();
                if removed != splice.old_range.len() || inserted != splice.new_range.len() {
                    return Err(InvalidChangeSet(
                        "splice payload extent disagrees with range".into(),
                    ));
                }
                old_end = splice.old_range.end;
                new_end = splice.new_range.end;
            }
            if old_end > change.old_extent
                || new_end > change.new_extent
                || change.old_extent - old_end != change.new_extent - new_end
            {
                return Err(InvalidChangeSet("unchanged suffix extents differ".into()));
            }
            if change.splices.is_empty() {
                return Err(InvalidChangeSet("address change is empty".into()));
            }
        }
        Ok(())
    }
}

pub type LayerChanges<L> = ChangeSet<<L as NonTopLayer>::Address, <L as NonTopLayer>::Unit>;
