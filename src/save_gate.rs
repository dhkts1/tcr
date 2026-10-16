//! Whether a periodic cache save has anything to write.
//!
//! Three caches are written by timers so that a restart keeps what they hold: the session-affinity
//! pins (`session-affinity.json`), the Sessions/Tools table (`session-wire.json`) and the
//! bound-token map (`server-tool-pins.json`). A dirty flag on each says something was touched;
//! that is not the same as the file being out of date. A touch can leave the content as it was,
//! and some fields move on every request without changing anything a restart reads from them.
//!
//! A [`SaveGate`] remembers two hashes of what the last successful write put on disk:
//!
//! - the **content**: everything the file holds except its own `savedAtMs` stamp. When the next
//!   save's content hashes the same, nothing is written.
//! - the **shape**: the part of the content a restart cannot do without, chosen by each cache.
//!   For the pins it is which session is pinned to which account; the touch times that move on
//!   every request are not shape. A shape change is written at once. A change to the rest is
//!   written once [`SaveGate::refresh`] has passed since the last write, which bounds how stale
//!   those fields can be on disk.
//!
//! The hash is `std`'s SipHash over the value's JSON, the bytes the file would hold, streamed
//! through the hasher without building the string. Two different contents colliding on a 64-bit
//! hash would skip one write, which the next change repairs.

use std::hash::Hasher;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

/// The hash of `value`'s JSON, computed without building the JSON string.
pub fn content_hash<T: Serialize + ?Sized>(value: &T) -> Result<u64, serde_json::Error> {
    let mut writer = HashWriter(std::collections::hash_map::DefaultHasher::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finish())
}

/// An `io::Write` that feeds every byte to a hasher and keeps none of them.
struct HashWriter(std::collections::hash_map::DefaultHasher);

impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a periodic save did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// The file was written; this many entries landed.
    Wrote(usize),
    /// The content is what the last write put on disk, so nothing was written.
    Unchanged,
    /// Only fields outside the shape changed, and the last write is younger than the gate's
    /// refresh period. Nothing was written; a save at this instant would write.
    Deferred(Instant),
}

/// The two hashes of the last successful write, and when it happened.
#[derive(Debug, Clone, Copy)]
struct Written {
    shape: u64,
    content: u64,
    at: Instant,
}

/// One cache file's record of its last successful write. See the module doc.
#[derive(Debug)]
pub struct SaveGate {
    refresh: Duration,
    last: Mutex<Option<Written>>,
}

impl SaveGate {
    /// A gate that writes a change outside the shape at most once per `refresh`.
    pub const fn new(refresh: Duration) -> Self {
        Self {
            refresh,
            last: Mutex::new(None),
        }
    }

    /// How long a change outside the shape may wait before it is written.
    pub fn refresh(&self) -> Duration {
        self.refresh
    }

    /// What a save of content hashing to (`shape`, `content`) at `now` should do: `None` to
    /// write, `Some(outcome)` to skip it with that outcome. Nothing written yet always writes.
    pub fn skip(&self, shape: u64, content: u64, now: Instant) -> Option<SaveOutcome> {
        let last = *self.last.lock().expect("save gate lock poisoned");
        let last = last?;
        if last.content == content {
            return Some(SaveOutcome::Unchanged);
        }
        if last.shape != shape {
            return None;
        }
        let due = last.at + self.refresh;
        (now < due).then_some(SaveOutcome::Deferred(due))
    }

    /// Record a successful write of content hashing to (`shape`, `content`) at `now`.
    pub fn wrote(&self, shape: u64, content: u64, now: Instant) {
        *self.last.lock().expect("save gate lock poisoned") = Some(Written {
            shape,
            content,
            at: now,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_the_hash_of_the_json() {
        let value = serde_json::json!({"a": [1, 2, 3], "b": "x"});
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hasher.write(serde_json::to_string(&value).unwrap().as_bytes());
        assert_eq!(content_hash(&value).unwrap(), hasher.finish());
        assert_ne!(
            content_hash(&value).unwrap(),
            content_hash(&serde_json::json!({"a": [1, 2, 4], "b": "x"})).unwrap()
        );
    }

    #[test]
    fn nothing_written_yet_writes() {
        let gate = SaveGate::new(Duration::from_secs(60));
        assert_eq!(gate.skip(1, 1, Instant::now()), None);
    }

    #[test]
    fn the_same_content_is_not_written_twice() {
        let gate = SaveGate::new(Duration::from_secs(60));
        let now = Instant::now();
        gate.wrote(1, 10, now);
        assert_eq!(gate.skip(1, 10, now), Some(SaveOutcome::Unchanged));
        // However long ago the write was.
        assert_eq!(
            gate.skip(1, 10, now + Duration::from_secs(3_600)),
            Some(SaveOutcome::Unchanged)
        );
    }

    #[test]
    fn a_shape_change_writes_at_once() {
        let gate = SaveGate::new(Duration::from_secs(60));
        let now = Instant::now();
        gate.wrote(1, 10, now);
        assert_eq!(gate.skip(2, 11, now + Duration::from_millis(1)), None);
    }

    #[test]
    fn a_change_outside_the_shape_waits_for_the_refresh() {
        let gate = SaveGate::new(Duration::from_secs(60));
        let now = Instant::now();
        gate.wrote(1, 10, now);
        let due = now + Duration::from_secs(60);
        assert_eq!(
            gate.skip(1, 11, now + Duration::from_secs(59)),
            Some(SaveOutcome::Deferred(due))
        );
        assert_eq!(gate.skip(1, 11, due), None);
    }
}
