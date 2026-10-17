//! The lifecycle the three periodically saved caches share: the session-affinity pins
//! ([`crate::affinity`]), the Sessions/Tools table ([`crate::session_wire_persist`]) and the
//! bound-token map ([`crate::bound_tokens`]).
//!
//! Each cache keeps its own file struct, its own TTL and its own resolve loop, so the bytes on
//! disk are each module's business. What they share lives here, once:
//!
//! - [`SavedFile`]: the manager's side. A dirty flag the request path raises, the wake-up the
//!   flusher sleeps on, and the [`SaveGate`] that skips a write whose content is already on
//!   disk.
//! - [`read_cache`]: the load envelope. A missing file is the ordinary first boot; anything else
//!   that stops the file being read is reported as the reason it was ignored, and the format
//!   version is read before the rest of the file, so an old or future file is reported as a
//!   version and not as corruption.
//! - [`write_cache`]: the atomic `0600` write every cache uses, at [`Durability::Cache`].

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Notify;

use crate::config::{ConfigError, Durability};
use crate::save_gate::{SaveGate, SaveOutcome};

/// One cache file as the manager holds it: whether it changed since the last flush, the
/// flusher's wake-up, and the record of the last write.
#[derive(Debug)]
pub struct SavedFile {
    /// Set whenever the cache is mutated, cleared by the flusher that writes it. A relaxed
    /// atomic rather than a channel because the setter sits on the request path and the
    /// reader is a timer: the only ordering that matters is "a change eventually causes a
    /// write", and a tick that observes the flag late writes on the next one.
    dirty: AtomicBool,
    /// Woken by every change, so the flusher sleeps while nothing changes rather than polling
    /// the flag. A permit is stored when nobody waits, so a change made between two waits is
    /// never missed.
    wake: Notify,
    /// The last successful write. See [`crate::save_gate`].
    gate: SaveGate,
}

impl SavedFile {
    /// A file whose changes outside the shape are written at most once per `refresh`.
    pub fn new(refresh: Duration) -> Self {
        Self {
            dirty: AtomicBool::new(false),
            wake: Notify::new(),
            gate: SaveGate::new(refresh),
        }
    }

    /// Flag the cache as changed and wake the flusher: one relaxed store and one notify, so it
    /// is safe on the request path.
    pub fn mark(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// Resolves once the cache has changed since the last time this resolved.
    pub async fn changed(&self) {
        self.wake.notified().await;
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// Whether the dirty flag is up, without consuming it.
    #[cfg(test)]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Write unconditionally (the shutdown save) and record the write, so the next gated save
    /// compares against it. `hashes` are the (shape, content) of what `write` puts on disk.
    pub fn save(
        &self,
        hashes: (u64, u64),
        write: impl FnOnce() -> Result<usize, ConfigError>,
    ) -> Result<usize, ConfigError> {
        let count = write()?;
        let (shape, content) = hashes;
        self.gate.wrote(shape, content, Instant::now());
        Ok(count)
    }

    /// The flusher's save: `write` only when the content differs from the last write, by the
    /// rule [`crate::save_gate`] states, and record it.
    pub fn save_if_changed(
        &self,
        hashes: (u64, u64),
        write: impl FnOnce() -> Result<usize, ConfigError>,
    ) -> Result<SaveOutcome, ConfigError> {
        let (shape, content) = hashes;
        let now = Instant::now();
        if let Some(skipped) = self.gate.skip(shape, content, now) {
            return Ok(skipped);
        }
        let count = write()?;
        self.gate.wrote(shape, content, now);
        Ok(SaveOutcome::Wrote(count))
    }
}

/// What [`read_cache`] made of a cache file.
#[derive(Debug)]
pub enum CacheRead<T> {
    /// No file: the first boot, or the first since the cache existed. Not a degradation.
    Missing,
    /// The file was ignored entirely, for this reason: unreadable, corrupt, or a format
    /// version this build does not read.
    Degraded(String),
    /// The file, parsed.
    Read(T),
}

/// Read the cache file at `path` as a `T` written at format `version`.
///
/// The version is read first, on its own, and a foreign one is reported as a version rather
/// than as corruption. Parsing the whole struct first gets this backwards: a file from another
/// format often lacks a field this one requires, so it fails as "corrupt: missing field", the
/// right refusal for the wrong reason, and the reason is what an operator reads when deciding
/// whether a cache file is damaged or merely old.
pub fn read_cache<T: DeserializeOwned>(path: &Path, version: u32) -> CacheRead<T> {
    #[derive(serde::Deserialize)]
    struct VersionPeek {
        version: u32,
    }

    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CacheRead::Missing,
        Err(err) => return CacheRead::Degraded(format!("unreadable: {err}")),
    };
    match serde_json::from_str::<VersionPeek>(&data) {
        Ok(peek) if peek.version != version => {
            return CacheRead::Degraded(format!(
                "format version {} is not {version}",
                peek.version
            ));
        }
        Ok(_) => {}
        Err(err) => return CacheRead::Degraded(format!("corrupt: {err}")),
    }
    // Truncated or otherwise damaged: a full disk, a power loss under `Durability::Cache`, or
    // a hand edit.
    match serde_json::from_str(&data) {
        Ok(file) => CacheRead::Read(file),
        Err(err) => CacheRead::Degraded(format!("corrupt: {err}")),
    }
}

/// How [`write_cache`] lays the JSON out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Indented, for a file small enough to read by eye.
    Pretty,
    /// One line, for a file of megabytes no one reads by eye.
    Compact,
}

/// Write `file` to `path` atomically at `0600`.
///
/// Every cache here is rewritten on a timer and read back as empty when missing or damaged,
/// so the write is atomic against this process dying but not flushed to the disk
/// ([`Durability::Cache`]).
pub fn write_cache<T: Serialize + ?Sized>(
    path: &Path,
    file: &T,
    layout: Layout,
) -> Result<(), ConfigError> {
    let json = match layout {
        Layout::Pretty => serde_json::to_string_pretty(file)?,
        Layout::Compact => serde_json::to_string(file)?,
    };
    crate::config::write_atomic_with(path, &json, Durability::Cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache file's body; `read_cache` reads its `version` on the side.
    #[derive(serde::Deserialize, Debug)]
    struct Fixture {
        items: Vec<u32>,
    }

    fn tmp(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-cache-file-test-{}-{label}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("cache.json")
    }

    #[test]
    fn a_missing_file_is_missing_not_degraded() {
        let path = tmp("missing");
        assert!(matches!(
            read_cache::<Fixture>(&path, 1),
            CacheRead::Missing
        ));
    }

    /// A file of another version that also lacks this version's fields is reported as a
    /// version, which is the whole point of reading the version first.
    #[test]
    fn a_foreign_version_with_other_fields_names_the_version() {
        let path = tmp("foreign");
        std::fs::write(&path, r#"{"version":7,"other":[]}"#).expect("write");
        match read_cache::<Fixture>(&path, 1) {
            CacheRead::Degraded(reason) => {
                assert_eq!(reason, "format version 7 is not 1");
            }
            other => panic!("expected a version refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_right_version_missing_a_field_is_corrupt() {
        let path = tmp("corrupt");
        std::fs::write(&path, r#"{"version":1}"#).expect("write");
        match read_cache::<Fixture>(&path, 1) {
            CacheRead::Degraded(reason) => assert!(reason.starts_with("corrupt:"), "{reason}"),
            other => panic!("expected corrupt, got {other:?}"),
        }
    }

    #[test]
    fn not_json_is_corrupt() {
        let path = tmp("garbage");
        std::fs::write(&path, "\0\0 not json").expect("write");
        assert!(matches!(
            read_cache::<Fixture>(&path, 1),
            CacheRead::Degraded(reason) if reason.starts_with("corrupt:")
        ));
    }

    #[test]
    fn a_good_file_is_read() {
        let path = tmp("good");
        std::fs::write(&path, r#"{"version":1,"items":[1,2]}"#).expect("write");
        assert!(matches!(
            read_cache::<Fixture>(&path, 1),
            CacheRead::Read(Fixture { items, .. }) if items == vec![1, 2]
        ));
    }

    #[test]
    fn a_mark_is_consumed_by_one_take() {
        let file = SavedFile::new(Duration::ZERO);
        assert!(!file.take());
        file.mark();
        assert!(file.is_dirty());
        assert!(file.take());
        assert!(!file.take());
    }

    #[test]
    fn a_gated_save_skips_what_the_last_save_wrote() {
        let file = SavedFile::new(Duration::from_secs(60));
        assert_eq!(file.save((1, 10), || Ok(3)).expect("save"), 3);
        let outcome = file
            .save_if_changed((1, 10), || panic!("the same content must not be written"))
            .expect("skip");
        assert_eq!(outcome, SaveOutcome::Unchanged);
        let outcome = file.save_if_changed((2, 11), || Ok(4)).expect("write");
        assert_eq!(outcome, SaveOutcome::Wrote(4));
    }

    #[test]
    fn a_failed_write_records_nothing() {
        let file = SavedFile::new(Duration::from_secs(60));
        let failed = file.save_if_changed((1, 10), || {
            Err(ConfigError::Io(std::io::Error::other("disk full")))
        });
        assert!(failed.is_err());
        assert_eq!(
            file.save_if_changed((1, 10), || Ok(1)).expect("retry"),
            SaveOutcome::Wrote(1),
            "a failed write must be retried, not remembered as on disk"
        );
    }

    #[tokio::test]
    async fn a_mark_before_the_wait_is_not_missed() {
        let file = SavedFile::new(Duration::ZERO);
        file.mark();
        tokio::time::timeout(Duration::from_secs(1), file.changed())
            .await
            .expect("the stored permit wakes the waiter");
    }
}
