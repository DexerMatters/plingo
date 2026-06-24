use crate::{
    NonTerminal, Terminal,
    component::{
        debug::DebugSink,
        lex::{LexErrorInfo, LexToken, Lexer, LexerState},
        parse::{
            AstToken, IncrementalParseStats, ParseChange, ParseErrorInfo, ParsePath, Parser,
            ParserConfig, TokenData,
            data::{
                ast::{AstArena, AstBox},
                product::ProductData,
            },
            grammar::Grammar,
            identity::{eof_fingerprint, error_fingerprint, token_fingerprint},
        },
        source::{Source, SourceEdit},
    },
    scheme::{context::Context, runtime::Runtime},
    utils::{RangeOrPoint, Span},
};
use fluent_uri::Uri;
use log::{Level, LevelFilter, Metadata, Record};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum JsonToken {
    #[regex(r"\s+")]
    #[skip]
    Whitespace,
    #[regex(r"\{")]
    LBrace,
    #[regex(r"\}")]
    RBrace,
    #[regex(r"\[")]
    LBracket,
    #[regex(r"\]")]
    RBracket,
    #[regex(r",")]
    Comma,
    #[regex(r":")]
    Colon,
    #[regex(r"true")]
    True,
    #[regex(r"false")]
    False,
    #[regex(r"null")]
    Null,
    #[regex(r"-?[0-9]+")]
    Number(#[parse(parse_i64)] i64),
    #[regex(r#""[^"]*""#)]
    String(#[parse(parse_json_string)] String),
    #[error]
    Error(LexErrorInfo),
}

fn parse_i64(text: &str) -> Result<i64, std::num::ParseIntError> {
    text.parse()
}

fn parse_json_string(text: &str) -> Result<String, std::convert::Infallible> {
    Ok(text.trim_matches('"').to_string())
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonValue {
    #[rule(JsonToken::String)]
    String(#[from(0)] AstToken<JsonToken>),
    #[rule(JsonToken::Number)]
    Number(#[from(0)] AstToken<JsonToken>),
    #[rule(JsonToken::True)]
    True,
    #[rule(JsonToken::False)]
    False,
    #[rule(JsonToken::Null)]
    Null,
    #[rule($obj(JsonObject))]
    Object(#[from(obj)] AstBox<JsonObject>),
    #[rule($arr(JsonArray))]
    Array(#[from(arr)] AstBox<JsonArray>),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonObject {
    #[rule(JsonToken::LBrace, JsonToken::RBrace)]
    Empty,
    #[rule(JsonToken::LBrace, $members(JsonMembers), JsonToken::RBrace)]
    Members(#[from(members)] AstBox<JsonMembers>),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonMembers {
    #[rule({$members(JsonMember)}{JsonToken::Comma})]
    Many(#[from(members)] Vec<AstBox<JsonMember>>),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonMember {
    #[rule($key(JsonToken::String), JsonToken::Colon, $val(JsonValue))]
    Pair(
        #[from(key)] AstToken<JsonToken>,
        #[from(val)] AstBox<JsonValue>,
    ),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonArray {
    #[rule(JsonToken::LBracket, JsonToken::RBracket)]
    Empty,
    #[rule(JsonToken::LBracket, $els(JsonElements), JsonToken::RBracket)]
    Elements(#[from(els)] AstBox<JsonElements>),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonElements {
    #[rule({$elements(JsonValue)}{JsonToken::Comma})]
    Many(#[from(elements)] Vec<AstBox<JsonValue>>),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

type JsonSink = DebugSink<ParseChange>;
type JsonDirectParser = Parser<JsonToken>;
type JsonRuntimeParser = Parser<JsonToken, JsonSink>;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct JsonSummary {
    keys: Vec<String>,
    errors: usize,
}

#[derive(Debug, Clone, Copy)]
enum Locate {
    First(&'static str),
    Occurrence { needle: &'static str, index: usize },
    Last(&'static str),
}

#[derive(Debug, Clone, Copy)]
enum EditOp {
    InsertBefore { locate: Locate, text: &'static str },
    InsertAfter { locate: Locate, text: &'static str },
    Delete { locate: Locate, len: usize },
}

#[derive(Debug, Clone, Copy)]
struct EditCase {
    name: &'static str,
    initial: &'static str,
    ops: &'static [EditOp],
    expected_keys: &'static [&'static str],
    min_errors: usize,
    requires_convergence: bool,
    expect_reparse: bool,
}

fn json_edit_cases() -> Vec<EditCase> {
    vec![
        EditCase {
            name: "valid_number_growth_and_member_append",
            initial: r#"{"a":1,"b":[2,3]}"#,
            ops: &[
                EditOp::InsertAfter {
                    locate: Locate::First("1"),
                    text: "0",
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","c":{}"#,
                },
            ],
            expected_keys: &["a", "b", "c"],
            min_errors: 0,
            requires_convergence: true,
            expect_reparse: true,
        },
        EditCase {
            name: "valid_nested_object_and_array_append",
            initial: r#"{"user":{"name":"zoe"},"flags":[true,false]}"#,
            ops: &[
                EditOp::InsertBefore {
                    locate: Locate::First("}"),
                    text: r#","age":7"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("]"),
                    text: ",null",
                },
            ],
            expected_keys: &["user", "flags"],
            min_errors: 0,
            requires_convergence: true,
            expect_reparse: true,
        },
        EditCase {
            name: "valid_deep_append_and_nested_member_add",
            initial: r#"{"arr":[1,2,3],"ok":{"x":1}}"#,
            ops: &[
                EditOp::InsertBefore {
                    locate: Locate::First("3"),
                    text: "0",
                },
                EditOp::InsertBefore {
                    locate: Locate::First("}"),
                    text: r#","y":2"#,
                },
            ],
            expected_keys: &["arr", "ok"],
            min_errors: 0,
            requires_convergence: true,
            expect_reparse: true,
        },
        EditCase {
            name: "valid_large_member_append",
            initial: r#"{"a":1}"#,
            ops: &[
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","j":true"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","k":[1,2,3]"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","l":{"a":1,"b":2}"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","m":null"#,
                },
            ],
            expected_keys: &["a", "j", "k", "l", "m"],
            min_errors: 0,
            requires_convergence: true,
            expect_reparse: true,
        },
        EditCase {
            name: "invalid_large_member_append_after_root",
            initial: r#"{"a":1}"#,
            ops: &[EditOp::InsertAfter {
                locate: Locate::Last("}"),
                text: r#","j":true,"k":[1,2,3],"l":{"a":1,"b":2},"m":null"#,
            }],
            expected_keys: &["a"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "invalid_null_suffix_keeps_tail",
            initial: r#"{"xddd":null,"he":222,"well":{}}"#,
            ops: &[EditOp::InsertAfter {
                locate: Locate::First("null"),
                text: "s",
            }],
            expected_keys: &["xddd", "he", "well"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "invalid_null_suffix_keeps_tail_w",
            initial: r#"{"xddd":null,"he":222,"well":{}}"#,
            ops: &[EditOp::InsertAfter {
                locate: Locate::First("null"),
                text: "w",
            }],
            expected_keys: &["xddd", "he", "well"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "invalid_missing_colon_recovers",
            initial: r#"{"a":1,"b":2,"c":3}"#,
            ops: &[EditOp::Delete {
                locate: Locate::Occurrence {
                    needle: ":",
                    index: 1,
                },
                len: 1,
            }],
            expected_keys: &["a", "b", "c"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "invalid_extra_comma_recovers",
            initial: r#"{"a":1,"b":2,"c":3}"#,
            ops: &[EditOp::InsertBefore {
                locate: Locate::First("\"b\""),
                text: ",",
            }],
            expected_keys: &["a", "b", "c"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "invalid_lexer_garbage_keeps_following_member",
            initial: r#"{"a":1,"b":2,"c":3}"#,
            ops: &[EditOp::InsertBefore {
                locate: Locate::First(",\"c\""),
                text: "xyz",
            }],
            expected_keys: &["a", "b", "c"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
    ]
}

fn json_runtime_cases() -> Vec<EditCase> {
    vec![
        EditCase {
            name: "runtime_string_growth",
            initial: r#"{"he":"101"}"#,
            ops: &[EditOp::InsertAfter {
                locate: Locate::First("101"),
                text: "3",
            }],
            expected_keys: &["he"],
            min_errors: 0,
            requires_convergence: true,
            expect_reparse: true,
        },
        EditCase {
            name: "runtime_string_shorten",
            initial: r#"{"he":"101"}"#,
            ops: &[EditOp::Delete {
                locate: Locate::Last("1"),
                len: 1,
            }],
            expected_keys: &["he"],
            min_errors: 0,
            requires_convergence: true,
            expect_reparse: true,
        },
        EditCase {
            name: "runtime_whitespace_delete_is_ignored",
            initial: r#"{"a":1, "b":2, "c":3}"#,
            ops: &[EditOp::Delete {
                locate: Locate::First(" "),
                len: 1,
            }],
            expected_keys: &["a", "b", "c"],
            min_errors: 0,
            requires_convergence: false,
            expect_reparse: false,
        },
        EditCase {
            name: "runtime_repeated_child_rewrite",
            initial: r#"[1,1,1]"#,
            ops: &[
                EditOp::Delete {
                    locate: Locate::Occurrence {
                        needle: "1",
                        index: 1,
                    },
                    len: 1,
                },
                EditOp::InsertBefore {
                    locate: Locate::Occurrence {
                        needle: ",",
                        index: 1,
                    },
                    text: "2",
                },
            ],
            expected_keys: &[],
            min_errors: 0,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "runtime_large_member_append",
            initial: r#"{"a":1}"#,
            ops: &[
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","j":true"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","k":[1,2,3]"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","l":{"a":1,"b":2}"#,
                },
                EditOp::InsertBefore {
                    locate: Locate::Last("}"),
                    text: r#","m":null"#,
                },
            ],
            expected_keys: &["a", "j", "k", "l", "m"],
            min_errors: 0,
            requires_convergence: false,
            expect_reparse: true,
        },
        EditCase {
            name: "runtime_large_member_append_after_root",
            initial: r#"{"a":1}"#,
            ops: &[EditOp::InsertAfter {
                locate: Locate::Last("}"),
                text: r#","j":true,"k":[1,2,3],"l":{"a":1,"b":2},"m":null"#,
            }],
            expected_keys: &["a"],
            min_errors: 1,
            requires_convergence: false,
            expect_reparse: true,
        },
    ]
}

fn collect_entries(lexer: &mut Lexer<JsonToken>, input: &str) -> Vec<LexToken<JsonToken>> {
    let token_ids: Vec<usize> = {
        let mut ids = Vec::new();
        lexer
            .lex_cont(
                LexerState::new(lexer.state_id_of::<JsonToken>().unwrap()),
                input.to_string(),
                |token_id, _, _, _| {
                    ids.push(token_id);
                    true
                },
            )
            .unwrap();
        ids
    };
    token_ids
        .into_iter()
        .map(|id| lexer.token(id).unwrap().clone())
        .collect()
}

fn token_data_from_entries(entries: &[LexToken<JsonToken>]) -> Vec<TokenData> {
    let mut data = entries
        .iter()
        .enumerate()
        .map(|(column, token)| match token.error {
            Some(info) => TokenData {
                id: token.id,
                terminal: None,
                start: token.start,
                length: token.length,
                column,
                fingerprint: error_fingerprint(&info, token.length),
            },
            None => TokenData {
                id: token.id,
                terminal: token.terminal,
                start: token.start,
                length: token.length,
                column,
                fingerprint: token_fingerprint(token.terminal, &token.value, token.length),
            },
        })
        .collect::<Vec<_>>();
    let eof_start = entries
        .last()
        .map(|token| token.start + token.length)
        .unwrap_or(0);
    data.push(TokenData {
        id: usize::MAX,
        terminal: None,
        start: eof_start,
        length: 0,
        column: data.len(),
        fingerprint: eof_fingerprint(),
    });
    data
}

#[test]
fn json_runtime_grammar_uses_repetition_not_manual_tail_nonterminals() {
    let grammar = Grammar::from_spec::<JsonValue>();
    let labels = grammar
        .non_terminals
        .iter()
        .map(|non_terminal| non_terminal.label)
        .collect::<Vec<_>>();

    assert!(labels.contains(&"JsonMembers"));
    assert!(labels.contains(&"JsonElements"));
    assert!(!labels.contains(&"JsonMembersTail"));
    assert!(!labels.contains(&"JsonElementsTail"));
}

fn find_location(text: &str, locate: Locate) -> anyhow::Result<usize> {
    let pos = match locate {
        Locate::First(needle) => text
            .find(needle)
            .ok_or_else(|| anyhow::anyhow!("missing substring {needle:?}"))?,
        Locate::Occurrence { needle, index } => text
            .match_indices(needle)
            .nth(index)
            .map(|(idx, _)| idx)
            .ok_or_else(|| anyhow::anyhow!("missing occurrence {index} of {needle:?}"))?,
        Locate::Last(needle) => text
            .rfind(needle)
            .ok_or_else(|| anyhow::anyhow!("missing last occurrence of {needle:?}"))?,
    };
    Ok(pos)
}

fn apply_edit(
    current: &mut String,
    uri: Uri<&'static str>,
    op: EditOp,
) -> anyhow::Result<SourceEdit> {
    match op {
        EditOp::InsertBefore { locate, text } => {
            let pos = find_location(current, locate)?;
            current.insert_str(pos, text);
            Ok(SourceEdit::Insert {
                key: Span::new_uri(uri, pos, pos)?,
                value: text.to_string(),
            })
        }
        EditOp::InsertAfter { locate, text } => {
            let pos = find_location(current, locate)?;
            let at = pos
                + match locate {
                    Locate::First(needle) | Locate::Last(needle) => needle.len(),
                    Locate::Occurrence { needle, .. } => needle.len(),
                };
            current.insert_str(at, text);
            Ok(SourceEdit::Insert {
                key: Span::new_uri(uri, at, at)?,
                value: text.to_string(),
            })
        }
        EditOp::Delete { locate, len } => {
            let pos = find_location(current, locate)?;
            current.replace_range(pos..pos + len, "");
            Ok(SourceEdit::Delete {
                key: Span::new_uri(uri, pos, pos + len)?,
            })
        }
    }
}

fn token_data_shape(data: &[TokenData]) -> Vec<(Option<u32>, usize, usize, usize)> {
    data.iter()
        .map(|token| {
            (
                token.terminal.map(|t| t.token_id),
                token.start,
                token.length,
                token.column,
            )
        })
        .collect()
}

fn apply_edits(
    initial: &str,
    ops: &[EditOp],
    uri: Uri<&'static str>,
) -> anyhow::Result<(String, Vec<SourceEdit>)> {
    let mut current = initial.to_string();
    let mut deltas = Vec::with_capacity(ops.len());
    for &op in ops {
        deltas.push(apply_edit(&mut current, uri.clone(), op)?);
    }
    Ok((current, deltas))
}

fn build_direct_case(
    initial: &str,
    ops: &[EditOp],
    config: ParserConfig,
) -> anyhow::Result<(
    JsonDirectParser,
    Lexer<JsonToken>,
    Uri<&'static str>,
    Vec<TokenData>,
    String,
)> {
    let uri = Span::new("test://json-comprehensive", 0, 0)?.uri;
    let (final_source, _) = apply_edits(initial, ops, uri.clone())?;
    let parser = Grammar::from_spec::<JsonValue>().build_lr1_with_config::<JsonToken, ()>(config);
    let mut lexer = Lexer::<JsonToken>::new()?;
    let entries = collect_entries(&mut lexer, &final_source);
    let token_data = token_data_from_entries(&entries);
    Ok((parser, lexer, uri, token_data, final_source))
}

fn direct_root_keys(
    ast: &AstArena,
    token_data: &[TokenData],
    source: &str,
    value: &JsonValue,
) -> anyhow::Result<Vec<String>> {
    let token_text = |id: usize| -> anyhow::Result<String> {
        let data = token_data
            .iter()
            .find(|data| data.id == id)
            .ok_or_else(|| anyhow::anyhow!("missing token data for entry {id}"))?;
        source
            .get(data.start..data.start + data.length)
            .map(|slice| slice.trim_matches('"').to_string())
            .ok_or_else(|| anyhow::anyhow!("invalid token span for entry {id}"))
    };

    let mut keys = Vec::new();
    let JsonValue::Object(obj) = value else {
        return Ok(keys);
    };
    let obj = ast
        .get(*obj)
        .ok_or_else(|| anyhow::anyhow!("missing JsonObject"))?;
    let JsonObject::Members(mems) = obj else {
        return Ok(keys);
    };
    let JsonMembers::Many(members) = ast
        .get(*mems)
        .ok_or_else(|| anyhow::anyhow!("missing JsonMembers"))?
    else {
        return Ok(keys);
    };
    for member in members {
        let member = ast
            .get(*member)
            .ok_or_else(|| anyhow::anyhow!("missing JsonMember"))?;
        let JsonMember::Pair(key, _) = member else {
            return Ok(keys);
        };
        keys.push(token_text(key.id)?);
    }
    Ok(keys)
}

fn direct_summary(
    parser: &JsonDirectParser,
    token_data: &[TokenData],
    source: &str,
    uri: Uri<&'static str>,
) -> anyhow::Result<JsonSummary> {
    let state = parser
        .session_state(uri.clone())
        .ok_or_else(|| anyhow::anyhow!("missing parser state"))?;
    let accepted = state.accepted();
    if accepted.len() != 1 {
        return Err(anyhow::anyhow!(
            "expected one accepted root, found {}",
            accepted.len()
        ));
    }
    let product = parser
        .session_product(uri.clone(), accepted[0])
        .ok_or_else(|| anyhow::anyhow!("missing accepted product"))?;
    let ProductData::Node { ast, .. } = &product.data else {
        return Err(anyhow::anyhow!("accepted product was not a node"));
    };
    let arenas = parser
        .session_arenas
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("missing parser arenas"))?;
    let root = arenas
        .ast
        .cloned::<JsonValue>(*ast)
        .ok_or_else(|| anyhow::anyhow!("missing root AST value"))?;

    let keys = direct_root_keys(&arenas.ast, token_data, source, &root)?;
    let errors = parser.latest_parse_diagnostics(uri).len();
    Ok(JsonSummary { keys, errors })
}

async fn runtime_root_keys(ctx: &Context, obj: &JsonObject) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    let JsonObject::Members(mems) = obj else {
        return Ok(keys);
    };
    let members = ctx
        .call(JsonRuntimeParser::deref_ast_box::<JsonMembers>, *mems)
        .await?;
    let JsonMembers::Many(members) = members else {
        return Ok(keys);
    };
    for member in members {
        let member = ctx
            .call(JsonRuntimeParser::deref_ast_box::<JsonMember>, member)
            .await?;
        let JsonMember::Pair(key, _) = member else {
            return Ok(keys);
        };
        let JsonToken::String(s) = ctx
            .call(JsonRuntimeParser::deref_ast_token::<JsonToken>, key)
            .await?
        else {
            return Err(anyhow::anyhow!("expected string token for object key"));
        };
        keys.push(s);
    }
    Ok(keys)
}

async fn runtime_summary(ctx: &Context, root: AstBox<JsonValue>) -> anyhow::Result<JsonSummary> {
    let value = ctx
        .call(JsonRuntimeParser::deref_ast_box::<JsonValue>, root)
        .await?;
    let keys = match &value {
        JsonValue::Object(obj) => {
            let obj = ctx
                .call(JsonRuntimeParser::deref_ast_box::<JsonObject>, *obj)
                .await?;
            runtime_root_keys(ctx, &obj).await?
        }
        _ => Vec::new(),
    };
    let diagnostics = ctx
        .call(JsonRuntimeParser::parse_diagnostics, root.uri)
        .await?;
    let errors = diagnostics.len();
    Ok(JsonSummary { keys, errors })
}

async fn recv_non_empty_parse_batch(
    rx: &mut mpsc::Receiver<Vec<ParseChange>>,
) -> anyhow::Result<Vec<ParseChange>> {
    let batch = timeout(Duration::from_secs(2), rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("parse sink channel closed"))?;
    Ok(batch)
}

async fn recv_parse_batches_until_quiet(
    rx: &mut mpsc::Receiver<Vec<ParseChange>>,
) -> anyhow::Result<Vec<Vec<ParseChange>>> {
    let mut batches = vec![recv_non_empty_parse_batch(rx).await?];
    loop {
        match timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(batch)) => batches.push(batch),
            Ok(None) | Err(_) => break,
        }
    }
    Ok(batches)
}

async fn run_runtime_case(
    case_name: &str,
    initial: &str,
    ops: &[EditOp],
) -> anyhow::Result<(
    JsonSummary,
    Vec<Vec<ParseChange>>,
    Vec<IncrementalParseStats>,
    String,
)> {
    let (source_tx, source_rx) = mpsc::channel(32);
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new(
        format!("test://json-comprehensive-runtime/{case_name}"),
        0,
        0,
    )?
    .uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri.clone(), 0, 0)?,
            value: initial.to_string(),
        })
        .await?;

    let mut batches = vec![recv_non_empty_parse_batch(&mut sink_rx).await?];
    let mut stats = Vec::new();
    let mut current = initial.to_string();
    for &op in ops {
        let delta = apply_edit(&mut current, uri.clone(), op)?;
        source_tx.send(delta).await?;
        let op_batches = recv_parse_batches_until_quiet(&mut sink_rx).await?;
        batches.push(
            op_batches
                .into_iter()
                .rev()
                .find(|batch| !batch.is_empty())
                .unwrap_or_default(),
        );
        let stat = runtime
            .context()
            .call(JsonRuntimeParser::incremental_stats_for, uri.clone())
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing incremental stats"))?;
        stats.push(stat);
    }

    let ctx = runtime.context();
    let root_path = ParsePath {
        uri: uri.clone(),
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = ctx
        .call(JsonRuntimeParser::get_ast_tree::<JsonValue>, root_path)
        .await?;
    if roots.len() != 1 {
        return Err(anyhow::anyhow!(
            "expected exactly one root AST box, found {}",
            roots.len()
        ));
    }
    let summary = runtime_summary(&ctx, roots[0]).await?;
    runtime.shutdown().await;
    Ok((summary, batches, stats, current))
}

async fn run_runtime_batched_case(
    case_name: &str,
    initial: &str,
    ops: &[EditOp],
) -> anyhow::Result<(
    JsonSummary,
    Vec<Vec<ParseChange>>,
    Vec<IncrementalParseStats>,
    String,
    usize,
    usize,
)> {
    let (source_tx, source_rx) = mpsc::channel(32);
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new(
        format!("test://json-comprehensive-runtime/{case_name}"),
        0,
        0,
    )?
    .uri;
    source_tx.try_send(SourceEdit::Insert {
        key: Span::new_uri(uri.clone(), 0, 0)?,
        value: initial.to_string(),
    })?;

    let mut batches = vec![recv_non_empty_parse_batch(&mut sink_rx).await?];
    let mut stats = Vec::new();
    let mut current = initial.to_string();
    for &op in ops {
        let delta = apply_edit(&mut current, uri.clone(), op)?;
        source_tx.try_send(delta)?;
    }
    batches.extend(recv_parse_batches_until_quiet(&mut sink_rx).await?);
    let stat = runtime
        .context()
        .call(JsonRuntimeParser::incremental_stats_for, uri.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing incremental stats"))?;
    stats.push(stat);

    let ctx = runtime.context();
    let token_data = ctx
        .call(
            Jl::parse_tokens,
            Span {
                uri: uri.clone(),
                range: RangeOrPoint::Range(0, usize::MAX),
            },
        )
        .await?;
    let mut fresh_lexer = Lexer::<JsonToken>::new()?;
    let fresh_entries = collect_entries(&mut fresh_lexer, &current);
    let expected_token_data = token_data_from_entries(&fresh_entries);
    assert_eq!(
        token_data_shape(&token_data),
        token_data_shape(&expected_token_data),
        "case {} runtime token stream diverged from fresh lexing",
        case_name
    );
    let mut direct_parser = Grammar::from_spec::<JsonValue>()
        .build_lr1_with_config::<JsonToken, ()>(ParserConfig::default());
    direct_parser
        .parse_tokens_at(uri.clone(), &token_data)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let _direct_summary = direct_summary(&direct_parser, &token_data, &current, uri.clone())?;
    let first_eof = token_data
        .iter()
        .position(|token| token.terminal.is_none())
        .unwrap_or(token_data.len());
    let root_path = ParsePath {
        uri: uri.clone(),
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = ctx
        .call(JsonRuntimeParser::get_ast_tree::<JsonValue>, root_path)
        .await?;
    if roots.len() != 1 {
        return Err(anyhow::anyhow!(
            "expected exactly one root AST box, found {}",
            roots.len()
        ));
    }
    let summary = runtime_summary(&ctx, roots[0]).await?;
    runtime.shutdown().await;
    Ok((
        summary,
        batches,
        stats,
        current,
        token_data.len(),
        first_eof,
    ))
}

async fn run_runtime_summary_case(
    case_name: &str,
    initial: &str,
    ops: &[EditOp],
) -> anyhow::Result<(JsonSummary, String)> {
    let (source_tx, source_rx) = mpsc::channel(32);
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _: &Vec<ParseChange> = &deltas;
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new(
        format!("test://json-comprehensive-runtime/{case_name}"),
        0,
        0,
    )?
    .uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri.clone(), 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    let mut current = initial.to_string();
    for &op in ops {
        let delta = apply_edit(&mut current, uri.clone(), op)?;
        source_tx.send(delta).await?;
        let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;
    }

    let ctx = runtime.context();
    let root_path = ParsePath {
        uri: uri.clone(),
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = ctx
        .call(JsonRuntimeParser::get_ast_tree::<JsonValue>, root_path)
        .await?;
    if roots.len() != 1 {
        return Err(anyhow::anyhow!(
            "expected exactly one root AST box, found {}",
            roots.len()
        ));
    }
    let summary = runtime_summary(&ctx, roots[0]).await?;
    runtime.shutdown().await;
    Ok((summary, current))
}

async fn run_runtime_diagnostic_counts(
    case_name: &str,
    initial: &str,
    ops: &[EditOp],
) -> anyhow::Result<Vec<usize>> {
    let (source_tx, source_rx) = mpsc::channel(32);
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new(
        format!("test://json-comprehensive-runtime/{case_name}"),
        0,
        0,
    )?
    .uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri.clone(), 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    let mut current = initial.to_string();
    let mut counts = Vec::with_capacity(ops.len() + 1);
    let initial_count = runtime
        .context()
        .call(JsonRuntimeParser::parse_diagnostics, uri.clone())
        .await?
        .len();
    counts.push(initial_count);

    for &op in ops {
        let delta = apply_edit(&mut current, uri.clone(), op)?;
        source_tx.send(delta).await?;
        let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;
        let count = runtime
            .context()
            .call(JsonRuntimeParser::parse_diagnostics, uri.clone())
            .await?
            .len();
        counts.push(count);
    }

    runtime.shutdown().await;
    Ok(counts)
}

fn visible_token_boundary_positions(source: &str) -> anyhow::Result<Vec<usize>> {
    let mut lexer = Lexer::<JsonToken>::new()?;
    let entries = collect_entries(&mut lexer, source);
    let mut positions = vec![0, source.len()];

    for token in entries {
        positions.push(token.start);
        positions.push(token.start + token.length);
    }

    positions.sort_unstable();
    positions.dedup();
    Ok(positions)
}

async fn runtime_token_data(
    ctx: &Context,
    uri: Uri<&'static str>,
) -> anyhow::Result<Vec<TokenData>> {
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    Ok(ctx
        .call(
            Jl::parse_tokens,
            Span {
                uri,
                range: RangeOrPoint::Range(0, usize::MAX),
            },
        )
        .await?)
}

async fn assert_runtime_matches_fresh_parse(
    ctx: &Context,
    uri: Uri<&'static str>,
    source: &str,
    label: &str,
) -> anyhow::Result<JsonSummary> {
    let token_data = runtime_token_data(ctx, uri.clone()).await?;
    let mut fresh_lexer = Lexer::<JsonToken>::new()?;
    let fresh_entries = collect_entries(&mut fresh_lexer, source);
    let expected_token_data = token_data_from_entries(&fresh_entries);
    assert_eq!(
        token_data_shape(&token_data),
        token_data_shape(&expected_token_data),
        "{label}: runtime token stream diverged from fresh lexing"
    );

    let mut direct_parser = Grammar::from_spec::<JsonValue>()
        .build_lr1_with_config::<JsonToken, ()>(ParserConfig::default());
    direct_parser
        .parse_tokens_at(uri.clone(), &token_data)
        .map_err(|e| anyhow::anyhow!("{label}: {}", e))?;
    let direct = direct_summary(&direct_parser, &token_data, source, uri.clone())?;

    let root_path = ParsePath {
        uri: uri.clone(),
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = ctx
        .call(JsonRuntimeParser::get_ast_tree::<JsonValue>, root_path)
        .await?;
    assert_eq!(
        roots.len(),
        1,
        "{label}: expected exactly one runtime root, found {}",
        roots.len()
    );
    let runtime = runtime_summary(ctx, roots[0]).await?;
    assert_eq!(
        runtime, direct,
        "{label}: runtime parse summary diverged from fresh direct parse"
    );
    Ok(runtime)
}

struct MeasureLogCapture {
    lines: Mutex<Vec<String>>,
}

impl MeasureLogCapture {
    const fn new() -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
        }
    }

    fn clear(&self) {
        self.lines
            .lock()
            .expect("measure log mutex poisoned")
            .clear();
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .expect("measure log mutex poisoned")
            .clone()
    }
}

impl log::Log for MeasureLogCapture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "Measure" && metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.lines
            .lock()
            .expect("measure log mutex poisoned")
            .push(record.args().to_string());
    }

    fn flush(&self) {}
}

#[derive(Default)]
struct MeasureAggregate {
    parse_samples: usize,
    lex_samples: usize,
    parse: HashMap<String, Duration>,
    lex: HashMap<String, Duration>,
}

fn measure_logger() -> &'static MeasureLogCapture {
    static LOGGER: OnceLock<MeasureLogCapture> = OnceLock::new();
    let logger = LOGGER.get_or_init(MeasureLogCapture::new);
    let _ = log::set_logger(logger);
    log::set_max_level(LevelFilter::Debug);
    logger.clear();
    logger
}

fn parse_measure_duration(value: &str) -> Option<Duration> {
    let (number, scale) = if let Some(value) = value.strip_suffix("ns") {
        (value, 1e-9)
    } else if let Some(value) = value.strip_suffix("µs") {
        (value, 1e-6)
    } else if let Some(value) = value.strip_suffix("ms") {
        (value, 1e-3)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1.0)
    } else {
        return None;
    };
    let value = number.parse::<f64>().ok()?;
    Some(Duration::from_secs_f64(value * scale))
}

fn aggregate_measure_lines(lines: &[String]) -> MeasureAggregate {
    let mut aggregate = MeasureAggregate::default();

    for line in lines {
        let mut parts = line.split_whitespace();
        let Some(kind) = parts.next() else {
            continue;
        };
        let Some(_uri) = parts.next() else {
            continue;
        };

        let (sample_count, totals) = match kind {
            "parse" => (&mut aggregate.parse_samples, &mut aggregate.parse),
            "lex" => (&mut aggregate.lex_samples, &mut aggregate.lex),
            _ => continue,
        };
        *sample_count += 1;

        for part in parts {
            let Some((phase, value)) = part.split_once('=') else {
                continue;
            };
            let Some(duration) = parse_measure_duration(value) else {
                continue;
            };
            *totals.entry(phase.to_string()).or_default() += duration;
        }
    }

    aggregate
}

fn print_measure_totals(label: &str, samples: usize, totals: &HashMap<String, Duration>) {
    eprintln!("{label} samples={samples}");
    let mut rows = totals.iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(left.1));
    for (phase, duration) in rows {
        eprintln!("  {phase}={duration:?}");
    }
}

fn assert_summary(case: &EditCase, summary: &JsonSummary) {
    let expected_keys = case
        .expected_keys
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    assert_eq!(summary.keys, expected_keys, "case: {}", case.name);
    if case.min_errors == 0 {
        assert_eq!(summary.errors, 0, "case: {}", case.name);
    } else {
        assert!(
            summary.errors >= case.min_errors,
            "case: {} expected at least {} error node(s), found {}",
            case.name,
            case.min_errors,
            summary.errors
        );
    }
}

#[test]
fn json_syntax_comprehensive_edit_matrix() -> anyhow::Result<()> {
    for case in json_edit_cases()
        .into_iter()
        .filter(|case| case.min_errors == 0)
    {
        let (mut parser, _lexer, uri, token_data, final_source) =
            build_direct_case(case.initial, case.ops, ParserConfig::default())?;
        parser
            .parse_tokens_at(uri.clone(), &token_data)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let summary = direct_summary(&parser, &token_data, &final_source, uri.clone())?;
        assert_summary(&case, &summary);
        assert!(
            !final_source.is_empty(),
            "case {} unexpectedly produced an empty document",
            case.name
        );
    }
    Ok(())
}

#[test]
fn json_syntax_rejects_unrecoverable_missing_root_brace_without_recovery() -> anyhow::Result<()> {
    let initial = r#"{"a":1,"b":2}"#;
    let ops = [EditOp::Delete {
        locate: Locate::Last("}"),
        len: 1,
    }];
    let (mut parser, _lexer, uri, token_data, _final_source) = build_direct_case(
        initial,
        &ops,
        ParserConfig {
            error_recovery: false,
            ..ParserConfig::default()
        },
    )?;

    let result = parser.parse_tokens_at(uri, &token_data);
    assert!(result.is_err());
    Ok(())
}

#[test]
fn json_syntax_null_suffix_preserves_tail_w() -> anyhow::Result<()> {
    let case = EditCase {
        name: "invalid_null_suffix_keeps_tail_w",
        initial: r#"{"xddd":null,"he":222,"well":{}}"#,
        ops: &[EditOp::InsertAfter {
            locate: Locate::First("null"),
            text: "w",
        }],
        expected_keys: &["xddd", "he", "well"],
        min_errors: 1,
        requires_convergence: false,
        expect_reparse: true,
    };

    let (mut parser, _lexer, uri, token_data, final_source) =
        build_direct_case(case.initial, case.ops, ParserConfig::default())?;
    parser
        .parse_tokens_at(uri.clone(), &token_data)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let summary = direct_summary(&parser, &token_data, &final_source, uri)?;
    assert_summary(&case, &summary);
    Ok(())
}

#[tokio::test]
async fn json_runtime_comprehensive_edit_matrix() -> anyhow::Result<()> {
    for case in json_runtime_cases() {
        let (summary, batches, stats, final_source) =
            run_runtime_case(case.name, case.initial, case.ops)
                .await
                .map_err(|e| anyhow::anyhow!("case {} failed: {e}", case.name))?;
        assert_summary(&case, &summary);
        assert!(
            batches.iter().any(|batch| !batch.is_empty()),
            "case {} never emitted a non-empty batch",
            case.name
        );
        assert!(
            !case.requires_convergence || stats.iter().all(|stat| stat.converged),
            "case {} did not converge for every incremental edit",
            case.name
        );
        assert!(
            !final_source.is_empty(),
            "case {} unexpectedly produced an empty runtime source",
            case.name
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn json_runtime_measure_profile_summary() -> anyhow::Result<()> {
    let logger = measure_logger();

    for case in json_runtime_cases() {
        let _ = run_runtime_case(case.name, case.initial, case.ops)
            .await
            .map_err(|e| anyhow::anyhow!("case {} failed: {e}", case.name))?;
    }

    let batched_case = EditCase {
        name: "runtime_large_member_append_profile",
        initial: r#"{"a":1}"#,
        ops: &[
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","j":true"#,
            },
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","k":[1,2,3]"#,
            },
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","l":{"a":1,"b":2}"#,
            },
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","m":null"#,
            },
        ],
        expected_keys: &["a", "j", "k", "l", "m"],
        min_errors: 0,
        requires_convergence: true,
        expect_reparse: true,
    };
    let _ =
        run_runtime_batched_case(batched_case.name, batched_case.initial, batched_case.ops).await?;

    let recovery_case = EditCase {
        name: "invalid_extra_comma_recovers_profile",
        initial: r#"{"a":1,"b":2,"c":3}"#,
        ops: &[EditOp::InsertBefore {
            locate: Locate::First("\"b\""),
            text: ",",
        }],
        expected_keys: &["a", "b", "c"],
        min_errors: 1,
        requires_convergence: false,
        expect_reparse: true,
    };
    let _ = run_runtime_case(recovery_case.name, recovery_case.initial, recovery_case.ops).await?;

    let _ = run_runtime_summary_case(
        "invalid_null_suffix_keeps_tail_profile",
        r#"{"xddd":null,"he":222,"well":{}}"#,
        &[EditOp::InsertAfter {
            locate: Locate::First("null"),
            text: "s",
        }],
    )
    .await?;

    let _ = run_runtime_diagnostic_counts(
        "fixed_error_clears_diagnostics_profile",
        r#"{"xddd":null,"he":222,"well":{}}"#,
        &[
            EditOp::InsertAfter {
                locate: Locate::First("null"),
                text: "s",
            },
            EditOp::Delete {
                locate: Locate::First("s"),
                len: 1,
            },
        ],
    )
    .await?;

    let lines = logger.snapshot();
    assert!(
        !lines.is_empty(),
        "no Measure logs captured; run this ignored profiling test in isolation"
    );
    let aggregate = aggregate_measure_lines(&lines);
    assert!(
        aggregate.parse_samples > 0,
        "no parse Measure samples captured"
    );
    assert!(aggregate.lex_samples > 0, "no lex Measure samples captured");

    print_measure_totals("parse", aggregate.parse_samples, &aggregate.parse);
    print_measure_totals("lex", aggregate.lex_samples, &aggregate.lex);
    Ok(())
}

#[tokio::test]
async fn json_runtime_batched_large_append_matrix() -> anyhow::Result<()> {
    let _ = env_logger::builder().is_test(true).try_init();
    let case = EditCase {
        name: "runtime_large_member_append",
        initial: r#"{"a":1}"#,
        ops: &[
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","j":true"#,
            },
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","k":[1,2,3]"#,
            },
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","l":{"a":1,"b":2}"#,
            },
            EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","m":null"#,
            },
        ],
        expected_keys: &["a", "j", "k", "l", "m"],
        min_errors: 0,
        requires_convergence: true,
        expect_reparse: true,
    };

    let (summary, batches, stats, final_source, token_count, first_eof) =
        run_runtime_batched_case(case.name, case.initial, case.ops)
            .await
            .map_err(|e| anyhow::anyhow!("case {} failed: {e}", case.name))?;
    assert!(
        token_count >= 17,
        "case {} lost visible tail tokens; token_count={}",
        case.name,
        token_count
    );
    assert_eq!(
        first_eof + 1,
        token_count,
        "case {} hit EOF before the final visible tail; first_eof={}, token_count={}",
        case.name,
        first_eof,
        token_count
    );
    assert_summary(&case, &summary);
    assert!(
        batches.iter().all(|batch| !batch.is_empty()),
        "case {} emitted an empty batch",
        case.name
    );
    assert!(
        !case.expect_reparse || stats.iter().all(|stat| stat.reparsed > 0),
        "case {} expected parser activity but got a skip-only edit",
        case.name
    );
    assert_eq!(
        final_source,
        r#"{"a":1,"j":true,"k":[1,2,3],"l":{"a":1,"b":2},"m":null}"#
    );
    Ok(())
}

#[tokio::test]
async fn json_runtime_invalid_edits_recover() -> anyhow::Result<()> {
    let case = EditCase {
        name: "invalid_extra_comma_recovers",
        initial: r#"{"a":1,"b":2,"c":3}"#,
        ops: &[EditOp::InsertBefore {
            locate: Locate::First("\"b\""),
            text: ",",
        }],
        expected_keys: &["a", "b", "c"],
        min_errors: 1,
        requires_convergence: false,
        expect_reparse: true,
    };

    let (summary, _, _, _) = run_runtime_case(case.name, case.initial, case.ops).await?;
    assert_summary(&case, &summary);
    Ok(())
}

#[tokio::test]
async fn json_runtime_null_suffix_preserves_tail() -> anyhow::Result<()> {
    let case = EditCase {
        name: "invalid_null_suffix_keeps_tail",
        initial: r#"{"xddd":null,"he":222,"well":{}}"#,
        ops: &[EditOp::InsertAfter {
            locate: Locate::First("null"),
            text: "s",
        }],
        expected_keys: &["xddd", "he", "well"],
        min_errors: 1,
        requires_convergence: false,
        expect_reparse: true,
    };

    let (summary, final_source) = run_runtime_summary_case(case.name, case.initial, case.ops)
        .await
        .map_err(|e| anyhow::anyhow!("case {} failed: {e}", case.name))?;
    assert_summary(&case, &summary);
    assert_eq!(final_source, r#"{"xddd":nulls,"he":222,"well":{}}"#);
    Ok(())
}

#[tokio::test]
async fn json_runtime_null_suffix_preserves_tail_w() -> anyhow::Result<()> {
    let case = EditCase {
        name: "invalid_null_suffix_keeps_tail_w",
        initial: r#"{"xddd":null,"he":222,"well":{}}"#,
        ops: &[EditOp::InsertAfter {
            locate: Locate::First("null"),
            text: "w",
        }],
        expected_keys: &["xddd", "he", "well"],
        min_errors: 1,
        requires_convergence: false,
        expect_reparse: true,
    };

    let (summary, final_source) = run_runtime_summary_case(case.name, case.initial, case.ops)
        .await
        .map_err(|e| anyhow::anyhow!("case {} failed: {e}", case.name))?;
    assert_summary(&case, &summary);
    assert_eq!(final_source, r#"{"xddd":nullw,"he":222,"well":{}}"#);
    Ok(())
}

#[tokio::test]
async fn json_runtime_fixed_error_clears_diagnostics() -> anyhow::Result<()> {
    let counts = run_runtime_diagnostic_counts(
        "fixed_error_clears_diagnostics",
        r#"{"xddd":null,"he":222,"well":{}}"#,
        &[
            EditOp::InsertAfter {
                locate: Locate::First("null"),
                text: "s",
            },
            EditOp::Delete {
                locate: Locate::First("s"),
                len: 1,
            },
        ],
    )
    .await?;

    assert_eq!(counts, vec![0, 1, 0]);
    Ok(())
}

#[tokio::test]
async fn json_runtime_skip_token_boundary_stress_is_parse_noop() -> anyhow::Result<()> {
    let initial =
        r#"{"alpha":1,"beta":[true,false,null,{"z":"q"}],"gamma":{"inner":[-1,2,3]},"empty":[]}"#;
    let expected_summary = JsonSummary {
        keys: ["alpha", "beta", "gamma", "empty"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        errors: 0,
    };
    let boundaries = visible_token_boundary_positions(initial)?;
    assert!(
        boundaries.len() > 30,
        "stress input should expose many token boundaries"
    );

    let (source_tx, source_rx) = mpsc::channel(256);
    let (sink_tx, mut sink_rx) = mpsc::channel(256);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new("test://json-skip-boundary-stress", 0, 0)?.uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri.clone(), 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let initial_batch = recv_non_empty_parse_batch(&mut sink_rx).await?;
    assert!(
        !initial_batch.is_empty(),
        "initial parse should emit root insertion"
    );

    let mut current = initial.to_string();
    let summary =
        assert_runtime_matches_fresh_parse(&runtime.context(), uri.clone(), &current, "initial")
            .await?;
    assert_eq!(summary, expected_summary);

    let skip_texts = [" ", "\t", "\n", " \n\t "];
    for (index, boundary) in boundaries.iter().copied().enumerate() {
        let skip = skip_texts[index % skip_texts.len()];
        let label = format!("boundary {index} at byte {boundary}");

        current.insert_str(boundary, skip);
        source_tx
            .send(SourceEdit::Insert {
                key: Span::new_uri(uri.clone(), boundary, boundary)?,
                value: skip.to_string(),
            })
            .await?;
        let insert_batch = recv_non_empty_parse_batch(&mut sink_rx).await?;
        assert!(
            insert_batch.is_empty(),
            "{label}: skip-token insertion emitted {} parse delta(s)",
            insert_batch.len()
        );
        let summary =
            assert_runtime_matches_fresh_parse(&runtime.context(), uri.clone(), &current, &label)
                .await?;
        assert_eq!(summary, expected_summary, "{label}: summary changed");

        current.replace_range(boundary..boundary + skip.len(), "");
        source_tx
            .send(SourceEdit::Delete {
                key: Span::new_uri(uri.clone(), boundary, boundary + skip.len())?,
            })
            .await?;
        let delete_batch = recv_non_empty_parse_batch(&mut sink_rx).await?;
        assert!(
            delete_batch.is_empty(),
            "{label}: skip-token deletion emitted {} parse delta(s)",
            delete_batch.len()
        );
        let summary = assert_runtime_matches_fresh_parse(
            &runtime.context(),
            uri.clone(),
            &current,
            &format!("{label} delete"),
        )
        .await?;
        assert_eq!(summary, expected_summary, "{label}: delete changed summary");
    }

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn json_runtime_extreme_mixed_edits_match_fresh_parse_after_each_step() -> anyhow::Result<()>
{
    let initial =
        r#"{"alpha":1,"beta":[true,false,null],"nil":null,"gamma":{"inner":"x"},"tail":0}"#;
    let (source_tx, source_rx) = mpsc::channel(256);
    let (sink_tx, mut sink_rx) = mpsc::channel(256);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, JsonRuntimeParser>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new("test://json-extreme-mixed-edits", 0, 0)?.uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri.clone(), 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let initial_batch = recv_non_empty_parse_batch(&mut sink_rx).await?;
    assert!(
        !initial_batch.is_empty(),
        "initial parse should emit root insertion"
    );

    let mut current = initial.to_string();
    let initial_summary =
        assert_runtime_matches_fresh_parse(&runtime.context(), uri.clone(), &current, "initial")
            .await?;
    assert_eq!(
        initial_summary,
        JsonSummary {
            keys: ["alpha", "beta", "nil", "gamma", "tail"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            errors: 0,
        }
    );

    struct Step {
        label: &'static str,
        op: EditOp,
        expect_empty_batch: bool,
        expected_keys: &'static [&'static str],
        expected_errors: usize,
    }

    let steps = [
        Step {
            label: "skip before beta",
            op: EditOp::InsertBefore {
                locate: Locate::First("\"beta\""),
                text: "\n\t ",
            },
            expect_empty_batch: true,
            expected_keys: &["alpha", "beta", "nil", "gamma", "tail"],
            expected_errors: 0,
        },
        Step {
            label: "delete skip before beta",
            op: EditOp::Delete {
                locate: Locate::First("\n\t "),
                len: 3,
            },
            expect_empty_batch: true,
            expected_keys: &["alpha", "beta", "nil", "gamma", "tail"],
            expected_errors: 0,
        },
        Step {
            label: "rename first key inside string token",
            op: EditOp::InsertAfter {
                locate: Locate::First("alpha"),
                text: "_long",
            },
            expect_empty_batch: false,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "tail"],
            expected_errors: 0,
        },
        Step {
            label: "grow first number token",
            op: EditOp::InsertAfter {
                locate: Locate::First(":1"),
                text: "23",
            },
            expect_empty_batch: false,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "tail"],
            expected_errors: 0,
        },
        Step {
            label: "append nested object to array",
            op: EditOp::InsertBefore {
                locate: Locate::First("]"),
                text: r#",{"deep":[1,2,3]}"#,
            },
            expect_empty_batch: false,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "tail"],
            expected_errors: 0,
        },
        Step {
            label: "insert lexer garbage after null",
            op: EditOp::InsertAfter {
                locate: Locate::Occurrence {
                    needle: "null",
                    index: 1,
                },
                text: "oops",
            },
            expect_empty_batch: true,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "tail"],
            expected_errors: 1,
        },
        Step {
            label: "delete lexer garbage",
            op: EditOp::Delete {
                locate: Locate::First("oops"),
                len: 4,
            },
            expect_empty_batch: true,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "tail"],
            expected_errors: 0,
        },
        Step {
            label: "delete whole trailing member",
            op: EditOp::Delete {
                locate: Locate::First(r#","tail":0"#),
                len: 9,
            },
            expect_empty_batch: false,
            expected_keys: &["alpha_long", "beta", "nil", "gamma"],
            expected_errors: 0,
        },
        Step {
            label: "insert replacement trailing object member",
            op: EditOp::InsertBefore {
                locate: Locate::Last("}"),
                text: r#","omega":{"x":false}"#,
            },
            expect_empty_batch: false,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "omega"],
            expected_errors: 0,
        },
        Step {
            label: "skip at end of document",
            op: EditOp::InsertAfter {
                locate: Locate::Last("}"),
                text: "\n",
            },
            expect_empty_batch: true,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "omega"],
            expected_errors: 0,
        },
        Step {
            label: "delete skip at end of document",
            op: EditOp::Delete {
                locate: Locate::Last("\n"),
                len: 1,
            },
            expect_empty_batch: true,
            expected_keys: &["alpha_long", "beta", "nil", "gamma", "omega"],
            expected_errors: 0,
        },
    ];

    for step in steps {
        let delta = apply_edit(&mut current, uri.clone(), step.op)?;
        source_tx.send(delta).await?;
        let batches = recv_parse_batches_until_quiet(&mut sink_rx).await?;
        let delta_count = batches.iter().map(Vec::len).sum::<usize>();
        let has_non_empty_batch = batches.iter().any(|batch| !batch.is_empty());
        assert_eq!(
            !has_non_empty_batch, step.expect_empty_batch,
            "{}: unexpected parse delta batch total length {}",
            step.label, delta_count
        );

        let summary = assert_runtime_matches_fresh_parse(
            &runtime.context(),
            uri.clone(),
            &current,
            step.label,
        )
        .await?;
        let expected_keys = step
            .expected_keys
            .iter()
            .map(|key| key.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            summary.keys, expected_keys,
            "{}: key summary changed",
            step.label
        );
        if step.expected_errors == 0 {
            assert_eq!(summary.errors, 0, "{}: expected no diagnostics", step.label);
        } else {
            assert!(
                summary.errors >= step.expected_errors,
                "{}: expected at least {} diagnostic(s), found {}",
                step.label,
                step.expected_errors,
                summary.errors
            );
        }

        if !step.expect_empty_batch {
            let stat = runtime
                .context()
                .call(JsonRuntimeParser::incremental_stats_for, uri.clone())
                .await?
                .ok_or_else(|| anyhow::anyhow!("{}: missing incremental stats", step.label))?;
            assert!(
                stat.reparsed > 0,
                "{}: visible edit did not report parser replay: {stat:?}",
                step.label
            );
        }
    }

    assert_eq!(
        current,
        r#"{"alpha_long":123,"beta":[true,false,null,{"deep":[1,2,3]}],"nil":null,"gamma":{"inner":"x"},"omega":{"x":false}}"#
    );

    runtime.shutdown().await;
    Ok(())
}
