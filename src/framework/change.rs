//! Private sequence-diff vocabulary (moved from `scheme::change`).
//!
//! Splices are implementation details of incremental ropes, lexers, and
//! parsers. The framework's lexer and parser keep them here so that
//! `src/framework` never depends on `scheme`.

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