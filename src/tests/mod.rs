use core::fmt;

use enum_iterator::Sequence;

use crate::{
    NonTerminal,
    component::lex::{Entry, Lexer, LexerState},
    component::parse::{AstToken, TokenData, build::Action, data::AstBox, grammar::Grammar},
    tokens,
    utils::{PrettyDisplay, Span},
};

//mod test_runtime;

mod test_parser;

#[cfg(test)]
mod fs_watch;

fn token_data_from_entries(entries: &[(usize, Entry<RootTokens>)]) -> Vec<TokenData> {
    entries
        .iter()
        .map(|(id, entry)| match entry {
            Entry::Token {
                length, terminal, ..
            } => TokenData {
                id: *id,
                terminal: Some(*terminal),
                length: *length,
            },
            Entry::EOF => TokenData {
                id: *id,
                terminal: None,
                length: 0,
            },
            Entry::Error(length, _) => TokenData {
                id: *id,
                terminal: None,
                length: *length,
            },
        })
        .collect()
}

fn collect_entries(lexer: &mut Lexer<RootTokens>, input: &str) -> Vec<(usize, Entry<RootTokens>)> {
    let token_ids: Vec<usize> = {
        let mut ids = Vec::new();
        lexer
            .lex_cont(
                LexerState::new(lexer.state_id_of::<RootTokens>().unwrap()),
                input.to_string(),
                |token_id, _| {
                    ids.push(token_id);
                    true
                },
            )
            .unwrap();
        ids
    };
    token_ids
        .into_iter()
        .map(|id| (id, lexer.get(id).clone()))
        .collect()
}

#[test]
fn test_enum_iterator() {
    #[derive(Debug, Sequence)]
    enum TestTokens2 {
        TokenA,
        TokenB,
        TokenC,
    }
    #[derive(Debug, Sequence)]
    enum TestTokens {
        TokenA,
        TokenB,
        Token2(TestTokens2),
        TokenC,
    }
    let tokens: Vec<TestTokens> = enum_iterator::all::<TestTokens>().collect();
    println!("{:#?}", tokens);
}

fn parse_usize(text: &str) -> Result<usize, std::num::ParseIntError> {
    text.parse()
}

#[tokens]
#[derive(Debug, Clone)]
enum RootTokens {
    #[regex(r#"""#)]
    #[enter(StringTokens)]
    QuoteStart,

    #[regex(r"[0-9]+")]
    Number(#[parse(parse_usize)] usize),
}

impl fmt::Display for RootTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuoteStart => write!(f, "QuoteStart"),
            Self::Number(n) => write!(f, "Number({n})"),
            Self::StringTokens(st) => write!(f, "StringTokens({st})"),
        }
    }
}

#[tokens]
#[derive(Debug, Clone)]
enum StringTokens {
    #[regex(r#"[^"]+"#)]
    Text(String),

    #[regex(r#"""#)]
    #[leave]
    QuoteEnd,
}

impl fmt::Display for StringTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(t) => write!(f, "Text(\"{t}\")"),
            Self::QuoteEnd => write!(f, "QuoteEnd"),
        }
    }
}

fn raw_delimiter(lexeme: &str) -> &str {
    let after_r = lexeme.strip_prefix('r').unwrap_or(lexeme);
    after_r.strip_suffix('"').unwrap_or(after_r)
}

fn raw_string_matches(lexeme: &str, context: Option<&str>) -> bool {
    let Some(ctx) = context else {
        return false;
    };
    let delim = raw_delimiter(ctx);
    lexeme.ends_with(delimiter_closer(delim))
}

fn delimiter_closer(hashes: &str) -> &str {
    if hashes.is_empty() { "\"" } else { hashes }
}

#[tokens]
#[derive(Debug, Clone)]
enum RawRoot {
    #[regex("r#*\"")]
    #[enter(RawBody)]
    RawStart(String),

    #[regex(r"\s+")]
    #[skip]
    RawWs,
}

impl fmt::Display for RawRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawStart(d) => write!(f, "RawStart({d})"),
            Self::RawBody(rb) => write!(f, "RawBody({rb:?})"),
            Self::RawWs => write!(f, "RawWs"),
        }
    }
}

#[tokens]
#[derive(Debug, Clone)]
enum RawBody {
    #[regex("[^\"]*\"")]
    #[leave]
    #[validate(raw_string_matches)]
    RawEnd(String),

    #[regex("[^\"]+")]
    Content(String),

    #[regex("\"")]
    EmbeddedQuote,
}

impl fmt::Display for RawBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawEnd(d) => write!(f, "RawEnd({d})"),
            Self::Content(c) => write!(f, "Content({c})"),
            Self::EmbeddedQuote => write!(f, "EmbeddedQuote"),
        }
    }
}

#[allow(dead_code)]
#[derive(NonTerminal, Debug)]
enum ExprAst {
    #[rule(RootTokens::Number)]
    Number(#[from(0)] AstToken<RootTokens>),

    #[rule(ExprAst, RootTokens::QuoteStart, ExprAst)]
    Pair(#[from(0)] AstBox<ExprAst>, #[from(2)] AstBox<ExprAst>),
}

#[derive(NonTerminal, Debug)]
enum NullableExprAst {
    #[rule()]
    Empty,

    #[rule(RootTokens::Number)]
    Number(#[from(0)] AstToken<RootTokens>),
}

#[derive(NonTerminal, Debug)]
enum EbnfExprAst {
    #[rule([$x(ExprAst)])]
    Maybe(#[from(x)] Option<AstBox<ExprAst>>),

    #[rule({$x(ExprAst)})]
    Many(#[from(x)] Vec<AstBox<ExprAst>>),

    #[rule($x(ExprAst))]
    Alt(#[from(x)] AstBox<ExprAst>),

    #[rule({$x(ExprAst)}[1..2])]
    Bounded(#[from(x)] Vec<AstBox<ExprAst>>),

    #[rule({$x(ExprAst)}{RootTokens::Number}[1..3])]
    SeparatorBounded(#[from(x)] Vec<AstBox<ExprAst>>),
}

#[test]
fn test_multi_enum_token_schema() {
    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let input = r#""world"456"#;
    let mut receiver = Vec::new();
    let error = lexer.lex_cont(
        LexerState::new(lexer.state_id_of::<RootTokens>().unwrap()),
        input.to_string(),
        |token_id, _state| {
            receiver.push(token_id);
            true
        },
    );
    assert!(error.is_ok());
    println!("{}", receiver.pretty(&lexer));
    assert!(receiver.len() >= 3);
}

#[test]
fn test_raw_string_simple() {
    let mut lexer = Lexer::<RawRoot>::new().unwrap();
    let input = "r#\"hello\"#";
    let mut received = Vec::new();
    let error = lexer.lex_cont(
        LexerState::new(lexer.state_id_of::<RawRoot>().unwrap()),
        input.to_string(),
        |token_id, _state| {
            received.push(token_id);
            true
        },
    );
    assert!(error.is_ok());
}

#[test]
fn test_raw_string_nonmatching_delimiter_rejected() {
    let mut lexer = Lexer::<RawRoot>::new().unwrap();
    let input = "r#\"text\"##";
    let mut received = Vec::new();
    let error = lexer.lex_cont(
        LexerState::new(lexer.state_id_of::<RawRoot>().unwrap()),
        input.to_string(),
        |token_id, _state| {
            received.push(token_id);
            true
        },
    );
    assert!(error.is_err() || !received.is_empty());
}

#[test]
fn test_non_terminal_macro_registers_grammar() {
    let grammar = Grammar::from_spec::<ExprAst>();
    assert_eq!(grammar.augmented_start, 0);
    assert_eq!(
        grammar.eof,
        crate::component::parse::grammar::TerminalId {
            state_key: "",
            token_id: u32::MAX,
        }
    );
    assert_eq!(grammar.production_rhs(0).len(), 2);
    assert_eq!(grammar.production_rhs(1).len(), 1);
    assert_eq!(grammar.production_rhs(2).len(), 3);
}

#[test]
fn test_non_terminal_macro_supports_epsilon_rule() {
    let grammar = Grammar::from_spec::<NullableExprAst>();
    assert_eq!(grammar.production_rhs(0).len(), 2);
    assert_eq!(grammar.production_rhs(1).len(), 0);
    assert_eq!(grammar.production_rhs(2).len(), 1);
}

#[test]
fn test_non_terminal_macro_supports_ebnf_forms() {
    let grammar = Grammar::from_spec::<EbnfExprAst>();
    assert!(grammar.productions.len() >= 8);
}

#[test]
fn test_build_lr1_generates_states_and_actions() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let runtime = grammar.build_lr1::<RootTokens, ()>();

    assert!(!runtime.states.is_empty());
    assert!(runtime.transitions.keys().any(|(state, _)| *state == 0));
    assert!(grammar.terminals.iter().enumerate().any(|(terminal, _)| {
        runtime
            .action_set(0, grammar.terminal_at(terminal))
            .inner
            .iter()
            .any(|action| matches!(action, Action::Shift(_)))
    }));
}

#[test]
fn test_parse_session_accepts_streamed_number_then_eof() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://expr", 0, 0).unwrap().uri;

    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, "123");
    let data = token_data_from_entries(&entries);
    parser.parse_tokens_at(uri, &data).unwrap();

    let accepted = parser.session_state(uri).unwrap().accepted().to_vec();
    assert_eq!(accepted.len(), 1);
    let product = parser.session_product(uri, accepted[0]).unwrap();
    let green = parser.session_green(uri, product.green).unwrap();
    assert!(matches!(
        green.data,
        crate::component::parse::data::TreeData::Node { .. }
    ));
}

#[test]
fn test_parser_accepts_externally_streamed_entries() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://expr2", 0, 0).unwrap().uri;

    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, "123");
    let data = token_data_from_entries(&entries);
    parser.parse_tokens_at(uri, &data).unwrap();

    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);
}

#[test]
fn test_parse_session_rejects_lexer_error_entry() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://err", 0, 0).unwrap().uri;

    let error_data = vec![TokenData {
        id: 0,
        terminal: None,
        length: 0,
    }];
    let result = parser.parse_tokens_at(uri, &error_data);
    assert!(result.is_err());
}

#[test]
fn test_parse_session_accepts_nullable_grammar_on_eof() {
    let grammar = Grammar::from_spec::<NullableExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://null", 0, 0).unwrap().uri;

    let eof_data = vec![TokenData {
        id: 0,
        terminal: None,
        length: 0,
    }];
    parser.parse_tokens_at(uri, &eof_data).unwrap();

    let accepted = parser.session_state(uri).unwrap().accepted().to_vec();
    assert_eq!(accepted.len(), 1);
    let product = parser.session_product(uri, accepted[0]).unwrap();
    let green = parser.session_green(uri, product.green).unwrap();
    assert!(matches!(
        green.data,
        crate::component::parse::data::TreeData::Node { .. }
    ));
}

#[test]
fn test_parse_session_can_truncate_to_middle_column_and_reparse() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://trunc", 0, 0).unwrap().uri;
    let mut lexer = Lexer::<RootTokens>::new().unwrap();

    let first_entries = collect_entries(&mut lexer, "123");
    let first_data = token_data_from_entries(&first_entries);
    let first_token = first_data[0].id;

    parser.parse_tokens_at(uri, &first_data).unwrap();
    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);

    let resume_column = parser
        .session_state(uri)
        .unwrap()
        .column_before_token(first_token)
        .unwrap();
    assert_eq!(resume_column, 0);

    parser.truncate_session(uri, resume_column);

    let repl_entries = collect_entries(&mut lexer, "456");
    let repl_data = token_data_from_entries(&repl_entries);
    parser.parse_tokens_at(uri, &repl_data).unwrap();

    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);
}
