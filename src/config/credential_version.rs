//! The `credentialVersion` key on each account row, and the stamp every row writer applies.

use serde_json::{Map, Value};

use super::{Account, Config};

/// Whether an on-disk `accounts[]` entry can yield a usable credential, and if
/// not, the account's `name` and why — for the warning [`load`] emits when it
/// drops the entry. An `apikey` account needs `apiKey`; every other type
/// (including the implicit `oauth` default) needs a non-empty `accessToken`.
/// An `importFrom`-shaped entry (a bare pointer at another file, resolved by
/// upstream at startup — not implemented here, see `config-bridge-coder.md`)
/// has no inline token either, so it falls out through the same check.
/// The credential-envelope version a later move to the OS keystore will need
/// to tell an old plaintext shape from a new one. Bumped only when the
/// on-disk SHAPE of `accessToken`/`refreshToken` changes — never for an
/// unrelated `Account` field — so a reader can always tell which shape it is
/// holding before trying to interpret it. `1` is the only token shape that has
/// ever existed: today's plaintext `accessToken`/`refreshToken` strings. The one
/// exception to "bumped only for a new shape" is a Codex row, written at
/// [`CODEX_CREDENTIAL_VERSION`] with this same shape, so that a build that cannot
/// tell providers apart refuses it instead of serving it (see there).
///
/// Deliberately NOT an `Account` struct field: `Account` is constructed as a
/// literal at several dozen call sites across the crate (tests, `oauth.rs`,
/// `manager/`, …), and giving it a new required field would force every one
/// of them to name a value that means nothing to them. The version lives
/// instead as a raw `credentialVersion` key on the JSON object, read and
/// written directly in [`load`] and [`save`] — the same layer that already
/// migrates the legacy `throttle` key and renames duplicated accounts by
/// editing the [`Value`] document rather than the typed struct. A config
/// written through this path ends up with `credentialVersion` in an
/// `Account`'s flattened `extra` map, which already exists to carry any key
/// this crate does not model, and round-trips untouched like any other.
pub(super) const CREDENTIAL_VERSION: u64 = 1;

/// The `credentialVersion` a CODEX row is written at, and the one version above
/// [`CREDENTIAL_VERSION`] that [`load`] accepts — on a Codex row only.
///
/// A Codex row's token shape is version 1's (plaintext `accessToken` /
/// `refreshToken`). What it needs is to be REFUSED, not served, by any build that
/// cannot tell providers apart. Such a build — `main` before Codex accounts
/// existed, and the `tcr` TcrBar bundles and supervises — has no `provider` field,
/// so it reads a Codex row as a Claude account: from then on the OpenAI access
/// token goes to api.anthropic.com on Claude traffic and to Anthropic's usage
/// probe and warmer, and the OpenAI refresh token goes to Anthropic's token
/// endpoint. The row is durable, too: a later restart on a newer build does not
/// heal it, because that build also reads a row with no `provider` key as
/// Anthropic. A build that has this version check (0.2.46 onward) refuses the
/// WHOLE file on a version it does not understand, so its server refuses to
/// start (`load_config` in `src/main.rs`) and leaves the file as it is: loud, and
/// no token goes anywhere. This lane exists for exactly "a newer shape this build
/// does not understand", so a Codex row carries 2. A build from before the check
/// (0.2.45 and older) never reads the key and still serves the row as a Claude
/// account; nothing in the file can stop it, which is why the import asks for
/// every tcr that may start on the config to be updated first.
///
/// Anthropic rows keep [`CREDENTIAL_VERSION`], so an Anthropic-only file gains no
/// new key and no new value, and every older build still loads it. The price is
/// that importing a Codex login makes a pre-Codex proxy with the check refuse to
/// start on the config until it is upgraded, which the import says.
const CODEX_CREDENTIAL_VERSION: u64 = 2;

/// The `credentialVersion` a row of `provider` is written at, and the highest
/// [`load`] accepts on one. One exhaustive `match`, so a third provider cannot be
/// stamped by default.
pub(super) const fn credential_version_for(provider: crate::provider::Provider) -> u64 {
    match provider {
        crate::provider::Provider::Anthropic => CREDENTIAL_VERSION,
        crate::provider::Provider::Codex => CODEX_CREDENTIAL_VERSION,
    }
}

/// Whether [`stamp_credential_version`] would change `entry`, one account row
/// (on disk, or an `Account`'s `extra` in memory).
///
/// An absent key always needs its provider's version, as it always did. A row
/// whose provider is written above [`CREDENTIAL_VERSION`] (a Codex row) also
/// needs it when it carries a SMALLER number: a row an earlier build wrote at 1,
/// or one copied by hand from an Anthropic row (every saved row carries 1), is
/// exactly the row an older build would serve. Anything else already present is
/// left alone: an Anthropic row's value, so an Anthropic-only file never changes;
/// a number above the provider's, so the refusal [`load`] performs on an unknown
/// future version can never be papered over; and a value that is not a version
/// number at all, which [`load`] refuses too.
pub(super) fn needs_credential_stamp(
    entry: &Map<String, Value>,
    provider: crate::provider::Provider,
) -> bool {
    let version = credential_version_for(provider);
    match entry.get("credentialVersion") {
        None => true,
        Some(Value::Number(n)) => {
            version > CREDENTIAL_VERSION && n.as_u64().is_some_and(|v| v < version)
        }
        Some(_) => false,
    }
}

/// Stamp `provider`'s credential version on `entry` where
/// [`needs_credential_stamp`] says it is missing or too old. Every writer of a row
/// calls this, so a Codex row reaches disk at [`CODEX_CREDENTIAL_VERSION`] whichever
/// path wrote it.
pub(super) fn stamp_credential_version(
    entry: &mut Map<String, Value>,
    provider: crate::provider::Provider,
) {
    if needs_credential_stamp(entry, provider) {
        entry.insert(
            "credentialVersion".to_string(),
            Value::Number(credential_version_for(provider).into()),
        );
    }
}

/// `entry`'s `credentialVersion` key, read directly off the raw JSON so
/// [`load`] can decide before anything is handed to `serde` for typed
/// deserialization.
///
/// - `Ok(None)`: the key is absent — every config written before this lane,
///   which means version 1 and needs no migration.
/// - `Ok(Some(v))`: the key is present and reads as a version number.
/// - `Err(())`: the key is present but is not a shape a version number could
///   ever take (a string, an object, a negative number, a float) — refused
///   the same as an unknown future version, because guessing what a
///   malformed version key meant is exactly the silent reinterpretation this
///   exists to prevent.
pub(super) fn credential_version_of(entry: &Value) -> Result<Option<u64>, ()> {
    match entry.get("credentialVersion") {
        None => Ok(None),
        Some(Value::Number(n)) => n.as_u64().ok_or(()).map(Some),
        Some(_) => Err(()),
    }
}

/// `config` with every Codex account's `credentialVersion` stamped (see
/// [`stamp_credential_version`]), for the two writers that serialize the whole
/// snapshot, [`save`] and [`save_settings`]. Borrowed unchanged when no Codex
/// account needs a stamp, which is every Anthropic-only config: an Anthropic row
/// written from memory keeps whatever its `extra` held, as before.
///
/// The merge writers stamp the rows they touch; these two serialize accounts
/// straight from memory, where a Codex row can carry no version at all — one the
/// server added live through `POST /_tcr/accounts`, or the single row of a
/// first login's fresh file. Unstamped, that row reaches disk in a shape every
/// older proxy loads as a Claude account: through the server's whole-file
/// fallback ([`save_tokens_for`] on a file it cannot read), or a first import.
pub(super) fn with_codex_credential_versions(config: &Config) -> std::borrow::Cow<'_, Config> {
    let quarantined =
        |account: &Account| credential_version_for(account.provider) > CREDENTIAL_VERSION;
    if !config.accounts.iter().any(|account| {
        quarantined(account) && needs_credential_stamp(&account.extra, account.provider)
    }) {
        return std::borrow::Cow::Borrowed(config);
    }
    let mut stamped = config.clone();
    for account in stamped
        .accounts
        .iter_mut()
        .filter(|account| quarantined(account))
    {
        stamp_credential_version(&mut account.extra, account.provider);
    }
    std::borrow::Cow::Owned(stamped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{account_fixture, read_json, tmp_path};
    use crate::config::{AccountWrite, load, save, save_account, save_settings, save_tokens};
    use serde_json::json;
    use std::fs;

    // --- load: the credential envelope declares its version ------------------

    /// A config written before `credentialVersion` existed loads exactly as
    /// it always has (no version key at all), and the very next `save` now
    /// writes `credentialVersion: 1` explicitly — nothing migrates by hand,
    /// and a later build can tell "this file predates the key" from "this
    /// file explicitly says version 1" apart, by there being no difference
    /// left to see.
    #[test]
    fn missing_credential_version_loads_as_version_1_and_backfills_on_save() {
        let path = tmp_path("cred-version-missing");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-a", "accessToken": "at-a", "refreshToken": "rt-a" }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].access_token, "at-a");

        let saved = tmp_path("cred-version-missing-saved");
        save(&saved, &config).unwrap();
        let value = read_json(&saved);
        assert_eq!(
            value["accounts"][0]["credentialVersion"],
            serde_json::json!(1),
            "a config with no credentialVersion key must re-save with it: {value}"
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&saved).ok();
    }

    /// An explicit `"credentialVersion": 1` — what every config saved after
    /// this lane carries — loads and round-trips unchanged.
    #[test]
    fn explicit_credential_version_1_round_trips() {
        let path = tmp_path("cred-version-explicit");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-a", "accessToken": "at-a", "credentialVersion": 1 }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);

        let saved = tmp_path("cred-version-explicit-saved");
        save(&saved, &config).unwrap();
        let value = read_json(&saved);
        assert_eq!(
            value["accounts"][0]["credentialVersion"],
            serde_json::json!(1)
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&saved).ok();
    }

    /// A `credentialVersion` this build has never heard of refuses the WHOLE
    /// file rather than guessing what the newer shape means — the exact
    /// failure a later move to the OS keystore needs this lane to catch.
    #[test]
    fn unknown_future_credential_version_refuses_to_load() {
        let path = tmp_path("cred-version-future");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-a", "accessToken": "at-a", "credentialVersion": 2 }
               ] }"#,
        )
        .unwrap();

        let err = load(&path).expect_err("an unknown credentialVersion must refuse to load");
        let message = err.to_string();
        assert!(
            message.contains("acct-a") && message.contains('2'),
            "error should name the account and the unknown version: {message}"
        );

        fs::remove_file(&path).ok();
    }

    /// A `credentialVersion` that is present but not a number at all (a
    /// hand-edited file, a half-written migration from some other tool) is
    /// refused the same way an unknown future number is — never silently
    /// treated as version 1.
    #[test]
    fn malformed_credential_version_refuses_to_load() {
        let path = tmp_path("cred-version-malformed");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-a", "accessToken": "at-a", "credentialVersion": "v1" }
               ] }"#,
        )
        .unwrap();

        let err = load(&path).expect_err("a non-numeric credentialVersion must refuse to load");
        assert!(
            err.to_string().contains("acct-a"),
            "error should name the account: {err}"
        );

        fs::remove_file(&path).ok();
    }

    // --- credentialVersion 2: a Codex row is refused by every older build ----

    /// The `credentialVersion` a loaded account carries in memory, read the way
    /// `load` reads a row's ([`credential_version_of`]). Not through a literal
    /// `.extra.get(…)`: this is an ACCOUNT's own `extra` map, and the drift scan in
    /// `config_keys` reads every such literal as a key of the top-level
    /// `Config::extra`, which `credentialVersion` is not.
    fn version_in_memory(account: &Account) -> Result<Option<u64>, ()> {
        credential_version_of(&Value::Object(account.extra.clone()))
    }

    /// A Codex row at `credentialVersion` 2, which every Codex row is written at,
    /// loads, and a whole-file save keeps it at 2.
    #[test]
    fn a_codex_row_at_credential_version_2_loads() {
        let path = tmp_path("cred-version-codex-2");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "alice@example.com", "provider": "codex", "accessToken": "at-a",
                   "accountUuid": "11111111-1111-4111-8111-111111111111", "credentialVersion": 2 }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).expect("a Codex row at credentialVersion 2 must load");
        assert_eq!(
            config.accounts[0].provider,
            crate::provider::Provider::Codex
        );
        let saved = tmp_path("cred-version-codex-2-saved");
        save(&saved, &config).unwrap();
        assert_eq!(
            read_json(&saved)["accounts"][0]["credentialVersion"],
            json!(2)
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&saved).ok();
    }

    /// 2 is a Codex row's version, not a general "newer" one: an Anthropic row
    /// carrying it is refused exactly as before, even in a file where the Codex
    /// row beside it legitimately carries 2.
    #[test]
    fn a_version_2_anthropic_row_is_refused_even_beside_a_codex_row() {
        let path = tmp_path("cred-version-anthropic-2");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "bob@example.com", "provider": "codex", "accessToken": "at-b",
                   "credentialVersion": 2 },
                 { "name": "alice@example.com", "accessToken": "at-a", "credentialVersion": 2 }
               ] }"#,
        )
        .unwrap();

        let err = load(&path).expect_err("an Anthropic row at credentialVersion 2 must refuse");
        let message = err.to_string();
        assert!(
            message.contains("alice@example.com")
                && message.contains("credentialVersion 2")
                && message.contains("up to 1"),
            "error should name the Anthropic row, its version and what it may carry: {message}"
        );

        fs::remove_file(&path).ok();
    }

    /// Above 2 is refused on a Codex row too: 2 is the newest shape this build
    /// understands for any row.
    #[test]
    fn credential_version_3_is_refused_on_a_codex_row() {
        let path = tmp_path("cred-version-codex-3");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "alice@example.com", "provider": "codex", "accessToken": "at-a",
                   "credentialVersion": 3 }
               ] }"#,
        )
        .unwrap();

        let err = load(&path).expect_err("credentialVersion 3 must refuse even on a Codex row");
        let message = err.to_string();
        assert!(
            message.contains("alice@example.com") && message.contains("credentialVersion 3"),
            "error should name the account and the unknown version: {message}"
        );

        fs::remove_file(&path).ok();
    }

    /// `save_account` writes a Codex row at 2 on both of its arms: the Added arm (a
    /// first import) and the Updated arm (a re-import over a row that carries no
    /// version). The Anthropic row it does not touch gains nothing.
    #[test]
    fn save_account_writes_a_codex_row_at_credential_version_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::paths::CONFIG_FILE);
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-anthropic" },
                { "name": "bob@example.com", "provider": "codex", "accessToken": "at-old",
                  "accountUuid": "22222222-2222-4222-8222-222222222222" }
            ] }"#,
        )
        .unwrap();

        let mut bob = account_fixture("bob@example.com");
        bob.provider = crate::provider::Provider::Codex;
        bob.account_uuid = Some("22222222-2222-4222-8222-222222222222".to_string());
        assert_eq!(save_account(&path, &bob).unwrap(), AccountWrite::Updated);

        let mut carol = account_fixture("carol@example.com");
        carol.provider = crate::provider::Provider::Codex;
        carol.account_uuid = Some("11111111-1111-4111-8111-111111111111".to_string());
        assert_eq!(save_account(&path, &carol).unwrap(), AccountWrite::Added);

        let after = read_json(&path);
        assert_eq!(
            after["accounts"][1]["credentialVersion"],
            json!(2),
            "the Updated arm writes a Codex row at 2: {after}"
        );
        assert_eq!(
            after["accounts"][2]["credentialVersion"],
            json!(2),
            "the Added arm writes a Codex row at 2: {after}"
        );
        assert!(
            after["accounts"][0].get("credentialVersion").is_none(),
            "an Anthropic row neither write touched gains nothing: {after}"
        );
    }

    /// The live persist path — `merge_tokens`, under every token rotation — stamps a
    /// Codex entry it writes at 2, and an Anthropic entry at 1 as it always did.
    #[test]
    fn a_token_persist_stamps_a_codex_entry_at_credential_version_2() {
        let path = tmp_path("persist-credential-version-codex");
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "alice@example.com", "accessToken": "at-a-old" },
                   { "name": "bob@example.com", "provider": "codex", "accessToken": "at-b-old" } ] }"#,
        )
        .unwrap();
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "alice@example.com", "accessToken": "at-a-new" },
                   { "name": "bob@example.com", "provider": "codex", "accessToken": "at-b-new" } ] }"#,
        )
        .unwrap();

        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-b-new"));
        assert_eq!(
            value["accounts"][1]["credentialVersion"],
            json!(2),
            "a persisted Codex entry must carry 2: {value}"
        );
        assert_eq!(
            value["accounts"][0]["credentialVersion"],
            json!(1),
            "an Anthropic entry is stamped 1, as before: {value}"
        );
        fs::remove_file(&path).ok();
    }

    /// A Codex row loaded with no version key is backfilled to 2, not 1, so the
    /// next whole-file save writes the version older builds refuse.
    #[test]
    fn a_loaded_codex_row_with_no_version_is_backfilled_to_2() {
        let path = tmp_path("cred-version-codex-missing");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "alice@example.com", "provider": "codex", "accessToken": "at-a" },
                 { "name": "bob@example.com", "accessToken": "at-b" }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(
            version_in_memory(&config.accounts[0]),
            Ok(Some(2)),
            "a Codex row is backfilled to 2"
        );
        assert_eq!(
            version_in_memory(&config.accounts[1]),
            Ok(Some(1)),
            "an Anthropic row is backfilled to 1, as before"
        );
        let saved = tmp_path("cred-version-codex-missing-saved");
        save(&saved, &config).unwrap();
        assert_eq!(
            read_json(&saved)["accounts"][0]["credentialVersion"],
            json!(2)
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&saved).ok();
    }

    /// A Codex row at 1 — written before Codex rows carried 2, or copied by hand
    /// from an Anthropic row, which every saved file stamps 1 — is raised to 2, by
    /// the load and by the first token persist that touches it. Left at 1 it is
    /// exactly the row an older build serves as a Claude account. The Anthropic row
    /// beside it keeps its 1.
    #[test]
    fn a_codex_row_at_credential_version_1_is_raised_to_2() {
        let path = tmp_path("cred-version-codex-1");
        let file = r#"{ "accounts": [
                 { "name": "alice@example.com", "provider": "codex", "accessToken": "at-a",
                   "credentialVersion": 1 },
                 { "name": "bob@example.com", "accessToken": "at-b", "credentialVersion": 1 }
               ] }"#;
        fs::write(&path, file).unwrap();

        let config = load(&path).unwrap();
        assert_eq!(
            version_in_memory(&config.accounts[0]),
            Ok(Some(2)),
            "the load raises a Codex row to 2"
        );
        assert_eq!(version_in_memory(&config.accounts[1]), Ok(Some(1)));

        // The disk still says 1 until a write touches the row: a rotation does.
        save_tokens(&path, &config).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["credentialVersion"],
            json!(2),
            "a persist raises a Codex entry to 2: {value}"
        );
        assert_eq!(value["accounts"][1]["credentialVersion"], json!(1));

        fs::remove_file(&path).ok();
    }

    /// The two writers that serialize the whole snapshot stamp a Codex account
    /// whose `extra` carries no version — a row the server added live, or a first
    /// login's fresh file — and leave an Anthropic account exactly as before (no
    /// key added).
    #[test]
    fn whole_file_writers_stamp_a_codex_account_built_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut config: Config = serde_json::from_str("{}").unwrap();
        config.accounts.push(account_fixture("alice@example.com"));
        let mut bob = account_fixture("bob@example.com");
        bob.provider = crate::provider::Provider::Codex;
        config.accounts.push(bob);

        let saved = dir.path().join("saved.json");
        save(&saved, &config).unwrap();
        let value = read_json(&saved);
        assert_eq!(
            value["accounts"][1]["credentialVersion"],
            json!(2),
            "save writes the Codex account at 2: {value}"
        );
        assert!(
            value["accounts"][0].get("credentialVersion").is_none(),
            "save leaves the Anthropic account as it always was: {value}"
        );

        // `save_settings` falls back to `save` when there is no file to read, so
        // give it one: this pins its own serialization, not the fallback.
        let settings = dir.path().join("settings.json");
        fs::write(&settings, r#"{ "accounts": [] }"#).unwrap();
        save_settings(&settings, &config).unwrap();
        let value = read_json(&settings);
        assert_eq!(
            value["accounts"][1]["credentialVersion"],
            json!(2),
            "save_settings writes the Codex account at 2: {value}"
        );
        assert!(
            value["accounts"][0].get("credentialVersion").is_none(),
            "save_settings leaves the Anthropic account as it always was: {value}"
        );
    }
}
