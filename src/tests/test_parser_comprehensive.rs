use crate::{
    NonTerminal,
    component::{
        debug::DebugSink,
        lex::{Entry, Lexer, LexerState},
        parse::{
            AstToken, GetParseTokens, IncrementalParseStats, ParseErrorInfo, ParseForest,
            ParsePath, Parser, ParserConfig, TokenData,
            data::{AstArena, AstBox, ProductData},
            identity::{eof_fingerprint, error_fingerprint, token_fingerprint},
            grammar::Grammar,
            policy::{DerefAstBox, DerefAstToken, GetAstTree, GetIncrementalStats, GetParseDiagnostics},
        },
        source::Source,
    },
    scheme::{Context, Delta, Runtime},
    tokens,
    utils::{RangeOrPoint, Span},
};
use fluent_uri::Uri;
use std::{marker::PhantomData, time::Duration};
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokens]
#[derive(Debug, Clone)]
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
    #[rule($head(JsonMember), $tail(JsonMembersTail))]
    Many(
        #[from(head)] AstBox<JsonMember>,
        #[from(tail)] AstBox<JsonMembersTail>,
    ),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonMembersTail {
    #[rule()]
    End,

    #[rule(JsonToken::Comma, $head(JsonMember), $tail(JsonMembersTail))]
    More(
        #[from(head)] AstBox<JsonMember>,
        #[from(tail)] AstBox<JsonMembersTail>,
    ),

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
    #[rule($head(JsonValue), $tail(JsonElementsTail))]
    Many(
        #[from(head)] AstBox<JsonValue>,
        #[from(tail)] AstBox<JsonElementsTail>,
    ),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

#[derive(NonTerminal, Debug, Clone)]
enum JsonElementsTail {
    #[rule()]
    End,

    #[rule(JsonToken::Comma, $head(JsonValue), $tail(JsonElementsTail))]
    More(
        #[from(head)] AstBox<JsonValue>,
        #[from(tail)] AstBox<JsonElementsTail>,
    ),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

type JsonSink = DebugSink<ParsePath, ParseForest>;
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

fn collect_entries(
    lexer: &mut Lexer<JsonToken>,
    input: &str,
) -> Vec<(usize, Entry<JsonToken>, usize, usize)> {
    let token_ids: Vec<(usize, usize, usize)> = {
        let mut ids = Vec::new();
        lexer
            .lex_cont(
                LexerState::new(lexer.state_id_of::<JsonToken>().unwrap()),
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

fn token_data_from_entries(entries: &[(usize, Entry<JsonToken>, usize, usize)]) -> Vec<TokenData> {
    entries
        .iter()
        .enumerate()
        .map(|(column, (id, entry, start, _end))| match entry {
            Entry::Token {
                length, terminal, value,
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
        })
        .collect()
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
) -> anyhow::Result<Delta<Span, String>> {
    match op {
        EditOp::InsertBefore { locate, text } => {
            let pos = find_location(current, locate)?;
            current.insert_str(pos, text);
            Ok(Delta::Insert {
                key: Span::new_uri(uri, pos, pos)?,
                value: text.to_string(),
            })
        }
        EditOp::InsertAfter { locate, text } => {
            let pos = find_location(current, locate)?;
            let at = pos + match locate {
                Locate::First(needle) | Locate::Last(needle) => needle.len(),
                Locate::Occurrence { needle, .. } => needle.len(),
            };
            current.insert_str(at, text);
            Ok(Delta::Insert {
                key: Span::new_uri(uri, at, at)?,
                value: text.to_string(),
            })
        }
        EditOp::Delete { locate, len } => {
            let pos = find_location(current, locate)?;
            current.replace_range(pos..pos + len, "");
            Ok(Delta::Delete {
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
) -> anyhow::Result<(String, Vec<Delta<Span, String>>)> {
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
) -> anyhow::Result<(JsonDirectParser, Lexer<JsonToken>, Uri<&'static str>, Vec<TokenData>, String)>
{
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
    let JsonMembers::Many(head, tail) = ast
        .get(*mems)
        .ok_or_else(|| anyhow::anyhow!("missing JsonMembers"))?
    else {
        return Ok(keys);
    };
    let head = ast
        .get(*head)
        .ok_or_else(|| anyhow::anyhow!("missing JsonMember"))?;
    let JsonMember::Pair(key, _) = head else {
        return Ok(keys);
    };
    keys.push(token_text(key.id)?);

    let mut current = tail;
    loop {
        let tail = ast
            .get(*current)
            .ok_or_else(|| anyhow::anyhow!("missing JsonMembersTail"))?;
        match tail {
            JsonMembersTail::End | JsonMembersTail::Error(_) => break,
            JsonMembersTail::More(head, next_tail) => {
                let member = ast
                    .get(*head)
                    .ok_or_else(|| anyhow::anyhow!("missing JsonMember"))?;
                let JsonMember::Pair(key, _) = member else {
                    return Ok(keys);
                };
                keys.push(token_text(key.id)?);
                current = next_tail;
            }
        }
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
    let errors = parser.parse_diagnostics(uri).len();
    Ok(JsonSummary { keys, errors })
}

async fn runtime_root_keys(ctx: &Context, obj: &JsonObject) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    let JsonObject::Members(mems) = obj else {
        return Ok(keys);
    };
    let members = ctx
        .post::<JsonRuntimeParser, DerefAstBox<JsonMembers>>(DerefAstBox(*mems))
        .await?;
    let JsonMembers::Many(head, tail) = members else {
        return Ok(keys);
    };
    let member = ctx
        .post::<JsonRuntimeParser, DerefAstBox<JsonMember>>(DerefAstBox(head))
        .await?;
    let JsonMember::Pair(key, _) = member else {
        return Ok(keys);
    };
    let JsonToken::String(s) = ctx
        .post::<JsonRuntimeParser, DerefAstToken<JsonToken>>(DerefAstToken(key))
        .await?
    else {
        return Err(anyhow::anyhow!("expected string token for object key"));
    };
    keys.push(s);

    let mut current = tail;
    loop {
        let tail = ctx
            .post::<JsonRuntimeParser, DerefAstBox<JsonMembersTail>>(DerefAstBox(current))
            .await?;
        match tail {
            JsonMembersTail::End | JsonMembersTail::Error(_) => break,
            JsonMembersTail::More(head, next_tail) => {
                let member = ctx
                    .post::<JsonRuntimeParser, DerefAstBox<JsonMember>>(DerefAstBox(head))
                    .await?;
                let JsonMember::Pair(key, _) = member else {
                    return Ok(keys);
                };
                let JsonToken::String(s) = ctx
                    .post::<JsonRuntimeParser, DerefAstToken<JsonToken>>(DerefAstToken(key))
                    .await?
                else {
                    return Err(anyhow::anyhow!("expected string token for object key"));
                };
                keys.push(s);
                current = next_tail;
            }
        }
    }
    Ok(keys)
}

async fn runtime_summary(ctx: &Context, root: AstBox<JsonValue>) -> anyhow::Result<JsonSummary> {
    let value = ctx
        .post::<JsonRuntimeParser, DerefAstBox<JsonValue>>(DerefAstBox(root))
        .await?;
    let keys = match &value {
        JsonValue::Object(obj) => {
            let obj = ctx
                .post::<JsonRuntimeParser, DerefAstBox<JsonObject>>(DerefAstBox(*obj))
                .await?;
            runtime_root_keys(ctx, &obj).await?
        }
        _ => Vec::new(),
    };
    let diagnostics = ctx
        .post::<JsonRuntimeParser, GetParseDiagnostics>(GetParseDiagnostics(
            root.uri.clone(),
        ))
        .await?;
    let errors = diagnostics.len();
    Ok(JsonSummary { keys, errors })
}

async fn recv_non_empty_parse_batch(
    rx: &mut mpsc::Receiver<Vec<Delta<ParsePath, ParseForest>>>,
) -> anyhow::Result<Vec<Delta<ParsePath, ParseForest>>> {
    let batch = timeout(Duration::from_secs(2), rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("parse sink channel closed"))?;
    Ok(batch)
}

async fn recv_parse_batches_until_quiet(
    rx: &mut mpsc::Receiver<Vec<Delta<ParsePath, ParseForest>>>,
) -> anyhow::Result<Vec<Vec<Delta<ParsePath, ParseForest>>>> {
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
    Vec<Vec<Delta<ParsePath, ParseForest>>>,
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

    let uri = Span::new(format!("test://json-comprehensive-runtime/{case_name}"), 0, 0)?.uri;
    source_tx
        .send(Delta::Insert {
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
            .post::<JsonRuntimeParser, GetIncrementalStats>(GetIncrementalStats(uri.clone()))
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
        .post::<JsonRuntimeParser, GetAstTree<JsonValue>>(GetAstTree(root_path, PhantomData))
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
    Vec<Vec<Delta<ParsePath, ParseForest>>>,
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

    let uri = Span::new(format!("test://json-comprehensive-runtime/{case_name}"), 0, 0)?.uri;
    source_tx
        .try_send(Delta::Insert {
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
        .post::<JsonRuntimeParser, GetIncrementalStats>(GetIncrementalStats(uri.clone()))
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing incremental stats"))?;
    stats.push(stat);

    let ctx = runtime.context();
    let token_data = ctx
        .post::<Jl, GetParseTokens>(GetParseTokens(Span {
            uri: uri.clone(),
            range: RangeOrPoint::Range(0, usize::MAX),
        }))
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
    let mut direct_parser =
        Grammar::from_spec::<JsonValue>().build_lr1_with_config::<JsonToken, ()>(
            ParserConfig::default(),
        );
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
        .post::<JsonRuntimeParser, GetAstTree<JsonValue>>(GetAstTree(root_path, PhantomData))
        .await?;
    if roots.len() != 1 {
        return Err(anyhow::anyhow!(
            "expected exactly one root AST box, found {}",
            roots.len()
        ));
    }
    let summary = runtime_summary(&ctx, roots[0]).await?;
    runtime.shutdown().await;
    Ok((summary, batches, stats, current, token_data.len(), first_eof))
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
            let _: &Vec<Delta<ParsePath, ParseForest>> = &deltas;
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

    let uri = Span::new(format!("test://json-comprehensive-runtime/{case_name}"), 0, 0)?.uri;
    source_tx
        .send(Delta::Insert {
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
        .post::<JsonRuntimeParser, GetAstTree<JsonValue>>(GetAstTree(root_path, PhantomData))
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

    let uri = Span::new(format!("test://json-comprehensive-runtime/{case_name}"), 0, 0)?.uri;
    source_tx
        .send(Delta::Insert {
            key: Span::new_uri(uri.clone(), 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    let mut current = initial.to_string();
    let mut counts = Vec::with_capacity(ops.len() + 1);
    let initial_count = runtime
        .context()
        .post::<JsonRuntimeParser, GetParseDiagnostics>(GetParseDiagnostics(uri.clone()))
        .await?
        .len();
    counts.push(initial_count);

    for &op in ops {
        let delta = apply_edit(&mut current, uri.clone(), op)?;
        source_tx.send(delta).await?;
        let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;
        let count = runtime
            .context()
            .post::<JsonRuntimeParser, GetParseDiagnostics>(GetParseDiagnostics(uri.clone()))
            .await?
            .len();
        counts.push(count);
    }

    runtime.shutdown().await;
    Ok(counts)
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
    for case in json_edit_cases().into_iter().filter(|case| case.min_errors == 0) {
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
    assert_eq!(final_source, r#"{"a":1,"j":true,"k":[1,2,3],"l":{"a":1,"b":2},"m":null}"#);
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

    let (summary, final_source) =
        run_runtime_summary_case(case.name, case.initial, case.ops)
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

    let (summary, final_source) =
        run_runtime_summary_case(case.name, case.initial, case.ops)
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
