//! The Mac's System Management Controller, read through AppleSMC without root:
//! fan speeds, fan modes and temperature sensors.
//!
//! Reading needs no privilege. Writing does: a non-root write came back
//! `kIOReturnNotPrivileged` (0xe00002c1) on 2026-10-18, so
//! [`SmcConnection::write`] and [`FanWriter`], the sequence that takes fans
//! from macOS and gives them back, exist for a root helper built on this
//! crate, and nothing in tcr itself writes a fan.
//!
//! # Where this came from
//!
//! A port of TcrBar's Swift `TcrSMC` library and its C shim `CSMC` (tcr#554),
//! with every fix that library's review made. The protocol follows Stats
//! (exelban/stats, MIT, `SMC/smc.swift`): selector 2 of the AppleSMC user
//! client, a one-byte command in `data8`, and an 80-byte parameter struct. The
//! places that follow Stats say so.
//!
//! # Why a crate of its own
//!
//! The main crate is `#![forbid(unsafe_code)]`, and talking to IOKit is
//! `unsafe` by construction. As with `tcr-fdpass`, the unsafe lives here, in
//! one file, and the proxy keeps its guarantee.
//!
//! # Linux
//!
//! CI builds the workspace on ubuntu. Everything that touches AppleSMC is
//! `#[cfg(target_os = "macos")]`; elsewhere [`HeatSource::read`] answers
//! `None`, the same answer a Mac VM with no SMC gets.
//! The codecs and the reader's rules are plain Rust and are tested everywhere.

mod codec;
mod fan;
#[cfg(target_os = "macos")]
mod macos;
mod reader;
mod source;
mod writer;

pub use codec::{
    FourCc, KeyAccess, KeySource, MAX_VALUE_SIZE, SmcError, SmcKeyInfo, SmcValue, check_write,
};
pub use fan::{FanIndex, FanMode, MAX_FANS, fan_key};
#[cfg(target_os = "macos")]
pub use macos::SmcConnection;
pub use reader::{
    Clock, ENUMERATION_RETRY_DELAY, FanReading, HeatReader, HeatReading, PLAUSIBLE_CELSIUS, Report,
    plausible,
};
#[cfg(not(target_os = "macos"))]
pub use source::NoSmc;
pub use source::{HeatSource, Opener, PlatformSmc};
pub use writer::{
    Entered, FanError, FanLimits, FanWriter, HAND_BACK_GAP, HAND_BACK_SETTLE_GAP,
    HAND_BACK_SETTLE_POLLS, HAND_BACK_TRIES, HandBackIncomplete, MODE_RETRIES, MODE_RETRY_GAP,
    SetManualFailed, Sleeper, UNLOCK_SETTLE,
};

#[cfg(test)]
mod tests;
