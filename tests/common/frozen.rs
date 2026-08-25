//! Frozen recovery policy constants and corpus seeds (plan §8.6, §11
//! Phase 0 step 5).
//!
//! These values are frozen by Phase 0 against the seeded corpus and are
//! consumed verbatim by the Phase 7 canonical search implementation. Any
//! later change is a benchmark-backed tuning change requiring a policy
//! version bump and full recovery gates (plan §13.1).

/// Recovery policy version recorded in baselines (plan §8.6).
pub const RECOVERY_POLICY_VERSION: u64 = 1;

// -- Primary search costs ---------------------------------------------------

/// Insertion cost applied to every terminal without an explicit override.
pub const INSERT_COST: u16 = 1;
/// Cost of deleting one real token.
pub const DELETE_COST: u16 = 1;
/// Cost of shifting one real token through the grammar error terminal.
pub const ERROR_SHIFT_COST: u16 = 1;

// -- Primary search budgets --------------------------------------------------

/// Consecutive real shifts required to validate a repair.
pub const VALIDATION_SHIFTS: u8 = 3;
/// Token horizon used when ranking equal-cost repairs.
pub const RANKING_HORIZON: usize = 250;
/// Maximum total repair cost considered by primary search.
pub const MAX_COST: u32 = 8;
/// Maximum expanded configurations before `BudgetExhausted`.
pub const MAX_EXPANDED: usize = 65_536;
/// Maximum live configurations before `BudgetExhausted`.
pub const MAX_LIVE: usize = 32_768;
/// Maximum successful candidates before falling back (hard budget).
pub const MAX_CANDIDATES: usize = 4_096;
/// Maximum frontier transition steps before `BudgetExhausted`.
pub const MAX_TRANSITION_STEPS: usize = 1_048_576;
/// Maximum cactus-stack nodes materialized during search.
pub const MAX_STACK_NODES: usize = 262_144;
/// Maximum repair-DAG nodes materialized during search.
pub const MAX_REPAIR_NODES: usize = 262_144;

// -- Regional fallback budgets ------------------------------------------------

/// Fallback pop cost per stack level.
pub const FALLBACK_POP_COST: u16 = 1;
/// Fallback cost per skipped real token.
pub const FALLBACK_SKIP_COST: u16 = 1;
pub const FALLBACK_MAX_EXPANDED: usize = 16_384;
pub const FALLBACK_MAX_TRANSITION_STEPS: usize = 262_144;
pub const FALLBACK_MAX_POP: usize = 32;
pub const FALLBACK_MAX_SKIP: usize = 250;

// -- Frozen corpus seeds -------------------------------------------------------

/// Seed for the JSON single-token mutation corpus.
pub const JSON_MUTATION_SEED_A: u64 = 0xC0FFEE;
/// Second JSON seed covering truncation-heavy mutations.
pub const JSON_MUTATION_SEED_B: u64 = 0xD15EA5E;
/// Seed for delimiter/pathological malformed-input corpora.
pub const JSON_MUTATION_SEED_PATHOLOGICAL: u64 = 0x0DDBA1;

/// Number of mutations per seeded corpus run.
pub const MUTATION_COUNT: usize = 24;

/// Fixture checksums recorded with every benchmark artifact (FNV-1a).
pub struct FixtureChecksums {
    pub json_512: u64,
    pub json_10k: u64,
    pub json_100k: u64,
    pub stlc_1k: u64,
}

impl FixtureChecksums {
    /// Computes checksums over the deterministic generators.
    pub fn current() -> Self {
        Self {
            json_512: super::fixtures::fnv1a(super::fixtures::json_array(512).as_bytes()),
            json_10k: super::fixtures::fnv1a(super::fixtures::json_array(10_000).as_bytes()),
            json_100k: super::fixtures::fnv1a(super::fixtures::json_array(100_000).as_bytes()),
            stlc_1k: super::fixtures::fnv1a(
                super::fixtures::stlc_program(180).as_bytes(),
            ),
        }
    }
}
