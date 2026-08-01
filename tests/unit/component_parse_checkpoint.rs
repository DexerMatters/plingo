use indexmap::IndexSet;

use super::frontier_checkpoint_for_column;
use crate::component::parse::{
    data::{
        gss::GssArena,
        product::{Product, ProductArena},
    },
    parsing::ParseColumn,
};

#[test]
fn frontier_checkpoint_is_cached_and_invalidated() {
    let mut products = ProductArena::new();
    let product = products.insert(Product::token(7, 11, 13));
    let mut gss = GssArena::new();
    let start = gss.node(0, 0, 0);
    let shifted = gss.node(1, 0, 0);
    assert!(gss.add_edge(shifted, start, product, 0));

    let mut column = ParseColumn::new(Some(0), IndexSet::from([shifted]));
    let first = frontier_checkpoint_for_column(&mut column, &mut gss).clone();
    let second = frontier_checkpoint_for_column(&mut column, &mut gss).clone();
    assert_eq!(first, second);

    column.set_error_derived();
    let changed = frontier_checkpoint_for_column(&mut column, &mut gss).clone();
    assert_ne!(first, changed);
}
