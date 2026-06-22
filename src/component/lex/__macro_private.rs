use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use rand::{SeedableRng, distr::Distribution, rngs::StdRng};

use crate::component::parse::grammar::TerminalId;

use super::{GenerateError, LexErrorInfo, LexInterrupt};

pub type BuildToken<Root> = Arc<dyn Fn(&str) -> Result<Root, LexInterrupt> + Send + Sync>;
pub type BuildErrorToken<Root> =
    Arc<dyn Fn(LexErrorInfo) -> Result<Root, LexInterrupt> + Send + Sync>;
pub type BuildLiftedToken<Root, Nested> =
    Arc<dyn Fn(&str, Nested) -> Result<Root, LexInterrupt> + Send + Sync>;
pub type WrapLiftedToken<Root, Nested> =
    Arc<dyn Fn(Nested) -> Result<Root, LexInterrupt> + Send + Sync>;
pub type ValidateLexeme =
    Arc<dyn for<'a, 'b> Fn(&'a str, Option<&'b str>) -> bool + Send + Sync>;

use std::error::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateDirective {
    None,
    Enter(String),
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateBoundary {
    pub target_terminal: TerminalId,
}

pub trait IntoLexemeResult<T> {
    type Error: Error + Send + Sync + 'static;

    fn into_lexeme_result(self) -> Result<T, Self::Error>;
}

impl<T> IntoLexemeResult<T> for T {
    type Error = std::convert::Infallible;

    fn into_lexeme_result(self) -> Result<T, Self::Error> {
        Ok(self)
    }
}

impl<T, E> IntoLexemeResult<T> for Result<T, E>
where
    E: Error + Send + Sync + 'static,
{
    type Error = E;

    fn into_lexeme_result(self) -> Result<T, Self::Error> {
        self
    }
}

#[derive(Clone)]
pub struct TokenSpec<Root> {
    pub regex: &'static str,
    pub terminal: TerminalId,
    pub precedence: usize,
    pub label: &'static str,
    pub action: StateDirective,
    pub skip: bool,
    pub build: BuildToken<Root>,
    pub captures_context: bool,
    pub validate: Option<ValidateLexeme>,
}

#[derive(Clone)]
pub struct StateRegistration<Root> {
    pub display_name: &'static str,
    pub type_name: String,
    pub rules: Arc<dyn Fn() -> Vec<TokenSpec<Root>> + Send + Sync>,
    pub recovery_error_builder: BuildErrorToken<Root>,
    pub boundary_error_builder: BuildErrorToken<Root>,
    pub boundary: Option<StateBoundary>,
}

impl<Root> StateRegistration<Root> {
    pub fn new(
        display_name: &'static str,
        type_name: impl Into<String>,
        rules: impl Fn() -> Vec<TokenSpec<Root>> + Send + Sync + 'static,
        recovery_error_builder: BuildErrorToken<Root>,
        boundary_error_builder: BuildErrorToken<Root>,
        boundary: Option<StateBoundary>,
    ) -> Self {
        Self {
            display_name,
            type_name: type_name.into(),
            rules: Arc::new(rules),
            recovery_error_builder,
            boundary_error_builder,
            boundary,
        }
    }
}

pub struct GeneratorCache(OnceLock<Result<rand_regex::Regex, rand_regex::Error>>);

impl GeneratorCache {
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }
}

pub fn generate_token<W, F>(
    cache: &GeneratorCache,
    token: &'static str,
    pattern: &'static str,
    seed: u64,
    dest: &mut W,
    accept: F,
) -> Result<(), GenerateError>
where
    W: fmt::Write,
    F: Fn(&str) -> bool,
{
    let generator = cache
        .0
        .get_or_init(|| rand_regex::Regex::compile(pattern, 8));
    let generator = generator
        .as_ref()
        .map_err(|source| GenerateError::RegexCompile {
            token,
            err: source.to_string(),
        })?;

    let mut rng = StdRng::seed_from_u64(seed);
    for _ in 0..256 {
        let candidate: String = generator.sample(&mut rng);
        if accept(&candidate) {
            dest.write_str(&candidate).map_err(GenerateError::Write)?;
            return Ok(());
        }
    }

    Err(GenerateError::NoAcceptedSample { token })
}
