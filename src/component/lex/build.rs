use std::{collections::HashMap, sync::Arc};

use regex_automata::{
    MatchKind,
    dfa::{StartKind, dense::DFA},
};
use regex_syntax::hir::{Hir, HirKind, Look};

use super::{
    __macro_private::{StateDirective, StateRegistration, TokenSpec},
    LexerCreationError, LexerRoot, ResolvedToken, State, StateAction, StateMatcher,
};

pub(super) fn lift_state_registrations<Root, Nested>(
    wrap: fn(Nested) -> Root,
) -> Vec<StateRegistration<Root>>
where
    Root: Send + Sync + 'static,
    Nested: LexerRoot + 'static,
{
    Nested::state_registrations()
        .into_iter()
        .map(|registration| {
            StateRegistration::new(
                registration.display_name,
                registration.type_name,
                move || {
                    (registration.rules)()
                        .into_iter()
                        .map(|spec| TokenSpec {
                            regex: spec.regex,
                            terminal: spec.terminal,
                            precedence: spec.precedence,
                            label: spec.label,
                            action: spec.action,
                            skip: spec.skip,
                            build: Arc::new(move |lexeme| (spec.build)(lexeme).map(wrap)),
                            captures_context: spec.captures_context,
                            validate: spec.validate,
                        })
                        .collect()
                },
            )
        })
        .collect()
}

pub(super) fn resolve_token<Root>(
    spec: TokenSpec<Root>,
    state_ids: &HashMap<&'static str, State>,
) -> Result<ResolvedToken<Root>, LexerCreationError> {
    let hir = regex_syntax::parse(spec.regex).map_err(|error| {
        LexerCreationError::RegexParsingError(spec.regex.to_string(), spec.label.to_string(), error)
    })?;

    if let Some(kind) = find_unsupported_regex_features(&hir) {
        return Err(LexerCreationError::UnsupportedRegexFeature(
            spec.label.to_string(),
            spec.regex.to_string(),
            kind,
        ));
    }

    let minimum_length = hir.properties().minimum_len().ok_or_else(|| {
        LexerCreationError::ImpossibleToken(spec.label.to_string(), spec.regex.to_string())
    })?;
    if minimum_length == 0 {
        return Err(LexerCreationError::EmptyMatchToken(
            spec.label.to_string(),
            spec.regex.to_string(),
        ));
    }
    let maximum_length = hir.properties().maximum_len().unwrap_or(usize::MAX);

    Ok(ResolvedToken {
        terminal: spec.terminal,
        precedence: spec.precedence,
        label: spec.label,
        action: resolve_state_action(spec.action, state_ids)?,
        skip: spec.skip,
        build: spec.build,
        minimum_length,
        maximum_length,
        captures_context: spec.captures_context,
        validate: spec.validate,
    })
}

pub(super) fn build_state_matcher(
    state: &'static str,
    patterns: &[&'static str],
) -> Result<StateMatcher, LexerCreationError> {
    let dfa = DFA::builder()
        .configure(
            DFA::config()
                .start_kind(StartKind::Anchored)
                .match_kind(MatchKind::All),
        )
        .build_many(patterns)
        .map_err(|source| LexerCreationError::RegexMatcherBuildError { state, source })?;
    let token_index_by_pattern = (0..patterns.len()).collect();
    Ok(StateMatcher {
        dfa,
        token_index_by_pattern,
    })
}

fn resolve_state_action(
    action: StateDirective,
    state_ids: &HashMap<&'static str, State>,
) -> Result<StateAction, LexerCreationError> {
    match action {
        StateDirective::None => Ok(StateAction::None),
        StateDirective::Enter(target) => state_ids
            .get(target)
            .cloned()
            .map(StateAction::Enter)
            .ok_or_else(|| LexerCreationError::UnknownState(target.to_string())),
        StateDirective::Leave => Ok(StateAction::Leave),
    }
}

fn find_unsupported_regex_features(hir: &Hir) -> Option<HirKind> {
    match hir.kind() {
        HirKind::Alternation(parts) | HirKind::Concat(parts) => {
            parts.iter().find_map(find_unsupported_regex_features)
        }
        HirKind::Capture(_) => Some(hir.kind().clone()),
        HirKind::Look(Look::Start) => Some(hir.kind().clone()),
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) | HirKind::Look(_) => None,
        HirKind::Repetition(rep) => find_unsupported_regex_features(&rep.sub),
    }
}
