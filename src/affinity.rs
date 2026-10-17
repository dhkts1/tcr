//! Durable session→account pins: the on-disk half of `Manager::affinity`.
//!
//! The pin map is what keeps one client session on one account so Anthropic's
//! prompt cache stays warm for it. It has always been memory-only, so every
//! restart cold-started every live session's prefix — the most expensive event
//! in this system, and it happened 50 times in 79.6 hours. This module makes the
//! map survive a bounce.
//!
//! Three properties the file has to have, each of which is a way the naive
//! version silently does the wrong thing:
//!
//! 1. **It stores an account IDENTITY, never the index.** The in-memory map is
//!    keyed on `usize` positions into the account list. A position is only
//!    meaningful against the exact list it was taken from: add, remove or reorder
//!    an account between boots and a restored index points at a DIFFERENT
//!    account, which is strictly worse than having no pin — the session is routed
//!    somewhere cold while the proxy believes it is warm, and the migration logic
//!    sees a settled pin and leaves it alone. So each pin stores the identity
//!    fields [`crate::identity`] already treats as an account's identity
//!    (`account_uuid` + org, falling back to `name`, within the account's
//!    provider) and is resolved back to an index at load through
//!    [`crate::identity::resolve`]. Anything that does not
//!    resolve to EXACTLY one account is DROPPED — a [`Resolved::Many`] tie is
//!    refused here for the same reason `save_disabled` refuses one: a guess that
//!    lands on the wrong record is the failure being prevented, not a lesser
//!    version of success.
//! 2. **Every pin carries a timestamp and expires at load.** A restored pin is
//!    worth something only while that account's prompt cache is still warm; see
//!    [`PIN_TTL_MS`] for the number and the reasoning behind it.
//! 3. **It is written incrementally, atomically, and can never take the proxy
//!    down.** Shutdown-only persistence would miss exactly the case this exists
//!    to survive: `--replace` follows SIGTERM with SIGKILL, and no shutdown hook
//!    runs for a SIGKILL. The write goes through [`crate::cache_file::write_cache`]
//!    (same-dir temp + `rename`), so a crash mid-write leaves the previous file
//!    intact rather than a truncated one. And every read failure — missing,
//!    truncated, corrupt, wrong version — degrades to "no pins" plus a log line.
//!    This is a cache; a cache that can panic the process is a downgrade on the
//!    memory-only version it replaces.
//!
//! The file is NOT the credential config, deliberately: `~/.config/tcr.json`
//! holds live OAuth tokens behind a delicate read-modify-write merge, and a
//! high-frequency cache write has no business anywhere near it. Pins live under
//! the cache dir ([`default_path`]) and are written `0600` because they name
//! accounts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache_file::{CacheRead, Layout, read_cache, write_cache};
use crate::config::{Account, ConfigError};
use crate::identity::{self, Resolved};
use crate::provider::Provider;

/// How stale a pin may be at load and still be restored: **15 minutes** since
/// its last touch (a pin's stamp is the time of the last request served on it).
///
/// Anthropic's default `cache_control` TTL is 5 minutes, refreshed on every
/// cache read — so a session that served a request within the last 5 minutes has
/// a warm prefix, and one idle far longer does not under any setting except the
/// 1-hour extended TTL. The window has to cover last-request → first-request-
/// after-the-restart, which includes the restart itself: a `--replace` upgrade is
/// a SIGTERM, a SIGKILL, a boot and often a `cargo build --release` in front of
/// it, so a 5-minute window would expire pins during the very event it exists to
/// survive. 15 minutes is that 5-minute warm window plus room for the bounce,
/// and still well under the 1-hour ceiling past which no cache can be warm.
///
/// Restoring a pin whose cache HAS gone cold is not free but it is cheap: the
/// session simply prefers an account it would otherwise have been assigned by
/// rotation, and the normal eligibility, migration and re-pin paths all still
/// apply on the next request. The expensive error is the mis-resolution in
/// property (1) above, not a slightly stale preference.
pub const PIN_TTL_MS: i64 = 15 * 60 * 1000;

/// Restore window for a pin whose client explicitly asked Anthropic's cache for
/// the extended `"ttl":"1h"` window (see [`StoredPin::extended_ttl`]), measured
/// 2026-08-21 against 52,951 real `request usage` records: gaps of 15-60 minutes
/// hit the cache 73-88% of the time and the hit rate does not fall off a cliff
/// until past 60 minutes (12%). 55 minutes is that 60-minute warm window minus
/// room for the restart itself, mirroring how [`PIN_TTL_MS`]'s 15 minutes is
/// derived from the 5-minute default window plus the same margin.
pub const EXTENDED_PIN_TTL_MS: i64 = 55 * 60 * 1000;

/// Maximum pins written to disk, mirroring the in-memory `AFFINITY_CAP`. On
/// overflow the freshest are kept, matching the in-memory LRU-by-last-touch
/// eviction, so a restore can never produce a map larger than the process would
/// have held anyway.
pub const PIN_CAP: usize = 1024;

/// Bumped whenever the meaning of a field changes. A file written by a different
/// version is ignored wholesale rather than half-read.
const FORMAT_VERSION: u32 = 1;

/// One persisted pin: the session key, the identity of the account it points at,
/// and when it was last touched.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredPin {
    /// The stable session key — `stable_hash` over the client's durable identity
    /// (see [`crate::proxy`]). Deterministic across processes for a given build,
    /// which is what makes any of this work; a `std` hash change between Rust
    /// releases would simply make the keys stop matching, and unmatched keys are
    /// inert (they pin sessions that will never ask again, and age out).
    pub key: u64,
    /// Display name of the pinned account — also the identity fallback for
    /// records with no `account_uuid`, exactly as [`identity::same_identity`]
    /// falls back.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    /// Epoch ms of the last request served on this pin. Compared against
    /// [`PIN_TTL_MS`] (or [`EXTENDED_PIN_TTL_MS`] below) at load.
    pub touched_at_ms: i64,
    /// Whether the client's own request carried Anthropic's extended
    /// `"cache_control": {"ttl": "1h"}` — see [`crate::cache_ttl`]. When `true`,
    /// [`load`] compares this pin's age against [`EXTENDED_PIN_TTL_MS`] instead
    /// of the caller's `ttl_ms`. A plain `bool` rather than a raw millisecond
    /// value: the only two windows Anthropic's cache actually offers are the
    /// 5-minute default and the 1-hour extended tier, so there is nothing an
    /// arbitrary number would express that this doesn't, and a bad file can only
    /// ever produce `false` (`#[serde(default)]`) — never a longer window than
    /// the caller intended.
    #[serde(default)]
    pub extended_ttl: bool,
    /// The provider of the pinned account: part of its identity, since
    /// [`identity::same_identity`] never matches records of different providers.
    /// Written only for a Codex account, the rule `config::Account::provider`
    /// follows, so a file written for an Anthropic-only fleet is exactly what it
    /// was. Absent reads as Anthropic, which is right for every pin written before
    /// this field existed: every account was Anthropic then. Without it a Codex
    /// account's pin would read back as Anthropic after a restart and match no row
    /// of its own provider.
    #[serde(default, skip_serializing_if = "Provider::is_anthropic")]
    pub provider: Provider,
}

impl StoredPin {
    /// The identity probe this pin resolves through, in the shape
    /// [`identity::resolve`] compares against stored records.
    fn probe(&self) -> Account {
        identity::probe(
            self.provider,
            &self.name,
            self.account_uuid.clone(),
            self.org_uuid.clone(),
            self.org_name.clone(),
        )
    }
}

/// The file itself.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PinFile {
    pub version: u32,
    pub saved_at_ms: i64,
    pub pins: Vec<StoredPin>,
}

/// [`PinFile`]'s write side: the same JSON, built from references to the caller's pins so a
/// save sorts and writes them without copying each one first.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PinFileRef<'a> {
    version: u32,
    saved_at_ms: i64,
    pins: Vec<&'a StoredPin>,
}

/// What a [`load`] made of the file. Every field except `pins` exists so the
/// caller can say out loud what it threw away — a silent drop here is
/// indistinguishable from the feature not working.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Restored pins, in the in-memory map's own shape: key → (account index,
    /// last-touch ms).
    pub pins: HashMap<u64, (usize, i64)>,
    /// The subset of `pins` whose [`StoredPin::extended_ttl`] was `true` — so
    /// the caller can re-seed its own extended-TTL bookkeeping across a restart
    /// rather than losing it until that session's next request.
    pub extended: std::collections::HashSet<u64>,
    /// Dropped because their last touch was older than the TTL.
    pub expired: usize,
    /// Dropped because no live account carries that identity (removed, renamed).
    pub unresolved: usize,
    /// Dropped because two or more live accounts carry that identity and the tie
    /// could not be broken. Refused, never guessed.
    pub ambiguous: usize,
    /// Set when the file was ignored ENTIRELY — unreadable, corrupt, truncated,
    /// or a version this build does not understand. `None` on a clean read and on
    /// a simple "no file yet".
    pub degraded: Option<String>,
}

/// Default pin-file path: `session-affinity.json` in the cache directory
/// ([`crate::paths::cache_dir`]: `$XDG_CACHE_HOME/tcr/`, else
/// `$HOME/.cache/tcr/`).
///
/// A cache dir, not `~/.config`: this is regenerable state written on a timer,
/// and the config file next door holds live OAuth credentials behind a merge
/// path that must not acquire a second high-frequency writer.
pub fn default_path() -> PathBuf {
    crate::paths::cache_dir().affinity_file()
}

/// The pins [`save`] writes, in the order it writes them: at most [`PIN_CAP`], the freshest,
/// sorted by key.
fn written_pins(pins: &[StoredPin]) -> Vec<&StoredPin> {
    let mut pins: Vec<&StoredPin> = pins.iter().collect();
    if pins.len() > PIN_CAP {
        pins.sort_by_key(|p| std::cmp::Reverse(p.touched_at_ms));
        pins.truncate(PIN_CAP);
    }
    // Stable order keeps the file diff-friendly and the write byte-identical when
    // nothing changed.
    pins.sort_by_key(|p| p.key);
    pins
}

/// The two hashes [`crate::save_gate::SaveGate`] compares for a save of `pins`: the shape
/// (each session's account and cache window, everything but the touch time) and the content
/// (everything [`save`] writes but its `savedAtMs`).
pub fn save_hashes(pins: &[StoredPin]) -> Result<(u64, u64), serde_json::Error> {
    let pins = written_pins(pins);
    let shape: Vec<PinShape<'_>> = pins
        .iter()
        .map(|p| PinShape {
            key: p.key,
            name: &p.name,
            account_uuid: p.account_uuid.as_deref(),
            org_uuid: p.org_uuid.as_deref(),
            org_name: p.org_name.as_deref(),
            extended_ttl: p.extended_ttl,
            provider: p.provider,
        })
        .collect();
    Ok((
        crate::save_gate::content_hash(&shape)?,
        crate::save_gate::content_hash(&pins)?,
    ))
}

/// A [`StoredPin`] without its touch time: what [`save_hashes`] calls the shape. A field added
/// to the pin and not here is still written, within `AFFINITY_TOUCH_REFRESH` of its change.
#[derive(Serialize)]
struct PinShape<'a> {
    key: u64,
    name: &'a str,
    account_uuid: Option<&'a str>,
    org_uuid: Option<&'a str>,
    org_name: Option<&'a str>,
    extended_ttl: bool,
    provider: Provider,
}

/// Write `pins` to `path` atomically at `0600`, keeping at most [`PIN_CAP`] of
/// them (freshest first).
///
/// Returns how many were written. The caller decides what a failure means; it is
/// never fatal here.
pub fn save(path: &Path, pins: &[StoredPin], now_ms: i64) -> Result<usize, ConfigError> {
    let pins = written_pins(pins);
    let count = pins.len();
    let file = PinFileRef {
        version: FORMAT_VERSION,
        saved_at_ms: now_ms,
        pins,
    };
    write_cache(path, &file, Layout::Pretty)?;
    Ok(count)
}

/// Carry the pin file's [`StoredPin::name`]s across an account rename, so warm
/// sessions survive `config::load`'s duplicate-name migration.
///
/// Returns how many pins were rewritten. A pin whose name matches no rename is
/// left exactly as it was, and a file that cannot be read or parsed is left
/// alone entirely — it is a cache, so the worst case is the cold prompt prefix
/// those sessions would have paid anyway. That is also why the whole thing is
/// infallible: a rename is not worth failing a boot over.
///
/// Only pins with no `account_uuid` actually NEED this — [`load`] resolves the
/// rest through [`identity::resolve`], which compares uuid and org and never
/// looks at the name. Rewriting all of them keeps the file honest about what it
/// points at, which is what the next person to read it will assume.
pub fn rename_pins(path: &Path, renames: &[(String, String)]) -> usize {
    if renames.is_empty() {
        return 0;
    }
    // A missing, unreadable, corrupt or foreign file is left for `load` to report.
    let CacheRead::Read(mut file) = read_cache::<PinFile>(path, FORMAT_VERSION) else {
        return 0;
    };
    let mut rewritten = 0;
    for pin in &mut file.pins {
        if let Some((_, to)) = renames.iter().find(|(from, _)| *from == pin.name) {
            pin.name.clone_from(to);
            rewritten += 1;
        }
    }
    if rewritten > 0
        && let Err(err) = write_cache(path, &file, Layout::Pretty)
    {
        tracing::warn!(error = %err, "could not carry session-affinity pins across the account rename");
        return 0;
    }
    rewritten
}

/// Read `path` and resolve each pin against `accounts`, dropping anything stale
/// or not resolvable to exactly one account.
///
/// Infallible by construction — every failure mode returns an empty map with
/// `degraded` set. See the module docs for why this must never propagate an
/// error, let alone panic.
pub fn load(path: &Path, accounts: &[Account], now_ms: i64, ttl_ms: i64) -> LoadReport {
    let file: PinFile = match read_cache(path, FORMAT_VERSION) {
        CacheRead::Read(file) => file,
        CacheRead::Missing => return LoadReport::default(),
        CacheRead::Degraded(reason) => {
            return LoadReport {
                degraded: Some(reason),
                ..LoadReport::default()
            };
        }
    };

    let mut report = LoadReport::default();
    for pin in file.pins {
        // Per-pin TTL: a client that asked for the extended 1h cache window gets
        // the wider restore window; everything else (including a malformed or
        // unrecognised recording) falls back to the caller's `ttl_ms` — never
        // wider than what was actually requested.
        let effective_ttl_ms = if pin.extended_ttl {
            EXTENDED_PIN_TTL_MS
        } else {
            ttl_ms
        };
        if now_ms.saturating_sub(pin.touched_at_ms) > effective_ttl_ms {
            report.expired += 1;
            continue;
        }
        match identity::resolve(accounts.iter().enumerate(), &pin.probe()) {
            Resolved::One(index) => {
                report.pins.insert(pin.key, (index, pin.touched_at_ms));
                if pin.extended_ttl {
                    report.extended.insert(pin.key);
                }
            }
            Resolved::None => report.unresolved += 1,
            Resolved::Many => report.ambiguous += 1,
        }
        if report.pins.len() >= PIN_CAP {
            break;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, uuid: Option<&str>, org: Option<&str>) -> Account {
        identity::probe(
            Provider::Anthropic,
            name,
            uuid.map(str::to_string),
            org.map(str::to_string),
            None,
        )
    }

    fn pin(key: u64, name: &str, uuid: Option<&str>, org: Option<&str>, touched: i64) -> StoredPin {
        pin_with_ttl(key, name, uuid, org, touched, false)
    }

    fn pin_with_ttl(
        key: u64,
        name: &str,
        uuid: Option<&str>,
        org: Option<&str>,
        touched: i64,
        extended_ttl: bool,
    ) -> StoredPin {
        StoredPin {
            key,
            name: name.to_string(),
            account_uuid: uuid.map(str::to_string),
            org_uuid: org.map(str::to_string),
            org_name: None,
            touched_at_ms: touched,
            extended_ttl,
            provider: Provider::Anthropic,
        }
    }

    fn tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-affinity-test-{}-{label}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("session-affinity.json")
    }

    /// The whole point: pins written by one process are read back by the next and
    /// land on the SAME accounts, resolved by identity rather than by position.
    #[test]
    fn round_trip_restores_the_same_accounts() {
        let path = tmp("round-trip");
        let accounts = [
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
        ];
        let now = 1_000_000;
        let written = save(
            &path,
            &[
                pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now),
                pin(22, "b@example.com", Some("uuid-b"), Some("org-1"), now),
            ],
            now,
        )
        .expect("save");
        assert_eq!(written, 2);

        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(report.pins.get(&11), Some(&(0, now)));
        assert_eq!(report.pins.get(&22), Some(&(1, now)));
        assert_eq!(
            (report.expired, report.unresolved, report.ambiguous),
            (0, 0, 0)
        );
    }

    /// The index trap. The saved pins are positions 0 and 1 in a list that is
    /// reordered, and has an account inserted in front of it, before the next
    /// boot. A position-based restore would silently point session 11 at the
    /// wrong account; identity resolution follows the account.
    #[test]
    fn reordered_accounts_resolve_by_identity_not_position() {
        let path = tmp("reorder");
        let now = 2_000_000;
        save(
            &path,
            &[
                pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now),
                pin(22, "b@example.com", Some("uuid-b"), Some("org-1"), now),
            ],
            now,
        )
        .expect("save");

        // Next boot: a new account first, and a and b swapped.
        let accounts = [
            acct("c@example.com", Some("uuid-c"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(
            report.pins.get(&11),
            Some(&(2, now)),
            "session 11 must follow account a to its new position, not stay at 0"
        );
        assert_eq!(report.pins.get(&22), Some(&(1, now)));
    }

    /// A removed account drops its pins rather than mis-resolving them.
    #[test]
    fn removed_account_drops_its_pin() {
        let path = tmp("removed");
        let now = 3_000_000;
        save(
            &path,
            &[
                pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now),
                pin(22, "b@example.com", Some("uuid-b"), Some("org-1"), now),
            ],
            now,
        )
        .expect("save");

        let accounts = [acct("b@example.com", Some("uuid-b"), Some("org-1"))];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.unresolved, 1);
        assert!(
            !report.pins.contains_key(&11),
            "a is gone; its pin must not survive"
        );
        assert_eq!(report.pins.get(&22), Some(&(0, now)));
    }

    /// An unbreakable identity tie is refused, not guessed. Two entries sharing a
    /// name with no UUID are genuinely indistinguishable (see `identity::resolve`).
    #[test]
    fn ambiguous_identity_is_dropped_never_guessed() {
        let path = tmp("ambiguous");
        let now = 4_000_000;
        save(&path, &[pin(11, "twin@example.com", None, None, now)], now).expect("save");

        let accounts = [
            acct("twin@example.com", None, None),
            acct("twin@example.com", None, None),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.ambiguous, 1);
        assert!(
            report.pins.is_empty(),
            "a tie routes nothing rather than the wrong thing"
        );
    }

    /// Stale pins are dropped at load: a warm-cache bet is only worth making
    /// while the cache can still be warm.
    #[test]
    fn expiry_drops_stale_pins_and_keeps_fresh_ones() {
        let path = tmp("expiry");
        let now = 10 * PIN_TTL_MS;
        save(
            &path,
            &[
                pin(
                    11,
                    "a@example.com",
                    Some("uuid-a"),
                    Some("org-1"),
                    now - 60_000,
                ),
                pin(
                    22,
                    "b@example.com",
                    Some("uuid-b"),
                    Some("org-1"),
                    now - PIN_TTL_MS - 1,
                ),
            ],
            now,
        )
        .expect("save");

        let accounts = [
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert_eq!(report.expired, 1);
        assert!(report.pins.contains_key(&11), "a minute old is warm");
        assert!(!report.pins.contains_key(&22), "past the TTL is cold");
    }

    /// The adaptive-TTL gate: a pin the client asked to cache for 1h gets the
    /// extended (~55min) restore window instead of the 15-minute default, a pin
    /// with no recorded TTL is still held to the default, and NEITHER survives
    /// past the extended ceiling. Malformed/absent TTL must never grant a
    /// LONGER window than the default — only ever the same or shorter.
    #[test]
    fn extended_ttl_pin_restores_a_30_minute_gap_the_default_would_drop() {
        let path = tmp("extended-ttl");
        let now = 10 * PIN_TTL_MS;
        let thirty_min_ago = now - 30 * 60 * 1000;
        let ninety_min_ago = now - 90 * 60 * 1000;
        save(
            &path,
            &[
                // 30 minutes old, recorded 1h TTL: must restore.
                pin_with_ttl(
                    11,
                    "a@example.com",
                    Some("uuid-a"),
                    Some("org-1"),
                    thirty_min_ago,
                    true,
                ),
                // Same age, no recorded TTL: must NOT restore under the 15-min
                // default.
                pin_with_ttl(
                    22,
                    "b@example.com",
                    Some("uuid-b"),
                    Some("org-1"),
                    thirty_min_ago,
                    false,
                ),
                // 90 minutes old with the extended TTL: past even the ~55-minute
                // extended ceiling, so it must not restore either.
                pin_with_ttl(
                    33,
                    "c@example.com",
                    Some("uuid-c"),
                    Some("org-1"),
                    ninety_min_ago,
                    true,
                ),
            ],
            now,
        )
        .expect("save");

        let accounts = [
            acct("a@example.com", Some("uuid-a"), Some("org-1")),
            acct("b@example.com", Some("uuid-b"), Some("org-1")),
            acct("c@example.com", Some("uuid-c"), Some("org-1")),
        ];
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(
            report.pins.contains_key(&11),
            "a 1h-TTL pin 30 minutes old must restore"
        );
        assert!(
            !report.pins.contains_key(&22),
            "no recorded TTL falls back to the 15-minute default and must not restore"
        );
        assert!(
            !report.pins.contains_key(&33),
            "90 minutes exceeds even the extended ceiling"
        );
    }

    /// Every read failure degrades to "no pins" plus a stated reason. None of
    /// these may panic or propagate: this is a cache.
    #[test]
    fn unreadable_file_shapes_degrade_to_empty() {
        let accounts = [acct("a@example.com", Some("uuid-a"), Some("org-1"))];
        let now = 5_000_000;

        // Missing — the ordinary first boot, not a degradation.
        let missing = tmp("missing").with_file_name("does-not-exist.json");
        let report = load(&missing, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert_eq!(report.degraded, None);

        // Truncated mid-object.
        let path = tmp("truncated");
        save(
            &path,
            &[pin(11, "a@example.com", Some("uuid-a"), Some("org-1"), now)],
            now,
        )
        .expect("save");
        let full = std::fs::read_to_string(&path).expect("read back");
        std::fs::write(&path, &full[..full.len() / 2]).expect("truncate");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(
            report
                .degraded
                .as_deref()
                .unwrap_or_default()
                .contains("corrupt"),
            "a truncated file must be reported, not silently empty: {:?}",
            report.degraded
        );

        // Not JSON at all.
        std::fs::write(&path, "\0\0\0 not json").expect("write garbage");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(report.degraded.is_some());

        // Valid JSON, wrong shape.
        std::fs::write(&path, "[1, 2, 3]").expect("write array");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(report.degraded.is_some());

        // A future format version is ignored wholesale rather than half-read.
        std::fs::write(&path, r#"{"version":99,"savedAtMs":0,"pins":[]}"#).expect("write v99");
        let report = load(&path, &accounts, now, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert!(
            report
                .degraded
                .as_deref()
                .unwrap_or_default()
                .contains("version"),
            "{:?}",
            report.degraded
        );
    }

    /// A file of another format version that lacks this version's `pins` is refused for its
    /// version, not as "corrupt: missing field": the reason is what tells an operator whether
    /// the cache is damaged or merely old.
    #[test]
    fn a_foreign_version_without_pins_is_refused_for_its_version() {
        let path = tmp("foreign-shape");
        std::fs::write(&path, r#"{"version":2,"savedAtMs":0,"sessions":[]}"#).expect("write v2");
        let report = load(&path, &[], 0, PIN_TTL_MS);
        assert!(report.pins.is_empty());
        assert_eq!(
            report.degraded.as_deref(),
            Some("format version 2 is not 1"),
            "the reason must name the version"
        );
    }

    /// The file is written `0600`: it names accounts, and it sits in a shared
    /// cache dir.
    #[test]
    fn save_writes_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("perms");
        save(&path, &[pin(11, "a@example.com", None, None, 1)], 1).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// Overflow keeps the freshest pins, mirroring the in-memory LRU eviction.
    #[test]
    fn save_caps_the_file_at_pin_cap_keeping_the_freshest() {
        let path = tmp("cap");
        let pins: Vec<StoredPin> = (0..(PIN_CAP as u64 + 10))
            .map(|i| pin(i, "a@example.com", Some("uuid-a"), Some("org-1"), i as i64))
            .collect();
        let written = save(&path, &pins, PIN_CAP as i64 + 100).expect("save");
        assert_eq!(written, PIN_CAP);

        let accounts = [acct("a@example.com", Some("uuid-a"), Some("org-1"))];
        // A TTL wide enough that nothing expires, so this measures the cap alone.
        let report = load(&path, &accounts, 0, i64::MAX);
        assert_eq!(report.pins.len(), PIN_CAP);
        assert!(
            report.pins.contains_key(&(PIN_CAP as u64 + 9)),
            "the freshest pin must survive the cap"
        );
        assert!(
            !report.pins.contains_key(&0),
            "the oldest pin is the one dropped"
        );
    }

    /// `save` writes through [`PinFileRef`] to skip copying every pin, and the file must still
    /// be exactly what [`PinFile`], the type `load` reads, serializes to.
    #[test]
    fn the_reference_file_serializes_exactly_like_the_owned_one() {
        let pins: Vec<StoredPin> = (0..5u64)
            .map(|i| pin(i, "a@example.com", Some("uuid-a"), Some("org-1"), i as i64))
            .collect();
        let owned = PinFile {
            version: FORMAT_VERSION,
            saved_at_ms: 42,
            pins: pins.clone(),
        };
        let borrowed = PinFileRef {
            version: FORMAT_VERSION,
            saved_at_ms: 42,
            pins: pins.iter().collect(),
        };
        assert_eq!(
            serde_json::to_string_pretty(&borrowed).expect("serialize the reference file"),
            serde_json::to_string_pretty(&owned).expect("serialize the owned file")
        );
    }

    // ---- carrying pins across the account rename ---------------------------

    /// A pin on a renamed account keeps pointing at it, and one on an untouched
    /// account is not disturbed. Without this, every session pinned to a renamed
    /// row resolves to nothing on the next boot and pays a full cold prefix —
    /// the most expensive event in this system.
    #[test]
    fn rename_pins_rewrites_only_the_renamed_names() {
        let path = tmp("rename");
        save(
            &path,
            &[
                pin(1, "henry@example.com", None, None, 100),
                pin(2, "alice@example.com", None, None, 100),
                pin(3, "henry@example.com", None, None, 200),
            ],
            0,
        )
        .expect("write pins");

        let renamed = rename_pins(
            &path,
            &[(
                "henry@example.com".to_string(),
                "henry@example.com/token".to_string(),
            )],
        );
        assert_eq!(renamed, 2, "both pins on that account follow it");

        // The rewritten pins resolve against the RENAMED fleet — the property
        // that actually matters, rather than the file's bytes.
        let accounts = [
            acct("henry@example.com/token", None, None),
            acct("alice@example.com", None, None),
        ];
        let report = load(&path, &accounts, 200, i64::MAX);
        assert_eq!(report.pins.len(), 3);
        assert_eq!(report.pins.get(&1).map(|(idx, _)| *idx), Some(0));
        assert_eq!(report.pins.get(&3).map(|(idx, _)| *idx), Some(0));
        assert_eq!(
            report.pins.get(&2).map(|(idx, _)| *idx),
            Some(1),
            "the untouched account's pin is untouched"
        );
        assert_eq!(report.unresolved, 0);

        std::fs::remove_file(&path).ok();
    }

    /// Nothing to do is not a failure, and neither is a missing file: this runs
    /// at boot on a cache, so every unhappy path must return quietly rather than
    /// hold up a server start.
    #[test]
    fn rename_pins_is_quiet_on_nothing_to_do() {
        let path = tmp("rename-quiet");
        assert_eq!(rename_pins(&path, &[]), 0, "no renames, no read");

        let missing = path.with_file_name("does-not-exist.json");
        assert_eq!(
            rename_pins(&missing, &[("a".to_string(), "b".to_string())]),
            0,
            "no pin file yet — the ordinary first-boot case"
        );

        std::fs::write(&path, "{ not json").expect("write junk");
        assert_eq!(
            rename_pins(&path, &[("a".to_string(), "b".to_string())]),
            0,
            "a corrupt cache is dropped, never a boot failure"
        );

        save(&path, &[pin(1, "solo@example.com", None, None, 100)], 0).expect("write pins");
        assert_eq!(
            rename_pins(&path, &[("a".to_string(), "b".to_string())]),
            0,
            "a rename matching no pin rewrites nothing"
        );

        std::fs::remove_file(&path).ok();
    }
}
