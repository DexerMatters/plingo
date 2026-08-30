//! STLC syntax used by the incremental parser and elaboration example.

#![allow(dead_code)]

use plingo::framework::{
    lex::LexErrorInfo,
    parse::{AstToken, ParseErrorInfo, data::AstBox},
};
use plingo::prelude::{NonTerminal, PrettyNonTerminal, PrettyTerminal, Terminal, abstract_tree};

#[derive(Terminal, PrettyTerminal, Debug, Clone, PartialEq, Eq, Hash)]
pub enum StlcToken {
    #[regex(r"[ \t\r]+")]
    #[skip]
    Whitespace,
    #[regex(r"\n")]
    Newline,
    #[regex(r"fun(?-u:\b)")]
    Fun,
    #[regex(r"let(?-u:\b)")]
    Let,
    #[regex(r"in(?-u:\b)")]
    In,
    #[regex(r"if(?-u:\b)")]
    If,
    #[regex(r"case(?-u:\b)")]
    Case,
    #[regex(r"of(?-u:\b)")]
    Of,
    #[regex(r"zero(?-u:\b)")]
    Zero,
    #[regex(r"succ(?-u:\b)")]
    Succ,
    #[regex(r"then(?-u:\b)")]
    Then,
    #[regex(r"else(?-u:\b)")]
    Else,
    #[regex(r"true(?-u:\b)")]
    True,
    #[regex(r"false(?-u:\b)")]
    False,
    #[regex(r"import(?-u:\b)")]
    Import,
    #[regex(r"export(?-u:\b)")]
    Export,
    #[regex(r"Nat(?-u:\b)")]
    Nat,
    #[regex(r"Unit(?-u:\b)")]
    Unit,
    #[regex(r"Bool(?-u:\b)")]
    Bool,
    #[regex(r"[0-9]+")]
    Number(String),
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident(String),
    #[regex(r":=")]
    Assign,
    #[regex(r"->")]
    Arrow,
    #[regex(r":")]
    Colon,
    #[regex(r"=")]
    Equal,
    #[regex(r"\+")]
    Plus,
    #[regex(r"\|")]
    Pipe,
    #[regex(r"\(")]
    LeftParen,
    #[regex(r"\)")]
    RightParen,
    #[regex(r"\.")]
    Dot,
    #[error]
    Error(LexErrorInfo),
}

impl std::fmt::Display for StlcToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(
    tree = StlcTree,
    domain = String,
    members(
        StlcDocument,
        StlcDeclaration,
        StlcPath,
        StlcParam,
        StlcType,
        StlcTypeAtom,
        StlcExpr
    ),
    syntax
)]
pub enum StlcDocument {
    #[rule($declarations({StlcDeclaration}{StlcToken::Newline}), [StlcToken::Newline])]
    Lines {
        #[from(declarations)]
        declarations: Vec<AstBox<StlcDeclaration>>,
    },
    #[parse_err]
    Error {
        #[from(0)]
        error: ParseErrorInfo,
    },
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(member_of = StlcTree, syntax)]
pub enum StlcDeclaration {
    // A declaration is curried over zero or more parameters.
    #[rule($name(StlcToken::Ident), $parameters({StlcParam}), [StlcToken::Colon, $annotation(StlcType)], StlcToken::Assign, $body(StlcExpr))]
    Value {
        #[from(name)]
        name: AstToken<StlcToken>,
        #[from(annotation)]
        annotation: Option<AstBox<StlcType>>,
        #[from(body)]
        body: AstBox<StlcExpr>,
        #[from(parameters)]
        parameters: Vec<AstBox<StlcParam>>,
    },
    // import a.b.c
    #[rule(StlcToken::Import, StlcPath)]
    Import {
        #[from(1)]
        path: AstBox<StlcPath>,
    },
    // export a.b.c
    #[rule(StlcToken::Export, StlcPath)]
    Export {
        #[from(1)]
        path: AstBox<StlcPath>,
    },
    #[parse_err]
    Error {
        #[from(0)]
        error: ParseErrorInfo,
    },
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(member_of = StlcTree, syntax)]
pub enum StlcPath {
    #[rule({StlcToken::Ident}{StlcToken::Dot})]
    Segments {
        #[from(0)]
        segments: Vec<AstToken<StlcToken>>,
    },
}

/// A parameter supports all of these forms:
///
/// ```text
/// a
/// a : Nat
/// (a)
/// (a : Nat)
/// ```
#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(member_of = StlcTree, syntax)]
pub enum StlcParam {
    #[rule($name(StlcToken::Ident), [StlcToken::Colon, $annotation(StlcType)])]
    Bare {
        #[from(name)]
        name: AstToken<StlcToken>,
        #[from(annotation)]
        annotation: Option<AstBox<StlcType>>,
    },
    #[rule(
       StlcToken::LeftParen,
        $name(StlcToken::Ident),
        [StlcToken::Colon, $annotation(StlcType)],
        StlcToken::RightParen
    )]
    Parenthesized {
        #[from(name)]
        name: AstToken<StlcToken>,
        #[from(annotation)]
        annotation: Option<AstBox<StlcType>>,
    },
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(member_of = StlcTree, syntax)]
pub enum StlcType {
    // Right-associative: Nat -> Nat -> Nat
    #[rule(StlcTypeAtom, StlcToken::Arrow, StlcType)]
    Arrow {
        #[from(0)]
        left: AstBox<StlcTypeAtom>,
        #[from(2)]
        right: AstBox<StlcType>,
    },
    #[rule(StlcTypeAtom)]
    Atom {
        #[from(0)]
        atom: AstBox<StlcTypeAtom>,
    },
    #[parse_err]
    Error {
        #[from(0)]
        error: ParseErrorInfo,
    },
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(member_of = StlcTree, syntax)]
pub enum StlcTypeAtom {
    #[rule(StlcToken::Nat)]
    Nat {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[rule(StlcToken::Unit)]
    Unit {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[rule(StlcToken::Bool)]
    Bool {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[rule(StlcToken::LeftParen, StlcType, StlcToken::RightParen)]
    Parenthesized {
        #[from(1)]
        ty: AstBox<StlcType>,
    },
}

#[derive(NonTerminal, PrettyNonTerminal, Debug, Clone)]
#[abstract_tree(member_of = StlcTree, syntax)]
pub enum StlcExpr {
    // if condition then when_true else when_false
    #[rule(
        StlcExpr:0 <-
        StlcToken::If,
        StlcExpr:1,
        StlcToken::Then,
        StlcExpr:0,
        StlcToken::Else,
        StlcExpr:0
    )]
    If {
        #[from(1)]
        condition: AstBox<StlcExpr>,
        #[from(3)]
        when_true: AstBox<StlcExpr>,
        #[from(5)]
        when_false: AstBox<StlcExpr>,
    },
    // case value of zero -> zero_branch | succ name -> successor_branch
    #[rule(
        StlcExpr:0 <-
        StlcToken::Case,
        StlcExpr:1,
        StlcToken::Of,
        StlcToken::Zero,
        StlcToken::Arrow,
        StlcExpr:0,
        StlcToken::Pipe,
        StlcToken::Succ,
        StlcToken::Ident,
        StlcToken::Arrow,
        StlcExpr:0
    )]
    Case {
        #[from(1)]
        scrutinee: AstBox<StlcExpr>,
        #[from(5)]
        zero_branch: AstBox<StlcExpr>,
        #[from(8)]
        binder: AstToken<StlcToken>,
        #[from(10)]
        successor_branch: AstBox<StlcExpr>,
    },
    #[rule(StlcToken::True)]
    True {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[rule(StlcToken::False)]
    False {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    // let a = value in body
    #[rule(
        StlcExpr:0 <-
        StlcToken::Let,
        StlcToken::Ident,
        StlcToken::Equal,
        StlcExpr:0,
        StlcToken::In,
        StlcExpr:0
    )]
    Let {
        #[from(1)]
        name: AstToken<StlcToken>,
        #[from(3)]
        value: AstBox<StlcExpr>,
        #[from(5)]
        body: AstBox<StlcExpr>,
    },
    // fun a -> body / fun (a : Nat) -> body
    #[rule(StlcExpr:0 <- StlcToken::Fun, StlcParam, StlcToken::Arrow, StlcExpr:0)]
    Lambda {
        #[from(1)]
        parameter: AstBox<StlcParam>,
        #[from(3)]
        body: AstBox<StlcExpr>,
    },
    // Left-associative addition.
    #[rule(StlcExpr:1 <- StlcExpr:1, StlcToken::Plus, StlcExpr:2)]
    Add {
        #[from(0)]
        left: AstBox<StlcExpr>,
        #[from(2)]
        right: AstBox<StlcExpr>,
    },
    // Left-associative application with tighter precedence than addition.
    #[rule(StlcExpr:2 <- StlcExpr:2, StlcExpr:3)]
    Apply {
        #[from(0)]
        function: AstBox<StlcExpr>,
        #[from(1)]
        argument: AstBox<StlcExpr>,
    },
    #[rule(StlcExpr:3 <- StlcToken::Succ, StlcExpr:3)]
    Succ {
        #[from(1)]
        value: AstBox<StlcExpr>,
    },
    #[rule(StlcExpr:3 <- StlcToken::LeftParen, StlcExpr:0, StlcToken::RightParen)]
    Group {
        #[from(1)]
        expression: AstBox<StlcExpr>,
    },
    #[rule(StlcToken::Number)]
    Number {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[rule(StlcToken::Ident)]
    Variable {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[rule(StlcToken::LeftParen, StlcToken::RightParen)]
    Unit {
        #[from(0)]
        token: AstToken<StlcToken>,
    },
    #[parse_err]
    Error {
        #[from(0)]
        error: ParseErrorInfo,
    },
}
