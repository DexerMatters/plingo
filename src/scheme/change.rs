//! Private sequence-diff vocabulary.
//!
//! Splices are implementation details of incremental ropes, lexers, and
//! parsers. They are not routed between components: graph nodes communicate by
//! observing materialized views.

use std::{ops::Range, sync::Arc};

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
