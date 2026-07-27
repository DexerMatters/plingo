use std::{collections::HashSet, hash::Hash, sync::Arc};

use super::{Scope, ScopeDatum, ScopeEdge};

/// A regular path language used by scope-graph resolution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathExpr<Label> {
    Empty,
    Epsilon,
    Label(Label),
    Or(Arc<PathExpr<Label>>, Arc<PathExpr<Label>>),
    Then(Arc<PathExpr<Label>>, Arc<PathExpr<Label>>),
    Star(Arc<PathExpr<Label>>),
}

impl<Label> PathExpr<Label>
where
    Label: Clone + Eq,
{
    pub fn label(label: Label) -> Self {
        Self::Label(label)
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, right) => right,
            (left, Self::Empty) => left,
            (left, right) if left == right => left,
            (left, right) => Self::Or(Arc::new(left), Arc::new(right)),
        }
    }

    pub fn then(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (Self::Epsilon, right) => right,
            (left, Self::Epsilon) => left,
            (left, right) => Self::Then(Arc::new(left), Arc::new(right)),
        }
    }

    pub fn star(self) -> Self {
        match self {
            Self::Empty | Self::Epsilon => Self::Epsilon,
            Self::Star(inner) => Self::Star(inner),
            expression => Self::Star(Arc::new(expression)),
        }
    }

    pub fn zero_or_more(label: Label) -> Self {
        Self::label(label).star()
    }

    pub fn nullable(&self) -> bool {
        match self {
            Self::Empty | Self::Label(_) => false,
            Self::Epsilon | Self::Star(_) => true,
            Self::Or(left, right) => left.nullable() || right.nullable(),
            Self::Then(left, right) => left.nullable() && right.nullable(),
        }
    }

    pub fn derivative(&self, label: &Label) -> Self {
        match self {
            Self::Empty | Self::Epsilon => Self::Empty,
            Self::Label(expected) => {
                if expected == label {
                    Self::Epsilon
                } else {
                    Self::Empty
                }
            }
            Self::Or(left, right) => left.derivative(label).or(right.derivative(label)),
            Self::Then(left, right) => {
                let first = left.derivative(label).then((**right).clone());
                if left.nullable() {
                    first.or(right.derivative(label))
                } else {
                    first
                }
            }
            Self::Star(inner) => inner.derivative(label).then(Self::Star(Arc::clone(inner))),
        }
    }
}

/// One resolution witness materialized by [`super::ResolutionNode`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResolutionPath<Label, Datum> {
    pub scopes: Arc<[Scope]>,
    pub labels: Arc<[Label]>,
    pub datum: Datum,
}

/// Resolves a materialized query while observing only the edge and datum
/// buckets reached by the traversal. The node runtime supplies bucket readers
/// that record dependencies even for empty frontiers.
///
/// Resolution is a set: relation buckets are hash-backed and therefore have no
/// meaningful traversal order. Returning a set prevents equivalent graph
/// snapshots from publishing spurious updates merely because iteration order
/// changed.
pub(crate) fn resolve_indexed<Label, Datum, Accepts, Lookup>(
    start: Scope,
    path: PathExpr<Label>,
    accepts: Accepts,
    mut lookup: Lookup,
) -> HashSet<ResolutionPath<Label, Datum>>
where
    Label: Clone + Eq + Hash,
    Datum: Clone + Eq + Hash,
    Accepts: Fn(&Datum) -> bool,
    Lookup: FnMut(Scope, bool) -> (Vec<ScopeEdge<Label>>, Vec<ScopeDatum<Datum>>),
{
    #[derive(Clone)]
    struct Search<Label> {
        scope: Scope,
        expression: PathExpr<Label>,
        scopes: Vec<Scope>,
        labels: Vec<Label>,
        states: HashSet<(Scope, PathExpr<Label>)>,
    }

    let initial = (start, path.clone());
    let mut initial_states = HashSet::new();
    initial_states.insert(initial);
    let mut pending = vec![Search {
        scope: start,
        expression: path,
        scopes: vec![start],
        labels: Vec::new(),
        states: initial_states,
    }];
    let mut answers = HashSet::new();

    while let Some(search) = pending.pop() {
        let nullable = search.expression.nullable();
        let (edges, datums) = lookup(search.scope, nullable);
        if nullable {
            for datum in datums
                .into_iter()
                .filter(|datum| datum.scope == search.scope)
                .map(|datum| datum.datum)
                .filter(|datum| accepts(datum))
            {
                answers.insert(ResolutionPath {
                    scopes: search.scopes.clone().into(),
                    labels: search.labels.clone().into(),
                    datum,
                });
            }
        }

        for edge in edges {
            let residual = search.expression.derivative(&edge.label);
            if residual == PathExpr::Empty {
                continue;
            }
            let state = (edge.target, residual.clone());
            if search.states.contains(&state) {
                continue;
            }
            let mut next = search.clone();
            next.scope = edge.target;
            next.expression = residual;
            next.scopes.push(edge.target);
            next.labels.push(edge.label);
            next.states.insert(state);
            pending.push(next);
        }
    }
    answers
}

#[cfg(test)]
mod tests {
    use super::{PathExpr, resolve_indexed};
    use crate::component::scope::{Scope, ScopeDatum};

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    enum Label {
        Lexical,
        Declaration,
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
    fn resolution_returns_all_matching_datums_on_one_scope() {
        let scope = Scope(0);
        let answers = resolve_indexed(
            scope,
            PathExpr::<Label>::Epsilon,
            |_| true,
            |_, _| {
                (
                    Vec::new(),
                    vec![
                        ScopeDatum {
                            scope,
                            datum: 1usize,
                        },
                        ScopeDatum {
                            scope,
                            datum: 2usize,
                        },
                    ],
                )
            },
        );
        assert_eq!(answers.len(), 2);
    }
}
