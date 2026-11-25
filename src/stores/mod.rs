//! The stores layer, above records: state tcr keeps between requests and across restarts, and the
//! single-instance lock.

pub mod affinity;
pub mod bound_tokens;
pub(crate) mod cache_file;
pub mod fan;
pub mod file_log;
pub mod heat;
pub mod restart;
pub mod save_gate;
pub mod session_wire_persist;
pub mod singleton;
