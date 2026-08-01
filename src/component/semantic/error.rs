//! Diagnostics produced by the elaborator framework and by rules.

use std::fmt;

use crate::component::parse::{AstKey, data::ProductId};

#[derive(Clone, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum FrameworkDiagnostic {
    #[error("the parser produced no typed interpretation")]
    MissingInterpretation,
    #[error("the parser produced {0} interpretations; the role must choose one")]
    AmbiguousInterpretation(usize),
    #[error("the role chose product {0}, which is not an accepted interpretation")]
    UnknownInterpretation(ProductId),
    #[error("this task already chose a different interpretation")]
    InterpretationAlreadyChosen,
    #[error("semantic AST artifact {0:?} is unavailable")]
    MissingAst(AstKey),
    #[error("semantic token {0} is unavailable")]
    MissingToken(usize),
    #[error("scope for {0:?} is unavailable")]
    MissingScope(AstKey),
    #[error("elaborator completed without selecting an interpretation")]
    InterpretationNotSelected,
    #[error("{0}")]
    Message(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ElaboratorError<D: fmt::Debug> {
    #[error("elaborator framework: {0}")]
    Framework(FrameworkDiagnostic),
    #[error("elaborator rule: {0:?}")]
    Rule(D),
}
impl<D: fmt::Debug> From<FrameworkDiagnostic> for ElaboratorError<D> {
    fn from(value: FrameworkDiagnostic) -> Self {
        Self::Framework(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ElaboratorDiagnostic<D> {
    Framework(FrameworkDiagnostic),
    Rule(D),
}
