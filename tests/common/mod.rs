//! Shared test infrastructure: JSON fixture grammar, deterministic fixture
//! generators, and the canonical oracle projection.

pub mod fixtures;
pub mod frozen;
pub mod json;
pub mod oracle;
/// Builds the canonical test URI for one document name.
pub fn uri(name: &str) -> fluent_uri::Uri<String> {
    plingo::utils::Span::new(format!("test://{name}"), 0, 0)
        .expect("test uri parses")
        .uri
}
