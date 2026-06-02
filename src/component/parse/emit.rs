use fluent_uri::Uri;

use crate::{
    scheme::Delta,
    utils::RangeOrPoint,
};

use super::{ParseForest, ParsePath, ProductId};

pub(crate) fn insert_root(uri: Uri<&'static str>, roots: Vec<ProductId>) -> Delta<ParsePath, ParseForest> {
    Delta::Insert {
        key: ParsePath {
            uri,
            path: Vec::new(),
            range: RangeOrPoint::Point(0),
        },
        value: ParseForest { roots },
    }
}

pub(crate) fn delete_root(uri: Uri<&'static str>, root_count: usize) -> Delta<ParsePath, ParseForest> {
    Delta::Delete {
        key: ParsePath {
            uri,
            path: Vec::new(),
            range: RangeOrPoint::from_range(0, root_count),
        },
    }
}

pub(crate) fn replace_root(
    uri: Uri<&'static str>,
    roots: Vec<ProductId>,
    root_count: usize,
) -> Vec<Delta<ParsePath, ParseForest>> {
    vec![delete_root(uri.clone(), root_count), insert_root(uri, roots)]
}
