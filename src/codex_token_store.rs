//! Synchronize an explicitly imported Codex login with its original `auth.json`.
//!
//! No ambient home or default login is read here. The manager supplies the recorded `codexHome`
//! while holding that account's refresh lock. Codex's file has `tokens.access_token`,
//! `tokens.refresh_token`, `tokens.id_token`, `tokens.account_id` and top-level `last_refresh`
//! (`openai/codex` `login/src/auth/storage.rs` and `login/src/token_data.rs`). Updating the JSON
//! value preserves every other field, including token fields a later Codex version adds.
//!
//! A write holds a private advisory lock shared by tcr writers, writes a private temporary file
//! in the same directory, and compares the complete original immediately before renaming.
//! Codex's current FileAuthStorage writes without this lock, so the comparison and rename are
//! not an atomic compare-and-swap against Codex: a Codex write in that final gap can still race.
//! The lock serializes cooperative tcr writers; the final comparison detects completed external
//! changes. No cross-process lock protocol is currently shared with Codex.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::codex_login::CodexLogin;
use crate::oauth::Tokens;

pub(crate) struct ImportedAuth {
    path: PathBuf,
    original: Vec<u8>,
    document: Value,
    pub(crate) login: CodexLogin,
    pub(crate) last_refresh_ms: Option<i64>,
}

fn invalid_document() -> io::Error {
    // Decoder diagnostics can quote credential values. Keep the error independent of the body.
    io::Error::new(io::ErrorKind::InvalidData, "invalid Codex login document")
}

pub(crate) fn read(home: &Path) -> io::Result<ImportedAuth> {
    let path = crate::paths::codex_auth_file(home);
    let original = fs::read(&path)?;
    let text = std::str::from_utf8(&original).map_err(|_| invalid_document())?;
    let document: Value = serde_json::from_str(text).map_err(|_| invalid_document())?;
    let login = crate::codex_login::parse_auth_json(text)
        .map_err(|_| invalid_document())?
        .ok_or_else(invalid_document)?;
    let last_refresh_ms = document
        .get("last_refresh")
        .and_then(Value::as_str)
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .and_then(|value| i64::try_from(value.unix_timestamp_nanos() / 1_000_000).ok());
    Ok(ImportedAuth {
        path,
        original,
        document,
        login,
        last_refresh_ms,
    })
}

/// Read the imported document without putting filesystem latency on a tokio worker.
pub(crate) async fn read_async(home: &Path) -> io::Result<ImportedAuth> {
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || read(&home))
        .await
        .map_err(|_| io::Error::other("Codex login read task failed"))?
}

/// Keep the compare, private temporary write, fsyncs and rename on one blocking task.
/// The source is identified by its SHA-256 digest. `false` preserves a changed source;
/// a byte conflict retaining the source is retryable. Other fields remain intact.
pub(crate) async fn write_back_async(
    home: &Path,
    expected_digest: &str,
    tokens: &Tokens,
) -> io::Result<bool> {
    let home = home.to_path_buf();
    let expected_digest = expected_digest.to_string();
    let tokens = tokens.clone();
    tokio::task::spawn_blocking(move || {
        write_back_with_digest(&home, &expected_digest, &tokens, || {})
    })
    .await
    .map_err(|_| io::Error::other("Codex login write task failed"))?
}

impl ImportedAuth {
    /// A changed token pair for the same ChatGPT workspace, and the same user when known.
    /// An expired access token still carries a potentially usable refresh token; the manager
    /// refreshes that pair before applying it.
    /// JWT issuance must advance, or an unchanged/missing issuance time needs an advancing
    /// file timestamp whose baseline belonged to the current refresh token. A different live
    /// pair alone proves no freshness: a failed writeback can leave an older pair on disk.
    /// After the authority rejects the current refresh token, an equal or unreadable issuance
    /// can recover from the changed live file pair; a known older issuance is still rejected.
    pub(crate) fn newer_tokens(
        &self,
        previous_refresh: &str,
        previous_access: &str,
        account_id: Option<&str>,
        user_id: Option<&str>,
        after_ms: Option<i64>,
        previous_rejected: bool,
    ) -> Option<Tokens> {
        let expected_account = account_id.filter(|id| !id.is_empty())?;
        if self.login.refresh_token == previous_refresh
            || self.login.access_token == previous_access
            || self.login.account_id.as_deref() != Some(expected_account)
            || self.login.is_fedramp
            || user_id
                .zip(self.login.user_id.as_deref())
                .is_some_and(|(old, new)| old != new)
            || after_ms.is_some_and(|previous| match self.last_refresh_ms {
                Some(current) => current < previous || (current == previous && !previous_rejected),
                None => !previous_rejected,
            })
        {
            return None;
        }
        match (
            crate::codex_oauth::jwt_iat_ms(previous_access),
            crate::codex_oauth::jwt_iat_ms(&self.login.access_token),
        ) {
            (Some(previous), Some(candidate)) if candidate < previous => return None,
            (Some(previous), Some(candidate)) if candidate > previous => {}
            // An equal or unreadable issuance time can only use a timestamp baseline read
            // from a file that still held the current refresh token before this POST, unless
            // the authority has now rejected that current token and the file offers recovery.
            _ if after_ms.is_some() || previous_rejected => {}
            _ => return None,
        }
        // account_id in the file must not conceal a different workspace in either JWT.
        for token in [
            Some(self.login.access_token.as_str()),
            self.document
                .get("tokens")?
                .get("id_token")
                .and_then(Value::as_str),
        ] {
            if token
                .and_then(crate::codex_oauth::id_claims)
                .and_then(|claims| claims.chatgpt_account_id)
                .is_some_and(|account| account != expected_account)
            {
                return None;
            }
        }
        let expires_at_ms = self.login.expires_at_ms.unwrap_or(0);
        Some(Tokens {
            id_token: self
                .document
                .get("tokens")?
                .get("id_token")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
            access_token: self.login.access_token.clone(),
            refresh_token: Some(self.login.refresh_token.clone()),
            expires_at_ms,
        })
    }
}

/// Erase unpublished credentials before unlinking, including when the parent directory has
/// become read-only. A successful rename disarms this guard without touching auth.json.
struct PrivateTokenFile {
    temporary: Option<tempfile::NamedTempFile>,
}

impl PrivateTokenFile {
    fn file_mut(&mut self) -> &mut tempfile::NamedTempFile {
        self.temporary
            .as_mut()
            .expect("unpublished token temporary")
    }

    fn publish(mut self, destination: &Path) -> io::Result<()> {
        let temporary = self.temporary.take().expect("unpublished token temporary");
        match temporary.persist(destination) {
            Ok(_) => Ok(()),
            Err(failure) => {
                self.temporary = Some(failure.file);
                Err(failure.error)
            }
        }
    }
}

impl Drop for PrivateTokenFile {
    fn drop(&mut self) {
        let Some(temporary) = self.temporary.as_ref() else {
            return;
        };
        if let Err(error) = temporary
            .as_file()
            .set_len(0)
            .and_then(|()| temporary.as_file().sync_all())
        {
            tracing::warn!(error_kind = ?error.kind(), "could not clear an unpublished Codex token temporary file");
        }
    }
}

/// Compare a source token after restart without storing the spent token itself.
pub(crate) fn refresh_digest(refresh: &str) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(refresh.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
pub(crate) fn write_back(home: &Path, previous_refresh: &str, tokens: &Tokens) -> io::Result<bool> {
    write_back_before_commit(home, previous_refresh, tokens, || {})
}

#[cfg(test)]
fn write_back_before_commit(
    home: &Path,
    previous_refresh: &str,
    tokens: &Tokens,
    before_commit: impl FnOnce(),
) -> io::Result<bool> {
    write_back_with_digest(
        home,
        &refresh_digest(previous_refresh),
        tokens,
        before_commit,
    )
}

fn write_back_with_digest(
    home: &Path,
    expected_digest: &str,
    tokens: &Tokens,
    before_commit: impl FnOnce(),
) -> io::Result<bool> {
    let writer_lock_path = crate::paths::codex_auth_lock_file(home);
    let writer_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(writer_lock_path)?;
    writer_lock.lock()?;
    // Keep this handle alive through the final comparison and rename. Codex does not take
    // this advisory lock; it protects cooperating tcr writers, not uncooperative processes.
    let _writer_lock = writer_lock;
    let path = crate::paths::codex_auth_file(home);
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "refusing to replace a symlinked Codex login",
        ));
    }
    let mut auth = read(home)?;
    if refresh_digest(&auth.login.refresh_token) != expected_digest {
        return Ok(false);
    }
    let refreshed = tokens
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "refresh response has no refresh token",
            )
        })?;
    let stored = auth
        .document
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(invalid_document)?;
    if let Some(id_token) = &tokens.id_token {
        stored.insert("id_token".to_string(), Value::String(id_token.clone()));
    }
    stored.insert(
        "access_token".to_string(),
        Value::String(tokens.access_token.clone()),
    );
    stored.insert(
        "refresh_token".to_string(),
        Value::String(refreshed.to_string()),
    );
    let refreshed_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| invalid_document())?;
    auth.document["last_refresh"] = Value::String(refreshed_at);
    let mut bytes = serde_json::to_vec_pretty(&auth.document).map_err(|_| invalid_document())?;
    bytes.push(b'\n');
    let mut temporary = PrivateTokenFile {
        temporary: Some(
            tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o600))
                .prefix(crate::paths::CODEX_AUTH_TEMP_PREFIX)
                .suffix(".tmp")
                .tempfile_in(home)?,
        ),
    };
    temporary.file_mut().write_all(&bytes)?;
    temporary.file_mut().as_file().sync_all()?;
    // Compare all bytes so an unrelated field edited while the temp was prepared is preserved
    // too. A Codex write completed before this read is preserved. Codex ignores our lock,
    // so a later external write can still race the rename; this is not an atomic CAS.
    before_commit();
    if fs::symlink_metadata(&auth.path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "refusing to replace a symlinked Codex login",
        ));
    }
    let current = fs::read(&auth.path)?;
    if current != auth.original {
        let current_document = serde_json::from_slice::<Value>(&current).ok();
        let refresh_changed = current_document
            .as_ref()
            .and_then(|document| document.get("tokens"))
            .and_then(|tokens| tokens.get("refresh_token"))
            .and_then(Value::as_str)
            .is_some_and(|refresh| refresh_digest(refresh) != expected_digest);
        if refresh_changed {
            return Ok(false);
        }
        // Preserve a metadata edit or a temporarily incomplete Codex write, while retaining
        // the latest pending rotation for a later attempt against the same spent source token.
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Codex login changed during token synchronization; retry the guarded write",
        ));
    }
    temporary.publish(&auth.path)?;
    let _ = fs::File::open(home).and_then(|directory| directory.sync_all());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn auth(refresh: &str) -> Value {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("at-{refresh}"),
                "refresh_token": refresh,
                "account_id": "11111111-1111-4111-8111-111111111111",
            },
            "last_refresh": "2026-10-01T00:00:00Z",
            "other": {"preserve": true},
        })
    }

    fn tokens(refresh: &str) -> Tokens {
        Tokens {
            id_token: None,
            access_token: format!("at-{refresh}"),
            refresh_token: Some(refresh.to_string()),
            expires_at_ms: crate::now_ms() + 3_600_000,
        }
    }

    #[test]
    fn a_codex_rotation_while_the_temporary_file_is_prepared_is_preserved() {
        let home = tempfile::tempdir().unwrap();
        let path = crate::paths::codex_auth_file(home.path());
        fs::write(&path, auth("rt-old").to_string()).unwrap();
        let rotated = auth("rt-codex-new").to_string();

        let written =
            write_back_before_commit(home.path(), "rt-old", &tokens("rt-tcr-new"), || {
                // Codex's current file backend writes in place without our advisory lock.
                fs::write(&path, &rotated).unwrap();
            })
            .unwrap();

        assert!(
            !written,
            "a file changed during temporary preparation must not be replaced"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), rotated);
    }

    #[test]
    fn a_codex_metadata_edit_keeps_the_spent_source_token_retryable() {
        let home = tempfile::tempdir().unwrap();
        let path = crate::paths::codex_auth_file(home.path());
        fs::write(&path, auth("rt-old").to_string()).unwrap();
        let mut edited = auth("rt-old");
        edited["codex_cli_field"] = Value::Bool(true);
        let original = edited.to_string();
        let result = write_back_before_commit(home.path(), "rt-old", &tokens("rt-new"), || {
            fs::write(&path, &original).unwrap();
        });
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "a metadata conflict must retain the manager's pending token publication"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(write_back(home.path(), "rt-old", &tokens("rt-new")).unwrap());
        let stored: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(stored["tokens"]["refresh_token"], "rt-new");
        assert_eq!(stored["codex_cli_field"], true);
    }

    #[test]
    fn two_tcr_writers_serialize_and_recheck_the_imported_refresh_token() {
        let home = tempfile::tempdir().unwrap();
        let path = crate::paths::codex_auth_file(home.path());
        fs::write(&path, auth("rt-old").to_string()).unwrap();
        let first_home = home.path().to_path_buf();
        let (prepared, ready) = mpsc::channel();
        let (release, commit) = mpsc::channel();
        let first = std::thread::spawn(move || {
            write_back_before_commit(&first_home, "rt-old", &tokens("rt-first"), || {
                prepared.send(()).unwrap();
                commit.recv().unwrap();
            })
            .unwrap()
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let second_home = home.path().to_path_buf();
        let (finished, done) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let second = std::thread::spawn(move || {
            started.send(()).unwrap();
            let written = write_back(&second_home, "rt-old", &tokens("rt-second")).unwrap();
            finished.send(written).unwrap();
            written
        });
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        let held = matches!(
            done.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        release.send(()).unwrap();
        let first_written = first.join().unwrap();
        let second_written = second.join().unwrap();

        assert!(held, "a second tcr writer bypassed the first writer's lock");
        assert!(first_written);
        assert!(
            !second_written,
            "the follower must re-read the newly rotated token"
        );
        let stored: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(stored["tokens"]["refresh_token"], "rt-first");
    }
    #[test]
    fn a_symlinked_codex_auth_file_is_not_replaced() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("linked-login.json");
        let path = crate::paths::codex_auth_file(home.path());
        let original = auth("rt-old").to_string();
        fs::write(&target, &original).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let result = write_back(home.path(), "rt-old", &tokens("rt-new"));
        assert!(
            result.is_err(),
            "symlinked credentials require an explicit writeback refusal"
        );
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), original);
    }

    #[test]
    fn a_failed_codex_auth_rename_leaves_no_live_temporary_credentials() {
        let home = tempfile::tempdir().unwrap();
        let path = crate::paths::codex_auth_file(home.path());
        let original = auth("rt-old").to_string();
        fs::write(&path, &original).unwrap();
        let result = write_back_before_commit(home.path(), "rt-old", &tokens("rt-new"), || {
            fs::set_permissions(home.path(), fs::Permissions::from_mode(0o500)).unwrap();
        });
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "the read-only directory must prevent the rename"
        );
        let leftovers: Vec<_> = fs::read_dir(home.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(crate::paths::CODEX_AUTH_TEMP_PREFIX) && name.ends_with(".tmp")
            })
            .map(|entry| entry.metadata().unwrap().len())
            .collect();
        assert!(
            leftovers.iter().all(|&length| length == 0),
            "failed publication left live credentials in temporary files: {leftovers:?}"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn a_codex_conflict_in_a_read_only_home_leaves_no_temporary_credentials() {
        let home = tempfile::tempdir().unwrap();
        let path = crate::paths::codex_auth_file(home.path());
        fs::write(&path, auth("rt-old").to_string()).unwrap();
        let mut edited = auth("rt-old");
        edited["codex_cli_field"] = Value::Bool(true);
        let original = edited.to_string();
        let result = write_back_before_commit(home.path(), "rt-old", &tokens("rt-new"), || {
            fs::write(&path, &original).unwrap();
            fs::set_permissions(home.path(), fs::Permissions::from_mode(0o500)).unwrap();
        });
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read_to_string(path).unwrap(), original);
        let lengths: Vec<_> = fs::read_dir(home.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .map(|entry| entry.metadata().unwrap().len())
            .collect();
        assert!(
            lengths.iter().all(|&length| length == 0),
            "an early conflict left live temporary credentials: {lengths:?}"
        );
    }
}
