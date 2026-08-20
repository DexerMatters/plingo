//! The reactive lexer component (plan §8.2): [`Tokens`] map view and
//! [`install_lexer`].
//!
//! One child visitor per uri over [`SourceText`] keeps an edit to document
//! A from scheduling document B's child (matrix 1). Each child re-lexes its
//! document from the committed text and publishes an immutable
//! [`TokenVec`]. Lex errors ride inside the [`TokenVec`] (not a separate
//! view); parse errors are the parser's concern.

use std::{collections::HashMap, marker::PhantomData, sync::Arc};
use std::sync::Mutex;

use fluent_uri::Uri;

use crate::framework::change::AddressChange;
use crate::framework::lex::{LexErrorInfo, LexToken, Lexer, LexerCreationError, LexerRoot};
use crate::framework::parse::TokenData;
use crate::framework::source::{SourceDelta, SourceSplice, SourceText};
use crate::reactive::prelude::*;
use crate::reactive_view as view;

// ---------------------------------------------------------------------------
// TokenVec and the Tokens view
// ---------------------------------------------------------------------------

/// One immutable lexer publication per document. `tokens` are the
/// public-facing token occurrences in document order; `errors` the lex
/// errors encountered during scanning (already populated in `tokens` as
/// error tokens); `data`/`changes`/`source` are the exact replay input the
/// parser component consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenVec<T: LexerRoot + Clone + std::fmt::Debug> {
    /// Token occurrences in source order (error tokens included).
    pub tokens: Vec<LexToken<T>>,
    /// The lex errors of this revision, in occurrence order.
    pub errors: Vec<LexErrorInfo>,
    /// Parser-facing coordinate data (occurrence ids, fingerprints).
    pub(crate) data: Arc<[TokenData]>,
    /// The sparse token delta from the previous revision.
    pub(crate) changes: Arc<[AddressChange<Uri<&'static str>, TokenData>]>,
    /// The source text this revision was lexed from.
    pub(crate) source: Arc<str>,
}

/// The ordered token stream of each open document (built-in lexer).
#[view(map, key = String, value = TokenVec<T>)]
pub struct Tokens<T: LexerRoot + Clone + std::fmt::Debug>(PhantomData<fn() -> T>);

// ---------------------------------------------------------------------------
// The component
// ---------------------------------------------------------------------------

struct LexerMachine<R: LexerRoot + Clone + std::fmt::Debug> {
    lexer: Lexer<R>,
    /// The pure lexer is keyed by `Uri<&'static str>` while the workspace
    /// channel is `String`; keep one leaked `'static` uri per document.
    uris: HashMap<String, Uri<&'static str>>,
}

impl<R: LexerRoot + Clone + std::fmt::Debug> LexerMachine<R> {
    fn new(lexer: Lexer<R>) -> Self {
        Self {
            lexer,
            uris: HashMap::new(),
        }
    }

    fn static_uri(&mut self, uri: &str) -> Uri<&'static str> {
        if let Some(cached) = self.uris.get(uri) {
            return *cached;
        }
        let leaked: &'static str = Box::leak(uri.to_string().into_boxed_str());
        let parsed = Uri::parse(leaked).expect("workspace uris are valid");
        self.uris.insert(uri.to_string(), parsed);
        parsed
    }

    /// Forgets one document: drops its leaked uri and its lexer state.
    fn forget(&mut self, uri: &str) {
        if let Some(static_uri) = self.uris.remove(uri) {
            self.lexer.forget_document(static_uri);
        }
    }
}

/// The built-in lexer component: observes [`SourceText`], emits
/// [`Tokens`] with one child visitor per uri.
pub struct LexerComponent<R: LexerRoot + Clone + std::fmt::Debug> {
    machine: Arc<Mutex<LexerMachine<R>>>,
}

impl<R: LexerRoot + Clone + std::fmt::Debug> LexerComponent<R> {
    pub fn new() -> Result<Self, LexerCreationError> {
        Ok(Self {
            machine: Arc::new(Mutex::new(LexerMachine::new(Lexer::new()?))),
        })
    }
}

impl<R: LexerRoot + Clone + std::fmt::Debug> Component for LexerComponent<R> {
    fn name(&self) -> &'static str {
        "framework::lex::lexer"
    }

    fn install(&self, builder: &mut EngineBuilder) -> Result<()> {
        builder.observe::<SourceText>()?;
        builder.emit::<Tokens<R>>()?;
        Ok(())
    }

    fn run(&self, cx: &RunContext) -> Result<()> {
        let text = cx.observed::<SourceText>()?; // Working state (T2)
        let out = cx.emitted::<Tokens<R>>()?;
        let machine = Arc::clone(&self.machine);
        text.visit_each(move |uri, value| -> Result<()> {
            let Some(source) = value else {
                // Retirement: the source document is gone; retract the
                // token publication and forget this document's state.
                out.remove(uri.clone())?;
                machine.lock().expect("lexer machine lock").forget(&uri);
                return Ok(());
            };
            let mut machine = machine.lock().expect("lexer machine lock");
            let static_uri = machine.static_uri(&uri);
            let previous = machine
                .lexer
                .latest
                .sources
                .get(&static_uri)
                .cloned()
                .unwrap_or_default();
            // Re-lex the whole committed document. The pure layer's
            // incremental machinery computes the exact sparse token delta
            // from the previous revision itself.
            let delta = SourceDelta {
                replace: true,
                splices: Arc::from([SourceSplice {
                    old_range: 0..previous.len(),
                    new_range: 0..source.len(),
                    removed: Arc::clone(&previous),
                    inserted: Arc::clone(&source),
                }]),
            };
            let document = machine
                .lexer
                .derive_document(static_uri, Arc::clone(&source), &delta)
                .map_err(|interrupt| Error::Internal(interrupt.to_string()))?;
            let errors = document
                .tokens
                .iter()
                .filter_map(|data| machine.lexer.token(data.id).and_then(|t| t.error))
                .collect::<Vec<_>>();
            let tokens = document
                .tokens
                .iter()
                .filter_map(|data| machine.lexer.token(data.id).cloned())
                .collect::<Vec<_>>();
            let changes: Arc<[AddressChange<Uri<&'static str>, TokenData>]> =
                document.changes.into();
            let source: Arc<str> = Arc::clone(&source);
            out.set(
                uri,
                TokenVec {
                    tokens,
                    errors,
                    data: document.tokens.into(),
                    changes,
                    source,
                },
            )?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Installs the built-in lexer pipeline: the [`Tokens`] publication and
/// the lexer component observing [`SourceText`].
pub fn install_lexer<R>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
{
    engine.install(LexerComponent::<R>::new().map_err(|error| Error::Internal(error.to_string()))?)
}