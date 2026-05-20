use std::{
    any::Any,
    error::Error,
    hash::{Hash, Hasher},
};

use super::{LexError, Token};

pub type BuildToken = fn(&str) -> Result<Token, LexError>;

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

pub trait TokenValue: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync>;
    fn eq_token_value(&self, other: &dyn TokenValue) -> bool;
    fn hash_token_value(&self, state: &mut dyn Hasher);
}

impl<T> TokenValue for T
where
    T: Any + Send + Sync + Eq + Hash,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync> {
        self
    }

    fn eq_token_value(&self, other: &dyn TokenValue) -> bool {
        other.as_any().downcast_ref::<T>() == Some(self)
    }

    fn hash_token_value(&self, state: &mut dyn Hasher) {
        state.write_u64(calculate_hash(self));
    }
}

fn calculate_hash<T: Hash>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

pub struct TokenSpec {
    pub regex: &'static str,
    pub precedence: usize,
    pub label: &'static str,
    pub action: StateDirective,
    pub skip: bool,
    pub build: BuildToken,
}

pub struct StateRegistration {
    pub display_name: &'static str,
    pub type_name: &'static str,
    pub rules: fn() -> Vec<TokenSpec>,
}

impl StateRegistration {
    pub const fn new(
        display_name: &'static str,
        type_name: &'static str,
        rules: fn() -> Vec<TokenSpec>,
    ) -> Self {
        Self {
            display_name,
            type_name,
            rules,
        }
    }
}

::inventory::collect!(StateRegistration);
