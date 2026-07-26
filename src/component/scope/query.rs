use std::{
    collections::HashSet,
    fmt,
    hash::Hash,
    sync::Arc,
};

use super::{Scope, data::ScopeSnapshot};

/// A regular path language used by scope-graph queries.
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
            Self::Star(inner) => inner
                .derivative(label)
                .then(Self::Star(Arc::clone(inner))),
        }
    }
}

/// A scope query. The predicate is supplied by application code and remains
/// independent of graph storage and traversal.
pub struct ScopeQuery<Label, Datum> {
    pub start: Scope,
    pub path: PathExpr<Label>,
    accepts: Arc<dyn Fn(&Datum) -> bool + Send + Sync>,
}

impl<Label, Datum> Clone for ScopeQuery<Label, Datum>
where
    Label: Clone,
{
    fn clone(&self) -> Self {
        Self {
            start: self.start,
            path: self.path.clone(),
            accepts: Arc::clone(&self.accepts),
        }
    }
}

impl<Label, Datum> fmt::Debug for ScopeQuery<Label, Datum>
where
    Label: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeQuery")
            .field("start", &self.start)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<Label, Datum> ScopeQuery<Label, Datum> {
    pub fn new(
        start: Scope,
        path: PathExpr<Label>,
        accepts: impl Fn(&Datum) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            start,
            path,
            accepts: Arc::new(accepts),
        }
    }

    fn accepts(&self, datum: &Datum) -> bool {
        (self.accepts)(datum)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResolutionPath<Label, Datum> {
    pub scopes: Arc<[Scope]>,
    pub labels: Arc<[Label]>,
    pub datum: Datum,
}

/// A query and the answer retained by an application analysis unit.
#[derive(Clone, Debug)]
pub struct RecordedQuery<Label, Datum> {
    pub query: ScopeQuery<Label, Datum>,
    pub answer: Arc<[ResolutionPath<Label, Datum>]>,
}

impl<Label, Datum> RecordedQuery<Label, Datum> {
    pub fn new(
        query: ScopeQuery<Label, Datum>,
        answer: impl Into<Arc<[ResolutionPath<Label, Datum>]>>,
    ) -> Self {
        Self {
            query,
            answer: answer.into(),
        }
    }
}

/// Result of confirming a retained query against a newer graph snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryConfirmation<Label, Datum> {
    pub unchanged: bool,
    pub answer: Arc<[ResolutionPath<Label, Datum>]>,
}

impl<Anchor, Label, Datum, Reference, Request>
    ScopeSnapshot<Anchor, Label, Datum, Reference, Request>
where
    Label: Clone + Eq + Hash,
    Datum: Clone,
{
    pub(crate) fn resolve_query(
        &self,
        query: &ScopeQuery<Label, Datum>,
    ) -> Vec<ResolutionPath<Label, Datum>> {
        #[derive(Clone)]
        struct Search<Label> {
            scope: Scope,
            expression: PathExpr<Label>,
            scopes: Vec<Scope>,
            labels: Vec<Label>,
            states: HashSet<(Scope, PathExpr<Label>)>,
        }

        let initial = (query.start, query.path.clone());
        let mut initial_states = HashSet::new();
        initial_states.insert(initial.clone());
        let mut pending = vec![Search {
            scope: query.start,
            expression: query.path.clone(),
            scopes: vec![query.start],
            labels: Vec::new(),
            states: initial_states,
        }];
        let mut answers = Vec::new();

        while let Some(search) = pending.pop() {
            if search.expression.nullable()
                && let Some((datum, _)) = self.graph.datums.get(&search.scope)
                && query.accepts(datum)
            {
                answers.push(ResolutionPath {
                    scopes: search.scopes.clone().into(),
                    labels: search.labels.clone().into(),
                    datum: datum.clone(),
                });
            }

            for edge in self
                .graph
                .edges
                .keys()
                .filter(|edge| edge.source == search.scope)
            {
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
                next.labels.push(edge.label.clone());
                next.states.insert(state);
                pending.push(next);
            }
        }
        answers
    }

    pub(crate) fn confirm_query(
        &self,
        recorded: &RecordedQuery<Label, Datum>,
    ) -> QueryConfirmation<Label, Datum>
    where
        Datum: Eq + Hash,
    {
        let answer = self.resolve_query(&recorded.query);
        let old = recorded.answer.iter().cloned().collect::<HashSet<_>>();
        let new = answer.iter().cloned().collect::<HashSet<_>>();
        QueryConfirmation {
            unchanged: old == new,
            answer: answer.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PathExpr;

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    enum Label {
        Lexical,
        Declaration,
    }

    #[test]
    fn derivatives_accept_expected_language() {
        let expression = PathExpr::zero_or_more(Label::Lexical)
            .then(PathExpr::label(Label::Declaration));
        let after_lexical = expression.derivative(&Label::Lexical);
        assert!(!after_lexical.nullable());
        assert!(after_lexical
            .derivative(&Label::Declaration)
            .nullable());
        assert!(expression.derivative(&Label::Declaration).nullable());
    }
}
