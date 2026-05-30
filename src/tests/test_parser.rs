use crate::{
    NonTerminal,
    component::{
        debug::DebugSink,
        lex::Lexer,
        parse::{
            AstToken, ParseForest, ParsePath, Parser,
            data::AstBox,
            grammar::Grammar,
            policy::{DerefAstBox, DerefAstToken, GetAstTree, GetRootAstBox},
        },
        source::Source,
    },
    scheme::{Context, Delta, Runtime},
    tokens,
    utils::{RangeOrPoint, Span},
};
use color_print::cprintln;
use std::{
    marker::PhantomData,
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};
use tokio::sync::mpsc;

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
    #[rule(JsonObject)]
    Object(#[from(0)] AstBox<JsonObject>),
    #[rule(JsonArray)]
    Array(#[from(0)] AstBox<JsonArray>),
}
#[derive(NonTerminal, Debug, Clone)]
enum JsonObject {
    #[rule(JsonToken::LBrace, JsonToken::RBrace)]
    Empty,
    #[rule(JsonToken::LBrace, JsonMembers, JsonToken::RBrace)]
    Members(#[from(1)] AstBox<JsonMembers>),
}
#[derive(NonTerminal, Debug, Clone)]
enum JsonMembers {
    #[rule(JsonMember)]
    One(#[from(0)] AstBox<JsonMember>),
    #[rule(JsonMembers, JsonToken::Comma, JsonMember)]
    More(
        #[from(0)] AstBox<JsonMembers>,
        #[from(2)] AstBox<JsonMember>,
    ),
}
#[derive(NonTerminal, Debug, Clone)]
enum JsonMember {
    #[rule(JsonToken::String, JsonToken::Colon, JsonValue)]
    Pair(#[from(0)] AstToken<JsonToken>, #[from(2)] AstBox<JsonValue>),
}
#[derive(NonTerminal, Debug, Clone)]
enum JsonArray {
    #[rule(JsonToken::LBracket, JsonToken::RBracket)]
    Empty,
    #[rule(JsonToken::LBracket, JsonElements, JsonToken::RBracket)]
    Elements(#[from(1)] AstBox<JsonElements>),
}
#[derive(NonTerminal, Debug, Clone)]
enum JsonElements {
    #[rule(JsonValue)]
    One(#[from(0)] AstBox<JsonValue>),
    #[rule(JsonElements, JsonToken::Comma, JsonValue)]
    More(
        #[from(0)] AstBox<JsonElements>,
        #[from(2)] AstBox<JsonValue>,
    ),
}

type JsonSink = DebugSink<ParsePath, ParseForest>;
type Jp = Parser<JsonToken, JsonSink>;

macro_rules! deref {
    ($ctx:expr, $ty:ty, $action:expr, $label:expr, $indent:expr) => {
        match $ctx.post::<Jp, $ty>($action).await {
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
                DerefAstToken<JsonToken>,
                DerefAstToken(*tok),
                "String tok",
                indent
            ) {
                println!("{pad}String \"{s}\"")
            }
        }
        JsonValue::Number(tok) => {
            if let Some(JsonToken::Number(n)) = deref!(
                ctx,
                DerefAstToken<JsonToken>,
                DerefAstToken(*tok),
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
                DerefAstBox<JsonObject>,
                DerefAstBox(*obj),
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
                DerefAstBox<JsonArray>,
                DerefAstBox(*arr),
                "JsonArray",
                indent
            ) {
                walk_array(ctx, &arr, indent + 1).await;
            }
        }
    }
}

async fn walk_object(ctx: &Context, obj: &JsonObject, indent: usize) {
    match obj {
        JsonObject::Empty => {}
        JsonObject::Members(mems) => {
            if let Some(mems) = deref!(
                ctx,
                DerefAstBox<JsonMembers>,
                DerefAstBox(*mems),
                "JsonMembers",
                indent
            ) {
                Box::pin(walk_members(ctx, &mems, indent)).await;
            }
        }
    }
}

async fn walk_array(ctx: &Context, arr: &JsonArray, indent: usize) {
    match arr {
        JsonArray::Empty => {}
        JsonArray::Elements(els) => {
            if let Some(els) = deref!(
                ctx,
                DerefAstBox<JsonElements>,
                DerefAstBox(*els),
                "JsonElements",
                indent
            ) {
                Box::pin(walk_elements(ctx, &els, indent)).await;
            }
        }
    }
}

async fn walk_members(ctx: &Context, mems: &JsonMembers, indent: usize) {
    match mems {
        JsonMembers::One(m) => walk_member(ctx, m, indent).await,
        JsonMembers::More(more, m) => {
            let more: Option<JsonMembers> = deref!(
                ctx,
                DerefAstBox<JsonMembers>,
                DerefAstBox(*more),
                "Members::More inner",
                indent
            );
            if let Some(more) = more {
                Box::pin(walk_members(ctx, &more, indent)).await;
            }
            walk_member(ctx, m, indent).await;
        }
    }
}

async fn walk_member(ctx: &Context, member: &AstBox<JsonMember>, indent: usize) {
    let pad = "  ".repeat(indent);
    let Some(m) = deref!(
        ctx,
        DerefAstBox<JsonMember>,
        DerefAstBox(*member),
        "JsonMember",
        indent
    ) else {
        return;
    };
    match m {
        JsonMember::Pair(key, val) => {
            let k = deref!(
                ctx,
                DerefAstToken<JsonToken>,
                DerefAstToken(key),
                "member key",
                indent
            );
            let v = deref!(
                ctx,
                DerefAstBox<JsonValue>,
                DerefAstBox(val),
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
    }
}

async fn walk_elements(ctx: &Context, els: &JsonElements, indent: usize) {
    match els {
        JsonElements::One(val) => {
            if let Some(v) = deref!(
                ctx,
                DerefAstBox<JsonValue>,
                DerefAstBox(*val),
                "Elements::One",
                indent
            ) {
                walk_value(ctx, &v, indent).await;
            }
        }
        JsonElements::More(more, val) => {
            let more: Option<JsonElements> = deref!(
                ctx,
                DerefAstBox<JsonElements>,
                DerefAstBox(*more),
                "Elements::More inner",
                indent
            );
            if let Some(more) = more {
                Box::pin(walk_elements(ctx, &more, indent)).await;
            }
            if let Some(v) = deref!(
                ctx,
                DerefAstBox<JsonValue>,
                DerefAstBox(*val),
                "Elements::More val",
                indent
            ) {
                walk_value(ctx, &v, indent).await;
            }
        }
    }
}
const JSON_SAMPLE: &str =
    r#"{"name":"plingo","features":["lexer","parser"],"ok":true,"count":2,"meta":null}"#;

#[tokio::test]
async fn json_runtime_parse_output() -> anyhow::Result<()> {
    let _ = env_logger::try_init();
    let uri = Span::new("file:///json-runtime.json", 0, 0)?.uri;
    let counter = Arc::new(AtomicUsize::new(0));

    let debug_sink = debug_sink!(|ctx, deltas| async move {
        cprintln!("<dim>---------Received---------</dim>");
        let _: &Vec<Delta<ParsePath, ParseForest>> = &deltas;
        for delta in &deltas {
            match delta {
                Delta::Insert { key, value } => {
                    if key.path.is_empty() {
                        cprintln!(
                            "<green>Inserted: +{} root(s) at {key}</green>",
                            value.roots.len()
                        );
                        let roots = ctx
                            .post::<Jp, GetRootAstBox<JsonValue>>(GetRootAstBox(
                                key.uri,
                                PhantomData,
                            ))
                            .await
                            .unwrap();
                        for rb in &roots {
                            if let Ok(val) = ctx
                                .post::<Jp, DerefAstBox<JsonValue>>(DerefAstBox(*rb))
                                .await
                            {
                                walk_value(ctx, &val, 0).await;
                            }
                        }
                    } else {
                        cprintln!(
                            "<green>  + {} subtree(s) at {key}</green>",
                            value.roots.len()
                        );
                    }
                }
                Delta::Delete { key } => {
                    cprintln!("<red>Deleted: {key}</red>");
                    let prev = ctx.last_snapshot();
                    if let Ok(trees) = prev
                        .post::<Jp, GetAstTree<JsonValue>>(GetAstTree(key.clone(), PhantomData))
                        .await
                    {
                        for rb in &trees {
                            if let Ok(val) = prev
                                .post::<Jp, DerefAstBox<JsonValue>>(DerefAstBox(*rb))
                                .await
                            {
                                cprintln!("  <red>-</red> {val:?}");
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    });

    let (sender, receiver) = mpsc::channel(8);
    let parser = Grammar::from_spec::<JsonValue>().build_lr1::<JsonToken, JsonSink>();
    type Jl = Lexer<JsonToken, Jp>;
    let mut runtime = Runtime::new()
        .with(Source::new(receiver))
        .with(Jl::new()?)
        .with(parser)
        .finish(debug_sink);
    runtime.run().await?;
    sender
        .send(Delta::Insert {
            key: Span {
                uri,
                range: RangeOrPoint::Point(0),
            },
            value: r#"{"hello":{"world":22}}"#.into(),
        })
        .await?;

    sender
        .send(Delta::Insert {
            key: Span {
                uri,
                range: RangeOrPoint::Point(20),
            },
            value: r#","test":true"#.into(),
        })
        .await?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}
