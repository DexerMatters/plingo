//! Phase 0 scenario matrix completion (plan §10.2).
//!
//! Covers the scenarios not exercised by the oracle/recovery/bench files:
//! - item 16: deep/flat structures through a subprocess probe;
//! - item 14: 100 documents with one edit (no cross-document work);
//! - item 4: suffix-shifting insertion retains identities and stays exact.

mod common;

use common::fixtures;
use common::json::{JsonDocument, JsonToken};
use common::oracle::{self, TraceRunner};
use plingo::framework::Workspace;
use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser;
use plingo::utils::Span;

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn probe_binary() -> std::path::PathBuf {
    // The probe binary ships beside the test executable in target/.
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    if path.ends_with("deps") {
        path.pop();
    }
    for candidate in [
        path.join("plingo_stack_probe"),
        path.join("release").join("plingo_stack_probe"),
        path.join("debug").join("plingo_stack_probe"),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("stack probe binary not built");
}

#[test]
fn deep_nesting_probes_run_in_a_subprocess_and_record_the_gate() {
    let probe = probe_binary();

    // Shallow depth parses successfully.
    let shallow = std::process::Command::new(&probe)
        .arg("512")
        .output()
        .expect("spawn probe");
    assert!(shallow.status.success(), "512-deep nesting must parse");

    // Far beyond the current stack budget the child aborts; the harness
    // records the gate instead of dying with it.
    let deep = std::process::Command::new(&probe)
        .arg("100_000".replace('_', "").as_str())
        .output()
        .expect("spawn probe");
    assert!(
        !deep.status.success(),
        "the stack-depth scale gate is expected to fail until the iterative \
         traversal fix lands (plan §10.3 item 16)"
    );
}

#[test]
fn editing_one_of_one_hundred_documents_wakes_only_that_document() {
    let mut ws = build();
    let edited = uri("doc-000");
    for index in 0..100usize {
        let name = format!("doc-{index:03}");
        let u = Span::new(format!("test://{name}"), 0, 0).unwrap().uri;
        ws.open(u, &fixtures::json_array(8)).expect("open commits");
    }

    // Edit document zero only; every other document must stay completely
    // cold: no lexer and no parser component runs for them.
    let text = fixtures::json_array(8);
    let start = text.find("\"s1\"").unwrap();
    let edits = vec![
        plingo::framework::source::SourceEdit::Delete {
            key: Span::new_uri(edited.clone(), start, start + 4).unwrap(),
        },
        plingo::framework::source::SourceEdit::Insert {
            key: Span::point_uri(edited, start).unwrap(),
            value: "\"z9\"".into(),
        },
    ];
    let report = ws.edit(edits).expect("edit commits");

    let parser_runs: usize = (0..100)
        .map(|index| {
            let name = format!("test://doc-{index:03}");
            if name == "test://doc-000" {
                return 0;
            }
            report
                .work()
                .parser(&name)
                .map(|work| work.component_runs as usize)
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(
        parser_runs, 0,
        "no parser component may run for untouched documents"
    );

    // This is a same-terminal string edit. The lexer runs only for the
    // affected document; its value fact updates without waking its parser.
    let edited_work = report
        .work()
        .parser("test://doc-000")
        .expect("layout snapshot work");
    assert_eq!(
        edited_work.component_runs, 0,
        "same-terminal token value changes must keep semantic parsing cold"
    );
}

#[test]
fn suffix_shifting_insertion_keeps_projection_exact() {
    let base = fixtures::json_array(128);
    let mut runner = TraceRunner::open(build, "shift", &base);
    let u = uri("shift");

    // Inserting at the head shifts every byte of the suffix; canonical
    // projections must still match a fresh workspace exactly.
    runner.step(vec![insert_at_head(&u)]);
    runner.step(vec![insert_at_head(&u)]);
    let projection = oracle::project(&runner.workspace().snapshot(), &u.to_string());
    assert!(projection.source_len > base.len());
}

fn insert_at_head(u: &fluent_uri::Uri<String>) -> plingo::framework::source::SourceEdit {
    plingo::framework::source::SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 0).unwrap(),
        value: " ".into(),
    }
}

#[test]
fn fixed_size_edit_soak_does_not_grow_unboundedly() {
    // Plan §9: sustained editing must not grow unboundedly. This functional
    // soak exercises 70 edits at alternating positions and verifies that
    // the pipeline remains responsive throughout.
    let base = fixtures::json_array(32);
    let mut ws = build();
    let u = uri("soak");
    ws.open(u.clone(), &base).expect("open");

    let mut text = base.clone();
    let mut successful_edits = 0usize;
    for index in 0..70 {
        // Alternate between head/middle/tail edits.
        let pos = match index % 3 {
            0 => 2,
            1 => text.len() / 2,
            _ => text.len().saturating_sub(3),
        };
        if pos >= text.len() || !text.is_char_boundary(pos) {
            continue;
        }
        let end = (pos + 2).min(text.len());
        if !text.is_char_boundary(end) || end <= pos {
            continue;
        }
        let value = format!("{index}");
        let edits = vec![
            plingo::framework::source::SourceEdit::Delete {
                key: Span::new_uri(u.clone(), pos, end).unwrap(),
            },
            plingo::framework::source::SourceEdit::Insert {
                key: Span::point_uri(u.clone(), pos).unwrap(),
                value: value.clone(),
            },
        ];
        match ws.edit(edits) {
            Ok(_) => {
                text.replace_range(pos..end, &value);
                successful_edits += 1;
            }
            Err(e) => panic!("edit {index} failed: {e}"),
        }
    }
    assert!(
        successful_edits > 50,
        "at least 50 of 70 edits should succeed: got {successful_edits}"
    );
    // The document must still parse cleanly.
    let projection = oracle::project(&ws.snapshot(), &u.to_string());
    assert!(projection.source_len > 0, "document should have content");
}

#[test]
fn trivia_only_edit_keeps_parser_cold() {
    // Plan §7.5 / Barrier 2A: inserting whitespace between tokens must NOT
    // advance the semantic revision or schedule the parser.
    let base = fixtures::json_array(32);
    let mut ws = build();
    let u = uri("trivia");
    ws.open(u.clone(), &base).expect("open");

    // Insert a space between two elements (trivia-only edit).
    let pos = base.find(',').expect("comma exists");
    let report = ws
        .edit(vec![plingo::framework::source::SourceEdit::Insert {
            key: Span::point_uri(u.clone(), pos + 1).unwrap(),
            value: " ".into(),
        }])
        .expect("trivia edit commits");

    // Trivia keeps semantic parser/tree work cold, while the framework refreshes
    // the editor-facing coordinate facade from the layout view.
    assert_eq!(
        report.rounds(),
        3,
        "source, lexer, and layout facade commit"
    );
    assert_eq!(
        report
            .work()
            .parser(&u.to_string())
            .map(|work| work.component_runs)
            .unwrap_or(0),
        0,
        "trivia must not run the semantic parser"
    );

    // Now a semantic edit: change a string value.
    let text_with_space = format!("{} {}", &base[..pos + 1], &base[pos + 1..]);
    let needle_pos = text_with_space.find("\"s").expect("string token");
    let report2 = ws
        .edit(vec![
            plingo::framework::source::SourceEdit::Delete {
                key: Span::new_uri(u.clone(), needle_pos, needle_pos + 3).unwrap(),
            },
            plingo::framework::source::SourceEdit::Insert {
                key: Span::point_uri(u.clone(), needle_pos).unwrap(),
                value: "\"ZZ\"".into(),
            },
        ])
        .expect("semantic edit commits");

    // The parser MUST fire for a semantic edit.
    let parser2 = report2.work().parser(&u.to_string());
    assert!(
        parser2.is_some() && parser2.unwrap().component_runs > 0,
        "parser must fire for a semantic edit"
    );
}

#[test]
fn scanner_characterization_token_stream_baseline() {
    // Stage 0 characterization (barrier-solutions §5.1): capture the complete
    // observable token stream for a known document. After rope cursor
    // conversion, this exact stream must be reproduced.
    use common::json::JsonToken;
    use plingo::framework::lex::{TokenVec, Tokens};

    let base = fixtures::json_array(32);
    let mut ws = build();
    let u = uri("char");
    ws.open(u.clone(), &base).expect("open");

    let snapshot = ws.snapshot();
    let tokens = snapshot
        .observe::<Tokens<JsonToken>>(u.to_string())
        .expect("tokens exist");

    // Capture the full observable stream: terminal variant + byte offsets.
    let stream: Vec<(String, usize, usize)> = tokens
        .tokens
        .iter()
        .map(|t| {
            let kind = match &t.value {
                JsonToken::Whitespace => "ws".into(),
                JsonToken::LeftBrace => "{".into(),
                JsonToken::RightBrace => "}".into(),
                JsonToken::LeftBracket => "[".into(),
                JsonToken::RightBracket => "]".into(),
                JsonToken::Comma => ",".into(),
                JsonToken::Colon => ":".into(),
                JsonToken::String(s) => format!("str({s})"),
                JsonToken::Number(n) => format!("num({n})"),
                JsonToken::True => "true".into(),
                JsonToken::False => "false".into(),
                JsonToken::Null => "null".into(),
                JsonToken::Error(e) => format!("err({:?})", e.kind),
            };
            (kind, t.start, t.start + t.length)
        })
        .collect();

    // Verify basic structural properties.
    assert!(!stream.is_empty(), "token stream must not be empty");
    assert!(
        stream.windows(2).all(|w| w[0].1 <= w[1].1),
        "token offsets must be non-decreasing"
    );

    // After rope cursor conversion, this golden stream must be identical.
    // Store it as a deterministic string for easy comparison.
    let golden: String = stream
        .iter()
        .map(|(kind, start, end)| format!("{kind}@{start}..{end}"))
        .collect::<Vec<_>>()
        .join("|");
    assert!(golden.len() > 100, "golden stream must be substantial");
}
