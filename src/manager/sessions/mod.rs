//! What the manager keeps per client session, and how it survives a restart.
//!
//! The session-affinity pins ([`pins`]), the account-bound tokens each conversation carries
//! ([`bound_tokens`]), and the Sessions/Tools cache of wire sessions ([`wire_sessions`]) are
//! each a live map keyed by session plus its debounced snapshot to disk and restore at boot.
//! [`capped_map`] is the size cap and eviction rule those session-keyed maps share.

pub(super) mod bound_tokens;
pub(super) mod capped_map;
mod pins;
mod wire_sessions;
