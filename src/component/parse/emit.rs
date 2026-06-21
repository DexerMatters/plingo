use fluent_uri::Uri;

use crate::scheme::change::{ReplacementBatch, ReplacementChange};

use super::{ParseAddress, ParseChange, ParseUnit, ProductId};

pub(crate) fn insert_root(uri: Uri<&'static str>, roots: Vec<ProductId>) -> ParseChange {
    let new_units = roots
        .into_iter()
        .map(|product| ParseUnit { product })
        .collect::<Vec<_>>();
    ReplacementChange::new(
        ParseAddress {
            uri,
            parent_path: Vec::new(),
        },
        ReplacementBatch {
            old_units: Vec::new(),
            new_changed_range: 0..new_units.len(),
            new_units,
            prefix_len: 0,
            suffix_len: 0,
            old_changed_range: 0..0,
        },
    )
}

pub(crate) fn delete_root(uri: Uri<&'static str>, roots: Vec<ProductId>) -> ParseChange {
    let old_units = roots
        .into_iter()
        .map(|product| ParseUnit { product })
        .collect::<Vec<_>>();
    ReplacementChange::new(
        ParseAddress {
            uri,
            parent_path: Vec::new(),
        },
        ReplacementBatch {
            old_changed_range: 0..old_units.len(),
            old_units,
            new_units: Vec::new(),
            prefix_len: 0,
            suffix_len: 0,
            new_changed_range: 0..0,
        },
    )
}
