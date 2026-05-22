use std::sync::Arc;

use super::LexInterrupt;

pub type BuildToken<Root> = Arc<dyn Fn(&str) -> Result<Root, LexInterrupt> + Send + Sync>;

use std::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDirective {
    None,
    Enter(&'static str),
    Leave,
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
    pub precedence: usize,
    pub label: &'static str,
    pub action: StateDirective,
    pub skip: bool,
    pub build: BuildToken<Root>,
    pub has_payload: bool,
    pub validate: Option<fn(&str, Option<&str>) -> bool>,
}

#[derive(Clone)]
pub struct StateRegistration<Root> {
    pub display_name: &'static str,
    pub type_name: &'static str,
    pub rules: Arc<dyn Fn() -> Vec<TokenSpec<Root>> + Send + Sync>,
}

impl<Root> StateRegistration<Root> {
    pub fn new(
        display_name: &'static str,
        type_name: &'static str,
        rules: impl Fn() -> Vec<TokenSpec<Root>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            display_name,
            type_name,
            rules: Arc::new(rules),
        }
    }
}
