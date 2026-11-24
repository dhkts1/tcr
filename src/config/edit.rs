//! The targeted read-modify-write every single-key writer uses, and the writers
//! for one entry's flags, the control account and the request key.

use std::path::Path;

use serde_json::{Map, Value};

use super::entry::{EntryMatch, EntryRefusal, find_account_entry, locate_account_entry};
use super::file::{read_document, write_atomic};
use super::group_edit::GroupWrite;
use super::{Account, ConfigError, ControlAccountRef, ProxyConfig};

/// What [`save_request_key`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKeyWrite {
    /// The file had no `proxy.requestKey`; this key is now in it.
    Written(String),
    /// Another writer put one there after this process loaded the file. That
    /// one is kept and returned, so the server and `tcr request-key` agree.
    AlreadyPresent(String),
}

/// Persist `proxy.requestKey = candidate` into the file at `path`, unless the
/// file already holds one, leaving every other key exactly as it is.
///
/// The same read-modify-write as every other targeted writer here
/// ([`save_control_account`], [`save_group_membership`]): [`read_document`] then
/// [`write_atomic`], never a whole-`Config` round trip, so nothing the server
/// did not load is reverted. Every later server write keeps the key: the merge
/// writers touch only their own fields, and the whole-file fallbacks serialize
/// [`ProxyConfig::request_key`] from memory, which the server sets from this
/// call's answer.
pub fn save_request_key(path: &Path, candidate: &str) -> Result<RequestKeyWrite, ConfigError> {
    let mut doc = read_document(path)?;
    let proxy = doc
        .entry("proxy".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(proxy) = proxy else {
        // A `proxy` that is not an object is a file `load` would already have
        // refused, so this is unreachable from a booted server; refuse rather
        // than overwrite it.
        return Err(ConfigError::Parse(
            <serde_json::Error as serde::de::Error>::custom(
                "`proxy` in the config is not an object",
            ),
        ));
    };
    if let Some(existing) = proxy
        .get("requestKey")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    {
        return Ok(RequestKeyWrite::AlreadyPresent(existing.to_string()));
    }
    proxy.insert(
        "requestKey".to_string(),
        Value::String(candidate.to_string()),
    );
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    Ok(RequestKeyWrite::Written(candidate.to_string()))
}

/// The `proxy.requestKey` in the file at `path`, read without loading the rest
/// of the config: `tcr request-key` must not run [`load`], which can rewrite
/// the file (account renames). `Ok(None)` when the file holds none or an empty
/// one.
pub fn read_request_key(path: &Path) -> Result<Option<String>, ConfigError> {
    let doc = read_document(path)?;
    let Some(proxy) = doc.get("proxy") else {
        return Ok(None);
    };
    let proxy: ProxyConfig = serde_json::from_value(proxy.clone())?;
    Ok(proxy.request_key.filter(|key| !key.is_empty()))
}

/// Private recovery metadata stored beside a Codex account's current credentials.
pub(crate) const CODEX_SYNC_PENDING_KEY: &str = "codexTokenSyncPending";

/// Update publication metadata without replacing any credential or user setting.
/// A concurrent re-login with another refresh token owns its own recovery record.
pub(crate) fn save_codex_token_sync_pending(
    path: &Path,
    target: &Account,
    expected_refresh: &str,
    pending: Option<&Value>,
) -> Result<(), ConfigError> {
    let mut document = read_document(path)?;
    let Ok(entry) = find_account_entry(&mut document, target) else {
        return Ok(());
    };
    if entry.get("refreshToken").and_then(Value::as_str) != Some(expected_refresh)
        || entry.get(CODEX_SYNC_PENDING_KEY) == pending
    {
        return Ok(());
    }
    match pending {
        Some(value) => {
            entry.insert(CODEX_SYNC_PENDING_KEY.to_string(), value.clone());
        }
        None => {
            entry.remove(CODEX_SYNC_PENDING_KEY);
        }
    }
    write_atomic(path, &serde_json::to_string_pretty(&document)?)?;
    Ok(())
}

/// What a targeted write did to the on-disk document when its target cannot be
/// missing (a group name rather than an `accounts` entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocEdit {
    /// The document changed and the file was rewritten.
    Updated,
    /// The document already said exactly this, so nothing was written and the
    /// file is byte-identical. A file holding single-use refresh tokens is
    /// rewritten only when it must be.
    Unchanged,
}

/// The outcome of one targeted read-modify-write, read the two ways its
/// callers need: [`edit_document`] asks whether to write the file, and the
/// manager's write-through asks whether memory may follow it.
pub trait Edit {
    /// The document changed, so the file must be rewritten.
    fn wrote(&self) -> bool;
    /// The file now carries the requested state: it was written, or it
    /// already said exactly this. Every other outcome left the file without it.
    fn landed(&self) -> bool;
}

/// `Edit` for the outcome enums whose success arms are `Updated` and
/// `Unchanged`; with `refusable`, also `From<EntryRefusal>` for the ones that
/// carry `NoEntry` and `Ambiguous`.
macro_rules! doc_outcome {
    (refusable $($outcome:ident),+) => {$(
        doc_outcome!($outcome);
        impl From<EntryRefusal> for $outcome {
            fn from(refusal: EntryRefusal) -> Self {
                match refusal {
                    EntryRefusal::NoEntry => Self::NoEntry,
                    EntryRefusal::Ambiguous => Self::Ambiguous,
                }
            }
        }
    )+};
    ($($outcome:ident),+) => {$(
        impl Edit for $outcome {
            fn wrote(&self) -> bool {
                matches!(self, Self::Updated)
            }
            fn landed(&self) -> bool {
                matches!(self, Self::Updated | Self::Unchanged)
            }
        }
    )+};
}

doc_outcome!(DocEdit, ControlWrite);
doc_outcome!(refusable DisabledWrite, PlanWrite, GroupWrite);

/// Read the raw document at `path`, apply `edit` to it, and rewrite the file
/// atomically only when the outcome says the document changed. The frame of
/// every targeted writer: the edit runs on the file's RAW JSON, never on a
/// `Config` round-trip, so a key the user edited while the proxy ran survives.
pub(super) fn edit_document<E: Edit>(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<E, ConfigError>,
) -> Result<E, ConfigError> {
    let mut doc = read_document(path)?;
    let outcome = edit(&mut doc)?;
    if outcome.wrote() {
        write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(outcome)
}

/// What a targeted [`save_disabled`] did to the on-disk document. Reported
/// rather than swallowed so the caller can say, in one line, whether a benched
/// account will actually still be benched after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledWrite {
    /// The flag was set (or dropped) on the matching entry and the file rewritten.
    Updated,
    /// The document already said exactly this, so nothing was written and the
    /// file is byte-identical. A file holding single-use refresh tokens is
    /// rewritten only when it must be.
    Unchanged,
    /// Nothing on disk carries that identity — the entry was deleted or renamed
    /// while the proxy ran — or the document has no usable `accounts` array.
    /// Nothing was written, so the flag will NOT survive a restart.
    NoEntry,
    /// More than one entry carries that identity and nothing stored breaks the
    /// tie, so no entry can be chosen without guessing. Nothing was written.
    ///
    /// [`crate::identity::same_identity`] falls back to name equality when either
    /// side lacks a uuid, so two entries sharing a name both match either runtime
    /// row. The caller selects by ROW INDEX (the TUI does), so silently taking the
    /// first match lands the flag on whichever entry happens to be earlier —
    /// benching a healthy account and returning an exhausted one to rotation, with
    /// the TUI showing the opposite. Refusing is the same posture the CLI takes on
    /// an ambiguous query.
    ///
    /// This is now genuine ambiguity only. `same_identity` ALSO treats an unknown
    /// org as a match, which used to make the legacy two-org shape (an entry with
    /// an org beside a pre-org entry that has none) report `Ambiguous` even though
    /// the two entries are trivially distinguishable — so neither of two real
    /// accounts could ever be durably benched. [`crate::identity::resolve`] breaks
    /// that tie on the org key; see [`find_account_entry`].
    Ambiguous,
}

impl std::fmt::Display for DisabledWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::NoEntry => "no matching entry on disk",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY the `disabled` flag of the one account matching `target`'s
/// identity into the file at `path`, leaving every other key — and every other
/// account — exactly as the user left it.
///
/// Same read-modify-write shape as [`save_tokens`], and for the same reason: the
/// running server's `Config` is a boot-time snapshot, so writing it whole (what
/// [`save`] does) reverts every setting the user edited while the proxy ran. The
/// edit therefore runs on the file's RAW JSON document and never on a `Config`
/// round-trip, so a key the user just deleted cannot reappear as its serde
/// default.
///
/// `disabled == false` REMOVES the key rather than writing `false` — matching
/// the CLI contract pinned by `cli::tests::set_enabled_false_drops_the_disabled_key`.
/// A stale `false`
/// already on disk is dropped for the same reason.
///
/// Unlike [`save_tokens`] there is deliberately NO whole-config fallback when the
/// file is unreadable or malformed. A rotated refresh token is single-use, so
/// losing one is unrecoverable and worth the clobber risk; a lost `disabled` flag
/// costs one un-benched account and is fixed by pressing `d` again. Writing a
/// whole boot-time snapshot over a file we could not even parse is the exact
/// clobber this module exists to prevent, so the error comes back instead.
///
/// An identity matching MORE than one entry writes nothing and reports
/// [`DisabledWrite::Ambiguous`] — see [`find_account_entry`] for why guessing the
/// first match is worse than refusing.
pub fn save_disabled(
    path: &Path,
    target: &Account,
    disabled: bool,
) -> Result<DisabledWrite, ConfigError> {
    edit_document(path, |doc| Ok(merge_disabled(doc, target, disabled)))
}

/// Set or remove `disabled` on the one entry in `doc` matching `target`. Reports
/// whether the document actually changed, so the caller can skip a pointless
/// rewrite of a credential file.
fn merge_disabled(doc: &mut Map<String, Value>, target: &Account, disabled: bool) -> DisabledWrite {
    let entry = match find_account_entry(doc, target) {
        Ok(entry) => entry,
        // No entry, or too many to choose between — either way nothing is written.
        Err(refusal) => return refusal.into(),
    };
    // `true` writes the key; `false` DROPS it (never a `false` literal).
    let desired = disabled.then_some(Value::Bool(true));
    if entry.get("disabled").cloned() == desired {
        return DisabledWrite::Unchanged;
    }
    match desired {
        Some(value) => entry.insert("disabled".to_string(), value),
        None => entry.remove("disabled"),
    };
    DisabledWrite::Updated
}

/// What a targeted [`save_plan`] did to the on-disk document. Same shape as
/// [`DisabledWrite`], and for the same reason: the caller has to be able to say
/// in one line whether the plan it just learned will survive a restart, or
/// whether it will be re-fetched on every probe forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWrite {
    /// The plan fields were written and the file rewritten.
    Updated,
    /// The entry already carried exactly these values, so nothing was written
    /// and the file is byte-identical. This is the steady state once a
    /// backfilled fleet has been written once.
    Unchanged,
    /// Nothing on disk carries that identity — deleted or renamed since the
    /// caller loaded the config. Nothing was written.
    NoEntry,
    /// More than one entry carries that identity; nothing was written. Same
    /// refusal, for the same reason, as [`DisabledWrite::Ambiguous`] — and it
    /// is not hypothetical here: the fleet this backfill exists for is exactly
    /// the one holding the same email in two orgs.
    Ambiguous,
}

impl std::fmt::Display for PlanWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::NoEntry => "no matching entry on disk",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY the plan fields (`organizationType`, `rateLimitTier`, `planDisplayName`,
/// `seatTier`) of the one account matching `target`'s identity, leaving every
/// other key — and every other account — exactly as the user left it.
///
/// Same read-modify-write shape as [`save_disabled`], and for the same reason it
/// spells out: the running server's `Config` is a boot-time snapshot, so writing
/// it whole (what [`save`] does) reverts every setting the user edited while the
/// proxy ran. This one runs on the probe loop, which is the worst possible place
/// to hold a stale snapshot — it fires every few minutes for the whole life of
/// the process.
///
/// A `None` argument writes nothing for that field rather than removing the key:
/// the caller is a profile fetch that may legitimately have learned one field and
/// not another — a Max org genuinely has no seat tier — and "I did not learn
/// this" must never erase what we already know. All `None` is therefore
/// [`PlanWrite::Unchanged`], never a rewrite.
///
/// Values are OVERWRITTEN when they differ — a plan changes (see the doc-comment
/// on [`Account::organization_type`]) — which is what makes the `Unchanged`
/// comparison, rather than a presence check, the right guard here.
pub fn save_plan(
    path: &Path,
    target: &Account,
    organization_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    seat_tier: Option<&str>,
    plan_display_name: Option<&str>,
) -> Result<PlanWrite, ConfigError> {
    edit_document(path, |doc| {
        Ok(merge_plan(
            doc,
            target,
            organization_type,
            rate_limit_tier,
            seat_tier,
            plan_display_name,
        ))
    })
}

/// Set the plan fields on the one entry in `doc` matching `target`. Reports
/// whether the document actually changed, so the caller can skip a pointless
/// rewrite of a credential file on every probe. See [`save_plan`].
fn merge_plan(
    doc: &mut Map<String, Value>,
    target: &Account,
    organization_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    seat_tier: Option<&str>,
    plan_display_name: Option<&str>,
) -> PlanWrite {
    let entry = match find_account_entry(doc, target) {
        Ok(entry) => entry,
        Err(refusal) => return refusal.into(),
    };
    let mut changed = false;
    for (key, value) in [
        ("organizationType", organization_type),
        ("rateLimitTier", rate_limit_tier),
        ("seatTier", seat_tier),
        ("planDisplayName", plan_display_name),
    ] {
        let Some(value) = value else { continue };
        let desired = Value::String(value.to_string());
        if entry.get(key) == Some(&desired) {
            continue;
        }
        entry.insert(key.to_string(), desired);
        changed = true;
    }
    if changed {
        PlanWrite::Updated
    } else {
        PlanWrite::Unchanged
    }
}

/// What a targeted [`save_control_account`] did to the on-disk document. Same
/// shape as [`DisabledWrite`] now, and for the same reason: writing the
/// identity object requires resolving `target` against the on-disk
/// `accounts[]` first, and an identity matching more than one entry cannot be
/// written without guessing which row "is" the control account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlWrite {
    /// The key was set (or removed) and the file rewritten.
    Updated,
    /// The document already said exactly this; nothing was written.
    Unchanged,
    /// More than one on-disk entry carries `target`'s identity; nothing was
    /// written. Same refusal as [`DisabledWrite::Ambiguous`], for the same
    /// reason: guessing which entry the new control account "is" risks
    /// silently binding control to the wrong row.
    Ambiguous,
}

impl std::fmt::Display for ControlWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY the top-level `controlAccount` key into the file at `path`,
/// leaving every other key exactly as the user left it. Same read-modify-write
/// shape as [`save_disabled`] and for the same reason: the running server's
/// `Config` is a boot-time snapshot, so writing it whole (what [`save`] does)
/// would revert every setting the user edited while the proxy ran — that
/// clobber is the exact bug fixed in `1d978ce`. The edit therefore runs on the
/// file's RAW JSON document via [`read_document`] + [`write_atomic`], never on
/// a `Config` round-trip through [`save`].
///
/// `target == None` REMOVES the key (matches `#[serde(skip_serializing_if =
/// "Option::is_none")]` on [`Config::control_account`]) rather than writing
/// `null` — the same "clear removes, never sets to a null-ish literal"
/// contract [`save_disabled`] uses for `disabled == false`.
///
/// `target == Some(account)` writes the [`ControlAccountRef::Identity`] shape
/// built from `account`'s own identity fields — never the legacy bare-string
/// shape, even when the document currently has one — after checking, via
/// [`locate_account_entry`] (the same resolver [`find_account_entry`] is built
/// on, reused rather than re-implemented), that `account`'s identity does not
/// match more than one on-disk entry. It is deliberately NOT an error for
/// `account` to match NO on-disk entry: the caller is free to name a rotation
/// slot that has not been written to disk yet, exactly as the legacy `&str`
/// signature always allowed.
pub fn save_control_account(
    path: &Path,
    target: Option<&Account>,
) -> Result<ControlWrite, ConfigError> {
    edit_document(path, |doc| Ok(merge_control_account(doc, target)))
}

/// Set or remove the top-level `controlAccount` key in `doc`. Reports whether
/// the document actually changed, so the caller can skip a pointless rewrite
/// of a credential file. See [`save_control_account`].
fn merge_control_account(doc: &mut Map<String, Value>, target: Option<&Account>) -> ControlWrite {
    if let Some(account) = target
        && locate_account_entry(doc, account) == EntryMatch::Many
    {
        return ControlWrite::Ambiguous;
    }
    let desired = match target {
        Some(account) => match serde_json::to_value(ControlAccountRef::from(account)) {
            Ok(value) => Some(value),
            Err(error) => {
                // Unreachable: a name always serializes. Leave the file alone.
                tracing::warn!(%error, "controlAccount reference did not serialize");
                return ControlWrite::Unchanged;
            }
        },
        None => None,
    };
    if doc.get("controlAccount").cloned() == desired {
        return ControlWrite::Unchanged;
    }
    match desired {
        Some(value) => doc.insert("controlAccount".to_string(), value),
        None => doc.remove("controlAccount"),
    };
    ControlWrite::Updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{DISABLE_SAMPLE, read_json, tmp_file, tmp_path};
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    /// An identity probe in the legacy (no-uuid) shape every real config uses,
    /// where `same_identity` reduces to name equality.
    fn by_name(name: &str) -> Account {
        crate::identity::probe(crate::provider::Provider::Anthropic, name, None, None, None)
    }

    /// Disabling writes `"disabled": true` onto the right entry and changes
    /// NOTHING else — the whole point of editing the raw document instead of
    /// round-tripping a boot-time `Config`. Strip the one key we asked for and
    /// the document must be identical to what the user had.
    #[test]
    fn disable_writes_the_flag_and_changes_nothing_else() {
        let path = tmp_path("disable-write");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), true).unwrap(),
            DisabledWrite::Updated
        );

        let mut after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        after["accounts"][0]
            .as_object_mut()
            .expect("the entry is an object")
            .remove("disabled");
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "the write touched something other than the disabled flag"
        );
        fs::remove_file(&path).ok();
    }

    /// Re-enabling DROPS the key rather than writing `false`, matching the CLI
    /// contract pinned by `cli::tests::set_enabled_false_drops_the_disabled_key`.
    /// A full disable→enable round trip must leave the file as it started.
    #[test]
    fn re_enable_drops_the_key_and_round_trips_the_document() {
        let path = tmp_path("disable-roundtrip");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "re-enable must DROP the disabled key, not write false: {after}"
        );
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "a disable→enable round trip must leave the document as it started"
        );
        fs::remove_file(&path).ok();
    }

    /// The unrelated account and the unmodelled keys (`warmupSeconds`, `pacing`,
    /// `routes`, per-account `models`) survive the write untouched. Named
    /// separately from the equality check above so weakening that one still trips
    /// a gate on the preserve-unknown-keys guarantee.
    #[test]
    fn disable_leaves_other_accounts_and_unmodelled_keys_untouched() {
        let path = tmp_path("disable-collateral");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();

        let after = read_json(&path);
        assert_eq!(after["warmupSeconds"], json!(900));
        assert_eq!(after["pacing"]["maxInFlightPerAccount"], json!(3));
        assert!(after["routes"].is_array());
        assert_eq!(after["accounts"][0]["models"], json!(["claude-fable-5"]));
        // The account we did NOT name keeps its credentials and gains no flag.
        assert_eq!(after["accounts"][1]["name"], json!("acct-b"));
        assert_eq!(after["accounts"][1]["accessToken"], json!("at-b"));
        assert_eq!(after["accounts"][1]["refreshToken"], json!("rt-b"));
        assert!(
            after["accounts"][1].get("disabled").is_none(),
            "the unrelated account was flagged too: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// A redundant write — the document already says exactly this — reports
    /// `Unchanged` and does not rewrite the file. A file holding single-use
    /// refresh tokens is not rewritten to say what it already said.
    #[test]
    fn redundant_write_reports_unchanged_and_leaves_the_file_alone() {
        let path = tmp_path("disable-noop");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        // Already enabled (no key at all) → re-enabling is a no-op.
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Unchanged
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            DISABLE_SAMPLE,
            "an Unchanged write must leave the file byte-identical, not reformat it"
        );

        // And once disabled, disabling again is a no-op too.
        save_disabled(&path, &by_name("acct-a"), true).unwrap();
        let disabled_text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), true).unwrap(),
            DisabledWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), disabled_text);
        fs::remove_file(&path).ok();
    }

    // --- save_control_account -----------------------------------------------

    /// Setting `controlAccount` writes the top-level key — the account's NAME,
    /// which is unique and therefore the whole key — and changes nothing else,
    /// same "raw document, not a `Config` round trip" contract as
    /// `save_disabled`. This is
    /// also `control_persist_preserves_unmodelled_top_level_keys` from the
    /// bridge: a key nothing here models (`routes`) survives the write.
    #[test]
    fn set_control_writes_the_key_and_changes_nothing_else() {
        let path = tmp_path("control-write");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_control_account(&path, Some(&by_name("acct-a"))).unwrap(),
            ControlWrite::Updated
        );

        let mut after = read_json(&path);
        assert_eq!(after["controlAccount"], json!("acct-a"));
        after
            .as_object_mut()
            .expect("document is an object")
            .remove("controlAccount");
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "the write touched something other than controlAccount"
        );
        fs::remove_file(&path).ok();
    }

    /// Clearing (`target: None`) REMOVES the key rather than writing `null` —
    /// matching `#[serde(skip_serializing_if = "Option::is_none")]` on
    /// `Config::control_account`, and a set→clear round trip leaves the file
    /// exactly as it started.
    #[test]
    fn clear_control_drops_the_key_and_round_trips_the_document() {
        let path = tmp_path("control-roundtrip");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        save_control_account(&path, Some(&by_name("acct-a"))).unwrap();
        assert_eq!(
            save_control_account(&path, None).unwrap(),
            ControlWrite::Updated
        );

        let after = read_json(&path);
        assert!(
            after.get("controlAccount").is_none(),
            "clearing must DROP the key, not write null: {after}"
        );
        let before: Value = serde_json::from_str(DISABLE_SAMPLE).unwrap();
        assert_eq!(
            after, before,
            "a set→clear round trip must leave the document as it started"
        );
        fs::remove_file(&path).ok();
    }

    /// A redundant write — the document already names this control account —
    /// reports `Unchanged` and leaves the file byte-identical.
    #[test]
    fn redundant_control_write_reports_unchanged_and_leaves_the_file_alone() {
        let path = tmp_path("control-noop");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        // Already unset → clearing again is a no-op.
        assert_eq!(
            save_control_account(&path, None).unwrap(),
            ControlWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), DISABLE_SAMPLE);

        save_control_account(&path, Some(&by_name("acct-a"))).unwrap();
        let with_control = fs::read_to_string(&path).unwrap();
        assert_eq!(
            save_control_account(&path, Some(&by_name("acct-a"))).unwrap(),
            ControlWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), with_control);
        fs::remove_file(&path).ok();
    }

    /// Two on-disk entries share `target`'s identity (both share a name and
    /// carry no uuid, so [`crate::identity::same_identity`] cannot tell them
    /// apart): the write is refused, `ControlWrite::Ambiguous`, and the file is
    /// untouched — mirroring [`DisabledWrite::Ambiguous`] for the same reason.
    #[test]
    fn control_write_on_duplicated_identity_is_refused() {
        let path = tmp_path("control-ambiguous");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "dup@example.com", "accessToken": "at-1" },
                { "name": "dup@example.com", "accessToken": "at-2" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_control_account(&path, Some(&by_name("dup@example.com"))).unwrap(),
            ControlWrite::Ambiguous
        );
        assert!(
            read_json(&path).get("controlAccount").is_none(),
            "an ambiguous write must not land on disk"
        );
        fs::remove_file(&path).ok();
    }

    /// A target carrying an `accountUuid` shared by both on-disk rows PLUS
    /// `orgUuid` resolves the write to the RIGHT one of the two — the "legacy
    /// two-org shape" `locate_account_entry`'s doc-comment describes: one
    /// person, two orgs, one uuid. That resolution is still how a WRITE finds
    /// its row, and it is the one thing the old org flag never was: an identity
    /// comparison, not a query.
    ///
    /// The file here is fed to `save_control_account` directly rather than
    /// through `config::load`, which would have renamed the second row first.
    /// This is the layer below that.
    #[test]
    fn control_write_with_org_uuid_disambiguates_duplicated_email() {
        let path = tmp_path("control-org-disambiguates");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "dup@example.com", "accessToken": "at-1",
                  "accountUuid": "33333333-3333-3333-3333-333333333333",
                  "orgUuid": "11111111-1111-1111-1111-111111111111" },
                { "name": "dup@example.com", "accessToken": "at-2",
                  "accountUuid": "33333333-3333-3333-3333-333333333333",
                  "orgUuid": "22222222-2222-2222-2222-222222222222" }
            ] }"#,
        )
        .unwrap();
        let target = crate::identity::probe(
            crate::provider::Provider::Anthropic,
            "dup@example.com",
            Some("33333333-3333-3333-3333-333333333333".to_string()),
            Some("22222222-2222-2222-2222-222222222222".to_string()),
            None,
        );

        assert_eq!(
            save_control_account(&path, Some(&target)).unwrap(),
            ControlWrite::Updated
        );

        let after = read_json(&path);
        assert_eq!(after["controlAccount"], json!("dup@example.com"));
        fs::remove_file(&path).ok();
    }

    /// A `"disabled": false` already on disk is normalized away on re-enable
    /// rather than left sitting there — same end state the CLI produces.
    #[test]
    fn stale_disabled_false_on_disk_is_dropped() {
        let path = tmp_path("disable-stale-false");
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a", "disabled": false } ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-a"), false).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "a stale false must be dropped, not kept: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// No on-disk entry carries that identity (deleted or renamed while the proxy
    /// ran): report `NoEntry` and write NOTHING, so the caller can warn that the
    /// flag will not survive a restart instead of silently believing it landed.
    #[test]
    fn disable_with_no_matching_entry_writes_nothing() {
        let path = tmp_path("disable-no-entry");
        fs::write(&path, DISABLE_SAMPLE).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-gone"), true).unwrap(),
            DisabledWrite::NoEntry
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            DISABLE_SAMPLE,
            "a no-match write must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// A document with no usable `accounts` list is `NoEntry`, never a fallback
    /// that writes a whole config over it — the clobber `save_tokens` accepts for
    /// a single-use token is NOT worth it for a flag that can be re-set by hand.
    #[test]
    fn disable_with_no_usable_accounts_list_writes_nothing() {
        for document in [r#"{ "upstream": "x" }"#, r#"{ "accounts": "nope" }"#] {
            let path = tmp_path("disable-unusable");
            fs::write(&path, document).unwrap();
            assert_eq!(
                save_disabled(&path, &by_name("acct-a"), true).unwrap(),
                DisabledWrite::NoEntry
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), document);
            fs::remove_file(&path).ok();
        }
    }

    /// An unreadable or malformed file surfaces the ERROR rather than taking
    /// `save_tokens`' whole-config fallback. Writing a boot-time snapshot over a
    /// file we could not parse is the clobber this module exists to prevent.
    #[test]
    fn disable_surfaces_errors_instead_of_clobbering() {
        let missing = tmp_path("disable-missing");
        assert!(!missing.exists());
        assert!(matches!(
            save_disabled(&missing, &by_name("acct-a"), true),
            Err(ConfigError::Io(_))
        ));
        assert!(
            !missing.exists(),
            "a missing config must not be created by a flag write"
        );

        let malformed = tmp_path("disable-malformed");
        fs::write(&malformed, "{ not json").unwrap();
        assert!(matches!(
            save_disabled(&malformed, &by_name("acct-a"), true),
            Err(ConfigError::Parse(_))
        ));
        assert_eq!(
            fs::read_to_string(&malformed).unwrap(),
            "{ not json",
            "a malformed config must be left exactly as found"
        );
        fs::remove_file(&malformed).ok();
    }

    /// `me@example.com` logged into two orgs, A and B, as one person (`uuid-person`).
    const ONE_EMAIL_IN_TWO_ORGS: &str = r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-b", "accountUuid": "uuid-person",
                  "orgUuid": "org-b", "orgName": "Org B" }
            ] }"#;

    /// Disable `me@example.com` in the org `org_uuid` (named `org_name`) in the file at `path`,
    /// and assert the write updated a row.
    fn disable_me_in(org_uuid: &str, org_name: &str, path: &Path) {
        let target = crate::identity::probe(
            crate::provider::Provider::Anthropic,
            "me@example.com",
            Some("uuid-person".to_string()),
            Some(org_uuid.to_string()),
            Some(org_name.to_string()),
        );
        assert_eq!(
            save_disabled(path, &target, true).unwrap(),
            DisabledWrite::Updated
        );
    }

    /// The flag lands on the right ORG when one email is logged into two — the
    /// same identity matching `merge_tokens` uses, so a rotated credential and a
    /// disabled flag can never land on two different entries.
    #[test]
    fn disable_picks_the_right_entry_when_one_email_has_two_orgs() {
        let path = tmp_file("disable-two-orgs", ONE_EMAIL_IN_TWO_ORGS);
        disable_me_in("org-b", "Org B", &path);

        let after = read_json(&path);
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "the flag landed on the wrong org: {after}"
        );
        assert_eq!(after["accounts"][1]["disabled"], json!(true));
        fs::remove_file(&path).ok();
    }

    /// An AMBIGUOUS identity is refused, never resolved to the first match. Two
    /// entries sharing a name both satisfy `same_identity` (it falls back to name
    /// equality when either side lacks a uuid), and the TUI selects by ROW INDEX —
    /// so guessing lands the flag on whichever entry is earlier in the file,
    /// benching a healthy account while the TUI renders the other one disabled.
    /// Nothing is written and the ambiguity is reported distinctly.
    #[test]
    fn disable_refuses_an_ambiguous_identity_and_writes_nothing() {
        let path = tmp_path("disable-ambiguous");
        let document = r#"{ "accounts": [
            { "name": "acct-dup", "accessToken": "at-first" },
            { "name": "acct-dup", "accessToken": "at-second" }
        ] }"#;
        fs::write(&path, document).unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-dup"), true).unwrap(),
            DisabledWrite::Ambiguous
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            document,
            "an ambiguous identity must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// The refusal is scoped to the AMBIGUOUS identity, not to the file: an
    /// unambiguous account in the same document still writes. Without this pair,
    /// "nothing was written" above would be satisfied by a `save_disabled` that had
    /// simply stopped working.
    #[test]
    fn a_duplicate_elsewhere_does_not_block_an_unambiguous_write() {
        let path = tmp_path("disable-ambiguous-neighbour");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "acct-dup", "accessToken": "at-first" },
                { "name": "acct-dup", "accessToken": "at-second" },
                { "name": "acct-unique", "accessToken": "at-unique" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("acct-unique"), true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][2]["disabled"], json!(true));
        assert!(
            after["accounts"][0].get("disabled").is_none()
                && after["accounts"][1].get("disabled").is_none(),
            "only the unambiguous entry may be touched: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// Two entries that share a NAME but are separated by org are NOT ambiguous —
    /// `same_identity` tells them apart, so the write still lands. The refusal must
    /// bite on genuinely indistinguishable entries only, or the two-org config the
    /// identity work exists to support would stop being writable.
    #[test]
    fn two_orgs_under_one_name_are_not_ambiguous() {
        let path = tmp_file("disable-two-orgs-unambiguous", ONE_EMAIL_IN_TWO_ORGS);
        disable_me_in("org-a", "Org A", &path);
        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        assert!(after["accounts"][1].get("disabled").is_none());
        fs::remove_file(&path).ok();
    }

    /// The shape the test above does NOT cover, and the one the refusal actually
    /// broke: the older entry predates org UUIDs and carries none, so its org key
    /// is `(None)` while its sibling's is `Some("org-a")`. `same_identity`
    /// deliberately treats an unknown org as a match — that is what lets a
    /// freshly-profiled login backfill a legacy entry — which also means the
    /// pre-org entry matches BOTH runtime rows. These are two real accounts, one
    /// person in two orgs, and refusing on the second `same_identity` hit left
    /// NEITHER of them durably benchable.
    ///
    /// Each row must reach its own entry: the org-carrying row by the exact match,
    /// and the pre-org row by name — it is the only entry with no org at all.
    #[test]
    fn a_pre_org_entry_beside_its_org_carrying_sibling_is_not_ambiguous() {
        let path = tmp_path("disable-legacy-backfill");
        let file = r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-a", "accountUuid": "uuid-person",
                  "orgUuid": "org-a", "orgName": "Org A" },
                { "name": "me@example.com", "accessToken": "at-legacy", "accountUuid": "uuid-person" }
            ] }"#;

        // The org-carrying row: both entries match it loosely, one matches exactly.
        fs::write(&path, file).unwrap();
        let with_org = crate::identity::probe(
            crate::provider::Provider::Anthropic,
            "me@example.com",
            Some("uuid-person".to_string()),
            Some("org-a".to_string()),
            Some("Org A".to_string()),
        );
        assert_eq!(
            save_disabled(&path, &with_org, true).unwrap(),
            DisabledWrite::Updated,
            "the fully-known identity resolves to the entry that carries the same org"
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["disabled"], json!(true));
        assert!(
            after["accounts"][1].get("disabled").is_none(),
            "the flag must not land on the pre-org sibling: {after}"
        );

        // And the pre-org row, whose own org is still unknown, resolves to the one
        // entry that likewise has none.
        fs::write(&path, file).unwrap();
        let pre_org = crate::identity::probe(
            crate::provider::Provider::Anthropic,
            "me@example.com",
            Some("uuid-person".to_string()),
            None,
            None,
        );
        assert_eq!(
            save_disabled(&path, &pre_org, true).unwrap(),
            DisabledWrite::Updated
        );
        let after = read_json(&path);
        assert_eq!(after["accounts"][1]["disabled"], json!(true));
        assert!(
            after["accounts"][0].get("disabled").is_none(),
            "the flag must not land on the org-carrying sibling: {after}"
        );
        fs::remove_file(&path).ok();
    }

    /// The refusal still fires where it must. Two entries that share a name and
    /// carry no UUID at all are genuinely indistinguishable — there is no stricter
    /// fact to prefer one by — so nothing is written and the caller is told.
    #[test]
    fn two_entries_with_nothing_to_tell_them_apart_are_still_refused() {
        let path = tmp_path("disable-truly-ambiguous");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-one" },
                { "name": "me@example.com", "accessToken": "at-two" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_disabled(&path, &by_name("me@example.com"), true).unwrap(),
            DisabledWrite::Ambiguous
        );
        let after = read_json(&path);
        assert!(after["accounts"][0].get("disabled").is_none());
        assert!(after["accounts"][1].get("disabled").is_none());
        fs::remove_file(&path).ok();
    }

    /// The flag write goes through the same atomic 0600 path as every other
    /// write, so it can never leave the token file world-readable.
    #[test]
    fn save_disabled_writes_owner_only_permissions() {
        let path = tmp_path("disable-perm");
        fs::write(&path, DISABLE_SAMPLE).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save_disabled(&path, &by_name("acct-a"), true).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).ok();
    }

    // --- save_plan (the durable half of the probe-loop plan backfill) -------

    /// The whole point of the backfill: a fleet already on disk with no plan
    /// fields at all gets them, on the one matching row, without a re-login.
    #[test]
    fn save_plan_writes_all_three_fields_onto_a_row_that_had_none() {
        let path = tmp_path("plan-backfill");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a" },
                { "name": "bob@example.com", "accessToken": "at-b" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_plan(
                &path,
                &by_name("alice@example.com"),
                Some("claude_team"),
                Some("default_raven"),
                Some("team_tier_1"),
                None,
            )
            .unwrap(),
            PlanWrite::Updated
        );

        let after = read_json(&path);
        assert_eq!(after["accounts"][0]["organizationType"], "claude_team");
        assert_eq!(after["accounts"][0]["rateLimitTier"], "default_raven");
        assert_eq!(after["accounts"][0]["seatTier"], "team_tier_1");
        assert!(
            after["accounts"][1].get("organizationType").is_none(),
            "only the matching row is touched — a targeted write, not a whole-file save"
        );
        fs::remove_file(&path).ok();
    }

    /// A plan CHANGES (an org upgrades), so a differing value overwrites —
    /// unlike the identity fields, which are backfill-only.
    #[test]
    fn save_plan_overwrites_a_stale_plan_rather_than_backfilling() {
        let path = tmp_path("plan-upgrade");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a",
                  "organizationType": "claude_max", "rateLimitTier": "default_claude_max_5x" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_plan(
                &path,
                &by_name("alice@example.com"),
                Some("claude_max"),
                Some("default_claude_max_20x"),
                None,
                None,
            )
            .unwrap(),
            PlanWrite::Updated
        );
        assert_eq!(
            read_json(&path)["accounts"][0]["rateLimitTier"],
            "default_claude_max_20x",
            "an upgraded org must not keep showing the tier it was first seen on"
        );
        fs::remove_file(&path).ok();
    }

    /// The steady state after the first backfill sweep: nothing changed, so the
    /// credential file is not rewritten on every probe for the rest of time.
    #[test]
    fn save_plan_is_unchanged_when_the_row_already_says_exactly_this() {
        let path = tmp_path("plan-steady");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a",
                  "organizationType": "claude_team", "seatTier": "team_standard" }
            ] }"#,
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();

        assert_eq!(
            save_plan(
                &path,
                &by_name("alice@example.com"),
                Some("claude_team"),
                None,
                Some("team_standard"),
                None,
            )
            .unwrap(),
            PlanWrite::Unchanged
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            before,
            "an unchanged plan must leave the file byte-identical"
        );
        fs::remove_file(&path).ok();
    }

    /// A `None` says "I did not learn this", never "erase what you have" — a
    /// Max org has no seat tier, and that must not blank one already stored.
    #[test]
    fn save_plan_never_erases_a_field_the_caller_did_not_learn() {
        let path = tmp_path("plan-partial");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a",
                  "organizationType": "claude_team", "seatTier": "team_tier_1" }
            ] }"#,
        )
        .unwrap();

        save_plan(
            &path,
            &by_name("alice@example.com"),
            Some("claude_team"),
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            read_json(&path)["accounts"][0]["seatTier"],
            "team_tier_1",
            "a field the caller passed None for must survive untouched"
        );
        fs::remove_file(&path).ok();
    }

    /// The fleet this backfill exists for is exactly the one holding the same
    /// email in two orgs, so the ambiguous case is the live case, not a corner.
    #[test]
    fn save_plan_refuses_an_identity_matching_two_entries() {
        let path = tmp_path("plan-ambiguous");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "me@example.com", "accessToken": "at-one" },
                { "name": "me@example.com", "accessToken": "at-two" }
            ] }"#,
        )
        .unwrap();

        assert_eq!(
            save_plan(
                &path,
                &by_name("me@example.com"),
                Some("claude_max"),
                None,
                None,
                None,
            )
            .unwrap(),
            PlanWrite::Ambiguous
        );
        let after = read_json(&path);
        assert!(after["accounts"][0].get("organizationType").is_none());
        assert!(after["accounts"][1].get("organizationType").is_none());
        fs::remove_file(&path).ok();
    }
}
