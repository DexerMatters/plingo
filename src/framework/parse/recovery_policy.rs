//! Recovery policy, segments, and typed products (plan §8.6–§8.8).

use std::sync::Arc;

/// Frozen recovery policy constants (plan §8.6; §11 Phase 0 step 5).
/// Any change is a benchmark-backed tuning change requiring a version bump.
#[derive(Clone, Debug)]
pub struct ParserRecoveryPolicy {
    pub enabled: bool,
    pub insert_costs: Arc<[u16]>,
    pub delete_cost: u16,
    pub error_shift_cost: u16,
    pub validation_shifts: u8,
    pub ranking_horizon: usize,
    pub max_cost: u32,
    pub max_expanded: usize,
    pub max_live: usize,
    pub max_candidates: usize,
    pub max_transition_steps: usize,
    pub max_stack_nodes: usize,
    pub max_repair_nodes: usize,
}

impl Default for ParserRecoveryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            insert_costs: Arc::from(vec![1u16]),
            delete_cost: 1,
            error_shift_cost: 1,
            validation_shifts: 3,
            ranking_horizon: 250,
            max_cost: 8,
            max_expanded: 65_536,
            max_live: 32_768,
            max_candidates: 4_096,
            max_transition_steps: 1_048_576,
            max_stack_nodes: 262_144,
            max_repair_nodes: 262_144,
        }
    }
}

/// Regional fallback budgets (plan §8.6).
#[derive(Clone, Debug)]
pub struct RegionalFallbackPolicy {
    pub pop_cost: u16,
    pub skip_cost: u16,
    pub max_expanded: usize,
    pub max_transition_steps: usize,
    pub max_pop: usize,
    pub max_skip: usize,
}

impl Default for RegionalFallbackPolicy {
    fn default() -> Self {
        Self {
            pop_cost: 1,
            skip_cost: 1,
            max_expanded: 16_384,
            max_transition_steps: 262_144,
            max_pop: 32,
            max_skip: 250,
        }
    }
}

/// A missing token repair (plan §8.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MissingToken {
    pub terminal_id: usize,
    pub anchor: crate::framework::parse::delta::TokenAnchor,
    pub repair_ordinal: u32,
}

/// A skipped real-token repair (plan §8.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SkippedToken {
    pub occurrence: u64,
    pub repair_ordinal: u32,
}

/// An explicit error region spanning repaired input (plan §8.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorRegion {
    pub segment_id: u64,
    pub start: crate::framework::parse::delta::TokenAnchor,
    pub end: crate::framework::parse::delta::TokenAnchor,
    pub missing: Arc<[MissingToken]>,
    pub skipped: Arc<[SkippedToken]>,
}

/// Typed recovery products carried upward to the nearest `#[parse_err]` handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryProduct {
    Missing(MissingToken),
    Skipped(SkippedToken),
    Error(ErrorRegion),
}
