//! The built-in parser component (plan §8.3): [`ParseUnits`],
//! [`ParseDiagnostics`], and [`install_parser`].
//!
//! One child visitor per uri over [`Tokens`] keeps an edit to document A
//! from re-running document B's parser child (matrix 1). Each child replays
//! the sparse token delta into the shared [`Parser`], publishes the
//! per-document [`ParseUnit`] and diagnostics, and walks the accepted root
//! value through the generated arena walker into the family tree view
//! (span-derived ids: unchanged subtrees keep their ids, matrix 2/3).

use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use fluent_uri::Uri;

use crate::framework::change::AddressChange;
use crate::framework::lex::{LexerRoot, Tokens};
use crate::framework::parse::data::ast::AstBox;
use crate::framework::parse::{
    AbstractTreeFamily, AstSnapshot, IncrementalParseStats, ParseErrorInfo, ParseStatus, Parser,
    TokenData,
};
use crate::framework::parse::__macro_private::NonTerminalSpec;
use crate::reactive::prelude::*;
use crate::reactive::view::NodeId;
use crate::reactive_view as view;

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One complete per-document parse publication: the accepted root's tree
/// node id, the status, and the incremental stats.
pub struct ParseUnit<A: 'static> {
    /// The syntax-tree node id of the accepted root.
    pub root: NodeId,
    pub status: ParseStatus,
    pub stats: IncrementalParseStats,
    _tree: PhantomData<fn() -> A>,
}

impl<A: 'static> Clone for ParseUnit<A> {
    fn clone(&self) -> Self {
        Self {
            root: self.root,
            status: self.status.clone(),
            stats: self.stats,
            _tree: PhantomData,
        }
    }
}

impl<A: 'static> std::fmt::Debug for ParseUnit<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParseUnit")
            .field("root", &self.root)
            .field("status", &self.status)
            .field("stats", &self.stats)
            .finish()
    }
}

impl<A: 'static> PartialEq for ParseUnit<A> {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
            && self.status == other.status
            && self.stats == other.stats
    }
}

impl<A: 'static> Eq for ParseUnit<A> {}

impl<A: 'static> ParseUnit<A> {
    fn new(root: NodeId, status: ParseStatus, stats: IncrementalParseStats) -> Self {
        Self {
            root,
            status,
            stats,
            _tree: PhantomData,
        }
    }
}

/// Per-document parse units (built-in parser).
#[view(map, key = String, value = ParseUnit<A>)]
pub struct ParseUnits<A: 'static>(PhantomData<fn() -> A>);

/// Per-document parser diagnostics (built-in parser). Parse errors are the
/// one separate view; lex errors ride inside the token vec.
#[view(map, key = String, value = Arc<Vec<ParseErrorInfo>>)]
pub struct ParseDiagnostics;

// ---------------------------------------------------------------------------
// The component
// ---------------------------------------------------------------------------

struct ParserMachine<R: LexerRoot + Clone + std::fmt::Debug, A: AbstractTreeFamily> {
    parser: Parser<R>,
    uris: HashMap<String, Uri<&'static str>>,
    _tree: PhantomData<fn() -> A>,
}

impl<R: LexerRoot + Clone + std::fmt::Debug, A: AbstractTreeFamily> ParserMachine<R, A> {
    fn static_uri(&mut self, uri: &str) -> Uri<&'static str> {
        if let Some(cached) = self.uris.get(uri) {
            return *cached;
        }
        let leaked: &'static str = Box::leak(uri.to_string().into_boxed_str());
        let parsed = Uri::parse(leaked).expect("workspace uris are valid");
        self.uris.insert(uri.to_string(), parsed);
        parsed
    }
}

/// The built-in parser component: observes [`Tokens`], emits
/// [`ParseUnits`] + [`ParseDiagnostics`] + `A::View`, with one child
/// visitor per uri.
pub struct ParserComponent<R, A>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: AbstractTreeFamily,
{
    machine: Arc<Mutex<ParserMachine<R, A>>>,
}

impl<R, A> ParserComponent<R, A>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: AbstractTreeFamily,
{
    pub fn from_parser(parser: Parser<R>) -> Self {
        Self {
            machine: Arc::new(Mutex::new(ParserMachine {
                parser,
                uris: HashMap::new(),
                _tree: PhantomData,
            })),
        }
    }
}

impl<R, A> Component for ParserComponent<R, A>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: AbstractTreeFamily,
{
    fn name(&self) -> &'static str {
        "framework::parse::parser"
    }

    fn install(&self, builder: &mut EngineBuilder) -> Result<()> {
        builder.observe::<Tokens<R>>()?;
        builder.emit::<ParseUnits<A>>()?;
        builder.emit::<ParseDiagnostics>()?;
        builder.emit::<A::View>()?;
        Ok(())
    }

    fn run(&self, cx: &RunContext) -> Result<()> {
        let tokens = cx.observed::<Tokens<R>>()?;
        let units = cx.emitted::<ParseUnits<A>>()?;
        let diagnostics = cx.emitted::<ParseDiagnostics>()?;
        let tree = cx.emitted::<A::View>()?;
        let machine = Arc::clone(&self.machine);
        tokens.visit_each(move |uri, value| -> Result<()> {
            let Some(token_vec) = value else {
                // Document retired: retract every per-document publication.
                units.remove(uri.clone())?;
                diagnostics.remove(uri)?;
                return Ok(());
            };
            let token_vec = Arc::clone(&token_vec);
            let mut machine = machine.lock().expect("parser machine lock");
            let static_uri = machine.static_uri(&uri);
            // Replay the lexer's sparse token delta, then publish.
            let snapshot = machine
                .parser
                .derive_changes(static_uri, token_vec.changes.as_ref())
                .and_then(|_| {
                    machine.parser.commit_snapshot(
                        static_uri,
                        Arc::clone(&token_vec.source),
                        token_vec.data.as_ref(),
                    )
                })
                .map_err(|error| Error::Internal(error.to_string()))?;
            let accepted = machine
                .parser
                .latest
                .roots
                .get(&static_uri)
                .map(|roots| roots.as_ref().clone())
                .unwrap_or_default();
            let parse_diagnostics: Arc<Vec<ParseErrorInfo>> =
                machine.parser.latest_parse_diagnostics(static_uri).into();
            let stats = machine
                .parser
                .incremental_stats(static_uri)
                .unwrap_or_default();
            let status = if parse_diagnostics.is_empty() {
                ParseStatus::Clean
            } else if snapshot.ast_keys().next().is_none() {
                ParseStatus::Unrecoverable {
                    diagnostics: parse_diagnostics.len(),
                }
            } else {
                ParseStatus::Recovered {
                    diagnostics: parse_diagnostics.len(),
                }
            };

            // Tree emission: walk the accepted root value through the
            // session arena. The generated walker derives node ids from
            // source spans, so unchanged subtrees keep their ids.
            let arenas = machine.parser.session_arenas.get(&static_uri);
            let root_ast_id = accepted
                .first()
                .and_then(|product| {
                    arenas?
                        .products
                        .products
                        .get(*product)?
                        .ast_ids
                        .last()
                        .copied()
                });

            let unit = if let Some(root_ast_id) = root_ast_id {
                let root_span = snapshot
                    .ast_span(root_ast_id)
                    .map(|span| (span.range.start() as u32, span.range.end() as u32))
                    .unwrap_or((0, 0));
                let root_value = arenas
                    .and_then(|arenas| arenas.ast.get(AstBox::new(root_ast_id, static_uri)))
                    .map(|value| value as &A);

                if let Some(root_value) = root_value {
                    let root_kind = <A as AbstractTreeFamily>::__tree_kind_of(root_value);
                    let uri_str = uri.clone();
            let tree_uri: &str = &uri_str;
            #[allow(unused)]
            let root_kind = { root_kind };
            let root_node = <A as AbstractTreeFamily>::__tree_view_id(
                tree_uri, root_span.0, root_span.1, root_kind);

            <A as AbstractTreeFamily>::__tree_walk_emit(
                &tree,
                tree_uri,
                &snapshot,
                &arenas.expect("root implies live session arenas").ast,
                root_node,
                root_value,
            )?;
                    ParseUnit::new(root_node, status, stats)
                } else {
                    ParseUnit::new(NodeId(u64::MAX), status, stats)
                }
            } else {
                ParseUnit::new(NodeId(u64::MAX), status, stats)
            };
            units.set(uri.clone(), unit)?;
            diagnostics.set(uri, parse_diagnostics)?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Installs the built-in parser pipeline: `ParseUnits<A>` + per-document
/// `ParseDiagnostics`, plus the parser component observing [`Tokens`].
/// `R` is the token root, `A` the family root. This entry does NOT publish
/// the family tree view (legacy `#[derive(NonTerminal)]` grammars like the
/// JSON fixture have no `#[abstract_tree]` surface); use
/// [`install_parser_tree`] when the family carries `AbstractTreeFamily`.
pub fn install_parser<R, A>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: NonTerminalSpec + 'static,
{
    let parser = crate::framework::parse::grammar::Grammar::from_spec::<A>().build_lr1::<R>();
    engine.install(ParserCoreComponent::<R, A>::from_parser(parser))
}

/// Installs the full parser pipeline including the generated family tree
/// view. `A` must be the root of an `#[abstract_tree(members(...))]`
/// family (it implements [`AbstractTreeFamily`]).
pub fn install_parser_tree<R, A>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: AbstractTreeFamily + NonTerminalSpec,
{
    let parser = crate::framework::parse::grammar::Grammar::from_spec::<A>().build_lr1::<R>();
    engine.install(ParserComponent::<R, A>::from_parser(parser))
}

// ---------------------------------------------------------------------------
// Tree-less parser component (legacy `NonTerminal` grammars)
// ---------------------------------------------------------------------------

struct ParserCoreMachine<R: LexerRoot + Clone + std::fmt::Debug, A: 'static> {
    parser: Parser<R>,
    uris: HashMap<String, Uri<&'static str>>,
    _ast: PhantomData<fn() -> A>,
}

impl<R: LexerRoot + Clone + std::fmt::Debug, A: 'static> ParserCoreMachine<R, A> {
    fn static_uri(&mut self, uri: &str) -> Uri<&'static str> {
        if let Some(cached) = self.uris.get(uri) {
            return *cached;
        }
        let leaked: &'static str = Box::leak(uri.to_string().into_boxed_str());
        let parsed = Uri::parse(leaked).expect("workspace uris are valid");
        self.uris.insert(uri.to_string(), parsed);
        parsed
    }
}

/// The built-in parser component without the family tree view: observes
/// [`Tokens`], emits [`ParseUnits`] + [`ParseDiagnostics`], one child
/// visitor per uri.
pub struct ParserCoreComponent<R, A>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    machine: Arc<Mutex<ParserCoreMachine<R, A>>>,
}

impl<R, A> ParserCoreComponent<R, A>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    pub fn from_parser(parser: Parser<R>) -> Self {
        Self {
            machine: Arc::new(Mutex::new(ParserCoreMachine {
                parser,
                uris: HashMap::new(),
                _ast: PhantomData,
            })),
        }
    }
}

impl<R, A> Component for ParserCoreComponent<R, A>
where
    R: LexerRoot + Clone + std::fmt::Debug,
    A: 'static,
{
    fn name(&self) -> &'static str {
        "framework::parse::parser_core"
    }

    fn install(&self, builder: &mut EngineBuilder) -> Result<()> {
        builder.observe::<Tokens<R>>()?;
        builder.emit::<ParseUnits<A>>()?;
        builder.emit::<ParseDiagnostics>()?;
        Ok(())
    }

    fn run(&self, cx: &RunContext) -> Result<()> {
        let tokens = cx.observed::<Tokens<R>>()?;
        let units = cx.emitted::<ParseUnits<A>>()?;
        let diagnostics = cx.emitted::<ParseDiagnostics>()?;
        let machine = Arc::clone(&self.machine);
        tokens.visit_each(move |uri, value| -> Result<()> {
            let Some(token_vec) = value else {
                units.remove(uri.clone())?;
                diagnostics.remove(uri)?;
                return Ok(());
            };
            let token_vec = Arc::clone(&token_vec);
            let mut machine = machine.lock().expect("parser machine lock");
            let static_uri = machine.static_uri(&uri);
            let snapshot = machine
                .parser
                .derive_changes(static_uri, token_vec.changes.as_ref())
                .and_then(|_| {
                    machine.parser.commit_snapshot(
                        static_uri,
                        Arc::clone(&token_vec.source),
                        token_vec.data.as_ref(),
                    )
                })
                .map_err(|error| Error::Internal(error.to_string()))?;
            let accepted = machine
                .parser
                .latest
                .roots
                .get(&static_uri)
                .map(|roots| roots.as_ref().clone())
                .unwrap_or_default();
            let parse_diagnostics: Arc<Vec<ParseErrorInfo>> =
                machine.parser.latest_parse_diagnostics(static_uri).into();
            let stats = machine
                .parser
                .incremental_stats(static_uri)
                .unwrap_or_default();
            let status = if parse_diagnostics.is_empty() {
                ParseStatus::Clean
            } else if snapshot.ast_keys().next().is_none() {
                ParseStatus::Unrecoverable {
                    diagnostics: parse_diagnostics.len(),
                }
            } else {
                ParseStatus::Recovered {
                    diagnostics: parse_diagnostics.len(),
                }
            };
            let root = accepted
                .first()
                .and_then(|product| {
                    let arenas = machine.parser.session_arenas.get(&static_uri)?;
                    arenas.products.products.get(*product)?.ast_ids.first().copied()
                })
                .map(|id| {
                    let span = snapshot
                        .ast_span(id)
                        .map(|s| (s.range.start() as u32, s.range.end() as u32));
                    (id, span)
                });
            let unit = match root {
                Some((_, Some((s, e)))) => {
                    ParseUnit::new(NodeId(u64::from(s) << 32 | u64::from(e)), status, stats)
                }
                _ => ParseUnit::new(NodeId(u64::MAX), status, stats),
            };
            units.set(uri.clone(), unit)?;
            diagnostics.set(uri, parse_diagnostics)?;
            Ok(())
        })
    }
}

// Re-exported for the generated walker's signature compatibility.
#[allow(unused_imports)]
use crate::framework::change as _change;
#[allow(unused_imports)]
use crate::framework::parse::TokenData as _TokenData;
#[allow(unused_imports)]
use crate::framework::parse::AstSnapshot as _AstSnapshot;
#[allow(unused_imports)]
type _AddressChange = AddressChange<Uri<&'static str>, TokenData>;