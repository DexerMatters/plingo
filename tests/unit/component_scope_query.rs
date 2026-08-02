use std::collections::HashSet;

use super::{PathExpr, PathOrder, ResolutionPath, resolve_indexed};
use crate::component::scope::{ScopeDomain, ScopeEdge, ScopeId, ScopeProperty};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Label {
    Lexical,
    Declaration,
    Import,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Domain;
impl ScopeDomain for Domain {
    type ScopeKey = ();
    type ScopeData = usize;
    type Label = Label;
    type Request = ();
}

#[test]
fn derivatives_accept_expected_language() {
    let expression =
        PathExpr::zero_or_more(Label::Lexical).then(PathExpr::label(Label::Declaration));
    let after_lexical = expression.derivative(&Label::Lexical);
    assert!(!after_lexical.nullable());
    assert!(after_lexical.derivative(&Label::Declaration).nullable());
    assert!(expression.derivative(&Label::Declaration).nullable());
}

#[test]
fn label_regex_macros_use_standard_regular_operators() {
    let expression: PathExpr<Label> = crate::lregex!(Label::Lexical * Label::Declaration);
    let after_lexical = expression.derivative(&Label::Lexical);
    assert!(!after_lexical.nullable());
    assert!(after_lexical.derivative(&Label::Declaration).nullable());

    let one_or_more: PathExpr<Label> = crate::lregex!(Label::Lexical+);
    assert!(!one_or_more.nullable());
    assert!(one_or_more.derivative(&Label::Lexical).nullable());

    let relative = crate::scope_path!((Label::Lexical | Label::Declaration)?);
    assert!(relative.nullable());
    assert!(relative.derivative(&Label::Lexical).nullable());
}

#[test]
fn resolution_returns_one_mapped_data_value_on_one_scope() {
    let scope = ScopeId::<Domain>::logical(0);
    let answers = resolve_indexed(
        scope,
        PathExpr::<Label>::Epsilon,
        |_| true,
        |_, _| (Vec::new(), Some(7)),
    );
    assert_eq!(answers.len(), 1);
    assert_eq!(answers.iter().next().expect("one answer").data(), &7);
}

#[test]
fn path_order_keeps_incomparable_paths_visible() {
    let scope = ScopeId::<Domain>::logical(0);
    let local = ResolutionPath {
        scopes: vec![scope, scope].into(),
        labels: vec![Label::Declaration].into(),
        data: 1,
    };
    let outer = ResolutionPath {
        scopes: vec![scope, scope, scope].into(),
        labels: vec![Label::Lexical, Label::Declaration].into(),
        data: 2,
    };
    let imported = ResolutionPath {
        scopes: vec![scope, scope, scope].into(),
        labels: vec![Label::Import, Label::Declaration].into(),
        data: 3,
    };
    let order = PathOrder::new().prefer(Label::Declaration, Label::Lexical);
    assert_eq!(
        order.compare(&local, &outer),
        Some(std::cmp::Ordering::Greater)
    );
    assert_eq!(order.compare(&outer, &imported), None);
    let transitive_order = PathOrder::new()
        .prefer(Label::Declaration, Label::Lexical)
        .prefer(Label::Lexical, Label::Import);
    assert_eq!(
        transitive_order.compare(&local, &imported),
        Some(std::cmp::Ordering::Greater)
    );
}

#[test]
fn path_order_partitions_shadowed_witnesses() {
    let scope = ScopeId::<Domain>::logical(0);
    let local = ResolutionPath {
        scopes: vec![scope, scope].into(),
        labels: vec![Label::Declaration].into(),
        data: 1,
    };
    let outer = ResolutionPath {
        scopes: vec![scope, scope, scope].into(),
        labels: vec![Label::Lexical, Label::Declaration].into(),
        data: 2,
    };
    let (visible, shadowed) = super::partition_visible(
        HashSet::from([local, outer]),
        &PathOrder::new().prefer(Label::Declaration, Label::Lexical),
    );
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].data(), &1);
    assert_eq!(shadowed.len(), 1);
    assert_eq!(shadowed[0].0.data(), &2);
    assert_eq!(shadowed[0].1.len(), 1);
    assert_eq!(shadowed[0].1[0].data(), &1);
}

#[test]
fn resolution_witness_exposes_data_without_struct_destructuring() {
    let scope = ScopeId::<Domain>::logical(0);
    let witness = ResolutionPath {
        scopes: vec![scope].into(),
        labels: Vec::new().into(),
        data: 7,
    };
    assert_eq!(witness.data(), &7);
    assert_eq!(witness.into_data(), 7);
}

#[test]
fn cyclic_scope_paths_terminate_and_return_reachable_data() {
    let first = ScopeId::<Domain>::logical(0);
    let second = ScopeId::<Domain>::logical(1);
    let answers = resolve_indexed(
        first,
        PathExpr::zero_or_more(Label::Lexical),
        |_| true,
        |scope, _| {
            let edges = match scope {
                scope if scope == first => vec![ScopeEdge {
                    source: first,
                    label: Label::Lexical,
                    target: second,
                    property: ScopeProperty::Cyclic,
                }],
                _ => vec![ScopeEdge {
                    source: second,
                    label: Label::Lexical,
                    target: first,
                    property: ScopeProperty::Cyclic,
                }],
            };
            (edges, Some(if scope == first { 1 } else { 2 }))
        },
    );
    assert_eq!(answers.len(), 2);
}

#[test]
fn unresolved_path_has_no_matching_witness() {
    let scope = ScopeId::<Domain>::logical(0);
    let answers = resolve_indexed(
        scope,
        PathExpr::label(Label::Declaration),
        |_| true,
        |_, _| (Vec::new(), None),
    );
    assert!(answers.is_empty());
}
