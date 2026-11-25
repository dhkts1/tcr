//! Banked-reset spends: the operator's Codex and Claude claims, and the tick that spends an
//! expiring one by itself. `auto` is the only part other modules run; the claims are reached
//! through the `Manager` methods and the response types `crate::manager` re-exports.

pub mod auto;
pub(in crate::manager) mod claude;
pub(in crate::manager) mod codex;
