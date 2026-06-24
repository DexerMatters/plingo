use std::collections::HashMap;

use regex_automata::{
    MatchKind,
    dfa::{StartKind, dense::DFA},
};
use regex_syntax::hir::{Hir, HirKind, Look};

use super::{
    __macro_private::{ScopeDirective, TokenMatcher, TokenSpec},
    LexerCreationError, LexerRoot, ResolvedToken, State, StateMatcher, TokenAction,
};

pub(super) fn resolve_token<Root: LexerRoot>(
    spec: TokenSpec<Root>,
    state_ids: &HashMap<String, State<Root>>,
) -> Result<ResolvedToken<Root>, LexerCreationError> {
    let (empty, minimum_length, maximum_length) = match spec.matcher {
        TokenMatcher::Empty => (true, 0, 0),
        TokenMatcher::Regex(regex) => {
            let hir = regex_syntax::parse(regex).map_err(|error| {
                LexerCreationError::RegexParsingError(
                    regex.to_string(),
                    spec.label.to_string(),
                    error,
                )
            })?;

            if let Some(kind) = find_unsupported_regex_features(&hir) {
                return Err(LexerCreationError::UnsupportedRegexFeature(
                    spec.label.to_string(),
                    regex.to_string(),
                    kind,
                ));
            }

            let minimum_length = hir.properties().minimum_len().ok_or_else(|| {
                LexerCreationError::ImpossibleToken(spec.label.to_string(), regex.to_string())
            })?;
            if minimum_length == 0 {
                return Err(LexerCreationError::EmptyMatchToken(
                    spec.label.to_string(),
                    regex.to_string(),
                ));
            }
            let maximum_length = hir.properties().maximum_len().unwrap_or(usize::MAX);
            (false, minimum_length, maximum_length)
        }
    };

    Ok(ResolvedToken {
        terminal: spec.terminal,
        precedence: spec.precedence,
        label: spec.label,
        empty,
        action: resolve_state_action(spec.action, state_ids)?,
        skip: spec.skip,
        build: spec.build,
        minimum_length,
        maximum_length,
        when: spec.when,
        recover_when: spec.recover_when,
        with_hook: spec.with,
    })
}

pub(super) fn build_state_matcher(
    state: &str,
    patterns: &[&'static str],
    token_index_by_pattern: Vec<usize>,
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
    Ok(StateMatcher {
        dfa,
        token_index_by_pattern,
    })
}

fn resolve_state_action<Root: LexerRoot>(
    action: ScopeDirective,
    state_ids: &HashMap<String, State<Root>>,
) -> Result<TokenAction<Root>, LexerCreationError> {
    match action {
        ScopeDirective::None => Ok(TokenAction::None),
        ScopeDirective::Enter { target } => state_ids
            .get(&target)
            .cloned()
            .map(|next| TokenAction::Enter { next })
            .ok_or(LexerCreationError::UnknownState(target)),
        ScopeDirective::Exit => Ok(TokenAction::Exit),
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
