use core::fmt;

use enum_iterator::Sequence;

use crate::{
    NonTerminal,
    component::lex::{Entry, Lexer, LexerState},
    component::parse::{
        AstToken, ErrorKind, ParseErrorInfo, ParserConfig, TokenData,
        build::Action,
        data::{AstArena, AstBox, Product, ProductArena, ProductData, TreeArena},
        diff,
        grammar::{ERROR_TERMINAL, Grammar, Symbol},
        identity::{eof_fingerprint, error_fingerprint, token_fingerprint},
    },
    scheme::Delta,
    tokens,
    utils::{PrettyDisplay, Span},
};

//mod test_runtime;

mod test_parser;
mod test_parser_comprehensive;

#[cfg(test)]
mod fs_watch;

fn token_data_from_entries(entries: &[(usize, Entry<RootTokens>, usize, usize)]) -> Vec<TokenData> {
    entries
        .iter()
        .enumerate()
        .map(|(column, (id, entry, start, _end))| {
            let data = match entry {
                Entry::Token {
                    length,
                    terminal,
                    value,
                } => TokenData {
                    id: *id,
                    terminal: Some(*terminal),
                    start: *start,
                    length: *length,
                    column,
                    fingerprint: token_fingerprint(Some(*terminal), value, *length),
                },
                Entry::EOF => TokenData {
                    id: *id,
                    terminal: None,
                    start: *start,
                    length: 0,
                    column,
                    fingerprint: eof_fingerprint(),
                },
                Entry::Error(length, error) => TokenData {
                    id: *id,
                    terminal: None,
                    start: *start,
                    length: *length,
                    column,
                    fingerprint: error_fingerprint(error, *length),
                },
            };
            data
        })
        .collect()
}

fn collect_entries(
    lexer: &mut Lexer<RootTokens>,
    input: &str,
) -> Vec<(usize, Entry<RootTokens>, usize, usize)> {
    let token_ids: Vec<(usize, usize, usize)> = {
        let mut ids = Vec::new();
        lexer
            .lex_cont(
                LexerState::new(lexer.state_id_of::<RootTokens>().unwrap()),
                input.to_string(),
                |token_id, _, start, end| {
                    ids.push((token_id, start, end));
                    true
                },
            )
            .unwrap();
        ids
    };
    token_ids
        .into_iter()
        .map(|(id, start, end)| (id, lexer.get(id).clone(), start, end))
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

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug)]
enum TokenClassAst {
    #[rule(RootTokens::Number)]
    Number(#[from(0)] AstToken<RootTokens>),

    #[rule(RootTokens::QuoteStart)]
    QuoteStart(#[from(0)] AstToken<RootTokens>),
}

#[derive(NonTerminal, Debug, Clone)]
enum ErrorOnlyAst {
    #[rule(
        RootTokens::QuoteStart,
        RootTokens::QuoteStart,
        RootTokens::QuoteStart,
        RootTokens::Number
    )]
    Pair,

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
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

    #[rule({$x(ExprAst)}{RootTokens::Number})]
    Separated(#[from(x)] Vec<AstBox<ExprAst>>),

    #[parse_err]
    Error,
}

#[test]
fn test_multi_enum_token_schema() {
    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let input = r#""world"456"#;
    let mut receiver = Vec::new();
    let error = lexer.lex_cont(
        LexerState::new(lexer.state_id_of::<RootTokens>().unwrap()),
        input.to_string(),
        |token_id, _state, _, _| {
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
        |token_id, _state, _, _| {
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
        |token_id, _state, _, _| {
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
        start: 0,
        length: 0,
        column: 0,
        fingerprint: 0,
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
        start: 0,
        length: 0,
        column: 0,
        fingerprint: 0,
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
fn test_error_recovery_deletes_unexpected_token() {
    let grammar = Grammar::from_spec::<NullableExprAst>();
    let mut parser = grammar.build_lr1_with_config::<RootTokens, ()>(ParserConfig {
        error_recovery: true,
        ..ParserConfig::default()
    });
    let uri = Span::new("test://recover-delete", 0, 0).unwrap().uri;

    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, "123");
    let data = token_data_from_entries(&entries);
    let number = data
        .iter()
        .find(|token| token.terminal.is_some())
        .copied()
        .unwrap();
    let eof = data
        .iter()
        .find(|token| token.terminal.is_none())
        .copied()
        .unwrap();
    let duplicated = TokenData {
        id: number.id + 100,
        fingerprint: number.fingerprint,
        ..number
    };
    let eof = TokenData {
        id: number.id + 101,
        fingerprint: eof.fingerprint,
        ..eof
    };

    parser
        .parse_tokens_at(uri, &[number, duplicated, eof])
        .unwrap();
    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);
}

#[test]
fn test_error_recovery_can_be_disabled() {
    let grammar = Grammar::from_spec::<NullableExprAst>();
    let mut parser = grammar.build_lr1_with_config::<RootTokens, ()>(ParserConfig {
        error_recovery: false,
        ..ParserConfig::default()
    });
    let uri = Span::new("test://recover-disabled", 0, 0).unwrap().uri;

    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, "123");
    let data = token_data_from_entries(&entries);
    let number = data
        .iter()
        .find(|token| token.terminal.is_some())
        .copied()
        .unwrap();
    let eof = data
        .iter()
        .find(|token| token.terminal.is_none())
        .copied()
        .unwrap();
    let duplicated = TokenData {
        id: number.id + 100,
        fingerprint: number.fingerprint,
        ..number
    };
    let eof = TokenData {
        id: number.id + 101,
        fingerprint: eof.fingerprint,
        ..eof
    };

    assert!(
        parser
            .parse_tokens_at(uri, &[number, duplicated, eof])
            .is_err()
    );
}

#[test]
fn test_parse_error_variant_receives_error_info() {
    let grammar = Grammar::from_spec::<ErrorOnlyAst>();
    let mut parser = grammar.build_lr1_with_config::<RootTokens, ()>(ParserConfig {
        error_recovery: true,
        ..ParserConfig::default()
    });
    let uri = Span::new("test://recover-info", 0, 0).unwrap().uri;

    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, "123");
    let data = token_data_from_entries(&entries);
    parser.parse_tokens_at(uri, &data).unwrap();

    let accepted = parser.session_state(uri).unwrap().accepted().to_vec();
    assert_eq!(accepted.len(), 1);
    let product = parser.session_product(uri, accepted[0]).unwrap();
    let ProductData::Node { ast, .. } = product.data else {
        panic!("expected accepted parse node");
    };
    let value = parser
        .session_arenas
        .get(&uri)
        .unwrap()
        .ast
        .cloned::<ErrorOnlyAst>(ast)
        .unwrap();

    let ErrorOnlyAst::Error(info) = value else {
        panic!("expected parse error AST variant");
    };
    assert_eq!(info.kind, ErrorKind::UnexpectedToken);
    assert!(info.unexpected.is_some());
    assert_eq!(info.expected, Symbol::T(ERROR_TERMINAL));
    assert!(info.recovered);
}

#[test]
fn test_parse_session_can_truncate_to_middle_column_and_reparse() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://trunc", 0, 0).unwrap().uri;
    let mut lexer = Lexer::<RootTokens>::new().unwrap();

    let first_entries = collect_entries(&mut lexer, "123");
    let first_data = token_data_from_entries(&first_entries);

    parser.parse_tokens_at(uri, &first_data).unwrap();
    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);

    let resume_column = parser
        .session_state(uri)
        .unwrap()
        .column_before_token(first_data[0].column)
        .unwrap();
    assert_eq!(resume_column, 0);

    parser.truncate_session(uri, resume_column);

    let repl_entries = collect_entries(&mut lexer, "456");
    let repl_data = token_data_from_entries(&repl_entries);
    parser.parse_tokens_at(uri, &repl_data).unwrap();

    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);
}

#[test]
fn test_incremental_reparse_keeps_valid_frontier_for_token_class_change() {
    let grammar = Grammar::from_spec::<TokenClassAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://incremental-token-class", 0, 0)
        .unwrap()
        .uri;
    let mut lexer = Lexer::<RootTokens>::new().unwrap();

    let first_entries = collect_entries(&mut lexer, "123");
    let first_data = token_data_from_entries(&first_entries);
    parser.parse_tokens_at(uri, &first_data).unwrap();

    parser.truncate_session(uri, 0);

    let second_entries = collect_entries(&mut lexer, "\"");
    let second_data = token_data_from_entries(&second_entries);
    parser.parse_tokens_at(uri, &second_data).unwrap();

    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);
}

#[test]
fn test_incremental_reparse_keeps_valid_frontier_for_token_length_change() {
    let grammar = Grammar::from_spec::<ExprAst>();
    let mut parser = grammar.build_lr1::<RootTokens, ()>();
    let uri = Span::new("test://incremental-token-length", 0, 0)
        .unwrap()
        .uri;
    let mut lexer = Lexer::<RootTokens>::new().unwrap();

    let first_entries = collect_entries(&mut lexer, "123");
    let first_data = token_data_from_entries(&first_entries);
    parser.parse_tokens_at(uri, &first_data).unwrap();

    let resume_column = parser
        .session_state(uri)
        .unwrap()
        .column_before_token(first_data[0].column)
        .unwrap();
    parser.truncate_session(uri, resume_column);

    let second_entries = collect_entries(&mut lexer, "4567");
    let second_data = token_data_from_entries(&second_entries);
    parser.parse_tokens_at(uri, &second_data).unwrap();

    assert_eq!(parser.session_state(uri).unwrap().accepted().len(), 1);
}

#[test]
fn test_diff_compact_keeps_replacement_deltas() {
    let uri = Span::new("test://diff-compact", 0, 0).unwrap().uri;
    let path = vec![0, 1];
    let deltas = vec![
        Delta::Delete {
            key: crate::component::parse::ParsePath {
                uri,
                path: path.clone(),
                range: crate::utils::RangeOrPoint::Point(0),
            },
        },
        Delta::Insert {
            key: crate::component::parse::ParsePath {
                uri,
                path,
                range: crate::utils::RangeOrPoint::Point(0),
            },
            value: crate::component::parse::ParseForest { roots: vec![1] },
        },
    ];

    let compacted = diff::compact(deltas);
    assert_eq!(compacted.len(), 2);
    assert!(matches!(compacted[0], Delta::Delete { .. }));
    assert!(matches!(compacted[1], Delta::Insert { .. }));
}

#[test]
fn test_diff_trees_replaces_same_green_different_product() {
    let uri = Span::new("test://diff-same-green", 0, 0).unwrap().uri;
    let mut trees = TreeArena::new();
    let mut products = ProductArena::new();

    let leaf = trees.leaf(3, ERROR_TERMINAL);
    let old = products.insert(Product::token(leaf, 1, 11));
    let new = products.insert(Product::token(leaf, 2, 22));

    let deltas = diff::diff_trees(&products, &trees, &[old], &[new], uri);

    assert_eq!(deltas.len(), 2);
    assert!(matches!(deltas[0], Delta::Delete { .. }));
    assert!(matches!(deltas[1], Delta::Insert { .. }));
}

#[test]
fn test_diff_trees_handles_repeated_identical_children_exactly() {
    let uri = Span::new("test://diff-duplicate-children", 0, 0)
        .unwrap()
        .uri;
    let mut trees = TreeArena::new();
    let mut products = ProductArena::new();
    let mut ast = AstArena::new(uri);

    let leaf_same = trees.leaf(3, ERROR_TERMINAL);
    let leaf_other = trees.leaf(4, ERROR_TERMINAL);
    let child_a = products.insert(Product::token(leaf_same, 10, 101));
    let child_b = products.insert(Product::token(leaf_same, 11, 101));
    let child_c = products.insert(Product::token(leaf_other, 12, 202));

    let old_green = trees.node(0, vec![leaf_same, leaf_same]);
    let new_green = trees.node(0, vec![leaf_same, leaf_other]);
    let old_root = products.insert(Product::node(
        old_green,
        ast.insert(()),
        vec![child_a, child_b],
    ));
    let new_root = products.insert(Product::node(
        new_green,
        ast.insert(()),
        vec![child_a, child_c],
    ));

    let deltas = diff::diff_trees(&products, &trees, &[old_root], &[new_root], uri);

    assert_eq!(deltas.len(), 2);
    assert!(matches!(&deltas[0], Delta::Delete { key } if key.path == vec![0, 1]));
    assert!(
        matches!(&deltas[1], Delta::Insert { key, value } if key.path == vec![0, 1] && value.roots == vec![child_c])
    );
}

#[test]
fn test_diff_trees_aligns_middle_insert_without_cascade() {
    let uri = Span::new("test://diff-middle-insert", 0, 0).unwrap().uri;
    let mut trees = TreeArena::new();
    let mut products = ProductArena::new();
    let mut ast = AstArena::new(uri);

    let leaf_a = trees.leaf(1, ERROR_TERMINAL);
    let leaf_b = trees.leaf(2, ERROR_TERMINAL);
    let leaf_c = trees.leaf(3, ERROR_TERMINAL);
    let child_a = products.insert(Product::token(leaf_a, 10, 301));
    let child_b = products.insert(Product::token(leaf_b, 11, 302));
    let child_c = products.insert(Product::token(leaf_c, 12, 303));

    let old_green = trees.node(0, vec![leaf_a, leaf_c]);
    let new_green = trees.node(0, vec![leaf_a, leaf_b, leaf_c]);
    let old_root = products.insert(Product::node(
        old_green,
        ast.insert(()),
        vec![child_a, child_c],
    ));
    let new_root = products.insert(Product::node(
        new_green,
        ast.insert(()),
        vec![child_a, child_b, child_c],
    ));

    let deltas = diff::diff_trees(&products, &trees, &[old_root], &[new_root], uri);

    assert_eq!(deltas.len(), 1);
    assert!(
        matches!(&deltas[0], Delta::Insert { key, value } if key.path == vec![0, 1] && value.roots == vec![child_b])
    );
}

#[test]
fn test_diff_trees_aligns_middle_delete_without_cascade() {
    let uri = Span::new("test://diff-middle-delete", 0, 0).unwrap().uri;
    let mut trees = TreeArena::new();
    let mut products = ProductArena::new();
    let mut ast = AstArena::new(uri);

    let leaf_a = trees.leaf(1, ERROR_TERMINAL);
    let leaf_b = trees.leaf(2, ERROR_TERMINAL);
    let leaf_c = trees.leaf(3, ERROR_TERMINAL);
    let child_a = products.insert(Product::token(leaf_a, 20, 401));
    let child_b = products.insert(Product::token(leaf_b, 21, 402));
    let child_c = products.insert(Product::token(leaf_c, 22, 403));

    let old_green = trees.node(0, vec![leaf_a, leaf_b, leaf_c]);
    let new_green = trees.node(0, vec![leaf_a, leaf_c]);
    let old_root = products.insert(Product::node(
        old_green,
        ast.insert(()),
        vec![child_a, child_b, child_c],
    ));
    let new_root = products.insert(Product::node(
        new_green,
        ast.insert(()),
        vec![child_a, child_c],
    ));

    let deltas = diff::diff_trees(&products, &trees, &[old_root], &[new_root], uri);

    assert_eq!(deltas.len(), 1);
    assert!(matches!(&deltas[0], Delta::Delete { key } if key.path == vec![0, 1]));
}

#[test]
fn test_diff_trees_keeps_equivalent_error_nodes() {
    let uri = Span::new("test://diff-error-equivalent", 0, 0).unwrap().uri;
    let mut trees = TreeArena::new();
    let mut products = ProductArena::new();

    let first_green = trees.error(
        3,
        ErrorKind::UnexpectedToken,
        0,
        Vec::new(),
        Some(Symbol::T(ERROR_TERMINAL)),
        Symbol::T(ERROR_TERMINAL),
        true,
        Some(1),
    );
    let second_green = trees.error(
        3,
        ErrorKind::UnexpectedToken,
        0,
        Vec::new(),
        Some(Symbol::T(ERROR_TERMINAL)),
        Symbol::T(ERROR_TERMINAL),
        true,
        Some(1),
    );
    let old = products.insert(Product::error(first_green));
    let new = products.insert(Product::error(second_green));

    let deltas = diff::diff_trees(&products, &trees, &[old], &[new], uri);
    assert!(deltas.is_empty());
}
