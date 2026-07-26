use std::collections::HashSet;

use super::{
    data::{
        AstOwner, FrameDraft, FrameKey, PatchBuilder, ScopeDatum, ScopeEdge, ScopeReference,
        ScopeSnapshot,
    },
    PathExpr, ScopeProperty, ScopeQuery,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Label {
    Up,
    Decl,
}

fn uri() -> fluent_uri::Uri<&'static str> {
    match crate::utils::Span::new("test://scope", 0, 0) {
        Ok(span) => span.uri,
        Err(error) => panic!("valid test URI: {error}"),
    }
}

fn owner(product: usize) -> AstOwner {
    AstOwner {
        uri: uri(),
        product,
    }
}

#[test]
fn shared_child_survives_one_parent_retraction() {
    let mut state = ScopeSnapshot::<(), Label, &'static str, &'static str, ()>::default();
    let mut patch = PatchBuilder::default();
    let root = state.root_scope(uri(), &mut patch);
    let child = FrameKey {
        owner: owner(3),
        incoming: root,
    };
    assert!(state
        .replace_frame(child.clone(), FrameDraft::default(), &mut patch)
        .is_ok());

    let left = FrameKey {
        owner: owner(1),
        incoming: root,
    };
    let right = FrameKey {
        owner: owner(2),
        incoming: root,
    };
    for parent in [left.clone(), right.clone()] {
        let mut draft = FrameDraft::default();
        draft.children.insert(child.clone());
        assert!(state.replace_frame(parent, draft, &mut patch).is_ok());
    }
    state.replace_roots(uri(), HashSet::from([left, right.clone()]), &mut patch);
    state.replace_roots(uri(), HashSet::from([right]), &mut patch);

    assert!(state.frames.contains_key(&child));
}

#[test]
fn exact_fact_ownership_preserves_other_frame() {
    let mut state = ScopeSnapshot::<(), Label, &'static str, &'static str, ()>::default();
    let mut patch = PatchBuilder::default();
    let root = state.root_scope(uri(), &mut patch);
    let first = FrameKey {
        owner: owner(1),
        incoming: root,
    };
    let second = FrameKey {
        owner: owner(2),
        incoming: root,
    };
    let first_scope = state.ast_scope(&first.owner, &mut patch);
    let second_scope = state.ast_scope(&second.owner, &mut patch);

    let mut first_draft = FrameDraft::default();
    first_draft.references.push(ScopeReference {
        scope: first_scope,
        reference: "first",
    });
    let mut second_draft = FrameDraft::default();
    second_draft.references.push(ScopeReference {
        scope: second_scope,
        reference: "second",
    });
    assert!(state
        .replace_frame(first.clone(), first_draft, &mut patch)
        .is_ok());
    assert!(state
        .replace_frame(second.clone(), second_draft, &mut patch)
        .is_ok());
    state.replace_roots(uri(), HashSet::from([first, second.clone()]), &mut patch);
    state.replace_roots(uri(), HashSet::from([second]), &mut patch);

    assert_eq!(state.references.len(), 1);
    assert_eq!(
        state.references.values().next().map(|fact| fact.reference),
        Some("second")
    );
}

#[test]
fn one_ast_owner_has_one_scope_across_contextual_frames() {
    let mut state = ScopeSnapshot::<&'static str, Label, (), (), ()>::default();
    let mut patch = PatchBuilder::default();
    let root = state.root_scope(uri(), &mut patch);
    let alternate = state.external_scope("alternate", &mut patch);
    let ast = owner(7);
    let first = FrameKey {
        owner: ast.clone(),
        incoming: root,
    };
    let second = FrameKey {
        owner: ast.clone(),
        incoming: alternate,
    };

    assert_ne!(first, second);
    assert_eq!(
        state.ast_scope(&first.owner, &mut patch),
        state.ast_scope(&second.owner, &mut patch)
    );
}

#[test]
fn regular_path_query_returns_datum_path() {
    let mut state = ScopeSnapshot::<(), Label, &'static str, (), ()>::default();
    let mut patch = PatchBuilder::default();
    let root = state.root_scope(uri(), &mut patch);
    let body_owner = owner(1);
    let declaration_owner = owner(2);
    let frame = FrameKey {
        owner: body_owner.clone(),
        incoming: root,
    };
    let body = state.ast_scope(&body_owner, &mut patch);
    let declaration = state.ast_scope(&declaration_owner, &mut patch);
    let mut draft = FrameDraft::default();
    draft.edges.push(ScopeEdge {
        source: body,
        label: Label::Up,
        target: root,
        property: ScopeProperty::Acyclic,
    });
    draft.edges.push(ScopeEdge {
        source: root,
        label: Label::Decl,
        target: declaration,
        property: ScopeProperty::Cyclic,
    });
    draft.datums.push(ScopeDatum {
        scope: declaration,
        datum: "x",
    });
    assert!(state
        .replace_frame(frame.clone(), draft, &mut patch)
        .is_ok());
    state.replace_roots(uri(), HashSet::from([frame]), &mut patch);

    let query = ScopeQuery::new(
        body,
        PathExpr::zero_or_more(Label::Up).then(PathExpr::label(Label::Decl)),
        |datum| *datum == "x",
    );
    let answers = state.resolve_query(&query);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].datum, "x");
}
