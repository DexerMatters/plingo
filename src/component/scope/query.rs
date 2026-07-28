use std::{collections::HashSet, hash::Hash, sync::Arc};

use super::{Scope, ScopeDatum, ScopeDomain, ScopeEdge};

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
///
/// [`crate::component::elaborate::Here::resolve`] starts it at the current
/// scope, while `resolve_from` accepts an explicit scope. Labels are typed by
/// the domain rather than parsed from strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RelativeRegex<Label>(PathExpr<Label>);

impl<Label> RelativeRegex<Label>
where
    Label: Clone + Eq,
{
    /// Matches the current scope without traversing an edge.
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

/// One resolution witness materialized by [`super::ResolutionNode`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ResolutionPath<D: ScopeDomain> {
    pub scopes: Arc<[Scope<D>]>,
    pub labels: Arc<[<D as ScopeDomain>::Label]>,
    pub datum: <D as ScopeDomain>::Datum,
}

impl<D: ScopeDomain> std::fmt::Debug for ResolutionPath<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolutionPath")
            .field("scopes", &self.scopes)
            .field("labels", &"..")
            .field("datum", &"..")
            .finish()
    }
}

/// Resolves a materialized query while observing only the edge and datum
/// buckets reached by the traversal. The node runtime supplies bucket readers
/// that record dependencies even for empty frontiers.
pub(crate) fn resolve_indexed<D, Accepts, Lookup>(
    start: Scope<D>,
    path: PathExpr<<D as ScopeDomain>::Label>,
    accepts: Accepts,
    mut lookup: Lookup,
) -> HashSet<ResolutionPath<D>>
where
    D: ScopeDomain,
    Accepts: Fn(&D::Datum) -> bool,
    Lookup: FnMut(Scope<D>, bool) -> (Vec<ScopeEdge<D>>, Vec<ScopeDatum<D>>),
{
    #[derive(Clone)]
    struct Search<D: ScopeDomain> {
        scope: Scope<D>,
        expression: PathExpr<<D as ScopeDomain>::Label>,
        scopes: Vec<Scope<D>>,
        labels: Vec<<D as ScopeDomain>::Label>,
        states: HashSet<(Scope<D>, PathExpr<<D as ScopeDomain>::Label>)>,
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
    use super::{resolve_indexed, PathExpr};
    use crate::component::{
        lex::{LexerRoot, SlotStore, TokenState},
        scope::{Scope, ScopeDatum, ScopeDomain},
    };

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    enum Label {
        Lexical,
        Declaration,
    }

    #[derive(Clone, PartialEq, Eq, Hash)]
    struct TestRoot;
    impl TokenState for TestRoot {
        fn display_name() -> &'static str {
            "TestRoot"
        }
        fn state_key() -> &'static str {
            "test"
        }
    }
    impl LexerRoot for TestRoot {
        type SlotValue = ();
        fn state_registrations(
        ) -> Vec<crate::component::lex::__macro_private::ScopeRegistration<Self>> {
            Vec::new()
        }
        fn slot_count() -> usize {
            0
        }
        fn recover_key(_: &SlotStore<Self>) -> Option<&str> {
            None
        }
    }

    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Domain;
    impl ScopeDomain for Domain {
        type Root = TestRoot;
        type Ast = ();
        type Anchor = ();
        type Label = Label;
        type Datum = usize;
        type Reference = ();
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
        let expression: PathExpr<Label> = crate::label_regex!(Label::Lexical * Label::Declaration);
        let after_lexical = expression.derivative(&Label::Lexical);
        assert!(!after_lexical.nullable());
        assert!(after_lexical.derivative(&Label::Declaration).nullable());

        let one_or_more: PathExpr<Label> = crate::label_regex!(Label::Lexical+);
        assert!(!one_or_more.nullable());
        assert!(one_or_more.derivative(&Label::Lexical).nullable());

        let relative = crate::relative_label_regex!((Label::Lexical | Label::Declaration)?);
        assert!(relative.nullable());
        assert!(relative.derivative(&Label::Lexical).nullable());
    }

    #[test]
    fn resolution_returns_all_matching_datums_on_one_scope() {
        let scope = Scope::<Domain>::allocated(0);
        let answers = resolve_indexed(
            scope,
            PathExpr::<Label>::Epsilon,
            |_| true,
            |_, _| {
                (
                    Vec::new(),
                    vec![
                        ScopeDatum::<Domain> { scope, datum: 1 },
                        ScopeDatum::<Domain> { scope, datum: 2 },
                    ],
                )
            },
        );
        assert_eq!(answers.len(), 2);
    }
}
