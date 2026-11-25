//! Keeping each account's picture current against its provider: usage, probes, tokens, warmth.
//!
//! What the manager knows about an account goes stale on its own: the quota windows move, the
//! access token expires, the plan changes, a quiet account's window lapses. [`usage`] folds in
//! what each response says, [`probing`] asks when no response has, [`refresh`] renews the OAuth
//! token, and [`warm`] sends the keep-warm request. They sit together because each one keeps a
//! fact on the account row current, and the selector reads those facts without fetching any.

pub(super) mod probing;
pub(super) mod refresh;
pub mod schedule;
pub(super) mod usage;
mod warm;
