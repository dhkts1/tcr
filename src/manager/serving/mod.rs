//! The state one request touches while it is being served, after selection chose its account.
//!
//! The account's upstream HTTP client ([`serving_client`]), the guard that counts the request in
//! flight ([`in_flight`]), the per-organization rate throttle it waits on ([`throttle`]), the
//! counts of Codex requests refused or relayed unpooled ([`codex_traffic`]), and what the client
//! is told when no account can serve it ([`exhaustion_hint`]). They sit together because the
//! proxy's request path calls each of them and the selector's code calls none of them.

pub(super) mod codex_traffic;
pub(super) mod exhaustion_hint;
pub(super) mod in_flight;
pub(super) mod serving_client;
mod throttle;
