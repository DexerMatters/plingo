use std::{collections::HashMap, sync::Arc};

use regex_automata::{
    MatchKind,
    dfa::{StartKind, dense::DFA},
};
use regex_syntax::hir::{Hir, HirKind, Look};

use super::{
    __macro_private::{
        BuildErrorToken, BuildLiftedToken, StateBoundary, StateDirective, StateRegistration,
        TokenSpec, ValidateLexeme, WrapLiftedToken,
    },
    LexerCreationError, LexerRoot, ResolvedToken, State, StateAction, StateMatcher, TokenState,
};

pub(super) fn lift_state_registrations<Root, Nested>(
    build_outer: BuildLiftedToken<Root, Nested>,
    wrap_nested: Option<WrapLiftedToken<Root, Nested>>,
    boundary_error_builder: BuildErrorToken<Root>,
    synthetic_key: &'static str,
    boundary: Option<StateBoundary>,
    terminal: crate::component::parse::grammar::TerminalId,
    label: &'static str,
    outer_validate: Option<ValidateLexeme>,
) -> Vec<StateRegistration<Root>>
where
    Root: Send + Sync + 'static,
    Nested: LexerRoot + 'static,
{
    let nested_root_key = <Nested as TokenState>::state_key().to_string();

    Nested::state_registrations()
        .into_iter()
        .map(|registration| {
            let original_name = registration.type_name.clone();
            let is_nested_root = original_name == nested_root_key;
            let mapped_name = if is_nested_root {
                synthetic_key.to_string()
            } else {
                format!("{synthetic_key}::{original_name}")
            };

            let original_rules = registration.rules.clone();
            let success_builder = build_outer.clone();
            let mapped_boundary_error = if is_nested_root {
                boundary_error_builder.clone()
            } else if let Some(wrap_nested) = wrap_nested.clone() {
                let wrapped_boundary = registration.boundary_error_builder.clone();
                Arc::new(move |info| {
                    let nested = (wrapped_boundary)(info)?;
                    wrap_nested(nested)
                }) as BuildErrorToken<Root>
            } else {
                boundary_error_builder.clone()
            };
            let mapped_recovery_error = if let Some(wrap_nested) = wrap_nested.clone() {
                let wrapped_recovery = registration.recovery_error_builder.clone();
                Arc::new(move |info| {
                    let nested = (wrapped_recovery)(info)?;
                    wrap_nested(nested)
                }) as BuildErrorToken<Root>
            } else {
                boundary_error_builder.clone()
            };

            StateRegistration::new(
                registration.display_name,
                mapped_name,
                {
                    let nested_root_key = nested_root_key.clone();
                    let outer_validate = outer_validate.clone();
                    move || {
                        (original_rules)()
                            .into_iter()
                            .map(|spec| {
                                let mapped_action = match spec.action {
                                    StateDirective::None => StateDirective::None,
                                    StateDirective::Leave => StateDirective::Leave,
                                    StateDirective::Enter(target) => {
                                        if target == nested_root_key {
                                            StateDirective::Enter(synthetic_key.to_string())
                                        } else {
                                            StateDirective::Enter(format!(
                                                "{synthetic_key}::{target}"
                                            ))
                                        }
                                    }
                                };
                                let nested_build = spec.build.clone();
                                let success_builder = success_builder.clone();
                                let combined_validate = match (spec.validate.clone(), outer_validate.clone()) {
                                    (Some(inner), Some(outer)) => Some(combine_validate(inner, outer)),
                                    (Some(inner), None) => Some(inner),
                                    (None, Some(outer)) => Some(outer),
                                    (None, None) => None,
                                };
                                TokenSpec {
                                    regex: spec.regex,
                                    terminal,
                                    precedence: spec.precedence,
                                    label,
                                    action: mapped_action,
                                    skip: spec.skip,
                                    build: Arc::new(move |lexeme| {
                                        let nested = (nested_build)(lexeme)?;
                                        success_builder(lexeme, nested)
                                    }),
                                    captures_context: spec.captures_context,
                                    validate: combined_validate,
                                }
                            })
                            .collect()
                    }
                },
                mapped_recovery_error,
                mapped_boundary_error,
                if is_nested_root { boundary } else { None },
            )
        })
        .collect()
}

fn combine_validate(inner: ValidateLexeme, outer: ValidateLexeme) -> ValidateLexeme {
    Arc::new(move |lexeme: &str, ctx: Option<&str>| inner(lexeme, ctx) && outer(lexeme, ctx))
}

pub(super) fn resolve_token<Root>(
    spec: TokenSpec<Root>,
    state_ids: &HashMap<String, State>,
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
    state: &str,
    patterns: &[&'static str],
) -> Result<StateMatcher, LexerCreationError> {
    let dfa = DFA::builder()
        .configure(
            DFA::config()
                .start_kind(StartKind::Anchored)
                .match_kind(MatchKind::All),
        )
        .build_many(patterns)
        .map_err(|source| LexerCreationError::RegexMatcherBuildError {
            state: state.to_string(),
            source,
        })?;
    let token_index_by_pattern = (0..patterns.len()).collect();
    Ok(StateMatcher {
        dfa,
        token_index_by_pattern,
    })
}

fn resolve_state_action(
    action: StateDirective,
    state_ids: &HashMap<String, State>,
) -> Result<StateAction, LexerCreationError> {
    match action {
        StateDirective::None => Ok(StateAction::None),
        StateDirective::Enter(target) => state_ids
            .get(&target)
            .cloned()
            .map(StateAction::Enter)
            .ok_or(LexerCreationError::UnknownState(target)),
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
