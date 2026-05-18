use enum_iterator::Sequence;
use tokio::sync::mpsc;

use crate::{
    Tokens,
    component::{
        lex::{Lexer, StateAction},
        source::Source,
    },
    scheme::{Delta, Outcome, Runtime},
    utils::Span,
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

#[derive(Debug, PartialEq, Tokens)]
enum RootTokens {
    #[regex(r#"""#)]
    #[enter(StringTokens)]
    QuoteStart,

    #[regex(r"[0-9]+")]
    Number(#[parse(parse_usize)] usize),
}

#[derive(Debug, PartialEq, Tokens)]
enum StringTokens {
    #[regex(r#"[^"]+"#)]
    Text(String),

    #[regex(r#"""#)]
    #[leave]
    QuoteEnd,
}

#[derive(Debug, PartialEq, Tokens)]
enum BestMatchTokens {
    #[regex(r"if")]
    KeywordIf,

    #[regex(r"[a-z]{2}")]
    TwoLetters,

    #[regex(r"abc")]
    LongerAbc,

    #[regex(r"a")]
    SingleA,
}

#[test]
fn test_multi_enum_token_schema() {
    let lexer = Lexer::new::<RootTokens>().unwrap();

    assert!(lexer.state_info().len() >= 2);
    assert!(lexer.tokens().len() >= 2);

    let root_id = lexer.state_id_of::<RootTokens>().unwrap();
    let string_id = lexer.state_id_of::<StringTokens>().unwrap();

    let quote_start = lexer
        .tokens_in_state(root_id)
        .unwrap()
        .iter()
        .find(|token| token.label == "RootTokens::QuoteStart")
        .unwrap();
    assert_eq!(quote_start.action, StateAction::Enter(string_id));

    let number = lexer
        .tokens_in_state(root_id)
        .unwrap()
        .iter()
        .find(|token| token.label == "RootTokens::Number")
        .unwrap();
    let built = number.build("42").unwrap();
    assert_eq!(
        built.downcast_ref::<RootTokens>(),
        Some(&RootTokens::Number(42))
    );
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
