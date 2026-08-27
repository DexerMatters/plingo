//! Deterministic fixtures for the incremental behavior matrix (plan §10.2).
//!
//! All generators are pure functions of their size/seed: repeated runs and
//! fresh workspaces produce byte-identical documents. The 100k-token JSON
//! fixture is the reference scale for work gates; the STLC fixture covers a
//! grammar with lexer modes, nested parser state, and newline-delimited
//! declarations.
use rand::Rng;
use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom as _;

// ---------------------------------------------------------------------------
// JSON fixtures
// ---------------------------------------------------------------------------

/// A deterministic flat JSON array with `elements` numeric items.
///
/// Element values cycle through a fixed pattern so single-token edits have
/// predictable local grammar effect (number -> number, or number -> string
/// for token-class scenarios).
pub fn json_array(elements: usize) -> String {
    let mut items = Vec::with_capacity(elements);
    for index in 0..elements {
        match index % 5 {
            0 => items.push(index.to_string()),
            1 => items.push(format!("\"s{index}\"")),
            2 => items.push("true".to_string()),
            3 => items.push("null".to_string()),
            _ => items.push(format!("{index}.5")),
        }
    }
    format!("[{}]", items.join(","))
}

/// A deterministic nested JSON document with `depth` nesting levels and
/// `elements` total scalar leaves. Used for stack-growth probing.
pub fn json_nested(depth: usize) -> String {
    let mut text = String::new();
    for _ in 0..depth {
        text.push('[');
    }
    text.push_str("1");
    for _ in 0..depth {
        text.push(']');
    }
    text
}

/// A JSON object with a head, an `elements`-item items array, and a tail —
/// the shape the plan's probe used for head/middle/tail edits.
pub fn json_document(elements: usize) -> String {
    let items = (0..elements)
        .map(|value| format!("\"k{value}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"head":12345,"items":{{{items}}},"middle":23456,"tail":34567}}"#)
}

// ---------------------------------------------------------------------------
// STLC fixtures
// ---------------------------------------------------------------------------

/// A deterministic, grammar-valid STLC program with `terms` top-level
/// newline-separated value declarations.
pub fn stlc_program(terms: usize) -> String {
    let mut program = String::new();
    for index in 0..terms {
        program.push_str(&format!(
            "x{index} := if true then fun y{index} -> y{index} + {index} else fun y{index} -> y{index}\n"
        ));
    }
    program
}

// ---------------------------------------------------------------------------
// Seeded mutation corpus (plan §10.2 item 11, §13.2)
// ---------------------------------------------------------------------------

/// One seeded single-token mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Inserts `text` at byte `at`.
    Insert { at: usize, text: String },
    /// Deletes the byte range.
    Delete { start: usize, len: usize },
    /// Replaces the byte range with `text` of equal byte length.
    Substitute {
        start: usize,
        len: usize,
        text: String,
    },
    /// Truncates the document to `at` bytes.
    Truncate { at: usize },
}

/// Generates `count` deterministic single-token mutations over `text`.
///
/// The seed fixes both mutation positions and kinds; the same seed always
/// yields the same corpus (plan §13.2: empirical inputs are frozen, not
/// discovered).
pub fn seeded_mutations(text: &str, seed: u64, count: usize) -> Vec<Mutation> {
    let mut rng = StdRng::seed_from_u64(seed);
    let insert_pool = ["\"", ",", ":", "{", "}", "[", "]", "1", "x", " "];
    let substitute_pool = [",", ":", "\"", "}", "]"];
    let mut mutations = Vec::with_capacity(count);
    let mut current_len = text.len();
    for _ in 0..count {
        if current_len == 0 {
            break;
        }
        let kind = rng.random_range(0..4);
        let at = rng.random_range(0..current_len);
        let mutation = match kind {
            0 => {
                let text_choice = insert_pool[rng.random_range(0..insert_pool.len())];
                current_len += text_choice.len();
                Mutation::Insert {
                    at,
                    text: text_choice.to_string(),
                }
            }
            1 => {
                // Delete one token-ish byte run (1..=3 bytes, UTF-8 safe).
                let mut len = rng.random_range(1..=3).min(current_len - at);
                while len > 0 && !text.is_char_boundary(at + len) {
                    len -= 1;
                }
                if len == 0 {
                    Mutation::Insert {
                        at,
                        text: "1".into(),
                    }
                } else {
                    current_len -= len;
                    Mutation::Delete { start: at, len }
                }
            }
            2 => {
                let mut len = rng.random_range(1..=2).min(current_len - at);
                while len > 0 && !text.is_char_boundary(at + len) {
                    len -= 1;
                }
                if len == 0 {
                    Mutation::Insert {
                        at,
                        text: "1".into(),
                    }
                } else {
                    let replacement = substitute_pool[rng.random_range(0..substitute_pool.len())];
                    current_len = current_len - len + replacement.len();
                    Mutation::Substitute {
                        start: at,
                        len,
                        text: replacement.to_string(),
                    }
                }
            }
            _ => {
                current_len = at;
                Mutation::Truncate { at }
            }
        };
        mutations.push(mutation);
    }
    mutations
}

/// Converts a mutation into workspace edits against one URI.
pub fn mutation_edits(
    uri: &fluent_uri::Uri<String>,
    text: &str,
    mutation: &Mutation,
) -> Vec<plingo::framework::source::SourceEdit> {
    use plingo::framework::source::SourceEdit;
    use plingo::utils::Span;
    match mutation {
        Mutation::Insert { at, text: value } => vec![SourceEdit::Insert {
            key: Span::point_uri(uri.clone(), *at).expect("insert point"),
            value: value.clone(),
        }],
        Mutation::Delete { start, len } => vec![SourceEdit::Delete {
            key: Span::new_uri(uri.clone(), *start, start + len).expect("delete range"),
        }],
        Mutation::Substitute { start, len, text } => vec![
            SourceEdit::Delete {
                key: Span::new_uri(uri.clone(), *start, start + len).expect("substitute range"),
            },
            SourceEdit::Insert {
                key: Span::point_uri(uri.clone(), *start).expect("substitute point"),
                value: text.clone(),
            },
        ],
        Mutation::Truncate { at } => vec![SourceEdit::Delete {
            key: Span::new_uri(uri.clone(), *at, text.len()).expect("truncate range"),
        }],
    }
}

/// Applies a mutation to a plain string (the authoritative mirror).
pub fn apply_mutation(text: &mut String, mutation: &Mutation) {
    match mutation {
        Mutation::Insert { at, text: value } => text.insert_str(*at, value),
        Mutation::Delete { start, len } => {
            text.replace_range(*start..start + len, "");
        }
        Mutation::Substitute {
            start,
            len,
            text: replacement,
        } => {
            let replacement = replacement.clone();
            text.replace_range(*start..start + len, &replacement);
        }
        Mutation::Truncate { at } => text.truncate(*at),
    }
}

// ---------------------------------------------------------------------------
// Persistent-error fixtures
// ---------------------------------------------------------------------------

/// A JSON document with two independent syntax errors (missing commas and a
/// truncated container) far apart in the byte layout. Edits near either
/// error must not re-run the other's recovery (plan §10.3).
pub fn json_two_errors() -> String {
    let filler = (0..64)
        .map(|value| format!("{value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"a":[{filler}],"b":{{"x":1 "y":2}},"c":[{filler}],"d":{{"p":true "q":false}}}}"#)
}

/// A JSON document whose tail container is unterminated: every edit before
/// the tail must replay through EOF (grammar-dependent convergence case).
pub fn json_unterminated_tail(elements: usize) -> String {
    let items = (0..elements)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"items":[{items},"tail":"#)
}

// ---------------------------------------------------------------------------
// Checksums (fixture identity in benchmark artifacts)
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit checksum; deterministic across platforms.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
