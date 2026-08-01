mod common;

use std::{
    fs,
    path::Path,
    sync::{Arc, mpsc},
};

use color_print::cprintln;
use common::{
    json::{JsonDocument, JsonToken},
    utils::print_json_ast,
};
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, DataChange, ModifyKind, RenameMode},
};
use plingo::{
    Graph, ReadGraph, Subscription, ViewUpdate,
    component::{
        lex::LexerNode,
        parse::{AstSnapshot, ParseRoots, ParseSnapshot, ParserNode, grammar::Grammar},
        source::{SourceEdit, SourceInput},
    },
    utils::Span,
};
use similar::{ChangeTag, TextDiff};

/// Downstream consumers render immutable parser snapshots; keyed AST views
/// provide the graph-owned insertion/retraction granularity.
struct DebugSink;

impl DebugSink {
    fn new() -> Self {
        Self
    }

    fn consume(
        &mut self,
        graph: &Graph,
        update: ViewUpdate<Arc<AstSnapshot>>,
    ) -> Result<(), String> {
        let (graph_snapshot, snapshot) = match update {
            ViewUpdate::Initial { snapshot, value } | ViewUpdate::Changed { snapshot, value } => {
                (snapshot, Some(value))
            }
            ViewUpdate::Removed { snapshot } => (snapshot, None),
        };
        cprintln!("<bold,cyan>parser snapshot</> <dim>graph_snapshot={graph_snapshot}</>");
        let Some(snapshot) = snapshot else {
            cprintln!("<red>  parser snapshot removed</>");
            return Ok(());
        };
        if let Some(roots) = graph.get::<ParseRoots<JsonToken, JsonDocument>>(snapshot.uri()) {
            print_json_ast(graph, roots.as_ref());
        }
        Ok(())
    }
}

fn drain_parser_updates(
    sink: &mut DebugSink,
    graph: &Graph,
    subscription: &Subscription<ParseSnapshot<JsonToken>>,
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
fn watcher_accepts_only_completed_file_revisions() {
    let path = Path::new("test_data/test.txt");
    let close = Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
        .add_path(path.to_path_buf());
    let in_progress = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
        .add_path(path.to_path_buf());
    let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
        .add_path(Path::new("test_data/.test.txt.tmp").to_path_buf())
        .add_path(path.to_path_buf());

    assert!(completed_write(&close, path));
    assert!(!completed_write(&in_progress, path));
    assert!(completed_write(&rename, path));
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
    graph.command(SourceInput::load(uri)).unwrap();
    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: current.clone(),
        }))
        .unwrap();

    let _demand = graph
        .demand::<ParserNode<JsonToken, JsonDocument>>(uri)
        .unwrap();
    let subscription = graph.subscribe::<ParseSnapshot<JsonToken>>(uri).unwrap();
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

        match graph.command(SourceInput::apply_all(edits)) {
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
