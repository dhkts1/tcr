//! Synchronize an explicitly imported Codex login with its original `auth.json`.
//!
//! No ambient home or default login is read here. The manager supplies the recorded `codexHome`
//! while holding that account's refresh lock. Codex's file has `tokens.access_token`,
//! `tokens.refresh_token`, `tokens.id_token`, `tokens.account_id` and top-level `last_refresh`
//! (`openai/codex` `login/src/auth/storage.rs` and `login/src/token_data.rs`). Updating the JSON
//! value preserves every other field, including token fields a later Codex version adds.
//!
//! A write uses a private temporary file in the same directory and compares the original file
//! again immediately before renaming. That final check is why this cannot use the unconditional
//! config writer: Codex may have rotated its single-use token during our upstream refresh.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

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
pub(crate) async fn write_back_async(
    home: &Path,
    previous_refresh: &str,
    tokens: &Tokens,
) -> io::Result<bool> {
    let home = home.to_path_buf();
    let previous_refresh = previous_refresh.to_string();
    let tokens = tokens.clone();
    tokio::task::spawn_blocking(move || write_back(&home, &previous_refresh, &tokens))
        .await
        .map_err(|_| io::Error::other("Codex login write task failed"))?
}

impl ImportedAuth {
    /// A changed, live token pair for the same ChatGPT workspace, and the same user when known.
    /// JWT issuance must advance, or an unchanged/missing issuance time needs an advancing
    /// file timestamp whose baseline belonged to the current refresh token. A different live
    /// pair alone proves no freshness: a failed writeback can leave an older pair on disk.
    pub(crate) fn newer_tokens(
        &self,
        previous_refresh: &str,
        previous_access: &str,
        account_id: Option<&str>,
        user_id: Option<&str>,
        after_ms: Option<i64>,
    ) -> Option<Tokens> {
        let expected_account = account_id.filter(|id| !id.is_empty())?;
        if self.login.refresh_token == previous_refresh
            || self.login.access_token == previous_access
            || self.login.account_id.as_deref() != Some(expected_account)
            || self.login.is_fedramp
            || user_id
                .zip(self.login.user_id.as_deref())
                .is_some_and(|(old, new)| old != new)
            || after_ms
                .is_some_and(|previous| self.last_refresh_ms.is_none_or(|now| now <= previous))
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
            // from a file that still held the current refresh token before this POST.
            _ if after_ms.is_some() => {}
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
        let expires_at_ms = self
            .login
            .expires_at_ms
            .filter(|&expiry| expiry > crate::now_ms())?;
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

/// `false` means the file changed: no destination bytes were written. Unknown fields and the
/// original id token are retained unless the refresh endpoint returned a replacement.
pub(crate) fn write_back(home: &Path, previous_refresh: &str, tokens: &Tokens) -> io::Result<bool> {
    let mut auth = read(home)?;
    if auth.login.refresh_token != previous_refresh {
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
    let mut temporary = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o600))
        .prefix(crate::paths::CODEX_AUTH_TEMP_PREFIX)
        .suffix(".tmp")
        .tempfile_in(home)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    // Compare all bytes so an unrelated field edited while the temp was prepared is preserved
    // too. In particular, a newly rotated refresh token can never be overwritten by this write.
    if fs::read(&auth.path)? != auth.original {
        return Ok(false);
    }
    if let Err(failure) = temporary.persist(&auth.path) {
        let error = failure.error;
        // Preserve the private, fully written recovery file if publication itself failed.
        let _ = failure.file.keep();
        return Err(error);
    }
    let _ = fs::File::open(home).and_then(|directory| directory.sync_all());
    Ok(true)
}
