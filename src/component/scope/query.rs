use std::{collections::HashSet, hash::Hash, sync::Arc};

use super::{ScopeDomain, ScopeEdge, ScopeId};

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

/// A path regular expression interpreted relative to a starting scope.
/// Labels are typed by the domain rather than parsed from strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RelativeRegex<Label>(PathExpr<Label>);

impl<Label> RelativeRegex<Label>
where
    Label: Clone + Eq,
{
    /// Matches the starting scope without traversing an edge.
    pub fn here() -> Self {
        Self(PathExpr::Epsilon)
    }

    pub fn empty() -> Self {
        Self(PathExpr::Empty)
    }

    pub fn label(label: Label) -> Self {
        Self(PathExpr::label(label))
    }

    pub fn zero_or_more(label: Label) -> Self {
        Self(PathExpr::zero_or_more(label))
    }

    pub fn or(self, other: Self) -> Self {
        Self(self.0.or(other.0))
    }

    pub fn then(self, other: Self) -> Self {
        Self(self.0.then(other.0))
    }

    pub fn star(self) -> Self {
        Self(self.0.star())
    }

    pub fn nullable(&self) -> bool {
        self.0.nullable()
    }

    pub fn derivative(&self, label: &Label) -> Self {
        Self(self.0.derivative(label))
    }

    pub(crate) fn into_path(self) -> PathExpr<Label> {
        self.0
    }
}

impl<Label> From<PathExpr<Label>> for RelativeRegex<Label> {
    fn from(path: PathExpr<Label>) -> Self {
        Self(path)
    }
}

/// A strict partial order over edge labels used to select visible paths.
///
/// `prefer(a, b)` means that a path using `a` at the first differing position
/// outranks a path using `b`. Incomparable paths remain visible and therefore
/// preserve ambiguity. A strict prefix is more specific than its extension,
/// matching the calculus' end-of-path ordering.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathOrder<Label> {
    preferred: Arc<[(Label, Label)]>,
}

impl<Label> Default for PathOrder<Label> {
    fn default() -> Self {
        Self {
            preferred: Arc::from([]),
        }
    }
}

impl<Label> PathOrder<Label>
where
    Label: Clone + Eq,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prefer(mut self, preferred: Label, less_preferred: Label) -> Self {
        let mut relations = self.preferred.to_vec();
        if preferred != less_preferred
            && !relations
                .iter()
                .any(|(left, right)| left == &preferred && right == &less_preferred)
        {
            relations.push((preferred, less_preferred));
        }
        let mut changed = true;
        while changed {
            changed = false;
            let snapshot = relations.clone();
            for (left, middle) in &snapshot {
                for (candidate, right) in &snapshot {
                    if middle == candidate
                        && left != right
                        && !relations.iter().any(|(l, r)| l == left && r == right)
                    {
                        relations.push((left.clone(), right.clone()));
                        changed = true;
                    }
                }
            }
        }
        self.preferred = relations.into();
        self
    }

    pub fn compare<Scope>(
        &self,
        left: &ResolutionPath<Scope>,
        right: &ResolutionPath<Scope>,
    ) -> Option<std::cmp::Ordering>
    where
        Scope: ScopeDomain<Label = Label>,
    {
        for (left_label, right_label) in left.labels.iter().zip(right.labels.iter()) {
            if left_label == right_label {
                continue;
            }
            if self
                .preferred
                .iter()
                .any(|(preferred, less)| preferred == left_label && less == right_label)
            {
                return Some(std::cmp::Ordering::Greater);
            }
            if self
                .preferred
                .iter()
                .any(|(preferred, less)| preferred == right_label && less == left_label)
            {
                return Some(std::cmp::Ordering::Less);
            }
            return None;
        }
        Some(left.labels.len().cmp(&right.labels.len()).reverse())
    }
}

/// One dependency-tracked resolution witness returned by semantic queries.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ResolutionPath<D: ScopeDomain> {
    pub scopes: Arc<[ScopeId<D>]>,
    pub labels: Arc<[<D as ScopeDomain>::Label]>,
    pub data: <D as ScopeDomain>::ScopeData,
}

impl<D: ScopeDomain> std::fmt::Debug for ResolutionPath<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolutionPath")
            .field("scopes", &self.scopes)
            .field("labels", &"..")
            .field("data", &"..")
            .finish()
    }
}

/// Resolves a materialized query while observing only the edge buckets and
/// mapped scope data reached by the traversal. The node runtime supplies
/// readers that record dependencies even for empty frontiers.
pub(crate) fn resolve_indexed<D, Accepts, Lookup>(
    start: ScopeId<D>,
    path: PathExpr<<D as ScopeDomain>::Label>,
    accepts: Accepts,
    mut lookup: Lookup,
) -> HashSet<ResolutionPath<D>>
where
    D: ScopeDomain,
    Accepts: Fn(&D::ScopeData) -> bool,
    Lookup: FnMut(ScopeId<D>, bool) -> (Vec<ScopeEdge<D>>, Option<D::ScopeData>),
{
    #[derive(Clone)]
    struct Search<D: ScopeDomain> {
        scope: ScopeId<D>,
        expression: PathExpr<<D as ScopeDomain>::Label>,
        scopes: Vec<ScopeId<D>>,
        labels: Vec<<D as ScopeDomain>::Label>,
        states: HashSet<(ScopeId<D>, PathExpr<<D as ScopeDomain>::Label>)>,
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
        let (edges, data) = lookup(search.scope, nullable);
        if nullable
            && let Some(data) = data
            && accepts(&data)
        {
            answers.insert(ResolutionPath {
                scopes: search.scopes.clone().into(),
                labels: search.labels.clone().into(),
                data,
            });
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
#[path = "../../../tests/unit/component_scope_query.rs"]
mod tests;
