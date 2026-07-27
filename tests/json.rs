mod common;

use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::{Arc, mpsc},
};

use color_print::cprintln;
use common::json::{JsonDocument, JsonToken};
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, ModifyKind, RenameMode},
};
use plingo::{
    Graph, Subscription, ViewUpdate,
    component::{
        lex::LexerNode,
        parse::{AstKey, ParseRoots, ParserNode, grammar::Grammar},
        source::{SourceEdit, SourceNode},
    },
    utils::Span,
};
use similar::{ChangeTag, TextDiff};

/// A deliberately small downstream sink: it observes parser roots and prints
/// the stable AST identities that were deleted or created by each commit.
struct DebugSink {
    previous: HashSet<AstKey>,
}

impl DebugSink {
    fn new() -> Self {
        Self {
            previous: HashSet::new(),
        }
    }

    fn consume(&mut self, graph: &Graph, update: ViewUpdate<Arc<[AstKey]>>) -> Result<(), String> {
        let (snapshot, current) = match update {
            ViewUpdate::Initial { snapshot, value } | ViewUpdate::Changed { snapshot, value } => {
                (snapshot, value.iter().cloned().collect())
            }
            ViewUpdate::Removed { snapshot } => (snapshot, HashSet::new()),
        };
        cprintln!("<bold,cyan>parser delta</> <dim>snapshot={snapshot}</>");

        let mut deleted = self
            .previous
            .difference(&current)
            .cloned()
            .collect::<Vec<_>>();
        let mut created = current
            .difference(&self.previous)
            .cloned()
            .collect::<Vec<_>>();
        deleted.sort_by_key(|key| (key.id, key.uri.to_string()));
        created.sort_by_key(|key| (key.id, key.uri.to_string()));

        for key in deleted {
            cprintln!("<red>  - delete <bold>node#{}</></>", key.id);
        }
        for key in created {
            let product = graph
                .read::<plingo::component::parse::ParsedAst<JsonToken, JsonDocument>>(key.clone())
                .map(|artifact| artifact.product);
            cprintln!(
                "<green>  + create <bold>node#{}</> <dim>product={:?}</></>",
                key.id,
                product
            );
        }

        self.previous = current;
        Ok(())
    }
}

fn drain_parser_updates(
    sink: &mut DebugSink,
    graph: &Graph,
    subscription: &Subscription<ParseRoots<JsonToken, JsonDocument>>,
) -> Result<(), String> {
    while let Ok(update) = subscription.try_recv() {
        sink.consume(graph, update)?;
    }
    Ok(())
}

/// Returns true only for an event that guarantees the writer has published a
/// complete file revision. In-place saves publish on close; atomic saves
/// publish when the replacement is renamed into the watched path.
fn completed_write(event: &Event, path: &Path) -> bool {
    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write))
        | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => true,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            event.paths.last().is_some_and(|target| target == path)
        }
        _ => false,
    }
}

#[test]
fn json_syntax_builds_with_macro_grammar() {
    let grammar = Grammar::from_spec::<JsonDocument>();
    assert!(grammar.terminal_count() > 0);
    assert_eq!(JsonToken::Null.to_string(), "Null");
}

#[test]
#[ignore = "interactive file watcher; run explicitly with --ignored --nocapture"]
fn test_json_runtime() {
    let data_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data");
    let path = fs::read_dir(&data_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_file())
        .expect("test_data must contain a source file");
    let mut current = fs::read_to_string(&path).unwrap();

    let uri = Span::new(
        format!("test://{}", path.file_name().unwrap().to_string_lossy()),
        0,
        0,
    )
    .unwrap()
    .uri;
    let parser = Grammar::from_spec::<JsonDocument>().build_lr1::<JsonToken>();
    let mut graph = Graph::new();
    graph
        .install(LexerNode::<JsonToken>::new().unwrap())
        .unwrap();
    graph
        .install(ParserNode::<JsonToken, JsonDocument>::from_parser(parser))
        .unwrap();
    graph.command(SourceNode::load(uri)).unwrap();
    graph
        .command(SourceNode::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: current.clone(),
        }))
        .unwrap();

    let subscription = graph
        .subscribe::<ParserNode<JsonToken, JsonDocument>>(uri)
        .unwrap();
    let mut sink = DebugSink::new();
    sink.consume(&graph, subscription.recv().unwrap()).unwrap();

    let (events, receiver) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = events.send(event);
        },
        Config::default(),
    )
    .unwrap();
    watcher
        .watch(&data_dir, RecursiveMode::NonRecursive)
        .unwrap();

    // Process each watched event immediately. Duplicate notifications are
    // naturally ignored when their file contents equal the committed source.
    loop {
        let event = receiver.recv().unwrap().unwrap();
        // `Modify(Data(_))` fires while an in-place writer is still active;
        // wait for its close event rather than treating a transient truncation
        // as a document revision. This adds no timer or debounce delay.
        if !completed_write(&event, &path)
            || !event.paths.iter().any(|event_path| event_path == &path)
        {
            continue;
        }

        let next = match fs::read_to_string(&path) {
            Ok(next) => next,
            Err(error) => {
                cprintln!("<yellow>waiting for watched file: {error}</>");
                continue;
            }
        };
        if next == current {
            continue;
        }

        let diff = TextDiff::from_chars(&current, &next);
        let chunks = diff.iter_all_changes().fold(
            Vec::<(ChangeTag, String)>::new(),
            |mut chunks, change| {
                if let Some((tag, value)) = chunks.last_mut()
                    && *tag == change.tag()
                {
                    value.push_str(change.value());
                } else {
                    chunks.push((change.tag(), change.value().to_owned()));
                }
                chunks
            },
        );
        let mut offset = 0;
        let mut edits = Vec::new();
        for (tag, value) in chunks {
            match tag {
                ChangeTag::Equal => offset += value.len(),
                ChangeTag::Delete => {
                    let end = offset + value.len();
                    edits.push(SourceEdit::Delete {
                        key: Span::new_uri(uri, offset, end).unwrap(),
                    });
                }
                ChangeTag::Insert => {
                    let length = value.len();
                    edits.push(SourceEdit::Insert {
                        key: Span::point_uri(uri, offset).unwrap(),
                        value,
                    });
                    offset += length;
                }
            }
        }

        match graph.command(SourceNode::apply_all(edits)) {
            Ok(()) => {
                drain_parser_updates(&mut sink, &graph, &subscription).unwrap();
                current = next;
            }
            Err(error) => {
                cprintln!("<yellow>parser rejected file revision: {error}</>");
            }
        }
    }
}
