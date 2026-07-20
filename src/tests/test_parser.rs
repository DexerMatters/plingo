use crate::{
    NonTerminal, Terminal,
    component::{
        debug::DebugSink,
        lex::{LexErrorInfo, Lexer},
        parse::{
            AstToken, ParseAddress, ParseChange, ParseChanges, ParseErrorInfo, ParsePath,
            ParseUnit, Parser, data::ast::AstBox, grammar::Grammar,
        },
        source::{Source, SourceEdit},
    },
    scheme::{context::Context, runtime::Runtime},
    tests::fs_watch,
    utils::{RangeOrPoint, Span},
};
use color_print::cprintln;
use std::time::Duration;
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
    #[rule({$m(JsonMember)}{JsonToken::Comma})]
    Many(#[from(m)] Vec<AstBox<JsonMember>>),

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
    #[rule({$v(JsonValue)}{JsonToken::Comma})]
    Many(#[from(v)] Vec<AstBox<JsonValue>>),

    #[parse_err]
    Error(#[from(0)] ParseErrorInfo),
}

type JsonSink = DebugSink<ParseAddress, ParseUnit>;
type Jp = Parser<JsonToken, JsonSink>;

macro_rules! deref {
    ($ctx:expr, $method:expr, $arg:expr, $label:expr, $indent:expr) => {
        match $ctx.call($method, $arg).await {
            Ok(v) => Some(v),
            Err(e) => {
                let p = "  ".repeat($indent);
                eprintln!("{p}ERR {}: {e}", $label);
                None
            }
        }
    };
}

async fn walk_value(ctx: &Context, v: &JsonValue, indent: usize) {
    let pad = "  ".repeat(indent);
    match v {
        JsonValue::String(tok) => {
            if let Some(JsonToken::String(s)) = deref!(
                ctx,
                Jp::deref_ast_token::<JsonToken>,
                *tok,
                "String tok",
                indent
            ) {
                println!("{pad}String \"{s}\"")
            }
        }
        JsonValue::Number(tok) => {
            if let Some(JsonToken::Number(n)) = deref!(
                ctx,
                Jp::deref_ast_token::<JsonToken>,
                *tok,
                "Number tok",
                indent
            ) {
                println!("{pad}Number {n}")
            }
        }
        JsonValue::True => println!("{pad}True"),
        JsonValue::False => println!("{pad}False"),
        JsonValue::Null => println!("{pad}Null"),
        JsonValue::Object(obj) => {
            println!("{pad}Object");
            if let Some(obj) = deref!(
                ctx,
                Jp::deref_ast_box::<JsonObject>,
                *obj,
                "JsonObject",
                indent
            ) {
                walk_object(ctx, &obj, indent + 1).await;
            }
        }
        JsonValue::Array(arr) => {
            println!("{pad}Array");
            if let Some(arr) = deref!(
                ctx,
                Jp::deref_ast_box::<JsonArray>,
                *arr,
                "JsonArray",
                indent
            ) {
                walk_array(ctx, &arr, indent + 1).await;
            }
        }
        JsonValue::Error(info) => {
            print_parse_error(&pad, info);
        }
    }
}

async fn walk_object(ctx: &Context, obj: &JsonObject, indent: usize) {
    let pad = "  ".repeat(indent);
    match obj {
        JsonObject::Empty => {}
        JsonObject::Members(mems) => {
            if let Some(mems) = deref!(
                ctx,
                Jp::deref_ast_box::<JsonMembers>,
                *mems,
                "JsonMembers",
                indent
            ) {
                Box::pin(walk_members(ctx, &mems, indent)).await;
            }
        }
        JsonObject::Error(info) => {
            print_parse_error(&pad, info);
        }
    }
}

async fn walk_array(ctx: &Context, arr: &JsonArray, indent: usize) {
    let pad = "  ".repeat(indent);
    match arr {
        JsonArray::Empty => {}
        JsonArray::Elements(els) => {
            if let Some(els) = deref!(
                ctx,
                Jp::deref_ast_box::<JsonElements>,
                *els,
                "JsonElements",
                indent
            ) {
                Box::pin(walk_elements(ctx, &els, indent)).await;
            }
        }
        JsonArray::Error(info) => {
            print_parse_error(&pad, info);
        }
    }
}

async fn walk_members(ctx: &Context, mems: &JsonMembers, indent: usize) {
    let pad = "  ".repeat(indent);
    match mems {
        JsonMembers::Many(items) => {
            for m in items {
                walk_member(ctx, m, indent).await;
            }
        }
        JsonMembers::Error(info) => {
            print_parse_error(&pad, &info);
        }
    }
}

async fn walk_member(ctx: &Context, member: &AstBox<JsonMember>, indent: usize) {
    let pad = "  ".repeat(indent);
    let Some(m) = deref!(
        ctx,
        Jp::deref_ast_box::<JsonMember>,
        *member,
        "JsonMember",
        indent
    ) else {
        return;
    };
    match m {
        JsonMember::Pair(key, val) => {
            let k = deref!(
                ctx,
                Jp::deref_ast_token::<JsonToken>,
                key,
                "member key",
                indent
            );
            let v = deref!(
                ctx,
                Jp::deref_ast_box::<JsonValue>,
                val,
                "member val",
                indent
            );
            match (k, v) {
                (Some(JsonToken::String(k)), Some(v)) => {
                    println!("{pad}Pair \"{k}\"");
                    Box::pin(walk_value(ctx, &v, indent + 1)).await;
                }
                _ => println!("{pad}Pair (?)"),
            }
        }
        JsonMember::Error(info) => {
            print_parse_error(&pad, &info);
        }
    }
}

async fn walk_elements(ctx: &Context, els: &JsonElements, indent: usize) {
    let pad = "  ".repeat(indent);
    match els {
        JsonElements::Many(items) => {
            for val in items {
                if let Some(v) = deref!(
                    ctx,
                    Jp::deref_ast_box::<JsonValue>,
                    *val,
                    "Elements::Many val",
                    indent
                ) {
                    walk_value(ctx, &v, indent).await;
                }
            }
        }
        JsonElements::Error(info) => {
            print_parse_error(&pad, info);
        }
    }
}

fn print_parse_error(pad: &str, info: &ParseErrorInfo) {
    cprintln!(
        "<red>{pad}Err</><dim> {kind:?} unexpected={unexp:?} expected={exp:?} location={loc:?}</>",
        kind = info.kind,
        unexp = info.unexpected,
        exp = info.expected,
        loc = info.location,
    );
}

async fn collect_root_json_member_keys(
    ctx: &Context,
    root: AstBox<JsonValue>,
) -> anyhow::Result<Vec<String>> {
    let value = ctx.call(Jp::deref_ast_box::<JsonValue>, root).await?;
    let JsonValue::Object(obj) = value else {
        return Ok(Vec::new());
    };
    let obj = ctx.call(Jp::deref_ast_box::<JsonObject>, obj).await?;
    let JsonObject::Members(mems) = obj else {
        return Ok(Vec::new());
    };
    let mems = ctx.call(Jp::deref_ast_box::<JsonMembers>, mems).await?;
    let JsonMembers::Many(items) = mems else {
        return Ok(Vec::new());
    };

    let mut keys = Vec::with_capacity(items.len());
    for member in items {
        let member = ctx.call(Jp::deref_ast_box::<JsonMember>, member).await?;
        let JsonMember::Pair(key, _) = member else {
            continue;
        };
        let JsonToken::String(key) = ctx.call(Jp::deref_ast_token::<JsonToken>, key).await? else {
            return Err(anyhow::anyhow!("expected string member key"));
        };
        keys.push(key);
    }
    Ok(keys)
}

#[test]
fn json_test_generator() -> anyhow::Result<()> {
    let mut writer = String::new();
    crate::generate!(JsonToken::Null, 7, &mut writer)?;
    println!("Generated token: {}", writer);
    Ok(())
}

#[test]
fn parent_token_generator_macro_works_from_child_module() -> anyhow::Result<()> {
    let mut writer = String::new();
    crate::generate!(super::RootTokens::Number, 5, &mut writer)?;
    assert!(writer.chars().all(|ch| ch.is_ascii_digit()));
    Ok(())
}

#[tokio::test]
#[ignore]
async fn json_runtime_parse_output() -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("RUST_LOG", "debug");
    }
    let _ = env_logger::builder().is_test(true).try_init();

    let dir = workspace_root::get_workspace_root().join("test_data");
    let (sender, receiver) = mpsc::channel(256);

    let debug_sink = debug_sink!(|ctx, deltas| async move {
        let _: &ParseChanges = &deltas;
        cprintln!("<dim>---------Received---------</dim>");

        let Some(first) = deltas.changes.first() else {
            return Ok(());
        };
        let uri = first.address.uri;
        let root_path = ParsePath {
            uri: uri,
            path: Vec::new(),
            range: RangeOrPoint::Point(0),
        };
        if let Ok(trees) = ctx.call(Jp::get_ast_tree::<JsonValue>, root_path).await {
            for rb in &trees {
                if let Ok(val) = ctx.call(Jp::deref_ast_box::<JsonValue>, *rb).await {
                    walk_value(ctx, &val, 0).await;
                }
            }
        }
        if let Ok(diagnostics) = ctx.call(Jp::parse_diagnostics, uri).await {
            for info in diagnostics {
                print_parse_error("", &info);
            }
        }
        for change in &deltas.changes {
            let key = ParsePath {
                uri: change.address.uri,
                path: change.address.parent_path.clone(),
                range: RangeOrPoint::from_range(
                    change.splices[0].old_range.start,
                    change.splices[0].old_range.end,
                ),
            };
            if !change.splices[0].removed.is_empty() {
                let prev = ctx.last_snapshot();
                let mut names = Vec::new();
                if let Ok(pids) = prev.call(Jp::get_node, key.clone()).await {
                    for &pid in &pids {
                        if let Ok(desc) = prev.call(Jp::describe_product, (key.uri, pid)).await {
                            names.push(desc);
                        } else {
                            names.push("?".to_string());
                        }
                    }
                }
                let desc = if names.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", names.join(", "))
                };
                cprintln!("<red>Deleted {key}{desc}</red>");
            }
            if !change.splices[0].inserted.is_empty() {
                cprintln!(
                    "<green>Inserted: +{} root(s) at {key}</green>",
                    change.splices[0].inserted.len()
                );
                let mut names = Vec::new();
                for unit in change.splices[0].inserted.iter() {
                    if let Ok(desc) = ctx
                        .call(Jp::describe_product, (key.uri, unit.product))
                        .await
                    {
                        names.push(desc);
                    }
                }
                let desc = if names.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", names.join(", "))
                };
                cprintln!(
                    "<green>  + {} subtree(s) at {key}{desc}</green>",
                    change.splices[0].inserted.len(),
                );
            }
        }
        Ok(())
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(receiver))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    fs_watch::watch_directory(sender, dir).await?;

    Ok(())
}

#[tokio::test]
async fn json_runtime_four_member_object_is_accepted() -> anyhow::Result<()> {
    let keys = run_json_incremental_case_member_keys(
        r#"{"adddss":222,"xssss":22,"v":2,"x":55}"#,
        Vec::new(),
    )
    .await?;

    assert_eq!(keys, vec!["adddss", "xssss", "v", "x"]);
    Ok(())
}

async fn recv_non_empty_parse_batch(
    rx: &mut mpsc::Receiver<ParseChanges>,
) -> anyhow::Result<Vec<ParseChange>> {
    let batch = timeout(Duration::from_secs(2), rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("parse sink channel closed"))?;
    Ok(batch.changes)
}

async fn recv_parse_batches_until_quiet(
    rx: &mut mpsc::Receiver<ParseChanges>,
) -> anyhow::Result<Vec<Vec<ParseChange>>> {
    let mut batches = vec![recv_non_empty_parse_batch(rx).await?];
    loop {
        match timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(batch)) => batches.push(batch.changes),
            Ok(None) | Err(_) => break,
        }
    }
    Ok(batches)
}

#[tokio::test]
async fn context_callable_methods_work_for_source_and_parser() -> anyhow::Result<()> {
    let (_source_tx, source_rx) = mpsc::channel(1);
    let (sink_tx, mut sink_rx) = mpsc::channel(8);
    let debug_sink = debug_sink!(|_ctx, deltas| {
        let sink_tx = sink_tx.clone();
        async move {
            let _ = sink_tx.send(deltas.clone()).await;
            Ok(())
        }
    });
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let ctx = runtime.context();
    let uri = Span::new("test://direct-source-call", 0, 0)?.uri;
    ctx.call(
        Source::<Jl>::apply_edit,
        SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0)?,
            value: r#"{"a":1}"#.to_string(),
        },
    )
    .await?;

    let text = ctx
        .call(
            Source::<Jl>::read_span,
            Span {
                uri,
                range: RangeOrPoint::Range(0, usize::MAX),
            },
        )
        .await?
        .to_string();
    assert_eq!(text, r#"{"a":1}"#);
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    let root_path = ParsePath {
        uri,
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = ctx.call(Jp::get_ast_tree::<JsonValue>, root_path).await?;
    let root = roots[0];
    let JsonValue::Object(obj) = ctx.call(Jp::deref_ast_box::<JsonValue>, root).await? else {
        anyhow::bail!("expected object root");
    };
    let JsonObject::Members(members) = ctx.call(Jp::deref_ast_box::<JsonObject>, obj).await? else {
        anyhow::bail!("expected object members");
    };
    let JsonMembers::Many(items) = ctx.call(Jp::deref_ast_box::<JsonMembers>, members).await?
    else {
        anyhow::bail!("expected member list");
    };
    let JsonMember::Pair(key, _) = ctx.call(Jp::deref_ast_box::<JsonMember>, items[0]).await?
    else {
        anyhow::bail!("expected pair member");
    };
    let JsonToken::String(key_text) = ctx.call(Jp::deref_ast_token::<JsonToken>, key).await? else {
        anyhow::bail!("expected string key token");
    };
    assert_eq!(key_text, "a");

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn parser_layer_can_return_on_demand_spans_for_ast_handles() -> anyhow::Result<()> {
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
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let ctx = runtime.context();
    let uri = Span::new("test://ast-handle-spans", 0, 0)?.uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0)?,
            value: r#"{"a":{"bc":2}}"#.to_string(),
        })
        .await?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    let root_path = ParsePath {
        uri,
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = ctx
        .call(Jp::get_ast_tree::<JsonValue>, root_path.clone())
        .await?;
    let root = roots[0];
    let JsonValue::Object(obj) = ctx.call(Jp::deref_ast_box::<JsonValue>, root).await? else {
        anyhow::bail!("expected root object");
    };
    let JsonObject::Members(members) = ctx.call(Jp::deref_ast_box::<JsonObject>, obj).await? else {
        anyhow::bail!("expected object members");
    };
    let JsonMembers::Many(items) = ctx.call(Jp::deref_ast_box::<JsonMembers>, members).await?
    else {
        anyhow::bail!("expected member list");
    };
    let JsonMember::Pair(_, value) = ctx.call(Jp::deref_ast_box::<JsonMember>, items[0]).await?
    else {
        anyhow::bail!("expected member pair");
    };
    let JsonValue::Object(nested) = ctx.call(Jp::deref_ast_box::<JsonValue>, value).await? else {
        anyhow::bail!("expected nested object");
    };
    let JsonObject::Members(nested_members) =
        ctx.call(Jp::deref_ast_box::<JsonObject>, nested).await?
    else {
        anyhow::bail!("expected nested members");
    };
    let JsonMembers::Many(nested_items) = ctx
        .call(Jp::deref_ast_box::<JsonMembers>, nested_members)
        .await?
    else {
        anyhow::bail!("expected nested member list");
    };
    let JsonMember::Pair(key, _) = ctx
        .call(Jp::deref_ast_box::<JsonMember>, nested_items[0])
        .await?
    else {
        anyhow::bail!("expected nested pair");
    };

    assert_eq!(
        ctx.call(Jp::span_of_ast_box::<JsonValue>, root).await?,
        Span::new_uri(uri, 0, 14)?
    );
    assert_eq!(
        ctx.call(Jp::span_of_ast_box::<JsonObject>, nested).await?,
        Span::new_uri(uri, 5, 13)?
    );
    assert_eq!(
        ctx.call(Jp::span_of_ast_token::<JsonToken>, key).await?,
        Span::new_uri(uri, 6, 10)?
    );

    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0)?,
            value: " ".to_string(),
        })
        .await?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    let shifted_roots = ctx.call(Jp::get_ast_tree::<JsonValue>, root_path).await?;
    let shifted_root = shifted_roots[0];
    let JsonValue::Object(shifted_obj) = ctx
        .call(Jp::deref_ast_box::<JsonValue>, shifted_root)
        .await?
    else {
        anyhow::bail!("expected shifted root object");
    };
    let JsonObject::Members(shifted_members) = ctx
        .call(Jp::deref_ast_box::<JsonObject>, shifted_obj)
        .await?
    else {
        anyhow::bail!("expected shifted object members");
    };
    let JsonMembers::Many(shifted_items) = ctx
        .call(Jp::deref_ast_box::<JsonMembers>, shifted_members)
        .await?
    else {
        anyhow::bail!("expected shifted member list");
    };
    let JsonMember::Pair(_, shifted_value) = ctx
        .call(Jp::deref_ast_box::<JsonMember>, shifted_items[0])
        .await?
    else {
        anyhow::bail!("expected shifted pair");
    };
    let JsonValue::Object(shifted_nested) = ctx
        .call(Jp::deref_ast_box::<JsonValue>, shifted_value)
        .await?
    else {
        anyhow::bail!("expected shifted nested object");
    };
    let JsonObject::Members(shifted_nested_members) = ctx
        .call(Jp::deref_ast_box::<JsonObject>, shifted_nested)
        .await?
    else {
        anyhow::bail!("expected shifted nested members");
    };
    let JsonMembers::Many(shifted_nested_items) = ctx
        .call(Jp::deref_ast_box::<JsonMembers>, shifted_nested_members)
        .await?
    else {
        anyhow::bail!("expected shifted nested member list");
    };
    let JsonMember::Pair(shifted_key, _) = ctx
        .call(Jp::deref_ast_box::<JsonMember>, shifted_nested_items[0])
        .await?
    else {
        anyhow::bail!("expected shifted nested pair");
    };

    assert_eq!(
        ctx.call(Jp::span_of_ast_box::<JsonObject>, shifted_nested)
            .await?,
        Span::new_uri(uri, 6, 14)?
    );
    assert_eq!(
        ctx.call(Jp::span_of_ast_token::<JsonToken>, shifted_key)
            .await?,
        Span::new_uri(uri, 7, 11)?
    );

    source_tx.try_send(SourceEdit::Delete {
        key: Span::new_uri(uri, 8, 10)?,
    })?;
    source_tx.try_send(SourceEdit::Insert {
        key: Span::new_uri(uri, 8, 8)?,
        value: "bc".to_string(),
    })?;
    assert!(recv_non_empty_parse_batch(&mut sink_rx).await?.is_empty());
    assert_eq!(
        ctx.call(Jp::span_of_ast_token::<JsonToken>, shifted_key)
            .await?,
        Span::new_uri(uri, 7, 11)?
    );

    runtime.shutdown().await;
    Ok(())
}

async fn run_json_incremental_case(
    initial: &str,
    changes: Vec<SourceEdit>,
) -> anyhow::Result<(
    Vec<Vec<ParseChange>>,
    Vec<crate::component::parse::IncrementalParseStats>,
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
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new("test://json-incremental", 0, 0)?.uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let mut batches = vec![recv_non_empty_parse_batch(&mut sink_rx).await?];
    let mut stats = Vec::new();

    for change in changes {
        source_tx.send(change).await?;
        batches.push(recv_non_empty_parse_batch(&mut sink_rx).await?);
        let stat = runtime
            .context()
            .call(Jp::incremental_stats_for, uri)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing incremental stats"))?;
        stats.push(stat);
    }

    runtime.shutdown().await;
    Ok((batches, stats))
}

async fn run_json_incremental_case_member_keys(
    initial: &str,
    changes: Vec<SourceEdit>,
) -> anyhow::Result<Vec<String>> {
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
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new("test://json-incremental", 0, 0)?.uri;
    source_tx
        .send(SourceEdit::Insert {
            key: Span::new_uri(uri, 0, 0)?,
            value: initial.to_string(),
        })
        .await?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    for change in changes {
        source_tx.send(change).await?;
        let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;
    }

    let root_path = ParsePath {
        uri,
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = runtime
        .context()
        .call(Jp::get_ast_tree::<JsonValue>, root_path)
        .await?;
    let keys = if let Some(root) = roots.first().copied() {
        let ctx = runtime.context();
        collect_root_json_member_keys(&ctx, root).await?
    } else {
        Vec::new()
    };

    runtime.shutdown().await;
    Ok(keys)
}

async fn run_json_incremental_batched_case_member_keys(
    initial: &str,
    changes: Vec<SourceEdit>,
) -> anyhow::Result<(Vec<ParseChange>, Vec<String>)> {
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
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(source_rx))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;

    let uri = Span::new("test://json-incremental-batch", 0, 0)?.uri;
    source_tx.try_send(SourceEdit::Insert {
        key: Span::new_uri(uri, 0, 0)?,
        value: initial.to_string(),
    })?;
    let _ = recv_non_empty_parse_batch(&mut sink_rx).await?;

    for change in changes {
        source_tx.try_send(change)?;
    }

    let batches = recv_parse_batches_until_quiet(&mut sink_rx).await?;
    let batch = batches
        .into_iter()
        .rev()
        .find(|batch| !batch.is_empty())
        .unwrap_or_default();

    let root_path = ParsePath {
        uri,
        path: Vec::new(),
        range: RangeOrPoint::Point(0),
    };
    let roots = runtime
        .context()
        .call(Jp::get_ast_tree::<JsonValue>, root_path)
        .await?;
    let keys = if let Some(root) = roots.first().copied() {
        let ctx = runtime.context();
        collect_root_json_member_keys(&ctx, root).await?
    } else {
        Vec::new()
    };

    runtime.shutdown().await;
    Ok((batch, keys))
}

#[tokio::test]
async fn json_incremental_runtime_emits_deltas_for_conv_cases() -> anyhow::Result<()> {
    let uri = Span::new("test://json-incremental", 0, 0)?.uri;

    let (string_len, string_len_stats) = run_json_incremental_case(
        r#"{"he":"101"}"#,
        vec![SourceEdit::Insert {
            key: Span::new_uri(uri, 10, 10)?,
            value: "3".to_string(),
        }],
    )
    .await?;
    assert_eq!(string_len[1].len(), 1);
    assert_eq!(string_len[1][0].splices[0].removed.len(), 1);
    assert_eq!(string_len[1][0].splices[0].inserted.len(), 1);
    assert!(string_len_stats[0].frontier_converged);
    assert!(string_len_stats[0].reconverged_new_boundary.is_some());
    assert!(string_len_stats[0].reused > 0);

    let (string_shorten, string_shorten_stats) = run_json_incremental_case(
        r#"{"he":"101"}"#,
        vec![SourceEdit::Delete {
            key: Span::new_uri(uri, 9, 10)?,
        }],
    )
    .await?;
    assert_eq!(string_shorten[1].len(), 1);
    assert_eq!(string_shorten[1][0].splices[0].removed.len(), 1);
    assert_eq!(string_shorten[1][0].splices[0].inserted.len(), 1);
    assert!(string_shorten_stats[0].frontier_converged);
    assert!(string_shorten_stats[0].reconverged_new_boundary.is_some());
    assert!(string_shorten_stats[0].reused > 0);

    let (repeated_child, repeated_child_stats) = run_json_incremental_case(
        r#"[1,1,1]"#,
        vec![
            SourceEdit::Delete {
                key: Span::new_uri(uri, 5, 6)?,
            },
            SourceEdit::Insert {
                key: Span::new_uri(uri, 5, 5)?,
                value: "2".to_string(),
            },
        ],
    )
    .await?;
    assert!(!repeated_child[1].is_empty());
    assert!(!repeated_child[2].is_empty());
    assert!(repeated_child_stats.iter().all(|stat| stat.reparsed > 0));

    Ok(())
}

#[tokio::test]
async fn json_runtime_name_change_emits_one_replacement_batch() -> anyhow::Result<()> {
    let source = r#"{"he":"101","well":{}}"#;
    let insert_at = source.find("he").unwrap() + "he".len();
    let uri = Span::new("test://json-incremental", 0, 0)?.uri;

    let (batches, stats) = run_json_incremental_case(
        source,
        vec![SourceEdit::Insert {
            key: Span::new_uri(uri, insert_at, insert_at)?,
            value: "llo".to_string(),
        }],
    )
    .await?;

    assert_eq!(batches[1].len(), 1);
    assert_eq!(batches[1][0].splices[0].removed.len(), 1);
    assert_eq!(batches[1][0].splices[0].inserted.len(), 1);
    assert!(stats[0].frontier_converged);
    Ok(())
}

#[tokio::test]
async fn json_string_edit_rebases_long_suffix() -> anyhow::Result<()> {
    let source = include_str!("../../test_data/test.txt");
    let uri = Span::new("test://json-incremental", 0, 0)?.uri;
    let insert_at = source.find("Main St").expect("fixture address") + 4;
    let (_, stats) = run_json_incremental_case(
        source,
        vec![SourceEdit::Insert {
            key: Span::new_uri(uri, insert_at, insert_at)?,
            value: "X".to_string(),
        }],
    )
    .await?;

    assert_eq!(stats[0].reparsed, 1);
    assert!(stats[0].reused > 20);
    assert!(stats[0].reconverged_new_boundary.is_some());
    Ok(())
}

#[tokio::test]
async fn json_runtime_whitespace_only_edit_is_ignored() -> anyhow::Result<()> {
    let source = r#"{"a":1,"b":2}"#;
    let insert_at = source.find(",\"b\"").unwrap() + 1;
    let uri = Span::new("test://json-incremental", 0, 0)?.uri;

    let (batches, _stats) = run_json_incremental_case(
        source,
        vec![SourceEdit::Insert {
            key: Span::new_uri(uri, insert_at, insert_at)?,
            value: " ".to_string(),
        }],
    )
    .await?;

    assert!(
        batches[1].is_empty(),
        "whitespace-only edit should not emit parse deltas"
    );
    Ok(())
}

#[tokio::test]
async fn json_edit_after_skipped_whitespace_reuses_suffix() -> anyhow::Result<()> {
    let source = r#"{"a":1,"b":"two","c":3,"d":4}"#;
    let whitespace_at = source.find(",\"b\"").unwrap() + 1;
    let string_at = source.find("two").unwrap() + 1;
    let uri = Span::new("test://json-incremental", 0, 0)?.uri;
    let (_, stats) = run_json_incremental_case(
        source,
        vec![
            SourceEdit::Insert {
                key: Span::new_uri(uri, whitespace_at, whitespace_at)?,
                value: " ".to_string(),
            },
            SourceEdit::Insert {
                key: Span::new_uri(uri, string_at + 1, string_at + 1)?,
                value: "X".to_string(),
            },
        ],
    )
    .await?;

    assert_eq!(stats[1].reparsed, 1);
    assert!(stats[1].reused > 4);
    Ok(())
}

#[tokio::test]
async fn json_runtime_whitespace_delete_is_ignored() -> anyhow::Result<()> {
    let source = r#"{"a":1, "b":2, "c":3}"#;
    let delete_at = source.find(", ").unwrap() + 1;
    let uri = Span::new("test://json-incremental", 0, 0)?.uri;

    let (batches, _stats) = run_json_incremental_case(
        source,
        vec![SourceEdit::Delete {
            key: Span::new_uri(uri, delete_at, delete_at + 1)?,
        }],
    )
    .await?;

    assert!(
        batches[1].is_empty(),
        "whitespace-delete edit should not emit parse deltas"
    );
    Ok(())
}

#[tokio::test]
async fn json_runtime_batched_replace_pair_emits_output() -> anyhow::Result<()> {
    let source = r#"{"a":1,"b":2}"#;
    let replace_at = source.find("a").unwrap();
    let uri = Span::new("test://json-incremental-batch", 0, 0)?.uri;

    let (batch, member_keys) = run_json_incremental_batched_case_member_keys(
        source,
        vec![
            SourceEdit::Delete {
                key: Span::new_uri(uri, replace_at, replace_at + 1)?,
            },
            SourceEdit::Insert {
                key: Span::new_uri(uri, replace_at, replace_at)?,
                value: "c".to_string(),
            },
        ],
    )
    .await?;

    assert!(
        !batch.is_empty(),
        "batched replace should emit parse deltas"
    );
    assert_eq!(member_keys, vec!["c", "b"]);
    Ok(())
}

#[tokio::test]
async fn json_error_recovery_preserves_following_member_after_null_suffix() -> anyhow::Result<()> {
    let source = r#"{"xddd":null,"he":222,"well":{}}"#;
    let insert_at = source.find("null").unwrap() + "null".len();
    let edited = format!("{}s{}", &source[..insert_at], &source[insert_at..]);
    let uri = Span::new("test://json-recovery", 0, 0)?.uri;

    let member_keys = run_json_incremental_case_member_keys(
        source,
        vec![SourceEdit::Insert {
            key: Span::new_uri(uri, insert_at, insert_at)?,
            value: "s".to_string(),
        }],
    )
    .await?;

    assert_eq!(edited, r#"{"xddd":nulls,"he":222,"well":{}}"#);
    assert_eq!(member_keys, vec!["xddd", "he", "well"]);
    Ok(())
}
