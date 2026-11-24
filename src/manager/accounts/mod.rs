//! The account rows and the operator's edits to them, and how each edit reaches the config file.
//!
//! These files share one subject: the per-account row ([`account_runtime`]) and every change an
//! operator makes to it (adding, replacing, benching, re-prioritising, naming the control
//! account), together with the two places those changes and refreshed tokens are written back
//! ([`write_through`] for one targeted key, [`token_persist`] for the token flush). Each edit
//! here ends in one of those two writers, which is why they sit beside the edits rather than
//! beside the refresh loop.

pub(super) mod account_runtime;
pub(super) mod add_account;
pub(super) mod control_account;
pub(super) mod disable_and_priority;
mod token_persist;
mod write_through;
