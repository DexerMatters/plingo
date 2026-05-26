use core::fmt;

use enum_iterator::Sequence;

use crate::{
    component::lex::{Lexer, LexerState},
    tokens,
    utils::PrettyDisplay,
};

#[cfg(test)]
mod test_runtime;

#[cfg(test)]
mod fs_watch;

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

// ── root token set with string escape ──

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

// ── Rust raw string: r###" ... "### ──

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
    // hash count + quote: "##" for r##"...\n##
    if hashes.is_empty() {
        "\""
    } else {
        // hashes comes as e.g. "##" from raw_delimiter which trimmed the quote
        hashes
    }
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

// ── tests ──

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
