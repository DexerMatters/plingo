//! A small JSON grammar used by parser and incremental-runtime tests.
//!
//! The grammar is deliberately expressed entirely through the public derive
//! macros: terminals describe the lexer and non-terminals describe the AST
//! productions.  Keeping this fixture here makes parser tests independent of
//! the component implementation details.

#![allow(dead_code)]

use plingo_macros::{NonTerminal, PrettyNonTerminal, PrettyTerminal, Terminal};

use plingo::component;

use component::{
    lex::LexErrorInfo,
    parse::{AstToken, ParseErrorInfo, data::AstBox},
};

#[derive(Terminal, PrettyTerminal, Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsonToken {
    #[regex(r"[[:space:]]+")]
    #[skip]
    Whitespace,
    #[regex(r"\{")]
    LeftBrace,
    #[regex(r"\}")]
    RightBrace,
    #[regex(r"\[")]
    LeftBracket,
    #[regex(r"\]")]
    RightBracket,
    #[regex(r",")]
    Comma,
    #[regex(r":")]
    Colon,
    #[regex(r#""[^"]*""#)]
    String(String),
    #[regex(r"-?[0-9]+")]
    Number(String),
    #[regex(r"true")]
    True,
    #[regex(r"false")]
    False,
    #[regex(r"null")]
    Null,
    #[error]
    Error(LexErrorInfo),
}

impl std::fmt::Display for JsonToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonDocument {
    #[rule(JsonValue)]
    Root(#[from(0)] AstBox<JsonValue>),
    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonValue {
    #[rule(JsonObject)]
    Object(#[from(0)] AstBox<JsonObject>),
    #[rule(JsonArray)]
    Array(#[from(0)] AstBox<JsonArray>),
    #[rule(JsonToken::String)]
    String(#[from(0)] AstToken<JsonToken>),
    #[rule(JsonToken::Number)]
    Number(#[from(0)] AstToken<JsonToken>),
    #[rule(JsonToken::True)]
    True(#[from(0)] AstToken<JsonToken>),
    #[rule(JsonToken::False)]
    False(#[from(0)] AstToken<JsonToken>),
    #[rule(JsonToken::Null)]
    Null(#[from(0)] AstToken<JsonToken>),
    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonObject {
    #[rule(JsonToken::LeftBrace, [JsonMembers], JsonToken::RightBrace)]
    Object(#[from(1)] Option<AstBox<JsonMembers>>),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonMembers {
    #[rule({JsonMember}{JsonToken::Comma})]
    Members(#[from(0)] Vec<AstBox<JsonMember>>),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonMember {
    #[rule(JsonToken::String, JsonToken::Colon, JsonValue)]
    Member(#[from(0)] AstToken<JsonToken>, #[from(2)] AstBox<JsonValue>),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonArray {
    #[rule(JsonToken::LeftBracket, [JsonElements], JsonToken::RightBracket)]
    Array(#[from(1)] Option<AstBox<JsonElements>>),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
pub enum JsonElements {
    #[rule({JsonValue}{JsonToken::Comma})]
    Elements(#[from(0)] Vec<AstBox<JsonValue>>),
}
