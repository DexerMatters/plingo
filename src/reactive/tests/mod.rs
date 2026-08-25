//! Plain-function reactive contract tests.
//!
//! The old shape-handle fixtures intentionally disappeared with the public
//! cutover. Behavioural coverage lives in the focused plain-function module.

mod kinds;
mod plain_functions;
mod sugar;
mod t1_consistency;
mod t2_glitch;
mod t3_determinism;
mod t4_min_delta;
mod t5_ownership;
mod t6_cycles_rollback;
