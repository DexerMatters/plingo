use std::{collections::BTreeMap, sync::Arc};

use super::{change::Revision, context::SnapshotId};

/// Number of committed revisions retained by each stateful layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRetention(usize);

impl SnapshotRetention {
    pub const fn new(limit: usize) -> Self {
        Self(if limit < 2 { 2 } else { limit })
    }

    pub const fn limit(self) -> usize {
        self.0
    }
}

impl Default for SnapshotRetention {
    fn default() -> Self {
        Self::new(64)
    }
}

/// Exact, bounded state history. Expired revisions are absent by design; callers
/// must never substitute a different revision for an absent one.
pub struct SnapshotStore<State> {
    entries: BTreeMap<SnapshotId, Arc<State>>,
    retention: SnapshotRetention,
}

impl<State> Default for SnapshotStore<State> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            retention: SnapshotRetention::default(),
        }
    }
}

impl<State> SnapshotStore<State> {
    pub fn initialize(&mut self, state: Arc<State>) {
        self.entries.entry(0).or_insert(state);
    }

    pub fn insert(&mut self, revision: SnapshotId, state: Arc<State>) {
        self.entries.insert(revision, state);
        while self.entries.len() > self.retention.limit() {
            self.entries.pop_first();
        }
    }

    pub fn get(&self, revision: SnapshotId) -> Option<&State> {
        self.entries.get(&revision).map(Arc::as_ref)
    }

    pub fn rollback(&mut self, revision: Revision) -> Option<Arc<State>> {
        let base = Arc::clone(self.entries.get(&revision.base)?);
        self.entries.remove(&revision.target);
        Some(base)
    }

    pub fn set_retention(&mut self, retention: SnapshotRetention) {
        self.retention = retention;
        while self.entries.len() > retention.limit() {
            self.entries.pop_first();
        }
    }

    pub fn retention(&self) -> SnapshotRetention {
        self.retention
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_exact_states_and_expires_oldest_revisions() {
        let mut store = SnapshotStore::default();
        store.set_retention(SnapshotRetention::new(2));
        store.initialize(Arc::new(0));
        store.insert(1, Arc::new(1));
        store.insert(2, Arc::new(2));
        assert_eq!(store.get(0), None);
        assert_eq!(store.get(1), Some(&1));
        assert_eq!(store.get(2), Some(&2));
    }

    #[test]
    fn rollback_restores_only_the_transaction_base() {
        let mut store = SnapshotStore::default();
        store.initialize(Arc::new(0));
        store.insert(1, Arc::new(1));
        assert_eq!(
            store.rollback(Revision { base: 0, target: 1 }).as_deref(),
            Some(&0)
        );
        assert_eq!(store.get(1), None);
        assert_eq!(store.get(0), Some(&0));
    }
}
