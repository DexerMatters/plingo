//! Deterministic matrix notes for the plain-function reactive fixtures.
//!
//! The executable matrix is covered by the T1–T6 modules and
//! `plain_functions`. Each case uses `Engine::plan`, `Engine::run`, and one
//! write-only `Engine::command` closure; no static producer declaration is
//! required because view registration is inferred from effects.
