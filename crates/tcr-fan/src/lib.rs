//! Fan control's shared half: the root fan helper's XPC contract, both ends'
//! code-signing identities, the rule that says whose the fans are, and root's
//! own logic, plus the macOS-only XPC bindings and the Macs Fan Control probe.
//!
//! Two binaries link this crate. `tcr-fan-helper` is the root daemon: it listens
//! on its Mach service, accepts only the proxy, and runs [`root::Root`]. `tcr`
//! dials it (today only `tcr fan probe`, which sends the version op). Each end
//! takes its wire codes, its requirement strings and its holder rule from here,
//! so the two cannot drift.
//!
//! Everything except [`xpc`] and [`macs`] is plain Rust, built and tested on
//! Linux by CI. Those two are `#[cfg(target_os = "macos")]` and hold every
//! `unsafe` block in the crate.

pub mod holder;
pub mod identity;
pub mod root;
pub mod wire;

#[cfg(target_os = "macos")]
pub mod macs;
#[cfg(target_os = "macos")]
pub mod xpc;

#[cfg(test)]
mod fake;
