use super::GssArena;
use crate::component::parse::data::product::{Product, ProductArena};

#[test]
fn cyclic_frontiers_compare_exactly() {
    let mut products = ProductArena::new();
    let old_product = products.insert(Product::token(1, 2, 3));
    let new_product = products.insert(Product::token(1, 4, 5));
    let mut arena = GssArena::new();
    let (a, b) = (arena.node(1, 0, 0), arena.node(2, 0, 0));
    let (c, d) = (arena.node(1, 4, 1), arena.node(2, 4, 1));
    arena.add_edge(a, b, old_product, 0);
    arena.add_edge(b, a, old_product, 0);
    arena.add_edge(c, d, new_product, 1);
    arena.add_edge(d, c, new_product, 1);

    let (_, product_mapping, _) = arena
        .match_frontiers((&[a], &[a]), (&[c], &[c]))
        .expect("control-equivalent cyclic frontiers");
    assert_eq!(product_mapping[&old_product], new_product);
}
