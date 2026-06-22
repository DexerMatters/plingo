use std::collections::HashMap;

use regex_automata::{
    MatchKind,
    dfa::{StartKind, dense::DFA},
};
use regex_syntax::hir::{Hir, HirKind, Look};

use super::{
    LexerCreationError, ResolvedToken, State, StateMatcher, TokenAction,
    __macro_private::{ScopeDirective, TokenSpec},
};

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
        when: spec.when,
        recover_when: spec.recover_when,
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

fn resolve_state_action<Root>(
    action: ScopeDirective<Root>,
    state_ids: &HashMap<String, State>,
) -> Result<TokenAction<Root>, LexerCreationError> {
    match action {
        ScopeDirective::None => Ok(TokenAction::None),
        ScopeDirective::Enter { target, key } => state_ids
            .get(&target)
            .cloned()
            .map(|next| TokenAction::Enter { next, key })
            .ok_or(LexerCreationError::UnknownState(target)),
        ScopeDirective::Leave { matches } => Ok(TokenAction::Leave { matches }),
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
