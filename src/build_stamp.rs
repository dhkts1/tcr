//! The build stamp `build.rs` writes, read here in the binary and nowhere else.
//!
//! `build.rs` re-runs whenever HEAD moves and stamps the wall clock, so whatever
//! reads these values is recompiled on every commit. That is this one small
//! module of the binary, not the library: the library (`crates/tcr-lib`) takes
//! the stamp as a value through [`tcr::build_info::install`], which `main` calls
//! before anything else runs. See `tcr::build_info` for what the stamp is for.

use tcr::build_info::BuildInfo;

/// Short commit sha this binary was compiled from, or `unknown`.
pub(crate) const SHA: &str = env!("TCR_BUILD_SHA");

/// Whether TRACKED files carried uncommitted changes when `build.rs` last ran:
/// `"true"`, `"false"` or `unknown`. Stale by design in one direction: see the
/// limitation section in `build.rs`, and `tcr::build_info::read_checkout_state`
/// for the live answer.
pub(crate) const DIRTY: &str = env!("TCR_BUILD_DIRTY");

/// When `build.rs` last stamped this binary (UTC), or `unknown`.
pub(crate) const BUILT_AT: &str = env!("TCR_BUILD_TIME");

/// This binary's stamp, as the library takes it.
pub(crate) fn build_info() -> BuildInfo {
    BuildInfo::from_stamp(SHA, DIRTY, BUILT_AT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build.rs` must emit all three variables unconditionally (`env!` would
    /// have failed the compile otherwise), so this asserts the values are also
    /// non-empty and, for the tri-state, one of the three legal tokens.
    #[test]
    fn the_build_stamped_every_field() {
        assert!(!SHA.is_empty(), "TCR_BUILD_SHA is stamped");
        assert!(!BUILT_AT.is_empty(), "TCR_BUILD_TIME is stamped");
        std::assert_matches!(
            DIRTY,
            "true" | "false" | "unknown",
            "TCR_BUILD_DIRTY is a tri-state token"
        );
        // Whatever git said, the stamp round-trips into a BuildInfo.
        let info = build_info();
        assert_eq!(info.sha, SHA);
        assert_eq!(info.built_at, BUILT_AT);
        assert_eq!(info.dirty_str(), DIRTY);
    }
}
