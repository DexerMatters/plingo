//! Canonical frontier checkpoints (plan §8.5).
//!
//! A checkpoint is keyed by the stable parser gap and stores an exact,
//! identity-free GSS/product key. A compact fingerprint rejects most
//! candidates cheaply; equality of the canonical key is still mandatory.

use super::ParseColumn;
use crate::framework::parse::{
    data::{
        gss::{CanonicalFrontierCache, CanonicalFrontierKey, GssArena},
        product::ProductArena,
    },
    types::ParserBoundaryId,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FrontierCheckpoint {
    /// Stable parser-gap identity for this checkpoint.
    pub(crate) anchor: Option<ParserBoundaryId>,
    pub(crate) key: Option<CanonicalFrontierKey>,
    pub(crate) fingerprint: u64,
    pub(crate) error_derived: bool,
}

impl FrontierCheckpoint {
    /// Uses the fingerprint only as a rejection filter. Equal fingerprints
    /// still require exact boundary, recovery, and canonical-key equality.
    /// State-level convergence is proved separately by the paired frontier
    /// match (plan §5.6).
    pub(crate) fn exact_match(&self, other: &Self) -> bool {
        // Two keyless checkpoints (cyclic frontiers) prove nothing; the
        // state-level paired match handles them (plan §5.6).
        let (Some(left), Some(right)) = (&self.key, &other.key) else {
            return false;
        };
        self.anchor == other.anchor
            && self.error_derived == other.error_derived
            && (self.fingerprint == other.fingerprint && left == right)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ColumnCheckpointCache {
    frontier: Option<FrontierCheckpoint>,
}

impl ColumnCheckpointCache {
    pub(crate) fn invalidate(&mut self) {
        self.frontier = None;
    }

    pub(crate) fn frontier(&self) -> Option<&FrontierCheckpoint> {
        self.frontier.as_ref()
    }

    pub(crate) fn store(&mut self, checkpoint: FrontierCheckpoint) {
        self.frontier = Some(checkpoint);
    }
}

pub(crate) fn frontier_checkpoint_for_column<'a>(
    column: &'a mut ParseColumn,
    gss: &GssArena,
    products: &ProductArena,
    frontier_cache: &mut CanonicalFrontierCache,
) -> &'a FrontierCheckpoint {
    if column.cached_frontier_checkpoint().is_none() {
        let base = column.base_active_nodes().collect::<Vec<_>>();
        let active = column.active_nodes().collect::<Vec<_>>();
        let (key, fingerprint) = gss
            .canonical_frontier_cached((&base, &active), products, frontier_cache)
            .map(|frontier| (Some(frontier.key), frontier.fingerprint))
            .unwrap_or((None, 0));
        column.cache_frontier_checkpoint(FrontierCheckpoint {
            anchor: column.boundary,
            key,
            fingerprint,
            error_derived: column.error_derived,
        });
    }
    column
        .cached_frontier_checkpoint()
        .expect("frontier checkpoint cached")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::framework::parse::data::gss::CanonicalFrontierKey;

    #[test]
    fn fingerprint_collision_still_requires_exact_key() {
        let equal = FrontierCheckpoint {
            anchor: None,
            key: Some(CanonicalFrontierKey {
                base: Arc::from([]),
                active: Arc::from([]),
            }),
            fingerprint: 17,
            error_derived: false,
        };
        let unequal = FrontierCheckpoint {
            key: None,
            ..equal.clone()
        };
        assert!(!equal.exact_match(&unequal));
        assert!(equal.exact_match(&equal));
    }
}
