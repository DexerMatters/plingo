use std::collections::HashMap;

use fluent_uri::Uri;

use super::{ParseForest, ParsePath, ProductId};
use super::data::{GreenId, ProductArena, TreeArena, TreeData};
use crate::scheme::Delta;
use crate::utils::RangeOrPoint;

fn build_maps(products: &ProductArena) -> (HashMap<GreenId, ProductId>, HashMap<GreenId, ProductId>) {
    let mut first: HashMap<GreenId, ProductId> = HashMap::new();
    let mut last: HashMap<GreenId, ProductId> = HashMap::new();
    for (i, p) in products.products.iter().enumerate() {
        first.entry(p.green).or_insert(i);
        last.insert(p.green, i);
    }
    (first, last)
}

pub(crate) fn diff_trees(
    products: &ProductArena,
    trees: &TreeArena,
    old_roots: &[ProductId],
    new_roots: &[ProductId],
    uri: Uri<&'static str>,
) -> Vec<Delta<ParsePath, ParseForest>> {
    if old_roots == new_roots {
        return Vec::new();
    }

    let (first_g2p, last_g2p) = build_maps(products);
    let mut deltas = Vec::new();

    let max = old_roots.len().max(new_roots.len());
    for i in 0..max {
        let path = vec![i];
        match (old_roots.get(i), new_roots.get(i)) {
            (Some(&o), Some(&n)) if o == n => {}
            (Some(&o), Some(&n)) => {
                diff_node(products, trees, &first_g2p, &last_g2p, o, n, &path, uri, &mut deltas);
            }
            (Some(_), None) => {
                deltas.push(Delta::Delete {
                    key: ParsePath {
                        uri,
                        path,
                        range: RangeOrPoint::Point(0),
                    },
                });
            }
            (None, Some(&n)) => {
                deltas.push(Delta::Insert {
                    key: ParsePath {
                        uri,
                        path,
                        range: RangeOrPoint::Point(0),
                    },
                    value: ParseForest { roots: vec![n] },
                });
            }
            (None, None) => unreachable!(),
        }
    }

    deltas
}

fn diff_node(
    products: &ProductArena,
    trees: &TreeArena,
    first_g2p: &HashMap<GreenId, ProductId>,
    last_g2p: &HashMap<GreenId, ProductId>,
    old_pid: ProductId,
    new_pid: ProductId,
    path: &[usize],
    uri: Uri<&'static str>,
    deltas: &mut Vec<Delta<ParsePath, ParseForest>>,
) {
    let old_prod = products.get(old_pid).unwrap();
    let new_prod = products.get(new_pid).unwrap();

    if old_prod.green == new_prod.green {
        return;
    }

    let old_tree = trees.get(old_prod.green).unwrap();
    let new_tree = trees.get(new_prod.green).unwrap();

    let (old_children, new_children) = match (&old_tree.data, &new_tree.data) {
        (TreeData::Node { children: o, .. }, TreeData::Node { children: n, .. }) => {
            (o.as_slice(), n.as_slice())
        }
        _ => {
            deltas.push(Delta::Delete {
                key: ParsePath {
                    uri,
                    path: path.to_vec(),
                    range: RangeOrPoint::Point(0),
                },
            });
            deltas.push(Delta::Insert {
                key: ParsePath {
                    uri,
                    path: path.to_vec(),
                    range: RangeOrPoint::Point(0),
                },
                value: ParseForest { roots: vec![new_pid] },
            });
            return;
        }
    };

    let max_c = old_children.len().max(new_children.len());
    for i in 0..max_c {
        let mut child_path = path.to_vec();
        child_path.push(i);

        match (old_children.get(i), new_children.get(i)) {
            (Some(o_green), Some(n_green)) if o_green == n_green => {
                let o_pid = first_g2p.get(o_green);
                let n_pid = last_g2p.get(n_green);
                if o_pid != n_pid {
                    if let Some(&o) = o_pid {
                        deltas.push(Delta::Delete {
                            key: ParsePath {
                                uri,
                                path: child_path.clone(),
                                range: RangeOrPoint::Point(0),
                            },
                        });
                    }
                    if let Some(&n) = n_pid {
                        deltas.push(Delta::Insert {
                            key: ParsePath {
                                uri,
                                path: child_path,
                                range: RangeOrPoint::Point(0),
                            },
                            value: ParseForest { roots: vec![n] },
                        });
                    }
                }
            }
            (Some(o_green), Some(n_green)) => {
                if let (Some(&o), Some(&n)) =
                    (first_g2p.get(o_green), last_g2p.get(n_green))
                {
                    diff_node(
                        products, trees, first_g2p, last_g2p, o, n,
                        &child_path, uri, deltas,
                    );
                }
            }
            (Some(_), None) => {
                deltas.push(Delta::Delete {
                    key: ParsePath {
                        uri,
                        path: child_path,
                        range: RangeOrPoint::Point(0),
                    },
                });
            }
            (None, Some(n_green)) => {
                if let Some(&n) = last_g2p.get(n_green) {
                    deltas.push(Delta::Insert {
                        key: ParsePath {
                            uri,
                            path: child_path,
                            range: RangeOrPoint::Point(0),
                        },
                        value: ParseForest { roots: vec![n] },
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

pub(crate) fn compact(deltas: Vec<Delta<ParsePath, ParseForest>>) -> Vec<Delta<ParsePath, ParseForest>> {
    use std::collections::BTreeSet;
    let mut insert_paths: BTreeSet<Vec<usize>> = BTreeSet::new();
    let mut delete_paths: BTreeSet<Vec<usize>> = BTreeSet::new();

    for d in &deltas {
        match d {
            Delta::Insert { key, .. } => { insert_paths.insert(key.path.clone()); }
            Delta::Delete { key } => { delete_paths.insert(key.path.clone()); }
        }
    }

    deltas
        .into_iter()
        .filter(|d| {
            match d {
                Delta::Insert { key, .. } => {
                    if delete_paths.contains(&key.path) { false } else { true }
                }
                Delta::Delete { key } => {
                    if insert_paths.contains(&key.path) { false } else { true }
                }
            }
        })
        .collect()
}
