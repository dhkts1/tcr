//! The whole-file write ([`save`]), the atomic 0600 write under every writer, and the raw read.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use serde_json::{Map, Value};

use super::{Config, ConfigError};

/// Persist `config` to `path` atomically (temp file in the same dir + rename),
/// with `0600` permissions so refreshed tokens never land world-readable.
///
/// Same-directory temp + rename keeps the swap atomic (rename is atomic within a
/// filesystem); a crash mid-write leaves the original intact.
///
/// A Codex account is written at `CODEX_CREDENTIAL_VERSION` even when the
/// snapshot carries no version for it (`Config::with_codex_credential_versions`); an
/// Anthropic-only config serializes exactly as it always has.
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    write_atomic(
        path,
        &serde_json::to_string_pretty(config.with_codex_credential_versions().as_ref())?,
    )
}

/// How far [`write_atomic_with`] pushes a file toward the disk before it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Durability {
    /// Flushed to the disk itself before the rename and again after it: two `F_FULLFSYNC`
    /// on macOS. For what cannot be rebuilt: the config, rotated refresh tokens, peer state.
    Flushed,
    /// Atomic against this process dying, SIGKILL included, because the kernel already
    /// holds the bytes; not against a power loss, which can leave the previous version or a
    /// truncated file. Only for a cache the proxy rebuilds from traffic and whose loader
    /// already reads a missing, truncated or corrupt file as empty, so the worst case is a
    /// cold cache.
    ///
    /// Why it exists, measured 2026-10-15 on the live proxy: the affinity and session-wire
    /// caches were rewritten every 5 s without a break (24 times each in 120 s), each write
    /// paying both flushes on a runtime worker, and the flush waits were 8% and 35% of the
    /// proxy's busy samples in two profiles.
    Cache,
}

/// The atomic 0600 write itself with full durability ([`Durability::Flushed`]), shared by
/// [`save`], [`save_tokens`] and every other state file that cannot be rebuilt, so every path
/// gets the same durability and permission guarantees. One implementation deliberately:
/// a second hand-rolled temp+rename is a second place for the ordering to be
/// subtly wrong. A rebuildable cache calls [`write_atomic_with`] and [`Durability::Cache`].
pub(crate) fn write_atomic(path: &Path, json: &str) -> Result<(), ConfigError> {
    write_atomic_with(path, json, Durability::Flushed)
}

/// [`write_atomic`] with the durability named: the same temp file, permissions and rename,
/// and the two disk flushes only when `durability` is [`Durability::Flushed`].
pub(crate) fn write_atomic_with(
    path: &Path,
    json: &str,
    durability: Durability,
) -> Result<(), ConfigError> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));

    // Ensure the parent dir exists (a freshly-provisioned box may lack
    // `~/.config`), so a token-refresh save never fails with ENOENT and drops the
    // rotated refresh token. Fails loudly on a real perms error (finding #2).
    fs::create_dir_all(dir)?;

    // `tempfile_in`, never `NamedTempFile::new()`: the temp file MUST live in the
    // destination's own directory. The system temp dir is routinely a different
    // filesystem, and `rename(2)` across filesystems fails with `EXDEV` — which
    // would silently cost us the atomic swap this function exists to provide.
    //
    // The unique temp name is now the crate's job (finding #6): two concurrent
    // saves (e.g. `probe_all` refreshing several expired accounts at once) must
    // never open and truncate the SAME temp file and interleave into a corrupt
    // write. The `.{name}.{pid}.{seq}.tmp` scheme this replaced was already sound
    // for that — a live pid is unique and the counter is unique within it — so on
    // the concurrency property alone this is a lateral move.
    //
    // What it genuinely buys is a different, previously undocumented property.
    // The old open was `create(true).truncate(true)` — no `O_EXCL`, so it FOLLOWED
    // SYMLINKS — on a fully predictable path. Anyone able to create a file in the
    // config dir could pre-plant that path as a symlink and have a token refresh
    // write live OAuth credentials into a file of their choosing. Measured: the
    // old open succeeds and writes through the symlink; `create_new` (`O_EXCL`)
    // refuses with AlreadyExists. `O_EXCL` also guarantees a FRESH inode, which
    // matters because `open(2)` applies its `mode` argument only on creation — the
    // old code's `.mode(0o600)` was silently ignored whenever the path already
    // existed (measured: a pre-existing 0666 file stays 0666).
    //
    // The name is prefixed rather than left as the crate's default. `tempfile`
    // names a file `.tmpXXXXXX`, which carries no attribution at all: a SIGKILL
    // between the create below and the `persist` further down strands a file
    // holding EVERY account's OAuth access and refresh tokens in `~/.config`,
    // and this proxy is SIGKILLed as documented operational reality. The old
    // `.{name}.{pid}.{seq}.tmp` scheme at least left an orphan that was greppable
    // and obviously ours; `.{name}.tcr-XXXXXX.tmp` restores that. Note this is
    // attribution only — nothing in this crate reaps orphans (`rg 'read_dir' src/`
    // finds none), and the random component means they no longer recycle the way
    // a pid×seq space did, so they accumulate until an operator removes them.
    let prefix = match path.file_name() {
        Some(name) => format!(".{}.tcr-", name.to_string_lossy()),
        None => ".tcr-".to_string(),
    };
    let mut file = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o600))
        .prefix(prefix.as_str())
        .suffix(".tmp")
        .tempfile_in(dir)?;
    file.write_all(json.as_bytes())?;
    // Make the temp file's DATA durable before the rename that publishes it, so a
    // crash cannot expose the destination as empty or half-written. This is the
    // only durability guarantee that holds unconditionally here; see the
    // directory sync below for the part that does not. A cache skips it: see
    // `Durability::Cache` for why that is safe for exactly those files.
    match durability {
        Durability::Flushed => file.as_file().sync_all()?,
        Durability::Cache => {}
    }

    match file.persist(path) {
        // The returned `File` is the persisted handle; nothing here writes through
        // it (every save is a fresh temp plus a rename), so it is dropped at once.
        Ok(_) => {}
        Err(tempfile::PersistError { error, file }) => {
            // NEVER `map_err(|e| e.error)`. `PersistError` OWNS the temp file
            // (`tempfile-3.27.0 src/file/mod.rs:516-521`) and `TempPath`'s `Drop` is
            // `fs::remove_file` unless cleanup is disabled (`:402-408`), so
            // discarding the struct to keep only its `io::Error` UNLINKS the
            // complete, fsynced JSON that is sitting right there.
            //
            // This is defence in depth, not the only line of it, and the
            // distinction is worth stating precisely. A rename failure here — full
            // disk, EACCES on the destination, an immutable flag, a read-only
            // remount — does not by itself lose the rotated single-use refresh
            // token: `Manager::persist_tokens` keeps it in the in-memory snapshot
            // on purpose for `persist_now` to flush at shutdown, and says so
            // (`src/manager/mod.rs:862-866`). What the retained file adds is a
            // SECOND, independent recovery artifact at a known path, which is what
            // covers the conjunction — a failed rename AND a process that never
            // reaches its shutdown flush. That conjunction is not hypothetical
            // here: `persist_now` is skipped on a SIGKILL and on the
            // never-served/early-cancel paths (`src/server.rs:1025`, `:1063`), and
            // this proxy is SIGKILLed as documented operational reality. The
            // `fs::rename` this replaced left exactly that artifact behind; the
            // migration removed it.
            //
            // `keep()` disables the delete-on-drop and hands back the path. The path
            // is logged (the contents are NOT — they are live OAuth tokens) because
            // a preserved file whose location never reaches an operator is barely
            // better than a deleted one.
            match file.keep() {
                Ok((_file, kept)) => tracing::warn!(
                    path = %path.display(),
                    retained = %kept.display(),
                    error = %error,
                    "could not rename the new state file into place; the fully-written temp file has been RETAINED at `retained` — move it over `path` by hand to recover"
                ),
                Err(keep_error) => tracing::error!(
                    path = %path.display(),
                    error = %error,
                    keep_error = %keep_error,
                    "could not rename the new state file into place, AND could not retain the temp file; its contents are lost"
                ),
            }
            return Err(error.into());
        }
    }

    // What is and is not durable, precisely. `sync_all` above makes the temp
    // file's DATA durable. `persist` is a bare `rename(2)`, which dirties only the
    // parent directory's inode — so without the sync below, a power loss seconds
    // after a successful return can roll the directory entry back and leave the
    // PREVIOUS contents in place. For `save_tokens` that means the previous
    // refresh token, which Anthropic has already consumed: the same dead-account
    // outcome, just by a different route. This gap predates the tempfile
    // migration; it is closed here.
    //
    // Best-effort ON PURPOSE, and this is the one place in this function where a
    // failure is not returned. The rename has ALREADY succeeded by this point —
    // the new bytes are the ones a reader sees. Turning a directory-sync failure
    // into an `Err` would tell `save_tokens` its write failed when it did not, and
    // the caller's recovery for that is to treat the rotated token as unpersisted.
    // A warn keeps the failure visible without inventing one. Portability is the
    // second reason: directory `fsync` is well-defined on Linux, while on macOS
    // `sync_all` issues `F_FULLFSYNC`, which a filesystem is free to refuse on a
    // directory fd. Measured on APFS (macOS 25.6, the deployment target): it
    // returns `Ok`, so the sync is real here and not a no-op — but that is one
    // filesystem, not a guarantee, which is why the failure is tolerated.
    //
    // The cost is real and was measured, not assumed: it roughly DOUBLES this
    // function (5.0 → 9.5 ms/write, N=200, APFS), because a second `F_FULLFSYNC`
    // is a second barrier. That is affordable only because of the actual call
    // rates. The affinity flusher is the hot caller and is the one that would hurt
    // — it does a blocking `std::fs` write inside async code (`src/server.rs:425`).
    // This comment used to say its steady state is no writes at all. Measured on
    // 2026-10-15, that was false: with a few dozen live sessions the pins change on
    // every tick. So the affinity, session-wire and bound-token caches now write
    // with `Durability::Cache` and skip this sync. The credential
    // writers run per token rotation. If a future caller writes at request rate,
    // revisit this line before assuming it is free.
    if durability == Durability::Flushed
        && let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all())
    {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "state file renamed into place, but the parent directory could not be fsynced; the rename may not survive a power loss"
        );
    }

    // NOT a guard against a looser pre-existing destination: `persist` is a
    // rename, so the destination's old inode — and its mode — are gone. This
    // cannot tighten anything.
    //
    // Nor is it needed for a later write: nothing here ever reopens a state file
    // for writing — every save is a fresh temp plus a rename, and `rename(2)`
    // needs permission on the DIRECTORY, not on the destination file. A 0400
    // config is rewritten perfectly (measured).
    //
    // It survives as a NORMALISATION, not a guard: the create mode above is
    // `0600 & ~umask`, so under a restrictive umask the file would land 0400 —
    // still correct, but surprising for a file the user is invited to hand-edit
    // when it goes corrupt. This keeps the mode from varying with the operator's
    // umask.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Read the config file as a raw JSON object. Parsing as a MAP (not a bare
/// `Value`) is deliberate: a file that is valid JSON but not an object — `[]`,
/// `null`, a half-written fragment — must take the malformed fallback rather
/// than be written back verbatim with the fresh tokens silently dropped.
pub(crate) fn read_document(path: &Path) -> Result<Map<String, Value>, ConfigError> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::config::tests::{SAMPLE, tmp_path};
    use std::path::PathBuf;

    #[test]
    fn save_writes_owner_only_permissions() {
        let config: Config = serde_json::from_str(SAMPLE).unwrap();
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-perm-{}.json", std::process::id()));
        save(&tmp, &config).unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(&tmp).ok();
    }

    // --- write_atomic's durability modes -----------------------------------

    /// A cache write skips only the disk flushes: it lands the same bytes at the same 0600
    /// mode as a flushed write, replaces the destination by rename, and leaves no temp file.
    #[test]
    fn a_cache_write_lands_what_a_flushed_write_lands() {
        let dir = tmp_path("durability-cache").with_extension("d");
        fs::create_dir_all(&dir).expect("create the test directory");
        let flushed = dir.join("flushed.json");
        let cache = dir.join("cache.json");
        let json = r#"{"pins":[{"key":1}]}"#;
        write_atomic_with(&flushed, json, Durability::Flushed).expect("flushed write");
        write_atomic_with(&cache, "{}", Durability::Cache).expect("cache write");
        write_atomic_with(&cache, json, Durability::Cache).expect("cache overwrite");

        assert_eq!(
            fs::read(&cache).expect("read the cache file"),
            fs::read(&flushed).expect("read the flushed file")
        );
        let mode = |p: &Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&cache), 0o600);
        assert_eq!(mode(&flushed), 0o600);
        let names: Vec<String> = fs::read_dir(&dir)
            .expect("read the test directory")
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names.len(), 2, "no temp file may be left behind: {names:?}");
        fs::remove_dir_all(&dir).ok();
    }

    /// What the two disk flushes cost per write, which is what `Durability::Cache` saves the
    /// caches rewritten every 5 s. A timing, not a gate, so it is ignored; run it with
    /// `cargo test --release --lib flushed_and_cache_write_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing benchmark, run by hand"]
    fn flushed_and_cache_write_costs() {
        let dir = tmp_path("durability-bench").with_extension("d");
        fs::create_dir_all(&dir).expect("create the test directory");
        let path = dir.join("bench.json");
        // About the size of the live session-affinity file on 2026-10-15.
        let json = "x".repeat(160 * 1024);
        let rounds: u32 = 50;
        for durability in [Durability::Flushed, Durability::Cache] {
            let start = std::time::Instant::now();
            for _ in 0..rounds {
                write_atomic_with(&path, &json, durability).expect("write");
            }
            println!(
                "{durability:?}: {:?} per 160 KB write",
                start.elapsed() / rounds
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    // --- write_atomic's failure path ---------------------------------------

    /// A destination that `rename(2)` cannot replace, in a directory we can still
    /// write to. Renaming a non-directory onto a directory fails (`EISDIR`/
    /// `ENOTDIR`) while `tempfile_in` on the parent still succeeds, which is
    /// exactly the shape needed: the temp file gets created and fully written, and
    /// only the final rename fails.
    fn unrenamable_target(tag: &str) -> (PathBuf, PathBuf) {
        let dir = tmp_path(tag).with_extension("d");
        let target = dir.join(crate::base::paths::CONFIG_FILE);
        fs::create_dir_all(&target).expect("create the blocking directory");
        (dir, target)
    }

    /// The recovery-artifact guard. `PersistError` owns the `NamedTempFile`, whose
    /// `Drop` is `fs::remove_file`, so `map_err(|e| e.error)` unlinks the complete,
    /// fsynced JSON on a rename failure. The rotated single-use token also lives in
    /// the in-memory snapshot (`src/manager/mod.rs:862-866`), so this file is the
    /// SECOND recovery path, not the only one — it is what covers a failed rename
    /// on a process that then dies before its shutdown flush.
    #[test]
    fn a_failed_rename_retains_the_written_temp_file() {
        let (dir, target) = unrenamable_target("persist-fail-retain");
        let json = r#"{"accounts":[{"name":"acct-a","accessToken":"at-a"}]}"#;

        let err = write_atomic(&target, json).expect_err("rename onto a directory must fail");
        std::assert_matches!(err, ConfigError::Io(_), "a rename failure is an I/O error");

        let orphans: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("read the destination directory")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.is_file())
            .collect();
        assert_eq!(
            orphans.len(),
            1,
            "the written temp file must survive a failed rename, found {orphans:?}"
        );
        assert_eq!(
            fs::read_to_string(&orphans[0]).expect("read the retained temp file"),
            json,
            "the retained file must hold the bytes we wrote"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// Attribution for the orphan the test above proves exists — and for the one a
    /// SIGKILL between create and rename strands. `tempfile`'s default `.tmpXXXXXX`
    /// says nothing about who wrote it or which file it was becoming; in `~/.config`
    /// that file holds every account's OAuth tokens.
    #[test]
    fn a_retained_temp_file_is_attributable_to_tcr_and_its_destination() {
        let (dir, target) = unrenamable_target("persist-fail-name");
        write_atomic(&target, "{}").expect_err("rename onto a directory must fail");

        let orphan = fs::read_dir(&dir)
            .expect("read the destination directory")
            .map(|e| e.expect("dir entry").path())
            .find(|p| p.is_file())
            .expect("a retained temp file");
        let name = orphan
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a utf-8 temp file name")
            .to_string();

        assert!(
            name.starts_with(&format!(".{}.tcr-", crate::base::paths::CONFIG_FILE)),
            "an orphan must name its owner and its destination, got {name}"
        );
        assert!(
            name.ends_with(".tmp"),
            "an orphan must be recognisable as a temp file, got {name}"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
