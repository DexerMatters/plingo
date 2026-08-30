//! Reactive lexer scenarios (ported from `tests/unit/component_lex_node.rs`):
//! lex errors, per-uri isolation, and deterministic token order, driven
//! through the `Workspace` + `install_lexer` API (plan §8.2, matrix 1).

use std::{fmt, sync::Arc};

use plingo::framework::lex::{LexErrorInfo, TokenVec, Tokens, install_lexer};
use plingo::framework::{SourceEdit, Workspace};
use plingo::prelude::Terminal;
use plingo::utils::Span;

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(root { Word })]
enum TestTokens {
    #[regex(r"[a-z]+")]
    Word(String),
    #[error]
    Error(LexErrorInfo),
}

impl fmt::Display for TestTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<TestTokens>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

#[test]
fn lexer_observes_source_without_a_lower_layer() {
    let mut ws = build(1);
    ws.open(uri("lex"), "hello").unwrap();
    let tokens: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://lex".to_string())
        .expect("committed tokens");
    // Deterministic order: the word token(s) in source order, then the
    // synthetic EOF token.
    assert_eq!(tokens.tokens.len(), 1, "one word, no EOF in public tokens");
    assert_eq!(tokens.tokens[0].value, TestTokens::Word("hello".into()));
    assert_eq!(tokens.tokens[0].length, 5);
    assert!(tokens.errors.is_empty());
}

#[test]
fn lex_errors_are_published_with_offsets() {
    let mut ws = build(1);
    // `@` is not a word character: it becomes an error token.
    ws.open(uri("err"), "a@b").unwrap();
    let tokens: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://err".to_string())
        .expect("committed tokens");
    assert!(
        !tokens.errors.is_empty(),
        "error token must be materialized"
    );
    let error = tokens.errors[0];
    assert!(error.start < error.end);
    assert!(error.end <= "a@b".len());
    // The error token also appears in the ordered token list in source
    // position.
    assert!(tokens.tokens.iter().any(|t| t.error.is_some()));
}

#[test]
fn per_uri_isolation_keeps_sibling_document_untouched() {
    let mut ws = build(1);
    ws.open(uri("a"), "alpha").unwrap();
    ws.open(uri("b"), "beta").unwrap();
    let before_b: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://b".to_string())
        .unwrap();
    // Edit document A only.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(uri("a"), 0).unwrap(),
        value: "x".into(),
    }])
    .unwrap();
    let after_b: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://b".to_string())
        .unwrap();
    assert_eq!(before_b, after_b, "B's lexer child never re-ran");
    // A changed.
    let after_a: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://a".to_string())
        .unwrap();
    assert_eq!(after_a.tokens[0].value, TestTokens::Word("xalpha".into()));
}

#[test]
fn deterministic_order_is_stable_across_worker_counts() {
    let text = "one two three";
    let mut single = build(1);
    let mut many = build(4);
    single.open(uri("det"), text).unwrap();
    many.open(uri("det"), text).unwrap();
    let s = single
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://det".to_string());
    let m = many
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://det".to_string());
    assert_eq!(s, m, "1-worker and N-worker commits are identical");
}

#[test]
fn closing_a_document_retracts_its_tokens() {
    let mut ws = build(1);
    ws.open(uri("gone"), "hello").unwrap();
    assert!(
        ws.snapshot()
            .observe::<Tokens<TestTokens>>("test://gone".to_string())
            .is_some()
    );
    ws.close(uri("gone")).unwrap();
    assert!(
        ws.snapshot()
            .observe::<Tokens<TestTokens>>("test://gone".to_string())
            .is_none()
    );
}

#[test]
fn equal_text_reopen_preserves_token_values() {
    let mut ws = build(1);
    ws.open(uri("eq"), "same").unwrap();
    let before: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://eq".to_string())
        .unwrap();
    ws.open(uri("eq"), "same").unwrap();
    let after: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://eq".to_string())
        .unwrap();
    assert_eq!(
        &*before.tokens, &*after.tokens,
        "reopening equal text preserves token values"
    );
    assert_eq!(
        &*before.errors, &*after.errors,
        "reopening equal text preserves token errors"
    );
}

/// Debug-only delta oracle (plan §20.2). On structural edits the lexer
/// publishes a fresh `TokenPatch`; this test materializes the old/new
/// token fact maps, computes the slow exact symmetric difference, and
/// requires the committed patch's inserted/removed/updated sets to equal
/// it exactly — rejecting missing, extra, overlapping, or equal-value
/// "updated" entries. A same-terminal value edit must change exactly one
/// fact value without bumping the semantic (parser-facing) revision.
#[test]
fn token_patch_matches_the_slow_symmetric_fact_diff() {
    use plingo::framework::lex::{LexedDocuments, SemanticRevisionId, TokenFactId, TokenFacts};

    /// (document_id, source-occurrence, value-discriminant) per committed
    /// fact, where the discriminant is fingerprint xor rotated terminal.
    fn fact_entries(ws: &Workspace) -> Vec<(u64, u64, u64)> {
        let mut entries: Vec<(u64, u64, u64)> = ws
            .snapshot()
            .inputs::<TokenFacts<TestTokens>>()
            .into_iter()
            .filter_map(|key| {
                let document_id = key.document_id;
                match key.token {
                    TokenFactId::Source(occurrence) => {
                        let value = ws
                            .snapshot()
                            .observe::<TokenFacts<TestTokens>>(key)
                            .map(|fact| {
                                fact.fingerprint.0 ^ (fact.terminal_id as u64).rotate_left(32)
                            })
                            .unwrap_or(0);
                        Some((document_id, occurrence.0, value))
                    }
                    TokenFactId::Synthetic(_) => None,
                }
            })
            .collect();
        entries.sort_unstable();
        entries
    }

    /// The slow exact symmetric difference of two fact-entry snapshots.
    fn slow_diff(
        before: &[(u64, u64, u64)],
        after: &[(u64, u64, u64)],
    ) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
        let keys = |entries: &[(u64, u64, u64)]| -> std::collections::BTreeSet<(u64, u64)> {
            entries
                .iter()
                .map(|(document, occurrence, _)| (*document, *occurrence))
                .collect()
        };
        let before_keys = keys(before);
        let after_keys = keys(after);
        let value_of = |entries: &[(u64, u64, u64)], key: (u64, u64)| -> Option<u64> {
            entries
                .iter()
                .find(|(document, occurrence, _)| (*document, *occurrence) == key)
                .map(|(_, _, value)| *value)
        };
        let mut inserted: Vec<u64> = after_keys
            .difference(&before_keys)
            .map(|(_, occurrence)| *occurrence)
            .collect();
        let mut removed: Vec<u64> = before_keys
            .difference(&after_keys)
            .map(|(_, occurrence)| *occurrence)
            .collect();
        let mut updated: Vec<u64> = after_keys
            .intersection(&before_keys)
            .filter(|key| value_of(before, **key) != value_of(after, **key))
            .map(|(_, occurrence)| *occurrence)
            .collect();
        inserted.sort_unstable();
        removed.sort_unstable();
        updated.sort_unstable();
        (inserted, removed, updated)
    }

    let patch_sets = |ws: &Workspace| -> (Vec<u64>, Vec<u64>, Vec<u64>) {
        let patch = ws
            .snapshot()
            .observe::<LexedDocuments<TestTokens>>("test://oracle".to_string())
            .expect("semantic document");
        (
            patch.patch.inserted.iter().map(|id| id.0).collect(),
            patch.patch.removed.iter().map(|id| id.0).collect(),
            patch.patch.updated.iter().map(|id| id.0).collect(),
        )
    };

    let u = uri("oracle");
    let mut ws = build(1);
    ws.open(u.clone(), "alpha beta gamma").unwrap();
    let before = fact_entries(&ws);
    // Three words plus two space error tokens are all semantic facts.
    assert_eq!(before.len(), 5, "three words and two space errors");

    // Same-terminal value change (alpha -> beta) in one command: exactly
    // one updated fact, no structural bump, no occurrence churn.
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 0, 5).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 0).unwrap(),
            value: "beta".into(),
        },
    ])
    .unwrap();
    let after = fact_entries(&ws);
    let (inserted, removed, updated) = slow_diff(&before, &after);
    assert_eq!(updated, vec![0], "exactly the retyped occurrence value");
    assert!(
        inserted.is_empty() && removed.is_empty(),
        "no occurrence churn"
    );
    // A value edit must not advance the parser-facing semantic revision
    // (the load's structural change already advanced it once): the
    // occurrence set, order, and terminals are unchanged. A follow-up
    // equal text edit is a no-op past the lexer.
    let revision_before_value_edit = SemanticRevisionId(1);
    let revision_after_value_edit = ws
        .snapshot()
        .observe::<LexedDocuments<TestTokens>>("test://oracle".to_string())
        .expect("semantic doc")
        .revision;
    assert_eq!(revision_after_value_edit, revision_before_value_edit);

    // Structural insertion at the head: the fresh patch's inserted set
    // equals the slow diff order-for-order, with no updates/removals.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 0).unwrap(),
        value: "zero ".into(),
    }])
    .unwrap();
    let after2 = fact_entries(&ws);
    let (inserted2, removed2, updated2) = slow_diff(&after, &after2);
    let (patch_inserted2, patch_removed2, patch_updated2) = patch_sets(&ws);
    assert_eq!(patch_inserted2, inserted2, "exactly the new occurrences");
    assert!(patch_removed2.is_empty(), "no removals on insertion");
    assert!(patch_updated2.is_empty(), "no updates on insertion");
    assert!(
        removed2.is_empty() && updated2.is_empty(),
        "slow diff agrees"
    );

    // Structural removal: the fresh patch's removed set equals the slow
    // diff (the word plus both neighbour space errors, which merge into
    // one fresh error occurrence).
    ws.edit(vec![SourceEdit::Delete {
        key: Span::new_uri(u.clone(), 6, 10).unwrap(), // "beta" in "zero beta beta gamma"
    }])
    .unwrap();
    let after3 = fact_entries(&ws);
    let (inserted3, removed3, updated3) = slow_diff(&after2, &after3);
    let (patch_inserted3, patch_removed3, patch_updated3) = patch_sets(&ws);
    assert_eq!(patch_inserted3, inserted3, "the merged error occurrence");
    assert_eq!(patch_removed3, removed3, "the disappeared occurrences");
    assert!(patch_updated3.is_empty(), "no updates on removal");
    assert!(updated3.is_empty(), "slow diff agrees");
}

/// Persistent-sharing oracle (plan §20.3): an edit before a long unchanged
/// suffix must reattach that suffix by pointer — the new semantic tape root
/// shares at least one immutable node with the old root, proving the
/// command never rebuilt the retained entries.
#[test]
fn prefix_edit_shares_the_unchanged_suffix_by_pointer() {
    use plingo::framework::lex::LexedDocuments;

    let u = uri("sharing");
    let mut ws = build(1);
    // 200 identical words: a suffix long enough that a rebuild would be
    // measurably distinct from pointer reattachment.
    let text = vec!["alpha"; 2000].join(" ");
    ws.open(u.clone(), &text).unwrap();
    let before = ws
        .snapshot()
        .observe::<LexedDocuments<TestTokens>>("test://sharing".to_string())
        .expect("semantic doc before");

    // Insert at the head: every retained word must stay pointer-shared.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 0).unwrap(),
        value: "zero ".into(),
    }])
    .unwrap();
    let after = ws
        .snapshot()
        .observe::<LexedDocuments<TestTokens>>("test://sharing".to_string())
        .expect("semantic doc after");
    assert!(
        before.semantic_tape_shares_subtree_with(&after),
        "the unchanged 2000-word suffix must be reattached by pointer, not rebuilt"
    );

    // A tail edit shares the retained PREFIX instead (symmetric proof).
    // "zero " + 2000 words; insert at the end (before the synthetic EOF).
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 5 + 2000 * 6 - 1).unwrap(),
        value: " omega".into(),
    }])
    .unwrap();
    let tail_after = ws
        .snapshot()
        .observe::<LexedDocuments<TestTokens>>("test://sharing".to_string())
        .expect("semantic doc after tail edit");
    assert!(
        after.semantic_tape_shares_subtree_with(&tail_after),
        "the unchanged prefix must stay pointer-shared across a tail edit"
    );
}

#[test]
fn multiple_disjoint_splices_replay_against_the_evolving_source() {
    let mut ws = build(1);
    let u = uri("multi-splice");
    ws.open(u.clone(), "one two three four").unwrap();

    // The first insertion shifts the second replacement by five bytes in
    // final coordinates. The lexer must replay the two edits in sequence,
    // not apply both against the original source root.
    ws.edit(vec![
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 0).unwrap(),
            value: "zero ".into(),
        },
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 8, 13).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 8).unwrap(),
            value: "tres".into(),
        },
    ])
    .unwrap();

    let tokens: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .observe::<Tokens<TestTokens>>("test://multi-splice".to_string())
        .expect("committed tokens");
    let words: Vec<_> = tokens
        .tokens
        .iter()
        .filter_map(|token| match &token.value {
            TestTokens::Word(word) => Some(word.as_str()),
            TestTokens::Error(_) => None,
        })
        .collect();
    assert_eq!(words, vec!["zero", "one", "two", "tres", "four"]);
}
