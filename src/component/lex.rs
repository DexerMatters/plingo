use std::{
    collections::HashMap,
    fmt::{Debug, Display},
    fs::File,
    io::BufReader,
    path::PathBuf,
};

use enum_iterator::Sequence;
use fluent_uri::Uri;
use internment::Intern;
use regex::Regex;
use regex_syntax::hir::{Hir, HirKind, Look};
use ropey::Rope;
use thiserror::Error;

use crate::utils::Spanned;

pub struct Lexer<T: Tokens> {
    arena: Vec<TokenInstance<T>>,
    schema: Vec<ResolvedToken<T>>,
}

impl<T: Tokens> Lexer<T> {
    pub fn new() -> Self {
        Self {
            arena: Vec::new(),
            schema: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenInstance<T: Tokens> {
    pub token: T,
    pub length: usize,
}

pub struct ResolvedToken<T: Tokens> {
    pub regex: Regex,
    pub precedence: usize,
    pub display: String,
    pub instance: T,
    // Regex infomation
    pub(crate) minimum_length: usize,
    pub(crate) maximum_length: usize,
}

impl<T: Tokens> ResolvedToken<T> {
    pub fn matches(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }
    pub fn matched_prefix_length(&self, input: &str) -> Option<usize> {
        self.regex.find(input).map(|m| m.end())
    }
}

pub trait Tokens: Display + PartialEq + Eq + Clone + Sequence {
    fn all_tokens() -> impl Iterator<Item = Self> {
        enum_iterator::all::<Self>()
    }
    fn pattern(&self) -> &'static str;
    fn precedence(&self) -> usize {
        // SAFETY: `self` is guaranteed to be a valid token variant,
        // so it will always be found in the iterator.
        Self::all_tokens().position(|t| t == *self).unwrap()
    }
    fn resolve(self) -> Result<ResolvedToken<Self>, LexError> {
        let pattern = self.pattern();

        // so parsing it with `regex_syntax` will also succeed.
        let hir = regex_syntax::parse(pattern)
            .map_err(|e| LexError::RegexParsingError(pattern.to_string(), self.display(), e))?;

        // Check for unsupported regex features in the HIR.
        if let Some(kind) = find_unsupported_regex_features(&hir) {
            return Err(LexError::UnsupportedRegexFeature(
                self.display(),
                pattern.to_string(),
                kind,
            ));
        }

        // Add `start` to ensure the regex matches from the beginning of the input string.
        let anchored_pattern = format!("^(?:{})", pattern);

        // SAFETY: The regex pattern is guaranteed by `regex_syntax`
        let regex = Regex::new(&anchored_pattern).unwrap();

        let props = hir.properties();
        let minimum_length = props
            .minimum_len()
            .ok_or_else(|| LexError::ImpossibleToken(self.display(), pattern.to_string()))?;
        let maximum_length = props.maximum_len().unwrap_or(usize::MAX);

        Ok(ResolvedToken {
            regex,
            precedence: self.precedence(),
            display: self.display(),
            instance: self,
            minimum_length,
            maximum_length,
        })
    }
    fn display(&self) -> String {
        format!("{}", self)
    }
}

fn find_unsupported_regex_features(hir: &Hir) -> Option<HirKind> {
    let kind = hir.kind();
    match kind {
        HirKind::Alternation(hirs) | HirKind::Concat(hirs) => hirs
            .iter()
            .filter_map(find_unsupported_regex_features)
            .next(),
        HirKind::Capture(_) => Some(kind.clone()),
        HirKind::Look(Look::Start) => Some(kind.clone()),
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) | HirKind::Look(_) => None,
        HirKind::Repetition(rep) => find_unsupported_regex_features(&rep.sub),
    }
}

#[derive(Debug, Error)]
pub enum LexError {
    #[error("Failed to open source for URI: {0}: {1}")]
    CannotOpen(Uri<&'static str>, std::io::Error),
    #[error("Failed to read source for URI: {0}: {1}")]
    ReadError(Uri<&'static str>, std::io::Error),
    #[error("Invalid URI: {0}")]
    InvalidUri(fluent_uri::ParseError),
    #[error("Cache error for URI: {0}")]
    CacheError(Uri<&'static str>),
    #[error("Error occurred while parsing regex pattern {0} for token {1}: {2}")]
    RegexParsingError(String, String, regex_syntax::Error),
    #[error("Regex pattern {1} for token {0} contains unsupported feature: {2:?}")]
    UnsupportedRegexFeature(String, String, HirKind),
    #[error("Token {0} with pattern {1} cannot be matched by any input string")]
    ImpossibleToken(String, String),
}
