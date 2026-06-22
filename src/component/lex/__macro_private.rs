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
pub type WhenGuard =
    Arc<dyn for<'a, 'b> Fn(&'a str, Option<&'b str>) -> bool + Send + Sync>;
pub type RecoverWhen =
    Arc<dyn for<'a, 'b> Fn(&'a str, Option<&'b str>) -> usize + Send + Sync>;
pub type EnterScopeKey<Root> = Arc<dyn Fn(&Root) -> Option<String> + Send + Sync>;
pub type ExitScopeGuard<Root> = Arc<dyn Fn(&Root, &str) -> bool + Send + Sync>;

use std::error::Error;

#[derive(Clone)]
pub enum ScopeDirective<Root> {
    None,
    Enter {
        target: String,
        key: EnterScopeKey<Root>,
    },
    Leave {
        matches: ExitScopeGuard<Root>,
    },
}

#[derive(Clone)]
pub struct TokenSpec<Root> {
    pub regex: &'static str,
    pub terminal: TerminalId,
    pub precedence: usize,
    pub label: &'static str,
    pub action: ScopeDirective<Root>,
    pub skip: bool,
    pub build: BuildToken<Root>,
    pub when: Option<WhenGuard>,
    pub recover_when: Option<RecoverWhen>,
}

#[derive(Clone)]
pub struct ScopeRegistration<Root> {
    pub display_name: &'static str,
    pub type_name: String,
    pub rules: Arc<dyn Fn() -> Vec<TokenSpec<Root>> + Send + Sync>,
    pub recovery_error_builder: BuildErrorToken<Root>,
    pub boundary_error_builder: BuildErrorToken<Root>,
}

impl<Root> ScopeRegistration<Root> {
    pub fn new(
        display_name: &'static str,
        type_name: impl Into<String>,
        rules: impl Fn() -> Vec<TokenSpec<Root>> + Send + Sync + 'static,
        recovery_error_builder: BuildErrorToken<Root>,
        boundary_error_builder: BuildErrorToken<Root>,
    ) -> Self {
        Self {
            display_name,
            type_name: type_name.into(),
            rules: Arc::new(rules),
            recovery_error_builder,
            boundary_error_builder,
        }
    }
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
