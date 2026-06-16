use std::{any::TypeId, cmp::Ordering, collections::HashMap};

use fluent_uri::Uri;

use super::{
    ParseForest, ParsePath, ProductId,
    data::{
        green::{GreenId, TreeArena, TreeData},
        product::{ProductArena, ProductData},
    },
};
use crate::scheme::Delta;
use crate::utils::RangeOrPoint;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cost {
    weight: usize,
    edits: usize,
}

impl Cost {
    const ZERO: Self = Self {
        weight: 0,
        edits: 0,
    };

    fn delete(weight: usize) -> Self {
        Self { weight, edits: 1 }
    }

    fn insert(weight: usize) -> Self {
        Self { weight, edits: 1 }
    }

    fn replace(old_weight: usize, new_weight: usize) -> Self {
        Self {
            weight: old_weight.saturating_add(new_weight),
            edits: 2,
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            weight: self.weight.saturating_add(other.weight),
            edits: self.edits.saturating_add(other.edits),
        }
    }
}

impl Ord for Cost {
    fn cmp(&self, other: &Self) -> Ordering {
        self.weight
            .cmp(&other.weight)
            .then_with(|| self.edits.cmp(&other.edits))
    }
}

impl PartialOrd for Cost {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ShapeKey {
    Node {
        green: GreenId,
        ty: TypeId,
        child_hashes: Vec<u64>,
    },
    Token {
        green: GreenId,
        fingerprint: u64,
        ty: TypeId,
    },
    Error {
        green: GreenId,
    },
    Missing {
        product: ProductId,
    },
}

#[derive(Clone, Debug)]
struct Summary {
    shape: ShapeKey,
    weight: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Pair,
    Delete,
    Insert,
}

impl Step {
    fn rank(self) -> u8 {
        match self {
            Step::Pair => 0,
            Step::Delete => 1,
            Step::Insert => 2,
        }
    }
}

#[derive(Clone, Debug)]
struct SequencePlan {
    cost: Cost,
    steps: Vec<Vec<Step>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SequenceKey {
    old_ptr: usize,
    old_len: usize,
    new_ptr: usize,
    new_len: usize,
}

impl SequenceKey {
    fn new(old: &[ProductId], new: &[ProductId]) -> Option<Self> {
        if old.is_empty() || new.is_empty() {
            return None;
        }
        Some(Self {
            old_ptr: old.as_ptr() as usize,
            old_len: old.len(),
            new_ptr: new.as_ptr() as usize,
            new_len: new.len(),
        })
    }
}

fn path_at(parent: &[usize], index: usize) -> Vec<usize> {
    let mut path = parent.to_vec();
    path.push(index);
    path
}

pub(crate) fn diff_trees(
    products: &ProductArena,
    trees: &TreeArena,
    old_roots: &[ProductId],
    new_roots: &[ProductId],
    uri: Uri<&'static str>,
) -> Vec<Delta<ParsePath, ParseForest>> {
    let mut cx = DiffCx::new(products, trees);
    let mut deltas = Vec::new();
    cx.diff_sequence(old_roots, new_roots, &[], 0, uri, &mut deltas);
    deltas
}

fn replace_node(
    new_pid: ProductId,
    path: &[usize],
    uri: Uri<&'static str>,
    deltas: &mut Vec<Delta<ParsePath, ParseForest>>,
) {
    deltas.push(Delta::Delete {
        key: ParsePath {
            uri: uri.clone(),
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
        value: ParseForest {
            roots: vec![new_pid],
        },
    });
}

struct DiffCx<'a> {
    products: &'a ProductArena,
    trees: &'a TreeArena,
    summary_cache: HashMap<ProductId, Summary>,
    pair_cost_cache: HashMap<(ProductId, ProductId), Cost>,
    sequence_cost_cache: HashMap<SequenceKey, Cost>,
}

impl<'a> DiffCx<'a> {
    fn new(products: &'a ProductArena, trees: &'a TreeArena) -> Self {
        Self {
            products,
            trees,
            summary_cache: HashMap::new(),
            pair_cost_cache: HashMap::new(),
            sequence_cost_cache: HashMap::new(),
        }
    }

    fn summary(&mut self, product_id: ProductId) -> Summary {
        if let Some(summary) = self.summary_cache.get(&product_id) {
            return summary.clone();
        }

        let summary = match self.products.get(product_id) {
            Some(product) => match &product.data {
                ProductData::Node { children, ty, .. } => {
                    let child_hashes = children
                        .iter()
                        .copied()
                        .map(|child| {
                            self.products
                                .get(child)
                                .map_or(0, |product| product.semantic_hash())
                        })
                        .collect::<Vec<_>>();
                    let weight = 1usize.saturating_add(
                        children
                            .iter()
                            .copied()
                            .map(|child| self.summary(child).weight)
                            .sum::<usize>(),
                    );
                    Summary {
                        shape: ShapeKey::Node {
                            green: product.green,
                            ty: *ty,
                            child_hashes,
                        },
                        weight,
                    }
                }
                ProductData::Token {
                    fingerprint, ty, ..
                } => Summary {
                    shape: ShapeKey::Token {
                        green: product.green,
                        fingerprint: *fingerprint,
                        ty: *ty,
                    },
                    weight: 1,
                },
                ProductData::Error => Summary {
                    shape: ShapeKey::Error {
                        green: product.green,
                    },
                    weight: 1,
                },
            },
            None => self.missing_summary(product_id),
        };

        self.summary_cache.insert(product_id, summary.clone());
        summary
    }

    fn missing_summary(&self, product_id: ProductId) -> Summary {
        Summary {
            shape: ShapeKey::Missing {
                product: product_id,
            },
            weight: 1,
        }
    }

    fn same_public_shape(&mut self, old: ProductId, new: ProductId) -> bool {
        self.summary(old).shape == self.summary(new).shape
    }

    fn weight(&mut self, product_id: ProductId) -> usize {
        self.summary(product_id).weight
    }

    fn diff_cost(&mut self, old: ProductId, new: ProductId) -> Cost {
        if old == new {
            return Cost::ZERO;
        }

        if let Some(cost) = self.pair_cost_cache.get(&(old, new)) {
            return *cost;
        }

        let old_summary = self.summary(old);
        let new_summary = self.summary(new);
        if old_summary.shape == new_summary.shape {
            self.pair_cost_cache.insert((old, new), Cost::ZERO);
            self.pair_cost_cache.insert((new, old), Cost::ZERO);
            return Cost::ZERO;
        }

        let cost = match (self.products.get(old), self.products.get(new)) {
            (
                Some(crate::component::parse::data::product::Product {
                    data:
                        ProductData::Node {
                            children: old_children,
                            ..
                        },
                    green: old_green,
                    ..
                }),
                Some(crate::component::parse::data::product::Product {
                    data:
                        ProductData::Node {
                            children: new_children,
                            ..
                        },
                    green: new_green,
                    ..
                }),
            ) => match (self.trees.get(*old_green), self.trees.get(*new_green)) {
                (Some(old_tree), Some(new_tree)) => match (&old_tree.data, &new_tree.data) {
                    (TreeData::Node { id: old_id, .. }, TreeData::Node { id: new_id, .. })
                        if old_id == new_id =>
                    {
                        self.sequence_cost(old_children, new_children)
                    }
                    _ => Cost::replace(old_summary.weight, new_summary.weight),
                },
                _ => Cost::replace(old_summary.weight, new_summary.weight),
            },
            (
                Some(crate::component::parse::data::product::Product {
                    data:
                        ProductData::Token {
                            fingerprint: old_fingerprint,
                            ..
                        },
                    green: old_green,
                    ..
                }),
                Some(crate::component::parse::data::product::Product {
                    data:
                        ProductData::Token {
                            fingerprint: new_fingerprint,
                            ..
                        },
                    green: new_green,
                    ..
                }),
            ) => match (self.trees.get(*old_green), self.trees.get(*new_green)) {
                (Some(old_tree), Some(new_tree)) => match (&old_tree.data, &new_tree.data) {
                    (TreeData::Leaf { id: old_id }, TreeData::Leaf { id: new_id })
                        if old_id == new_id && old_fingerprint == new_fingerprint =>
                    {
                        Cost::ZERO
                    }
                    _ => Cost::replace(old_summary.weight, new_summary.weight),
                },
                _ => Cost::replace(old_summary.weight, new_summary.weight),
            },
            (
                Some(crate::component::parse::data::product::Product {
                    data: ProductData::Error,
                    green: old_green,
                    ..
                }),
                Some(crate::component::parse::data::product::Product {
                    data: ProductData::Error,
                    green: new_green,
                    ..
                }),
            ) => match (self.trees.get(*old_green), self.trees.get(*new_green)) {
                (Some(old_tree), Some(new_tree)) => match (&old_tree.data, &new_tree.data) {
                    (
                        TreeData::Error {
                            kind: old_kind,
                            node: old_node,
                            unexpected: old_unexpected,
                            expected: old_expected,
                            recovered: old_recovered,
                            location: old_location,
                            ..
                        },
                        TreeData::Error {
                            kind: new_kind,
                            node: new_node,
                            unexpected: new_unexpected,
                            expected: new_expected,
                            recovered: new_recovered,
                            location: new_location,
                            ..
                        },
                    ) if old_kind == new_kind
                        && old_node == new_node
                        && old_unexpected == new_unexpected
                        && old_expected == new_expected
                        && old_recovered == new_recovered
                        && old_location == new_location =>
                    {
                        Cost::ZERO
                    }
                    _ => Cost::replace(old_summary.weight, new_summary.weight),
                },
                _ => Cost::replace(old_summary.weight, new_summary.weight),
            },
            _ => Cost::replace(old_summary.weight, new_summary.weight),
        };

        self.pair_cost_cache.insert((old, new), cost);
        self.pair_cost_cache.insert((new, old), cost);
        cost
    }

    fn sequence_cost(&mut self, old: &[ProductId], new: &[ProductId]) -> Cost {
        if old.is_empty() {
            return new
                .iter()
                .copied()
                .map(|pid| Cost::insert(self.weight(pid)))
                .fold(Cost::ZERO, Cost::add);
        }
        if new.is_empty() {
            return old
                .iter()
                .copied()
                .map(|pid| Cost::delete(self.weight(pid)))
                .fold(Cost::ZERO, Cost::add);
        }

        let Some(key) = SequenceKey::new(old, new) else {
            return Cost::ZERO;
        };
        if let Some(cost) = self.sequence_cost_cache.get(&key) {
            return *cost;
        }

        let plan = self.sequence_alignment(old, new);
        self.sequence_cost_cache.insert(key, plan.cost);
        plan.cost
    }

    fn sequence_alignment(&mut self, old: &[ProductId], new: &[ProductId]) -> SequencePlan {
        let Some(key) = SequenceKey::new(old, new) else {
            return SequencePlan {
                cost: Cost::ZERO,
                steps: Vec::new(),
            };
        };

        let mut prefix = 0usize;
        let mut old_end = old.len();
        let mut new_end = new.len();
        while prefix < old_end
            && prefix < new_end
            && self.same_public_shape(old[prefix], new[prefix])
        {
            prefix += 1;
        }
        while prefix < old_end
            && prefix < new_end
            && self.same_public_shape(old[old_end - 1], new[new_end - 1])
        {
            old_end -= 1;
            new_end -= 1;
        }

        let old_mid = &old[prefix..old_end];
        let new_mid = &new[prefix..new_end];
        if old_mid.is_empty() {
            let cost = new_mid
                .iter()
                .copied()
                .map(|pid| Cost::insert(self.weight(pid)))
                .fold(Cost::ZERO, Cost::add);
            self.sequence_cost_cache.insert(key, cost);
            return SequencePlan {
                cost,
                steps: Vec::new(),
            };
        }
        if new_mid.is_empty() {
            let cost = old_mid
                .iter()
                .copied()
                .map(|pid| Cost::delete(self.weight(pid)))
                .fold(Cost::ZERO, Cost::add);
            self.sequence_cost_cache.insert(key, cost);
            return SequencePlan {
                cost,
                steps: Vec::new(),
            };
        }

        let rows = old_mid.len();
        let cols = new_mid.len();
        let mut costs = vec![vec![Cost::ZERO; cols + 1]; rows + 1];
        let mut steps = vec![vec![Step::Pair; cols]; rows];

        for i in (0..rows).rev() {
            costs[i][cols] = Cost::delete(self.weight(old_mid[i])).add(costs[i + 1][cols]);
        }
        for j in (0..cols).rev() {
            costs[rows][j] = Cost::insert(self.weight(new_mid[j])).add(costs[rows][j + 1]);
        }

        for i in (0..rows).rev() {
            for j in (0..cols).rev() {
                let delete = Cost::delete(self.weight(old_mid[i])).add(costs[i + 1][j]);
                let insert = Cost::insert(self.weight(new_mid[j])).add(costs[i][j + 1]);
                let pair = self
                    .diff_cost(old_mid[i], new_mid[j])
                    .add(costs[i + 1][j + 1]);

                let mut best = (pair, Step::Pair);
                for candidate in [(delete, Step::Delete), (insert, Step::Insert)] {
                    let ordering = candidate.0.cmp(&best.0);
                    if ordering.is_lt() || (ordering.is_eq() && candidate.1.rank() < best.1.rank())
                    {
                        best = candidate;
                    }
                }

                costs[i][j] = best.0;
                steps[i][j] = best.1;
            }
        }

        let cost = costs[0][0];
        self.sequence_cost_cache.insert(key, cost);
        SequencePlan { cost, steps }
    }

    fn emit_delete(
        &self,
        parent_path: &[usize],
        slot: usize,
        uri: Uri<&'static str>,
        deltas: &mut Vec<Delta<ParsePath, ParseForest>>,
    ) {
        deltas.push(Delta::Delete {
            key: ParsePath {
                uri,
                path: path_at(parent_path, slot),
                range: RangeOrPoint::Point(0),
            },
        });
    }

    fn emit_insert(
        &self,
        parent_path: &[usize],
        slot: usize,
        new_pid: ProductId,
        uri: Uri<&'static str>,
        deltas: &mut Vec<Delta<ParsePath, ParseForest>>,
    ) {
        deltas.push(Delta::Insert {
            key: ParsePath {
                uri,
                path: path_at(parent_path, slot),
                range: RangeOrPoint::Point(0),
            },
            value: ParseForest {
                roots: vec![new_pid],
            },
        });
    }

    fn diff_product(
        &mut self,
        old_pid: ProductId,
        new_pid: ProductId,
        path: &[usize],
        uri: Uri<&'static str>,
        deltas: &mut Vec<Delta<ParsePath, ParseForest>>,
    ) {
        if old_pid == new_pid {
            return;
        }
        if self.same_public_shape(old_pid, new_pid) {
            return;
        }

        let Some(old_product) = self.products.get(old_pid) else {
            replace_node(new_pid, path, uri, deltas);
            return;
        };
        let Some(new_product) = self.products.get(new_pid) else {
            replace_node(new_pid, path, uri, deltas);
            return;
        };
        let Some(old_tree) = self.trees.get(old_product.green) else {
            replace_node(new_pid, path, uri, deltas);
            return;
        };
        let Some(new_tree) = self.trees.get(new_product.green) else {
            replace_node(new_pid, path, uri, deltas);
            return;
        };

        match (
            &old_product.data,
            &new_product.data,
            &old_tree.data,
            &new_tree.data,
        ) {
            (
                ProductData::Node {
                    children: old_children,
                    ..
                },
                ProductData::Node {
                    children: new_children,
                    ..
                },
                TreeData::Node { id: old_id, .. },
                TreeData::Node { id: new_id, .. },
            ) if old_id == new_id => {
                self.diff_sequence(old_children, new_children, path, 0, uri, deltas);
            }
            (
                ProductData::Token { .. },
                ProductData::Token { .. },
                TreeData::Leaf { id: old_id },
                TreeData::Leaf { id: new_id },
            ) if old_id == new_id => {
                replace_node(new_pid, path, uri, deltas);
            }
            (
                ProductData::Error,
                ProductData::Error,
                TreeData::Error { .. },
                TreeData::Error { .. },
            ) => {
                replace_node(new_pid, path, uri, deltas);
            }
            _ => replace_node(new_pid, path, uri, deltas),
        }
    }

    fn diff_sequence(
        &mut self,
        old: &[ProductId],
        new: &[ProductId],
        parent_path: &[usize],
        prefix_offset: usize,
        uri: Uri<&'static str>,
        deltas: &mut Vec<Delta<ParsePath, ParseForest>>,
    ) {
        let mut old_start = 0usize;
        let mut new_start = 0usize;
        let mut old_end = old.len();
        let mut new_end = new.len();

        while old_start < old_end
            && new_start < new_end
            && self.same_public_shape(old[old_start], new[new_start])
        {
            old_start += 1;
            new_start += 1;
        }
        while old_start < old_end
            && new_start < new_end
            && self.same_public_shape(old[old_end - 1], new[new_end - 1])
        {
            old_end -= 1;
            new_end -= 1;
        }

        let old_mid = &old[old_start..old_end];
        let new_mid = &new[new_start..new_end];
        if old_mid.is_empty() && new_mid.is_empty() {
            return;
        }

        if old_mid.is_empty() {
            for (index, &pid) in new_mid.iter().enumerate() {
                self.emit_insert(
                    parent_path,
                    prefix_offset + old_start + index,
                    pid,
                    uri.clone(),
                    deltas,
                );
            }
            return;
        }

        if new_mid.is_empty() {
            for (index, _) in old_mid.iter().enumerate() {
                self.emit_delete(
                    parent_path,
                    prefix_offset + old_start + index,
                    uri.clone(),
                    deltas,
                );
            }
            return;
        }

        let plan = self.sequence_alignment(old_mid, new_mid);
        let mut i = 0usize;
        let mut j = 0usize;
        let mut shift: isize = 0;

        while i < old_mid.len() || j < new_mid.len() {
            let slot = (prefix_offset as isize + old_start as isize + i as isize + shift) as usize;
            if i == old_mid.len() {
                self.emit_insert(parent_path, slot, new_mid[j], uri.clone(), deltas);
                j += 1;
                shift += 1;
                continue;
            }
            if j == new_mid.len() {
                self.emit_delete(parent_path, slot, uri.clone(), deltas);
                i += 1;
                shift -= 1;
                continue;
            }

            let step = plan.steps[i][j];
            match step {
                Step::Pair => {
                    self.diff_product(
                        old_mid[i],
                        new_mid[j],
                        &path_at(parent_path, slot),
                        uri.clone(),
                        deltas,
                    );
                    i += 1;
                    j += 1;
                }
                Step::Delete => {
                    self.emit_delete(parent_path, slot, uri.clone(), deltas);
                    i += 1;
                    shift -= 1;
                }
                Step::Insert => {
                    self.emit_insert(parent_path, slot, new_mid[j], uri.clone(), deltas);
                    j += 1;
                    shift += 1;
                }
            }
        }
    }
}

pub(crate) fn compact(
    deltas: Vec<Delta<ParsePath, ParseForest>>,
) -> Vec<Delta<ParsePath, ParseForest>> {
    deltas
}
