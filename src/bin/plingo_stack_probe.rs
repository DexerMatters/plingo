//! Deep-stack probing binary (plan §11 Phase 0 step 3).
//!
//! Runs one nested-JSON workspace open at the requested depth so a stack
//! overflow aborts this child process instead of a test harness. Exit
//! codes: 0 = parsed, 2 = pipeline error, SIGABRT = stack overflow
//! (recorded by the caller).

#[path = "../../tests/common/fixtures.rs"]
mod fixtures;
#[path = "../../tests/common/json.rs"]
mod json;

use json::{JsonDocument, JsonToken};
use plingo::framework::Workspace;
use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser;

fn main() {
    let depth: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(4_096);
    let text = fixtures::json_nested(depth);
    eprintln!("probe depth={depth} bytes={}", text.len());
    let result = Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .and_then(|mut ws| ws.open(plingo_uri(), &text));
    match result {
        Ok(_) => {
            eprintln!("probe ok");
        }
        Err(error) => {
            eprintln!("probe pipeline error: {error}");
            std::process::exit(2);
        }
    }
}

fn plingo_uri() -> fluent_uri::Uri<String> {
    plingo::utils::Span::new("test://stack-probe", 0, 0)
        .expect("probe uri parses")
        .uri
}
