use core::fmt;

use enum_iterator::Sequence;
use tokio::sync::mpsc;

use crate::{
    component::{
        lex::{Entry, ErrorToken, Lexer, LexerState},
        source::Source,
    },
    scheme::{Delta, Outcome, Runtime},
    tokens,
    utils::{PrettyDisplay, Span},
};

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
    #[regex(r##"#*""##)]
    #[enter(StringTokens)]
    QuoteStart(String),
}

fn validate_content(text: &str, state: Option<&str>) -> bool {
    if let Some(state) = state {
        !text.contains(format!("\"{}", &state[0..state.len() - 2]).as_str())
    } else {
        false
    }
}

fn check_hash_count(lexeme: &str, parent_capture: Option<&str>) -> bool {
    let hash_count = lexeme.chars().filter(|&c| c == '#').count();
    let expected = parent_capture
        .map(|s| s.chars().filter(|&c| c == '#').count())
        .unwrap_or(0);
    hash_count == expected
}

#[tokens]
#[derive(Debug, Clone)]
enum StringTokens {
    #[regex(r#".*"#)]
    #[validate(validate_content)]
    Content(String),
    #[regex(r##""#*"##)]
    #[validate(check_hash_count)]
    #[leave]
    QuoteEnd,
}

impl fmt::Display for RootTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl fmt::Display for StringTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[test]
fn test_multi_enum_token_schema() {
    let mut lexer = Lexer::<RootTokens>::new().unwrap();
    let input = r###"##"ssss"##"###;
    let mut receiever = Vec::new();
    let error = lexer.lex_cont(
        LexerState::new(lexer.state_id_of::<RootTokens>().unwrap()),
        input,
        |token_id| {
            receiever.push(token_id);
            true
        },
    );
    error.map(|err| println!("Lexing error: {:?}", err));
    println!("{}", receiever.pretty(&lexer));
}

#[tokio::test]
async fn test_source_and_sink() -> anyhow::Result<()> {
    let test_file_uri = format!(
        "file://{}",
        workspace_root::get_workspace_root()
            .join("test_data/test.txt")
            .display()
    );
    let (sender, receiver) = mpsc::channel(100);
    let debug_sink = debug_sink!(
        resolve = |_, action| async move {
            println!("Received action: {:?}", action);
            Outcome::ok(String::new())
        },
        consume = |_, deltas| async move {
            println!("Received deltas: {:?}", deltas);
            Ok(())
        }
    );
    let runtime = Runtime::new()
        .with(Source::new(receiver))
        .finish(debug_sink);

    tokio::spawn(async move { runtime.run().await });
    sender
        .send(Delta::Insert {
            key: Span::new(test_file_uri, 0, 0)?,
            value: "Hello world".to_string(),
        })
        .await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait for the runtime to process the message

    Ok(())
}
