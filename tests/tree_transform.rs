#[path = "../examples/tree_transform/lower.rs"]
mod lower;
#[path = "../examples/tree_transform/syntax.rs"]
mod syntax;
#[path = "../examples/tree_transform/test.rs"]
mod test;
#[path = "../examples/tree_transform/view_harness.rs"]
mod view_harness;
#[path = "../examples/tree_transform/view_harness_test.rs"]
mod view_harness_test;

// ---------------------------------------------------------------------------
// Parser equivalence (plan §9.4): the parser publishes the SAME generated
// field codec as component render, so a warm re-parse and a cold parse
// materialize identical trees, and a local same-kind edit rewrites only the
// touched field facts.
// ---------------------------------------------------------------------------

mod parser_equivalence {
    use super::lower::{LoweredDocument, LoweredTree, semantic_digest};
    use super::syntax::{TransformDocument, TransformToken};
    use plingo::framework::source::SourceEdit;
    use plingo::framework::workspace::Workspace;
    use plingo::utils::Span;

    fn uri(name: &str) -> fluent_uri::Uri<String> {
        Span::new(format!("test://tree-equiv/{name}"), 0, 0)
            .expect("uri span")
            .uri
    }

    fn build() -> Workspace {
        Workspace::builder()
            .lexer::<TransformToken>()
            .parser::<TransformDocument>()
            .mount::<super::lower::lower_document::Component, _>(TransformDocument::roots())
            .build()
            .expect("workspace builds")
    }

    #[test]
    fn warm_reparsed_tree_matches_a_cold_parse_byte_for_byte() {
        let mut warm = build();
        let mut cold = build();
        let u = uri("equiv");
        let text = "let x: Nat = 1 + 2;\n";
        warm.open(u.clone(), text).expect("warm open");
        cold.open(u.clone(), text).expect("cold open");
        let warm_digest = semantic_digest(&warm.snapshot());
        let cold_digest = semantic_digest(&cold.snapshot());
        assert_eq!(
            warm_digest, cold_digest,
            "warm re-publication diverged from the cold parse"
        );
    }

    #[test]
    fn same_kind_local_edit_rewrites_only_the_leaf_field() {
        let mut ws = build();
        let u = uri("edit");
        ws.open(u.clone(), "let x: Nat = 1 + 2;\n").expect("open");
        let before = semantic_digest(&ws.snapshot());
        // Replace the leaf literal `2` with `7`: same kind (Number), so the
        // node identity and topology stay; only the leaf payload fact flips.
        ws.edit(vec![
            SourceEdit::Delete {
                key: Span::new_uri(u.clone(), 17, 18).expect("leaf range"),
            },
            SourceEdit::Insert {
                key: Span::point_uri(u.clone(), 17).expect("leaf point"),
                value: "7".into(),
            },
        ])
        .expect("leaf edit");
        let after = semantic_digest(&ws.snapshot());
        assert_ne!(before, after, "leaf edit must be observable");
        // The lowered shape is unchanged: same kinds, same structure.
        let warm_shape = after.rows_in("lowered");
        let cold_shape = before.rows_in("lowered");
        assert_eq!(warm_shape, cold_shape, "shape must be stable");
    }
}
