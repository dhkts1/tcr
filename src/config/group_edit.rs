//! The writers for group membership and for `groupSettings`.

use std::path::Path;

use serde_json::{Map, Value};

use super::edit::{DocEdit, edit_document};
use super::entry::find_account_entry;
use super::{Account, ConfigError};

/// What a targeted [`save_group_membership`] did to the on-disk document. Same
/// shape as [`DisabledWrite`], for the same reason — the caller must be able to
/// say, in one line, whether a group edit will actually survive a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupWrite {
    /// The label was added to (or removed from) the matching entry's `groups`
    /// array and the file rewritten.
    Updated,
    /// The entry already carried (or already lacked) this label, so nothing
    /// was written and the file is byte-identical.
    Unchanged,
    /// Nothing on disk carries that identity — deleted or renamed since the
    /// caller loaded the config, or no usable `accounts` array. Nothing was
    /// written.
    NoEntry,
    /// More than one entry carries that identity and nothing stored breaks the
    /// tie. Nothing was written — same refusal [`save_disabled`] makes, for
    /// the same reason: guessing which entry to edit risks labelling the
    /// wrong account.
    Ambiguous,
}

impl std::fmt::Display for GroupWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::NoEntry => "no matching entry on disk",
            Self::Ambiguous => "more than one entry on disk carries this identity",
        })
    }
}

/// Persist ONLY one `groups` label of the one account matching `target`'s
/// identity into the file at `path` — add it when `member`, remove it
/// otherwise — leaving every other key, and every other account, exactly as
/// the user left it.
///
/// Same read-modify-write shape as [`save_disabled`] and for the same reason:
/// `Config` is a boot-time snapshot, and rewriting the whole file ([`save`])
/// would revert every out-of-band edit and drop unmodelled forward-compat
/// keys. The edit runs on the file's raw JSON document via [`read_document`] +
/// [`write_atomic`], never on a `Config` round-trip.
///
/// Removing an account's LAST label drops the `groups` key entirely rather
/// than leaving `[]` on disk. `Account::in_group` treats an absent key and an
/// empty array identically, so either choice is behaviourally safe — this
/// picks "absent" so an account that has ever had a label removed does not
/// carry a permanent empty-array litter, matching the `disabled == false`
/// drops-the-key contract [`save_disabled`] uses. `groups_round_trip...` and
/// this module's own tests pin the choice.
pub fn save_group_membership(
    path: &Path,
    target: &Account,
    group: &str,
    member: bool,
) -> Result<GroupWrite, ConfigError> {
    edit_document(path, |doc| {
        Ok(merge_group_membership(doc, target, group, member))
    })
}

/// Add or remove `group` on the one entry in `doc` matching `target`. Reports
/// whether the document actually changed, so the caller can skip a pointless
/// rewrite of a credential file.
fn merge_group_membership(
    doc: &mut Map<String, Value>,
    target: &Account,
    group: &str,
    member: bool,
) -> GroupWrite {
    let entry = match find_account_entry(doc, target) {
        Ok(entry) => entry,
        Err(refusal) => return refusal.into(),
    };
    let mut groups: Vec<String> = entry
        .get("groups")
        .and_then(Value::as_array)
        .map_or_default(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    let already = groups.iter().any(|g| g == group);
    if member == already {
        return GroupWrite::Unchanged;
    }
    if member {
        groups.push(group.to_string());
    } else {
        groups.retain(|g| g != group);
    }
    if groups.is_empty() {
        entry.remove("groups");
    } else {
        entry.insert(
            "groups".to_string(),
            Value::Array(groups.into_iter().map(Value::String).collect()),
        );
    }
    GroupWrite::Updated
}

/// What a targeted [`save_group_reserved`] did to the on-disk document.
pub type GroupReserveWrite = DocEdit;

/// What a targeted [`save_group_parked`] did to the on-disk document.
pub type GroupParkWrite = DocEdit;

/// What a targeted [`save_group_allow_control`] did to the on-disk document.
pub type GroupControlWrite = DocEdit;

/// What a targeted [`save_group_color`] did to the on-disk document.
pub type GroupColorWrite = DocEdit;

/// One key under `groupSettings.<group>` that a writer sets. A group write
/// targets a GROUP NAME, not an `accounts` entry, so it can never miss or be
/// ambiguous: its outcome is a plain [`DocEdit`].
///
/// `spillToPool` ([`GroupSettings::spill_to_pool`]) has no variant because
/// neither the CLI nor TcrBar sets it; it is set by editing the file by hand.
/// A writer for it is one variant here and one line in [`GroupKey::wire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKey {
    Reserved,
    Parked,
    AllowControl,
    Color,
}

impl GroupKey {
    /// The key's spelling in the file.
    const fn wire(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Parked => "parked",
            Self::AllowControl => "allowControlAccount",
            Self::Color => "color",
        }
    }

    /// What the key carries on disk, read the way the writer would write it: a
    /// flag counts only as a literal `true`, a color only as a string. Anything
    /// else reads as unset, so asking for "unset" over a hand-written `false`
    /// (or a value of the wrong type) is `Unchanged` and leaves it on disk.
    fn current(self, raw: Option<&Value>) -> Option<Value> {
        match self {
            Self::Reserved | Self::Parked | Self::AllowControl => raw
                .and_then(Value::as_bool)
                .filter(|set| *set)
                .map(Value::Bool),
            Self::Color => raw
                .and_then(Value::as_str)
                .map(|hex| Value::String(hex.to_string())),
        }
    }
}

/// Persist ONLY `groupSettings.<group>.<key>` into the file at `path`, leaving
/// every other key (every account, every other group's settings) exactly as
/// the user left it. Same read-modify-write shape as [`save_group_membership`]
/// and for the same reason: `Config` is a boot-time snapshot, and a whole-file
/// [`save`] would revert any out-of-band edit.
fn save_group_setting(
    path: &Path,
    group: &str,
    key: GroupKey,
    desired: Option<Value>,
) -> Result<DocEdit, ConfigError> {
    edit_document(path, |doc| Ok(set_group_setting(doc, group, key, desired)))
}

/// Set `groupSettings.<group>.<key>` to `desired`, or remove it on `None`.
///
/// Clearing drops the key rather than writing `false` or `null`, and drops the
/// group's whole `groupSettings` entry once it is empty, and `groupSettings`
/// itself once that is empty. This mirrors the drops-the-key contract
/// [`merge_group_membership`] uses for an account's last label, so a group
/// that has ever had a setting set and then cleared carries no permanent
/// litter.
fn set_group_setting(
    doc: &mut Map<String, Value>,
    group: &str,
    key: GroupKey,
    desired: Option<Value>,
) -> DocEdit {
    let already = key.current(
        doc.get("groupSettings")
            .and_then(Value::as_object)
            .and_then(|settings| settings.get(group))
            .and_then(Value::as_object)
            .and_then(|entry| entry.get(key.wire())),
    );
    if already == desired {
        return DocEdit::Unchanged;
    }

    let settings = doc
        .entry("groupSettings".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    // A hand-edited `groupSettings` that is not an object is corrupt input we
    // must not silently coerce — replace it with a fresh object rather than
    // panicking, since `already` above already proved it carried no usable
    // value for this group either way.
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    let Some(settings_obj) = settings.as_object_mut() else {
        // Unreachable: the guard above just made it an object.
        return DocEdit::Unchanged;
    };

    match desired {
        Some(value) => {
            let entry = settings_obj
                .entry(group.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Some(obj) = entry.as_object_mut() {
                obj.insert(key.wire().to_string(), value);
            }
        }
        None => {
            if let Some(entry) = settings_obj.get_mut(group)
                && let Some(obj) = entry.as_object_mut()
            {
                obj.remove(key.wire());
                if obj.is_empty() {
                    settings_obj.remove(group);
                }
            }
        }
    }
    if settings_obj.is_empty() {
        doc.remove("groupSettings");
    }
    DocEdit::Updated
}

/// Persist ONLY `groupSettings.<group>.reserved`. `false` drops the key; see
/// [`set_group_setting`].
pub fn save_group_reserved(
    path: &Path,
    group: &str,
    reserved: bool,
) -> Result<GroupReserveWrite, ConfigError> {
    save_group_setting(
        path,
        group,
        GroupKey::Reserved,
        reserved.then_some(Value::Bool(true)),
    )
}

/// Persist ONLY `groupSettings.<group>.parked`. `false` drops the key; see
/// [`set_group_setting`].
pub fn save_group_parked(
    path: &Path,
    group: &str,
    parked: bool,
) -> Result<GroupParkWrite, ConfigError> {
    save_group_setting(
        path,
        group,
        GroupKey::Parked,
        parked.then_some(Value::Bool(true)),
    )
}

/// Persist ONLY `groupSettings.<group>.allowControlAccount`. `false` drops the
/// key; see [`set_group_setting`].
pub fn save_group_allow_control(
    path: &Path,
    group: &str,
    allow: bool,
) -> Result<GroupControlWrite, ConfigError> {
    save_group_setting(
        path,
        group,
        GroupKey::AllowControl,
        allow.then_some(Value::Bool(true)),
    )
}

/// Persist ONLY `groupSettings.<group>.color`.
///
/// `color = None` (`tcr group color --clear`) drops the key (see
/// [`set_group_setting`]), so the group reverts to [`Config::group_color`]'s
/// derived value on the very next read.
///
/// `color`, when `Some`, is trusted as already validated/normalized — call
/// through [`validate_hex_color`] first; this function does not re-check it.
pub fn save_group_color(
    path: &Path,
    group: &str,
    color: Option<&str>,
) -> Result<GroupColorWrite, ConfigError> {
    save_group_setting(
        path,
        group,
        GroupKey::Color,
        color.map(|hex| Value::String(hex.to_string())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::GROUPED_ACCOUNTS_JSON;
    use crate::config::{ColorSource, Config, derive_group_color};
    use std::fs;

    #[test]
    fn save_group_color_round_trips_and_clear_drops_the_key() {
        let tmp = std::env::temp_dir().join(format!("tcr-cfg-color-{}.json", std::process::id()));
        fs::write(&tmp, GROUPED_ACCOUNTS_JSON).unwrap();

        let outcome = save_group_color(&tmp, "codereview", Some("#32d74b")).unwrap();
        assert_eq!(outcome, GroupColorWrite::Updated);
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(
            config.group_color("codereview"),
            ("#32d74b".to_string(), ColorSource::Set)
        );

        // Setting the SAME value again is a no-op write.
        let outcome = save_group_color(&tmp, "codereview", Some("#32d74b")).unwrap();
        assert_eq!(outcome, GroupColorWrite::Unchanged);

        // Clearing drops the `color` key (and the whole `groupSettings.<group>`
        // entry, since color was the only thing in it) — the derived value
        // returns.
        let outcome = save_group_color(&tmp, "codereview", None).unwrap();
        assert_eq!(outcome, GroupColorWrite::Updated);
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(
            config.group_color("codereview"),
            (derive_group_color("codereview"), ColorSource::Derived)
        );
        let raw = fs::read_to_string(&tmp).unwrap();
        assert!(
            !raw.contains("groupSettings"),
            "the last property on the group's settings entry is gone, so the \
             whole map is dropped rather than left as `{{}}`: {raw}"
        );

        fs::remove_file(&tmp).ok();
    }

    /// Every group key writes its own spelling, and a clear over a value the
    /// writer would never have written (a hand-typed `false`, a color that is
    /// not a string) is `Unchanged`: the file is left byte for byte.
    #[test]
    fn group_settings_write_their_own_keys_and_leave_hand_written_unset_values() {
        let tmp = std::env::temp_dir().join(format!(
            "tcr-cfg-group-keys-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let original = r#"{
  "accounts": [],
  "groupSettings": {
    "g": {
      "parked": false,
      "color": 5
    }
  }
}"#;
        fs::write(&tmp, original).unwrap();

        assert_eq!(
            save_group_parked(&tmp, "g", false).unwrap(),
            DocEdit::Unchanged
        );
        assert_eq!(
            save_group_color(&tmp, "g", None).unwrap(),
            DocEdit::Unchanged
        );
        assert_eq!(
            save_group_reserved(&tmp, "g", false).unwrap(),
            DocEdit::Unchanged
        );
        assert_eq!(fs::read_to_string(&tmp).unwrap(), original);

        assert_eq!(
            save_group_parked(&tmp, "g", true).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            save_group_reserved(&tmp, "g", true).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            save_group_allow_control(&tmp, "g", true).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            save_group_color(&tmp, "h", Some("#32d74b")).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            fs::read_to_string(&tmp).unwrap(),
            r##"{
  "accounts": [],
  "groupSettings": {
    "g": {
      "allowControlAccount": true,
      "color": 5,
      "parked": true,
      "reserved": true
    },
    "h": {
      "color": "#32d74b"
    }
  }
}"##
        );

        assert_eq!(save_group_color(&tmp, "h", None).unwrap(), DocEdit::Updated);
        assert_eq!(
            save_group_parked(&tmp, "g", false).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            save_group_reserved(&tmp, "g", false).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            save_group_allow_control(&tmp, "g", false).unwrap(),
            DocEdit::Updated
        );
        assert_eq!(
            fs::read_to_string(&tmp).unwrap(),
            r#"{
  "accounts": [],
  "groupSettings": {
    "g": {
      "color": 5
    }
  }
}"#
        );

        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn save_group_allow_control_round_trips_and_false_drops_the_key() {
        let tmp =
            std::env::temp_dir().join(format!("tcr-cfg-allow-control-{}.json", std::process::id()));
        fs::write(&tmp, GROUPED_ACCOUNTS_JSON).unwrap();

        let outcome = save_group_allow_control(&tmp, "codereview", true).unwrap();
        assert_eq!(outcome, GroupControlWrite::Updated);
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert!(config.group_allows_control("codereview"));
        assert!(
            !config.group_allows_control("dev"),
            "only the named group changes"
        );

        // Setting the SAME value again is a no-op write.
        let outcome = save_group_allow_control(&tmp, "codereview", true).unwrap();
        assert_eq!(outcome, GroupControlWrite::Unchanged);

        // Setting `false` drops the `allowControlAccount` key (and the whole
        // `groupSettings.<group>` entry, since it was the only thing in it).
        let outcome = save_group_allow_control(&tmp, "codereview", false).unwrap();
        assert_eq!(outcome, GroupControlWrite::Updated);
        let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert!(!config.group_allows_control("codereview"));
        let raw = fs::read_to_string(&tmp).unwrap();
        assert!(
            !raw.contains("groupSettings"),
            "the last property on the group's settings entry is gone, so the \
             whole map is dropped rather than left as `{{}}`: {raw}"
        );

        fs::remove_file(&tmp).ok();
    }
}
