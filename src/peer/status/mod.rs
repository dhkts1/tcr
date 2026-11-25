//! The peer documents tcr prints and serves, each derived read-only from the peers file and
//! the peer state file: the `tcr status` peers block ([`rows`]), the `tcr peer ls --json`
//! document ([`ls`]) and the peer graph ([`graph`]).

pub mod graph;
pub mod ls;
pub mod rows;
