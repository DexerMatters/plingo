//! Reactive publication of persistent lexer roots.
//!
//! `SemanticTokenDocuments` is the parser boundary and changes only for token
//! structure. `TokenLayoutDocuments` changes for source coordinates. `Tokens`
//! remains a lazy snapshot façade for callers that explicitly request an
//! ordered token stream; it is not an internal semantic dependency.

use std::marker::PhantomData;
use std::{
    collections::BTreeSet,
    ops::Deref,
    sync::{Arc, OnceLock},
};

use fluent_uri::Uri;
use parking_lot::Mutex;
use reactive_macros::view;

use crate::{
    framework::{
        lex::{
            LexErrorInfo, LexToken, Lexer, LexerCreationError, LexerRoot, LexicalDocument,
            SemanticTokenDocument, SemanticTokenDocuments, TokenFact, TokenFactId, TokenFactKey,
            TokenFacts, TokenLayoutDocument, TokenLayoutDocuments, TokenOccurrenceId, LexInterrupt,
        },
        source::SourceRevisions,
    },
    reactive::{
        Engine, Error, Result,
        kind::{Map, emit_patch, emit_view, observe_view},
        peek_committed,
    },
};

/// Lazy, snapshot-scoped public token sequence.  It materializes only when a
/// caller iterates/indexes the façade; lexer commands never rebuild it.
#[derive(Clone)]
pub struct TokenList<T: LexerRoot + Clone + std::fmt::Debug> {
    document: Arc<LexicalDocument<T>>,
    cache: Arc<OnceLock<Arc<[LexToken<T>]>>>,
}

impl<T: LexerRoot + Clone + std::fmt::Debug> TokenList<T> {
    fn new(document: Arc<LexicalDocument<T>>) -> Self {
        Self {
            document,
            cache: Arc::new(OnceLock::new()),
        }
    }

    fn values(&self) -> &[LexToken<T>] {
        self.cache
            .get_or_init(|| {
                let mut tokens = Vec::with_capacity(self.document.semantic.len());
                for rank in 0..self.document.lexical.len() {
                    let Some(occurrence) = self.document.lexical_at(rank) else {
                        continue;
                    };
                    if !occurrence.is_semantic() {
                        continue;
                    }
                    if let Some(token) = occurrence.materialize(self.document.lexical_start(rank)) {
                        tokens.push(token);
                    }
                }
                tokens.into()
            })
            .as_ref()
    }
}

impl<T: LexerRoot + Clone + std::fmt::Debug> Deref for TokenList<T> {
    type Target = [LexToken<T>];

    fn deref(&self) -> &Self::Target {
        self.values()
    }
}

impl<T: LexerRoot + Clone + std::fmt::Debug> std::fmt::Debug for TokenList<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.values()).finish()
    }
}

impl<T: LexerRoot + Clone + std::fmt::Debug> PartialEq for TokenList<T> {
    fn eq(&self, other: &Self) -> bool {
        // Layout revision is the complete façade identity. The stable
        // document namespace prevents two fresh documents at the same
        // revision from comparing equal without scanning their tapes.
        self.document.document == other.document.document
            && self.document.layout_revision == other.document.layout_revision
    }
}
impl<T: LexerRoot + Clone + std::fmt::Debug> Eq for TokenList<T> {}

/// Lazy snapshot list of lex errors.
#[derive(Clone)]
pub struct TokenErrors<T: LexerRoot + Clone + std::fmt::Debug> {
    document: Arc<LexicalDocument<T>>,
    cache: Arc<OnceLock<Arc<[LexErrorInfo]>>>,
}

impl<T: LexerRoot + Clone + std::fmt::Debug> TokenErrors<T> {
    fn new(document: Arc<LexicalDocument<T>>) -> Self {
        Self {
            document,
            cache: Arc::new(OnceLock::new()),
        }
    }

    fn values(&self) -> &[LexErrorInfo] {
        self.cache
            .get_or_init(|| {
                let mut errors = Vec::new();
                for rank in 0..self.document.lexical.len() {
                    let Some(token) = self.document.lexical_at(rank) else {
                        continue;
                    };
                    if let Some(kind) = token.error {
                        let start = self.document.lexical_start(rank);
                        errors.push(LexErrorInfo {
                            kind,
                            start,
                            end: start.saturating_add(token.byte_len as usize),
                        });
                    }
                }
                errors.into()
            })
            .as_ref()
    }
}

impl<T: LexerRoot + Clone + std::fmt::Debug> Deref for TokenErrors<T> {
    type Target = [LexErrorInfo];

    fn deref(&self) -> &Self::Target {
        self.values()
    }
}

impl<T: LexerRoot + Clone + std::fmt::Debug> std::fmt::Debug for TokenErrors<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.values()).finish()
    }
}

impl<T: LexerRoot + Clone + std::fmt::Debug> PartialEq for TokenErrors<T> {
    fn eq(&self, other: &Self) -> bool {
        self.document.document == other.document.document
            && self.document.layout_revision == other.document.layout_revision
    }
}
impl<T: LexerRoot + Clone + std::fmt::Debug> Eq for TokenErrors<T> {}

/// Editor-facing lazy ordered-token façade.  The fields intentionally preserve
/// the old ergonomic API (`tokens.iter()`, indexing, `errors.len()`) without
/// making either list a command-time reactive payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenVec<T: LexerRoot + Clone + std::fmt::Debug> {
    pub tokens: TokenList<T>,
    pub errors: TokenErrors<T>,
}

impl<T: LexerRoot + Clone + std::fmt::Debug> TokenVec<T> {
    fn new(document: Arc<LexicalDocument<T>>) -> Self {
        Self {
            tokens: TokenList::new(Arc::clone(&document)),
            errors: TokenErrors::new(document),
        }
    }

    /// Resolves a parser token through the lazy snapshot façade.
    pub fn token(&self, token: crate::framework::parse::AstToken<T>) -> Option<&LexToken<T>> {
        self.tokens.iter().find(|entry| entry.id == token.raw_id())
    }
}

/// Public snapshot façade map.  Internal parser/lower components never
/// observe this view.
#[view]
pub struct Tokens<T: LexerRoot + Clone + std::fmt::Debug>(Map<String, TokenVec<T>>);

struct LexerMachine<R: LexerRoot + Clone + std::fmt::Debug> {
    lexer: Lexer<R>,
}

impl<R: LexerRoot + Clone + std::fmt::Debug> LexerMachine<R> {
    fn new(lexer: Lexer<R>) -> Self {
        Self { lexer }
    }

    fn forget(&mut self, uri: &str) {
        let uri: Uri<String> = uri.parse().expect("workspace URIs are valid");
        self.lexer.forget_document(uri);
    }
}

fn patch_token_facts<R>(
    document: &LexicalDocument<R>,
    patch: &crate::framework::lex::TokenPatch,
) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
{
    let facts = emit_patch::<TokenFacts<R>>()?;
    for occurrence in patch.removed.iter().copied() {
        facts.remove(TokenFactKey {
            document_id: document.document.0,
            token: TokenFactId::Source(occurrence),
        })?;
    }
    let changed: BTreeSet<_> = patch
        .inserted
        .iter()
        .chain(patch.updated.iter())
        .copied()
        .collect();
    for occurrence in changed {
        let Some(rank) = document.lexical_rank_of(occurrence) else {
            continue;
        };
        let Some(token) = document.lexical_at(rank) else {
            continue;
        };
        if !token.is_semantic() {
            continue;
        }
        facts.upsert(
            TokenFactKey {
                document_id: document.document.0,
                token: TokenFactId::Source(occurrence),
            },
            TokenFact {
                terminal_id: token.terminal_key(),
                fingerprint: token.fingerprint(),
                value: Arc::clone(&token.value),
            },
        )?;
    }
    Ok(())
}

/// Lexes one source child and publishes only its persistent root handles and
/// direct fact patch.  No complete token vector is built here.
fn lex_document<R>(machine: Arc<Mutex<LexerMachine<R>>>, uri: String) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
{
    crate::framework::workspace::record_lexer_work(&uri, |work| {
        work.component_runs += 1;
    });
    let revisions = observe_view::<SourceRevisions>()?;
    let revision = revisions.get(&uri)?;
    let Some(revision) = revision.map(|revision| (*revision).clone()) else {
        let previous = crate::reactive::peek_committed::<TokenLayoutDocuments<R>>(uri.clone())?
            .map(|document| Arc::clone(&document.document));
        if let Some(document) = previous {
            let facts = emit_patch::<TokenFacts<R>>()?;
            // Closing is lifecycle reclamation, not a local edit; walk only the
            // document-owned semantic root to retract its owned fact domain.
            for token in document.semantic.iter() {
                facts.remove(TokenFactKey {
                    document_id: document.document.0,
                    token: TokenFactId::Source(token.occurrence),
                })?;
            }
        }
        machine.lock().forget(&uri);
        emit_view::<Tokens<R>>()?.remove(uri.clone())?;
        emit_view::<SemanticTokenDocuments<R>>()?.remove(uri.clone())?;
        return emit_view::<TokenLayoutDocuments<R>>()?.remove(uri);
    };

    let was_published = peek_committed::<TokenLayoutDocuments<R>>(uri.clone())?.is_some();
    let mut machine = machine.lock();
    let static_uri: Uri<String> = uri.parse().expect("workspace URI is valid");
    let had_private_document = machine.lexer.latest.documents.contains_key(&static_uri);
    let derived = machine
        .lexer
        .derive_document(Arc::clone(&revision))
        .map_err(|error| match error {
            LexInterrupt::StaleSourceRevision { uri } => Error::Internal(
                format!("stale source revision: {uri}").into(),
            ),
            error => Error::Internal(error.to_string().into()),
        })?;
    let document = derived.document;
    if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
        eprintln!(
            "lex publish uri={uri} source={:?} revision={} previous={:?} semantic_len={}",
            revision.text,
            revision.id.0,
            revision.previous.map(|id| id.0),
            document.semantic.len()
        );
    }
    // A keyed child can retire without executing its body. If the lexer
    // machine retained the private document across that membership gap,
    // TokenFacts were retracted with the old invocation and the incremental
    // patch alone may mention only changed occurrences. Republish the final
    // semantic token domain before the new invocation commits.
    if had_private_document && !was_published {
        let facts = emit_patch::<TokenFacts<R>>()?;
        for rank in 0..document.lexical.len() {
            let Some(token) = document.lexical_at(rank) else {
                continue;
            };
            if !token.is_semantic() {
                continue;
            }
            facts.upsert(
                TokenFactKey {
                    document_id: document.document.0,
                    token: TokenFactId::Source(token.id),
                },
                TokenFact {
                    terminal_id: token.terminal_key(),
                    fingerprint: token.fingerprint(),
                    value: Arc::clone(&token.value),
                },
            )?;
        }
    } else {
        patch_token_facts(document.as_ref(), &derived.patch)?;
    }
    emit_view::<Tokens<R>>()?.insert(uri.clone(), TokenVec::new(Arc::clone(&document)))?;
    emit_view::<TokenLayoutDocuments<R>>()?.insert(
        uri.clone(),
        TokenLayoutDocument {
            document_uri: uri.clone(),
            document_id: document.document.0,
            layout_revision: document.layout_revision,
            document: Arc::clone(&document),
        },
    )?;
    emit_view::<SemanticTokenDocuments<R>>()?.insert(
        uri.clone(),
        SemanticTokenDocument {
            document_uri: uri,
            document_id: document.document.0,
            revision: document.semantic_revision,
            structure_revision: document.structure_revision,
            patch: derived.patch,
            document,
        },
    )?;
    Ok(())
}

/// Installs one keyed lexer child per source URI.
pub fn install_lexer<R>(engine: &mut Engine) -> Result<()>
where
    R: LexerRoot + Clone + std::fmt::Debug,
{
    let machine = Arc::new(Mutex::new(LexerMachine::new(Lexer::new().map_err(
        |error: LexerCreationError| Error::Internal(error.to_string().into()),
    )?)));
    // Cut C: one first-class component keyed by source-revision membership.
    engine.install_component_each_key::<LexerDefinition<R>, SourceRevisions, _>(move |uri| {
        lex_document::<R>(Arc::clone(&machine), uri)
    })?;
    Ok(())
}

/// Definition marker for the framework lexer stage (Cut C).
#[doc(hidden)]
pub struct LexerDefinition<R>(PhantomData<fn() -> R>);

impl<R: LexerRoot + Clone + std::fmt::Debug> crate::reactive::component::ComponentDefinition
    for LexerDefinition<R>
{
    fn __descriptor() -> &'static str {
        "plingo::framework::lex::lexer"
    }
}
