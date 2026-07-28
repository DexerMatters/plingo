//! Colored rendering of the real JSON AST values published by `ParserNode`.
#![allow(dead_code)]
//!
//! This test utility only dereferences immutable [`AstSnapshot`] values. It
//! never reparses source text or reaches into mutable parser arenas.

use std::collections::HashMap;

use color_print::cprintln;
use plingo::{
    Graph,
    component::{
        lex::{TokenArtifact, TokenOrder},
        parse::{AstKey, AstSnapshot, AstToken, ParseSnapshot, data::AstBox},
        source::DocumentText,
    },
};

use super::json::{
    JsonArray, JsonDocument, JsonElements, JsonMember, JsonMembers, JsonObject, JsonToken,
    JsonValue,
};

/// Prints a concrete, indented JSON AST for the parser roots in `roots`.
/// Positions are resolved from the immutable parser snapshot.
pub fn print_json_ast(graph: &Graph, roots: &[AstKey]) {
    if roots.is_empty() {
        cprintln!("<bold,red>AST</> <dim>(no parse roots)</>");
        return;
    }
    let uri = roots[0].uri;
    let Some(snapshot) = graph.read::<ParseSnapshot<JsonToken>>(uri) else {
        cprintln!("<bold,red>AST</> <red>no parser snapshot is materialized</>");
        return;
    };
    let tokens = token_texts(graph, uri);

    cprintln!("<bold,cyan>AST</>");
    let mut tree = JsonTree {
        snapshot: &snapshot,
        tokens,
    };
    for root in roots {
        tree.document(AstBox::new(root.id, root.uri), 0);
    }
}

struct JsonTree<'a> {
    snapshot: &'a AstSnapshot,
    tokens: HashMap<usize, String>,
}

impl JsonTree<'_> {
    fn document(&mut self, node: AstBox<JsonDocument>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonDocument", node.id);
            return;
        };
        self.line(depth, "cyan", "JsonDocument", self.position(node));
        match value.as_ref() {
            JsonDocument::Root(value) => self.value(*value, depth + 1),
            JsonDocument::Error(error) => self.error(depth + 1, &format!("{error:?}")),
        }
    }

    fn value(&mut self, node: AstBox<JsonValue>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonValue", node.id);
            return;
        };
        self.line(depth, "cyan", "JsonValue", self.position(node));
        match value.as_ref() {
            JsonValue::Object(object) => self.object(*object, depth + 1),
            JsonValue::Array(array) => self.array(*array, depth + 1),
            JsonValue::String(token) => self.scalar(depth + 1, "String", *token),
            JsonValue::Number(token) => self.scalar(depth + 1, "Number", *token),
            JsonValue::True(token) | JsonValue::False(token) => {
                self.scalar(depth + 1, "Boolean", *token)
            }
            JsonValue::Null(token) => self.scalar(depth + 1, "Null", *token),
            JsonValue::Error(error) => self.error(depth + 1, &format!("{error:?}")),
        }
    }

    fn object(&mut self, node: AstBox<JsonObject>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonObject", node.id);
            return;
        };
        self.line(depth, "blue", "JsonObject", self.position(node));
        let JsonObject::Object(members) = value.as_ref();
        if let Some(members) = members {
            self.members(*members, depth + 1);
        } else {
            self.dim(depth + 1, "(empty)");
        }
    }

    fn members(&mut self, node: AstBox<JsonMembers>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonMembers", node.id);
            return;
        };
        self.line(depth, "blue", "JsonMembers", self.position(node));
        let JsonMembers::Members(members) = value.as_ref();
        for member in members {
            self.member(*member, depth + 1);
        }
    }

    fn member(&mut self, node: AstBox<JsonMember>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonMember", node.id);
            return;
        };
        let JsonMember::Member(key, value) = value.as_ref();
        let indent = "  ".repeat(depth);
        cprintln!(
            "{indent}<bold,yellow>JsonMember</> <yellow>{}</>{}",
            self.token(*key),
            self.position(node),
        );
        self.value(*value, depth + 1);
    }

    fn array(&mut self, node: AstBox<JsonArray>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonArray", node.id);
            return;
        };
        self.line(depth, "magenta", "JsonArray", self.position(node));
        let JsonArray::Array(elements) = value.as_ref();
        if let Some(elements) = elements {
            self.elements(*elements, depth + 1);
        } else {
            self.dim(depth + 1, "(empty)");
        }
    }

    fn elements(&mut self, node: AstBox<JsonElements>, depth: usize) {
        let Some(value) = self.snapshot.get(node) else {
            self.missing(depth, "JsonElements", node.id);
            return;
        };
        self.line(depth, "magenta", "JsonElements", self.position(node));
        let JsonElements::Elements(elements) = value.as_ref();
        for (index, value) in elements.iter().enumerate() {
            let indent = "  ".repeat(depth + 1);
            cprintln!("{indent}<dim>[{index}]</>");
            self.value(*value, depth + 2);
        }
    }

    fn scalar(&self, depth: usize, kind: &str, token: AstToken<JsonToken>) {
        let indent = "  ".repeat(depth);
        cprintln!(
            "{indent}<bold,green>{kind}</> <green>{}</>",
            self.token(token)
        );
    }

    fn token(&self, token: AstToken<JsonToken>) -> String {
        self.tokens
            .get(&token.id)
            .cloned()
            .unwrap_or_else(|| format!("<missing token#{}>", token.id))
    }

    fn position<T>(&self, node: AstBox<T>) -> String
    where
        T: Send + Sync + 'static,
    {
        node.span(self.snapshot).map_or_else(
            |_| String::new(),
            |span| {
                let line_col = span.to_line_col(self.snapshot.source());
                let start = line_col.start();
                let end = line_col.end();
                format!(" <dim>@{}:{}-{}:{}</>", start.0, start.1, end.0, end.1)
            },
        )
    }

    fn line(&self, depth: usize, color: &str, label: &str, position: String) {
        let indent = "  ".repeat(depth);
        match color {
            "blue" => cprintln!("{indent}<bold,blue>{label}</>{position}"),
            "magenta" => cprintln!("{indent}<bold,magenta>{label}</>{position}"),
            _ => cprintln!("{indent}<bold,cyan>{label}</>{position}"),
        }
    }

    fn dim(&self, depth: usize, value: &str) {
        let indent = "  ".repeat(depth);
        cprintln!("{indent}<dim>{value}</>");
    }

    fn error(&self, depth: usize, value: &str) {
        let indent = "  ".repeat(depth);
        cprintln!("{indent}<bold,red>Error</> <red>{value}</>");
    }

    fn missing(&self, depth: usize, kind: &str, id: usize) {
        self.error(depth, &format!("missing {kind} node#{id}"));
    }
}

fn token_texts(graph: &Graph, uri: fluent_uri::Uri<&'static str>) -> HashMap<usize, String> {
    let Some(source) = graph.read::<DocumentText>(uri) else {
        return HashMap::new();
    };
    let Some(order) = graph.read::<TokenOrder<JsonToken>>(uri) else {
        return HashMap::new();
    };

    order
        .iter()
        .filter_map(|key| {
            let token = graph.read::<TokenArtifact<JsonToken>>(key.clone())?;
            let end = token.start.checked_add(token.length)?;
            let text = source.get(token.start..end)?.to_owned();
            Some((token.id, text))
        })
        .collect()
}
