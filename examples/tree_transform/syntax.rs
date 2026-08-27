//! A compact declaration language used to exercise parser-tree lowering.
//!
//! This syntax deliberately has more than one expression shape, but remains
//! small enough for tests to isolate payload and child-order dependencies.

#![allow(dead_code)]

use plingo::abstract_tree;

use plingo::framework::{
    lex::LexErrorInfo,
    parse::{AstToken, ParseErrorInfo, data::AstBox},
};
use plingo_macros::{NonTerminal, PrettyNonTerminal, PrettyTerminal, Terminal};

#[derive(Terminal, PrettyTerminal, Debug, Clone, PartialEq, Eq, Hash)]
pub enum TransformToken {
    #[regex(r"[ \t\r\n]+")]
    #[skip]
    Whitespace,
    #[regex(r"let(?-u:\b)")]
    Let,
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
    #[regex(r"[0-9]+")]
    Number,
    #[regex(r"=")]
    Assign,
    #[regex(r"\+")]
    Plus,
    #[regex(r"-")]
    Minus,
    #[regex(r";")]
    Semicolon,
    #[regex(r"\(")]
    LeftParen,
    #[regex(r"\)")]
    RightParen,
    #[error]
    Error(LexErrorInfo),
}

impl std::fmt::Display for TransformToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(members(TransformDocument, TransformDeclaration, TransformExpr))]
pub enum TransformDocument {
    #[rule($declarations({TransformDeclaration}{TransformToken::Semicolon}), [TransformToken::Semicolon])]
    Program(#[from(declarations)] Vec<AstBox<TransformDeclaration>>),
    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(members(TransformDocument, TransformDeclaration, TransformExpr))]
pub enum TransformDeclaration {
    #[rule(
        TransformToken::Let,
        TransformToken::Ident,
        TransformToken::Assign,
        TransformExpr
    )]
    Binding(
        #[from(1)] AstToken<TransformToken>,
        #[from(3)] AstBox<TransformExpr>,
    ),
    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(members(TransformDocument, TransformDeclaration, TransformExpr))]
pub enum TransformExpr {
    #[rule(TransformExpr:0 <- TransformExpr:0, TransformToken::Plus, TransformExpr:1)]
    Add(
        #[from(0)] AstBox<TransformExpr>,
        #[from(2)] AstBox<TransformExpr>,
    ),
    #[rule(TransformExpr:0 <- TransformExpr:0, TransformToken::Minus, TransformExpr:1)]
    Subtract(
        #[from(0)] AstBox<TransformExpr>,
        #[from(2)] AstBox<TransformExpr>,
    ),
    #[rule(TransformExpr:1 <- TransformToken::LeftParen, TransformExpr:0, TransformToken::RightParen)]
    Group(#[from(1)] AstBox<TransformExpr>),
    #[rule(TransformToken::Number)]
    Number(#[from(0)] AstToken<TransformToken>),
    #[rule(TransformToken::Ident)]
    Name(#[from(0)] AstToken<TransformToken>),
    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}
