//! Parser-attached, demand-driven elaborator nodes.
//!
//! An elaborator role is an ordinary keyed node provider. Its rule context
//! publishes typed outputs and shared scope-graph relations transactionally.
//!
//! Module layout:
//! - role: the role contract and the multi-role registry
//! - error: framework and rule diagnostics
//! - task: task identity and published task views
//! - ops: rule operations — AST traversal and scope construction
//! - node: the graph node that runs one role
//! - cx: the transactional rule context
//! - rules: the rule-set builder

mod cx;
mod error;
mod node;
mod ops;
mod role;
mod rules;
mod task;
pub use ops::{AstWalk, AstWalkField};
pub use plingo_macros::ElaboratorRole;

pub use cx::ElaboratorCx;
pub use error::{ElaboratorDiagnostic, ElaboratorError, FrameworkDiagnostic};
pub use node::{ElaboratorKey, ElaboratorNode};
pub use role::{Elaboration, ElaboratorRole, Elaborators, NoDiagnostic, ScopeAccess, elaborators};
pub use rules::{RuleBuildError, RuleResult, RuleSet, rules};
pub use task::{Child, ElaboratorDiagnostics, ElaboratorOutput, ElaboratorTask};

#[cfg(test)]
#[path = "../../../tests/unit/component_semantic.rs"]
mod tests;
