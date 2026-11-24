//! Reading the file: which account rows are set aside and why, and
//! [`load_or_init`] for a first run.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::credential_version::{
    CREDENTIAL_VERSION, credential_version_for, credential_version_of, stamp_credential_version,
};
use super::entry::provider_of_entry;
use super::file::save;
use super::{
    AccountType, Config, ConfigError, ThrottleConfig, default_codex_upstream, default_upstream,
};

/// Why [`unusable_account`] refused an `accounts[]` entry.
#[derive(Debug, PartialEq)]
enum Unusable {
    /// `accessToken` is absent or empty.
    MissingAccessToken,
    /// The entry's `type` is [`AccountType::ApiKey`]. tcr does not serve
    /// API-key accounts (it would send the key as a Bearer token, which the
    /// API refuses), so the row is set aside however it is filled in. Before
    /// this variant, such a row written in the shape it asks for (`apiKey`, no
    /// `accessToken`) failed the WHOLE load with `missing field accessToken`.
    ApiKeyAccount,
    /// The entry's `type` is present but not a string, so it cannot be an
    /// [`AccountType`]. Holds the raw value, for the warning.
    UnreadableType(String),
    /// The entry carries its own `upstream` (a row shape some configs use for a
    /// third-party vendor) whose host is neither Anthropic's API nor the host
    /// this config forwards to. tcr ignores a per-account `upstream` and sends
    /// every account's token to the config's one upstream as a Bearer
    /// (`proxy::build_upstream_headers`), so loading this row would hand the
    /// vendor's key to a host it was never issued for.
    ///
    /// "The config's one upstream" is per provider: a Codex row's token goes to
    /// `codexUpstream` (`proxy::build_codex_upstream_headers`), so a Codex row is
    /// held to Codex's hosts, its default and `codexUpstream`'s, and an
    /// Anthropic row to Anthropic's. Holds the raw value, for the warning, and
    /// the provider whose hosts it was held to.
    ForeignUpstream {
        upstream: String,
        provider: crate::provider::Provider,
    },
}

/// The host Anthropic serves its API on, parsed from [`default_upstream`] so
/// the two can never disagree.
fn anthropic_api_host() -> Option<String> {
    reqwest::Url::parse(&default_upstream())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

/// The host Codex is served on by default, parsed from
/// [`default_codex_upstream`] so the two can never disagree, as
/// [`anthropic_api_host`] is from [`default_upstream`].
fn codex_default_host() -> Option<String> {
    upstream_host(&default_codex_upstream())
}

/// The lowercased host of `raw`, or `None` when it does not parse as a URL
/// with a host. Compared by host, never by substring, so
/// `https://api.anthropic.com.example.net` is not mistaken for Anthropic.
fn upstream_host(raw: &str) -> Option<String> {
    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

/// `own_upstream_host` is the host of the config's top-level `upstream` (the
/// one every Anthropic account is actually forwarded to); a per-account
/// `upstream` on that host, or on Anthropic's API host, loads normally. Any
/// other value, including one that is not a string or not a URL, is
/// [`Unusable::ForeignUpstream`]: a key whose destination cannot be shown to
/// be one tcr sends to is not sent at all.
///
/// A Codex row is held to its own provider's pair instead:
/// `own_codex_upstream_host`, the host of `codexUpstream` (where every Codex
/// account is forwarded), and Codex's default host. Held to Anthropic's pair,
/// every Codex row naming its own upstream would be quarantined, and one
/// naming Anthropic's API would load and send its OpenAI key to `codexUpstream`.
fn unusable_account(
    entry: &Value,
    own_upstream_host: Option<&str>,
    own_codex_upstream_host: Option<&str>,
) -> Option<(String, Unusable)> {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string();
    let has_nonempty = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    // Parsed through `AccountType`'s own serde, so the type names are spelled
    // in exactly one place.
    let account_type = match entry.get("type") {
        None => AccountType::default(),
        Some(raw) => match AccountType::deserialize(raw) {
            Ok(account_type) => account_type,
            Err(_) => return Some((name, Unusable::UnreadableType(raw.to_string()))),
        },
    };
    if matches!(account_type, AccountType::ApiKey) {
        return Some((name, Unusable::ApiKeyAccount));
    }
    if !has_nonempty("accessToken") {
        return Some((name, Unusable::MissingAccessToken));
    }
    if let Some(upstream) = entry.get("upstream") {
        let host = upstream.as_str().and_then(upstream_host);
        // The row's provider, read as every matcher over the raw document reads
        // it. A value this build does not know is held to Anthropic's hosts, the
        // rule every row had before providers existed; should it survive this,
        // the typed parse refuses the file over that provider anyway.
        let provider = entry
            .as_object()
            .and_then(provider_of_entry)
            .unwrap_or_default();
        let (default_host, own_host) = match provider {
            crate::provider::Provider::Anthropic => (anthropic_api_host(), own_upstream_host),
            crate::provider::Provider::Codex => (codex_default_host(), own_codex_upstream_host),
        };
        let allowed = host
            .as_deref()
            .is_some_and(|host| Some(host) == default_host.as_deref() || Some(host) == own_host);
        if !allowed {
            let shown = match upstream.as_str() {
                Some(raw) => raw.to_string(),
                None => upstream.to_string(),
            };
            return Some((
                name,
                Unusable::ForeignUpstream {
                    upstream: shown,
                    provider,
                },
            ));
        }
    }
    None
}

/// Load and parse the config at `path`.
///
/// Checks every account's `credentialVersion` before anything else touches
/// the entry — see [`CREDENTIAL_VERSION`] — backfilling an absent key in the
/// document (`1`, or `2` on a Codex row: `CODEX_CREDENTIAL_VERSION`) and
/// refusing the whole file on a version number this build does not understand
/// for that row's provider.
///
/// Deserializes leniently at the account boundary: an `accounts[]` entry that
/// cannot yield a usable credential ([`unusable_account`]) is dropped, with a
/// warning naming it, rather than failing the WHOLE file the way a plain
/// `serde_json::from_str` over `Config` would — `Account::access_token` is a
/// required `String` with no `serde(default)` (deliberately: an empty-string
/// token must never reach rotation and send `Bearer ` upstream), so one such
/// entry used to take every other account in the file down with it. It is
/// skipped rather than crashing the fleet. Every `Account` that survives into memory still has a
/// real, non-empty credential — the 142 call sites reading `access_token` as a
/// plain `String` are entitled to assume that.
///
/// The same quarantine covers an entry whose own `upstream` names a
/// third-party host ([`Unusable::ForeignUpstream`]): its token is that
/// vendor's key, and tcr would send it to its one configured upstream (for a
/// Codex row, `codexUpstream`). It also covers an API-key row
/// ([`Unusable::ApiKeyAccount`]), which tcr does not serve, and a row whose
/// `type` is not a string ([`Unusable::UnreadableType`]).
///
/// If every account entry is unusable, that is still an error (an empty fleet
/// booting silently is its own bug) — reported through the same
/// [`ConfigError::Parse`] shape a malformed file already uses, not a new
/// variant.
///
/// The names of every quarantined account land in
/// [`Config::quarantined_accounts`] — not just the log line — so a caller
/// that cannot tolerate a quietly-shrunk fleet (the CLI's edit verbs; see
/// `cli::load_for_edit`) can refuse instead of writing a `save()` that would
/// permanently drop the entry's raw JSON (its `importFrom` pointer included).
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let data = fs::read_to_string(path)?;
    let mut doc: Value = serde_json::from_str(&data)?;

    // A stale `throttle` key is MIGRATED, never rejected — "our code should auto
    // migrate" (the mandate behind this function). `throttle` shipped through a
    // Sparkle auto-update with no migration step of its own, so refusing to
    // parse it left every affected install unable to boot: the CLI exited
    // non-zero on every verb, and `main.rs::load_config`'s fallback for `tcr
    // server` is worse — it swallows the error and boots a zero-account fleet
    // that answers every request with 429 while looking alive. There is no
    // input on which this key may still cause a parse error.
    //
    // The two-branch rule:
    //
    // * `throttle` parses into an ACTIVE `ThrottleConfig` (has an effective
    //   `minSpacingMs`) — that is what the old single key actually did, a
    //   per-organization limiter. It becomes `accountThrottle`, verbatim, and
    //   `fleetThrottle` is left absent to take its own (looser) default. Tuning
    //   is preserved exactly; the fleet ceiling is additive insurance.
    // * `throttle` parses into an INERT `ThrottleConfig` (`{}`, a `burst` with
    //   no spacing, or `minSpacingMs: 0`) — the old key's escape hatch meant "no
    //   throttling at all", so both new keys are set to `{}` to preserve that
    //   intent exactly.
    //
    // If `accountThrottle` or `fleetThrottle` is already present alongside
    // `throttle` (a half-run of the old manual migration script), the new
    // key(s) win and `throttle` is dropped with a warning — still no error. If
    // `throttle` holds something that cannot parse as a `ThrottleConfig` at all
    // (a string, a number), it is dropped with a warning — still no error.
    //
    // `migrated_legacy_throttle` records that this happened so the boot path in
    // `main.rs` can persist the migrated document back to disk once, so the
    // fix actually self-heals instead of re-warning on every start.
    let mut migrated_legacy_throttle = false;
    if let Some(throttle_value) = doc.as_object_mut().and_then(|obj| obj.remove("throttle")) {
        migrated_legacy_throttle = true;
        let has_new_keys = doc.as_object().is_some_and(|obj| {
            obj.contains_key("accountThrottle") || obj.contains_key("fleetThrottle")
        });
        if has_new_keys {
            tracing::warn!(
                "config carries both a legacy `throttle` key and a new `accountThrottle`/\
                 `fleetThrottle` key; the new key(s) win and the legacy `throttle` key is dropped"
            );
        } else {
            match serde_json::from_value::<ThrottleConfig>(throttle_value.clone()) {
                Ok(legacy) if legacy.is_active() => {
                    tracing::warn!(
                        min_spacing_ms = ?legacy.min_spacing_ms,
                        burst = ?legacy.burst,
                        "migrating legacy `throttle` key to `accountThrottle`; \
                         `fleetThrottle` takes its own default"
                    );
                    if let Some(obj) = doc.as_object_mut() {
                        obj.insert("accountThrottle".to_string(), throttle_value);
                    }
                }
                Ok(_) => {
                    tracing::warn!(
                        "migrating inert legacy `throttle` key: disabling both \
                         `accountThrottle` and `fleetThrottle`"
                    );
                    if let Some(obj) = doc.as_object_mut() {
                        obj.insert("accountThrottle".to_string(), Value::Object(Map::new()));
                        obj.insert("fleetThrottle".to_string(), Value::Object(Map::new()));
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "legacy `throttle` key does not parse as a throttle config; dropping it"
                    );
                }
            }
        }
    }

    // Credential-envelope version check, ahead of everything else that reads
    // an account entry: refuse the whole file rather than silently
    // reinterpreting a shape a future `tcr` wrote and this build has never
    // seen. A missing key backfills in the document itself, so the very next
    // `save` writes it explicitly and no migration step is needed today — see
    // [`CREDENTIAL_VERSION`]'s doc-comment for why this lives here rather than
    // on `Account`.
    //
    // What this build understands is per ROW: up to 2 on a Codex row, up to 1
    // on every other ([`CODEX_CREDENTIAL_VERSION`] says why), with the row's
    // provider read the way every matcher over the raw document reads it. A
    // Codex row below 2 is raised to it here, as the stamp every writer applies
    // would raise it. A provider this build does not know gets the Anthropic
    // ceiling and no stamp: the typed parse below refuses the file over that
    // provider anyway.
    if let Some(accounts) = doc.get_mut("accounts").and_then(Value::as_array_mut) {
        for entry in accounts.iter_mut() {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>")
                .to_string();
            let provider = entry.as_object().and_then(provider_of_entry);
            let understood = provider.map_or(CREDENTIAL_VERSION, credential_version_for);
            match credential_version_of(entry) {
                Ok(Some(v)) if v > understood => {
                    return Err(ConfigError::Parse(
                        <serde_json::Error as serde::de::Error>::custom(format!(
                            "account \"{name}\" carries credentialVersion {v}, which this build \
                             only understands up to {understood}; refusing to load rather \
                             than silently reinterpret its credential shape"
                        )),
                    ));
                }
                Ok(_) => {
                    if let (Some(obj), Some(provider)) = (entry.as_object_mut(), provider) {
                        stamp_credential_version(obj, provider);
                    }
                }
                Err(()) => {
                    return Err(ConfigError::Parse(
                        <serde_json::Error as serde::de::Error>::custom(format!(
                            "account \"{name}\" carries a credentialVersion that is not a \
                             version number; refusing to load rather than guess what it means"
                        )),
                    ));
                }
            }
        }
    }

    let mut quarantined = Vec::new();
    // Where each surviving account sat in the FILE's `accounts` array, before
    // the quarantine drop shifted the in-memory indices. The rename migration
    // below writes by that index, so it must be the on-disk one.
    let mut file_index = Vec::new();

    // The host every account is forwarded to: the file's own top-level
    // `upstream`, or the default when the key is absent (what serde fills in
    // below).
    let own_upstream_host = match doc.get("upstream").and_then(Value::as_str) {
        Some(raw) => upstream_host(raw),
        None => upstream_host(&default_upstream()),
    };
    // The same for every Codex account: the file's `codexUpstream`, or its
    // default.
    let own_codex_upstream_host = match doc.get("codexUpstream").and_then(Value::as_str) {
        Some(raw) => upstream_host(raw),
        None => codex_default_host(),
    };

    if let Some(accounts) = doc.get_mut("accounts").and_then(Value::as_array_mut) {
        let had_accounts = !accounts.is_empty();
        let mut position = 0usize;
        accounts.retain(|entry| {
            let index = position;
            position += 1;
            match unusable_account(
                entry,
                own_upstream_host.as_deref(),
                own_codex_upstream_host.as_deref(),
            ) {
            None => {
                file_index.push(index);
                true
            }
            Some((name, Unusable::MissingAccessToken)) => {
                tracing::warn!(account = %name, missing = "accessToken", "No token for \"{name}\", skipping");
                quarantined.push(name);
                false
            }
            Some((name, Unusable::ApiKeyAccount)) => {
                tracing::warn!(
                    account = %name,
                    "Account \"{name}\" is an API-key account; tcr does not serve API-key accounts, skipping"
                );
                quarantined.push(name);
                false
            }
            Some((name, Unusable::UnreadableType(raw))) => {
                tracing::warn!(
                    account = %name,
                    account_type = %raw,
                    "Account \"{name}\" has a \"type\" that is not a string ({raw}); skipping"
                );
                quarantined.push(name);
                false
            }
            Some((name, Unusable::ForeignUpstream { upstream, provider })) => {
                match provider {
                    crate::provider::Provider::Anthropic => tracing::warn!(
                        account = %name,
                        upstream = %upstream,
                        "Account \"{name}\" names a non-Anthropic upstream ({upstream}); tcr would send its key to its own upstream instead, skipping"
                    ),
                    crate::provider::Provider::Codex => tracing::warn!(
                        account = %name,
                        upstream = %upstream,
                        "Codex account \"{name}\" names an upstream that is not its Codex upstream ({upstream}); tcr would send its key to its own codexUpstream instead, skipping"
                    ),
                }
                quarantined.push(name);
                false
            }
        }});
        if had_accounts && accounts.is_empty() {
            return Err(ConfigError::Parse(
                <serde_json::Error as serde::de::Error>::custom(format!(
                    "no usable accounts remain after skipping {} (no credential, an API-key account, an unreadable type, or a non-Anthropic upstream): {}",
                    quarantined.len(),
                    quarantined.join(", ")
                )),
            ));
        }
    }

    Config::from_loaded(
        path,
        doc,
        &file_index,
        quarantined,
        migrated_legacy_throttle,
    )
}

impl Config {
    /// The config [`load`] read from `path`: `doc` after its accounts were screened, carrying
    /// the names set aside and whether a legacy `throttle` key was migrated, with duplicate
    /// account names made unique.
    fn from_loaded(
        path: &Path,
        doc: Value,
        file_index: &[usize],
        quarantined: Vec<String>,
        migrated_legacy_throttle: bool,
    ) -> Result<Self, ConfigError> {
        let mut config: Self = serde_json::from_value(doc)?;
        config.quarantined_accounts = quarantined;
        config.migrated_legacy_throttle = migrated_legacy_throttle;
        config.migrate_duplicate_names(path, file_index);
        Ok(config)
    }
}

/// [`load`], except that a config file which does not exist yet is CREATED on
/// disk with the defaults and then returned. The `bool` is whether this call
/// created it.
///
/// The single entry point for a first run. Before this existed, a missing file
/// meant two different things depending on which verb you typed: `tcr server`
/// booted on in-memory defaults and never wrote them, and every other verb died
/// with `config i/o error: No such file or directory` and exit 1 — which TcrBar
/// renders as a failed status poll, so a fresh install off the dmg showed an
/// error in the menu bar while the proxy behind it was perfectly healthy. There
/// is no first-run state worth preserving in "the file is absent": writing the
/// defaults is what every verb wanted anyway.
///
/// The defaults come from parsing `{}` so the file gets this struct's own
/// `#[serde(default)]` values (`upstream`, `switchThreshold`, an empty
/// `accounts`), and it is written through [`save`] so the key order, the atomic
/// temp+rename and the `0600` permissions are byte-for-byte what any later save
/// produces — a first-run file is not a special shape.
///
/// Only [`std::io::ErrorKind::NotFound`] is treated this way. A parse error, a
/// permission error, an unwritable parent directory: all stay errors, and a
/// corrupt file is never overwritten by a default.
pub fn load_or_init(path: &Path) -> Result<(Config, bool), ConfigError> {
    match load(path) {
        Ok(config) => Ok((config, false)),
        Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            let config: Config = serde_json::from_str("{}")?;
            save(path, &config)?;
            eprintln!("[tcr] created {} with no accounts", path.display());
            Ok((config, true))
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{SAMPLE, read_json, tmp_file, tmp_path};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt as _;

    // --- load: skip unusable accounts, keep the rest -------------------------

    /// One account with no `accessToken` (an `importFrom`-shaped entry, e.g.:
    /// upstream reads the credential from another file at startup — not
    /// implemented here) does not take the other, usable accounts down with
    /// it, and the skipped account is named in a warning rather than silently
    /// vanishing.
    #[test]
    fn load_skips_one_unusable_account_and_keeps_the_rest() {
        let path = tmp_path("load-skip-one");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-good", "accessToken": "at-good" },
                 { "name": "acct-import", "importFrom": "/some/other/file.json" }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "acct-good");
        assert_eq!(config.accounts[0].access_token, "at-good");
        fs::remove_file(&path).ok();
    }

    /// An `apikey` account with no `apiKey` at all is skipped, not fatal: tcr
    /// sets aside every API-key row, filled in or not.
    #[test]
    fn load_skips_apikey_account_missing_api_key() {
        let path = tmp_path("load-skip-apikey");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-good", "accessToken": "at-good" },
                 { "name": "acct-apikey", "type": "apikey" }
               ] }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "acct-good");
        fs::remove_file(&path).ok();
    }

    /// Every account unusable is still a load error — an empty fleet must
    /// never boot silently — reported through the same `ConfigError::Parse`
    /// shape a malformed file already uses.
    #[test]
    fn load_errors_when_every_account_is_unusable() {
        let path = tmp_path("load-all-unusable");
        fs::write(
            &path,
            r#"{ "accounts": [
                 { "name": "acct-import", "importFrom": "/some/other/file.json" }
               ] }"#,
        )
        .unwrap();

        let err = load(&path).unwrap_err();
        std::assert_matches!(err, ConfigError::Parse(_), "expected a Parse-shaped error");
        fs::remove_file(&path).ok();
    }

    /// An empty `accounts` list is not "all unusable" — it is unchanged,
    /// pre-existing, valid behaviour (no accounts configured at all).
    #[test]
    fn load_tolerates_empty_accounts_list() {
        let path = tmp_path("load-empty-accounts");
        fs::write(&path, r#"{ "accounts": [] }"#).unwrap();
        let config = load(&path).unwrap();
        assert!(config.accounts.is_empty());
        fs::remove_file(&path).ok();
    }

    /// `load` names every account it drops in
    /// [`Config::quarantined_accounts`], not only in the log line — the CLI's
    /// `load_for_edit` refuses on this rather than risk a `save()` that would
    /// permanently erase the entry.
    #[test]
    fn load_reports_quarantined_account_names() {
        let path = tmp_file("load-quarantine-names", GOOD_ROW_AND_IMPORT_ROW);

        let config = load(&path).unwrap();
        assert_eq!(config.quarantined_accounts, vec!["acct-import".to_string()]);
        fs::remove_file(&path).ok();
    }

    /// An `apikey` row in the shape the loader itself asks for (`apiKey`, no
    /// `accessToken`) used to fail the WHOLE load with `missing field
    /// accessToken`, taking every other account down with it. tcr does not
    /// serve API-key accounts, so the row is quarantined like any other
    /// unusable row and the rest of the config loads.
    #[test]
    fn load_quarantines_an_api_key_account() {
        let path = tmp_file(
            "load-quarantine-apikey",
            r#"{"accounts":[{"name":"ok","accessToken":"a"},{"name":"key","type":"apikey","apiKey":"sk-ant-api-fake"}]}"#,
        );

        let config = load(&path).expect("one API-key row must not fail the whole load");
        assert_only_ok_loads_and_quarantined_is(&config, "key");
        fs::remove_file(&path).ok();
    }

    /// An `apikey` row is set aside even when it also carries an
    /// `accessToken`: tcr would send that value as a Bearer, which is not how
    /// an API key authenticates.
    #[test]
    fn load_quarantines_an_api_key_account_that_also_has_an_access_token() {
        let path = tmp_file(
            "load-quarantine-apikey-token",
            r#"{"accounts":[{"name":"ok","accessToken":"a"},{"name":"key","type":"apikey","accessToken":"sk-ant-api-fake"}]}"#,
        );

        let config = load(&path).unwrap();
        assert_only_ok_loads_and_quarantined_is(&config, "key");
        fs::remove_file(&path).ok();
    }

    /// A `type` that is not a string cannot be an `AccountType`. It used to
    /// fail the whole load; now only that row is set aside.
    #[test]
    fn load_quarantines_an_account_whose_type_is_not_a_string() {
        let path = tmp_file(
            "load-quarantine-type-number",
            r#"{"accounts":[{"name":"ok","accessToken":"a"},{"name":"odd","type":5,"accessToken":"b"}]}"#,
        );

        let config = load(&path).unwrap();
        assert_only_ok_loads_and_quarantined_is(&config, "odd");
        fs::remove_file(&path).ok();
    }

    /// The type names parse to their variants, an absent `type` is OAuth, and
    /// a string tcr does not know loads as it did when the field was a plain
    /// `String` and is written back exactly as the user wrote it, so a value
    /// another tool wrote survives a tcr save.
    #[test]
    fn account_type_parses_known_names_and_round_trips_an_unknown_one() {
        let path = tmp_path("load-account-type-round-trip");
        fs::write(
            &path,
            r#"{"accounts":[
                 {"name":"plain","accessToken":"a"},
                 {"name":"named","type":"oauth","accessToken":"b"},
                 {"name":"other","type":"OAuth-Legacy","accessToken":"c"}
               ]}"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert!(config.quarantined_accounts.is_empty());
        let types: Vec<&AccountType> = config.accounts.iter().map(|a| &a.account_type).collect();
        assert_eq!(
            types,
            vec![
                &AccountType::OAuth,
                &AccountType::OAuth,
                &AccountType::Unrecognized("OAuth-Legacy".to_string()),
            ]
        );
        assert!(config.accounts[0].account_type.is_oauth());
        assert!(!config.accounts[2].account_type.is_oauth());

        let out = tmp_path("load-account-type-round-trip-out");
        save(&out, &config).unwrap();
        let value = read_json(&out);
        assert_eq!(value["accounts"][1]["type"], "oauth");
        assert_eq!(value["accounts"][2]["type"], "OAuth-Legacy");
        assert_eq!(
            serde_json::to_value(AccountType::ApiKey).unwrap(),
            serde_json::json!("apikey")
        );
        fs::remove_file(&path).ok();
        fs::remove_file(&out).ok();
    }

    /// #474's parsed `type` beside the `provider` key. A Codex row, with no `type` or
    /// with `"type": "oauth"`, loads as an OAuth Codex account (so it is refreshed and
    /// probed, at its own provider's endpoints) and is never set aside as an API-key
    /// row or an unknown type, while the API-key row beside it is quarantined and the
    /// file still loads. A whole-file save keeps both Codex rows at the
    /// `credentialVersion` every build that cannot tell providers apart refuses.
    ///
    /// Watched red: cap every row at version 1, main's check, and the whole file is
    /// refused over the Codex rows' 2.
    #[test]
    fn a_codex_row_loads_as_codex_beside_a_quarantined_api_key_row() {
        use crate::provider::Provider;
        let path = tmp_path("load-codex-beside-apikey");
        fs::write(
            &path,
            r#"{"accounts":[
                 {"name":"key","type":"apikey","apiKey":"sk-ant-api-fake"},
                 {"name":"bare","provider":"codex","accessToken":"at-b","credentialVersion":2},
                 {"name":"typed","provider":"codex","type":"oauth","accessToken":"at-t",
                  "credentialVersion":2}
               ]}"#,
        )
        .unwrap();

        let config =
            load(&path).expect("an API-key row must not fail a file that also holds Codex rows");
        assert_eq!(config.quarantined_accounts, vec!["key".to_string()]);
        let loaded: Vec<(&str, Provider, &AccountType)> = config
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.provider, &a.account_type))
            .collect();
        assert_eq!(
            loaded,
            vec![
                ("bare", Provider::Codex, &AccountType::OAuth),
                ("typed", Provider::Codex, &AccountType::OAuth),
            ]
        );
        assert!(config.accounts.iter().all(|a| a.account_type.is_oauth()));

        let saved = tmp_path("load-codex-beside-apikey-saved");
        save(&saved, &config).unwrap();
        let value = read_json(&saved);
        for row in 0..2 {
            assert_eq!(value["accounts"][row]["provider"], json!("codex"));
            assert_eq!(value["accounts"][row]["type"], json!("oauth"));
            assert_eq!(value["accounts"][row]["credentialVersion"], json!(2));
        }
        fs::remove_file(&path).ok();
        fs::remove_file(&saved).ok();
    }

    /// The credential-version refusal runs BEFORE #474's quarantine and is never
    /// softened into one: an Anthropic row at 2, or a Codex row at 3, still refuses
    /// the whole file while an API-key row beside it is merely set aside. Quarantining
    /// the over-version row instead would let the rest of the file load around a
    /// credential shape this build does not understand for that row's provider.
    ///
    /// Watched red: let every row carry up to 2, the Codex ceiling, and the
    /// Anthropic case loads.
    #[test]
    fn a_credential_version_refusal_still_refuses_the_file_beside_an_api_key_row() {
        for (label, over) in [
            (
                "an Anthropic row at 2",
                r#"{"name":"alice@example.com","accessToken":"at-a","credentialVersion":2}"#,
            ),
            (
                "a Codex row at 3",
                r#"{"name":"bob@example.com","provider":"codex","accessToken":"at-b","credentialVersion":3}"#,
            ),
        ] {
            let path = tmp_path("refuse-beside-apikey");
            fs::write(
                &path,
                format!(
                    r#"{{"accounts":[
                         {{"name":"key","type":"apikey","apiKey":"sk-ant-api-fake"}},
                         {{"name":"ok","accessToken":"at-ok"}},
                         {over}
                       ]}}"#
                ),
            )
            .unwrap();
            let err = load(&path).expect_err(label);
            assert!(
                err.to_string().contains("refusing to load"),
                "{label}: the file is refused, not quarantined around: {err}"
            );
            fs::remove_file(&path).ok();
        }
    }

    /// A row whose own `upstream` names a third-party host
    /// carries that vendor's key, and tcr forwards every account to its one
    /// configured upstream with the row's token as a Bearer. Such a row is
    /// quarantined at load, like a row with no token: it never serves, the
    /// other accounts load, and the load itself does not fail.
    #[test]
    fn load_quarantines_a_foreign_upstream_account() {
        let path = tmp_path("load-foreign-upstream");
        fs::write(
            &path,
            r#"{"accounts":[{"name":"ok","accessToken":"a"},{"name":"vendor","type":"oauth","accessToken":"sk-x","upstream":"https://api.example.net/anthropic"}]}"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        let names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["ok"], "only `ok` may be usable");
        assert_eq!(config.quarantined_accounts, vec!["vendor".to_string()]);
        fs::remove_file(&path).ok();
    }

    /// The host is compared after parsing, not by substring: a host that only
    /// starts with Anthropic's name is still foreign.
    #[test]
    fn load_quarantines_a_lookalike_anthropic_upstream() {
        let path = tmp_path("load-lookalike-upstream");
        fs::write(
            &path,
            r#"{"accounts":[{"name":"ok","accessToken":"a"},{"name":"vendor","accessToken":"sk-x","upstream":"https://api.anthropic.com.example.net/v1"}]}"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "ok");
        assert_eq!(config.quarantined_accounts, vec!["vendor".to_string()]);
        fs::remove_file(&path).ok();
    }

    /// An `upstream` naming Anthropic's own API host, or the host this file
    /// already forwards to, still loads as a normal account.
    #[test]
    fn load_keeps_an_anthropic_or_own_upstream_account() {
        let path = tmp_path("load-anthropic-upstream");
        fs::write(
            &path,
            r#"{"upstream":"http://127.0.0.1:9000","accounts":[
                 {"name":"direct","accessToken":"a","upstream":"https://api.anthropic.com"},
                 {"name":"stub","accessToken":"b","upstream":"http://127.0.0.1:9000/"}
               ]}"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        let names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["direct", "stub"]);
        assert!(config.quarantined_accounts.is_empty());
        fs::remove_file(&path).ok();
    }

    /// The per-account `upstream` check is per provider, because a Codex row's token
    /// goes to `codexUpstream` and never to the top-level `upstream`. A Codex row
    /// naming Codex's default host, or the configured `codexUpstream` (a loopback
    /// stub, as every in-process Codex test configures it), loads; one naming
    /// Anthropic's API is set aside, as is an Anthropic row naming chatgpt.com. A
    /// Codex row with no `upstream` of its own, the shape `tcr login --from-codex`
    /// writes, loads whatever `codexUpstream` says, exactly as before the check.
    ///
    /// Watched red: hold every row to Anthropic's pair, as the check alone did, and
    /// `default` and `stub` are quarantined while `crossed` loads.
    #[test]
    fn load_holds_a_codex_rows_own_upstream_to_the_codex_hosts() {
        use crate::provider::Provider;
        let path = tmp_path("load-codex-own-upstream");
        fs::write(
            &path,
            r#"{"codexUpstream":"http://127.0.0.1:9100","accounts":[
                 {"name":"claude","accessToken":"a"},
                 {"name":"plain","provider":"codex","accessToken":"b","credentialVersion":2},
                 {"name":"default","provider":"codex","accessToken":"c","credentialVersion":2,
                  "upstream":"https://chatgpt.com"},
                 {"name":"stub","provider":"codex","accessToken":"d","credentialVersion":2,
                  "upstream":"http://127.0.0.1:9100/"},
                 {"name":"crossed","provider":"codex","accessToken":"e","credentialVersion":2,
                  "upstream":"https://api.anthropic.com"},
                 {"name":"claude-at-openai","accessToken":"f","upstream":"https://chatgpt.com"}
               ]}"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        let loaded: Vec<(&str, Provider)> = config
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.provider))
            .collect();
        assert_eq!(
            loaded,
            vec![
                ("claude", Provider::Anthropic),
                ("plain", Provider::Codex),
                ("default", Provider::Codex),
                ("stub", Provider::Codex),
            ],
            "a Codex row naming a Codex host loads, as Codex"
        );
        assert_eq!(
            config.quarantined_accounts,
            vec!["crossed".to_string(), "claude-at-openai".to_string()],
            "a row naming the OTHER provider's host is set aside"
        );
        fs::remove_file(&path).ok();
    }

    /// `quarantined_accounts` is `#[serde(skip)]`: a `Config` carrying names in
    /// it must not write them anywhere on `save()`, and a hand-edited file
    /// carrying a same-named key must not be read into it either — the field
    /// exists purely for the in-process caller, never the file.
    #[test]
    fn quarantined_accounts_do_not_serialize() {
        let path = tmp_file("load-quarantine-serialize", GOOD_ROW_AND_IMPORT_ROW);
        let config = load(&path).unwrap();
        assert_eq!(config.quarantined_accounts, vec!["acct-import".to_string()]);

        let out = tmp_path("load-quarantine-serialize-out");
        save(&out, &config).unwrap();
        let value = read_json(&out);
        assert!(
            value.get("quarantinedAccounts").is_none()
                && value.get("quarantined_accounts").is_none(),
            "quarantined_accounts leaked into the saved file: {value}"
        );
        // Round-tripping the SAVED file back through `load` must not resurrect
        // a `quarantinedAccounts` key from the file into the field either.
        let reloaded = load(&out).unwrap();
        assert!(reloaded.quarantined_accounts.is_empty());

        fs::remove_file(&path).ok();
        fs::remove_file(&out).ok();
    }

    // --- legacy `throttle` auto-migration -------------------------------------
    //
    // Regression coverage for a shipped incident: a Sparkle auto-update split
    // `throttle` into `accountThrottle`/`fleetThrottle` with no migration, and
    // a stale `throttle` key used to be a hard `load` error. It must never be
    // one again — every one of these loads successfully.

    /// The incident's exact shape: an active legacy throttle migrates verbatim
    /// into `accountThrottle`, and `fleetThrottle` takes its own default.
    #[test]
    fn legacy_throttle_key_migrates_to_account_throttle() {
        let path = tmp_path("throttle-migrate-active");
        fs::write(
            &path,
            r#"{ "accounts": [], "throttle": { "burst": 4, "minSpacingMs": 350 } }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert!(config.migrated_legacy_throttle);
        assert_eq!(config.account_throttle.effective_min_spacing(), Some(350));
        assert_eq!(config.account_throttle.effective_burst(), 4);
        assert_eq!(config.fleet_throttle.effective_min_spacing(), Some(100));
        assert_eq!(config.fleet_throttle.effective_burst(), 32);

        fs::remove_file(&path).ok();
    }

    /// The old key's escape hatch (`"throttle": {}`) meant "no throttling at
    /// all" — migrating it must disable BOTH new buckets to preserve that.
    #[test]
    fn legacy_empty_throttle_disables_both_buckets() {
        let path = tmp_path("throttle-migrate-empty");
        fs::write(&path, r#"{ "accounts": [], "throttle": {} }"#).unwrap();

        let config = load(&path).unwrap();
        assert!(config.migrated_legacy_throttle);
        assert!(!config.account_throttle.is_active());
        assert!(!config.fleet_throttle.is_active());

        fs::remove_file(&path).ok();
    }

    /// A `burst` with no `minSpacingMs` is inert per `ThrottleConfig::is_active`
    /// — migration must treat it the same as `{}`, not as "active".
    #[test]
    fn legacy_throttle_burst_only_is_inert_and_disables_both_buckets() {
        let path = tmp_path("throttle-migrate-burst-only");
        fs::write(&path, r#"{ "accounts": [], "throttle": { "burst": 4 } }"#).unwrap();

        let config = load(&path).unwrap();
        assert!(!config.account_throttle.is_active());
        assert!(!config.fleet_throttle.is_active());

        fs::remove_file(&path).ok();
    }

    /// `minSpacingMs: 0` normalizes to "unset" (`ThrottleConfig::
    /// effective_min_spacing`'s footgun handling) — migration must see the
    /// legacy key as inert here too.
    #[test]
    fn legacy_throttle_zero_spacing_is_inert_and_disables_both_buckets() {
        let path = tmp_path("throttle-migrate-zero-spacing");
        fs::write(
            &path,
            r#"{ "accounts": [], "throttle": { "minSpacingMs": 0 } }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert!(!config.account_throttle.is_active());
        assert!(!config.fleet_throttle.is_active());

        fs::remove_file(&path).ok();
    }

    /// A half-run of the old manual migration script left BOTH the legacy key
    /// and a new key in the same file. The new key wins; `throttle` is dropped;
    /// this must still load without error.
    #[test]
    fn legacy_throttle_alongside_new_key_defers_to_the_new_key() {
        let path = tmp_path("throttle-migrate-conflict");
        fs::write(
            &path,
            r#"{ "accounts": [],
                 "throttle": { "burst": 4, "minSpacingMs": 350 },
                 "accountThrottle": { "burst": 9, "minSpacingMs": 999 } }"#,
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.account_throttle.effective_min_spacing(), Some(999));
        assert_eq!(config.account_throttle.effective_burst(), 9);

        fs::remove_file(&path).ok();
    }

    /// `throttle` holding something that cannot parse as a `ThrottleConfig` at
    /// all (a bare string) must be dropped, not fail the whole load — dropping
    /// it leaves both buckets at their normal (ON) defaults, same as if
    /// `throttle` had never been present at all.
    #[test]
    fn legacy_throttle_unparseable_value_is_dropped_without_error() {
        let path = tmp_path("throttle-migrate-unparseable");
        fs::write(&path, r#"{ "accounts": [], "throttle": "nonsense" }"#).unwrap();

        let config = load(&path).unwrap();
        assert!(config.migrated_legacy_throttle);
        assert_eq!(config.account_throttle.effective_min_spacing(), Some(350));
        assert_eq!(config.fleet_throttle.effective_min_spacing(), Some(100));

        fs::remove_file(&path).ok();
    }

    /// `migrated_legacy_throttle` is `#[serde(skip)]`, same contract as
    /// `quarantined_accounts` — never written to the file.
    #[test]
    fn migrated_legacy_throttle_does_not_serialize() {
        let path = tmp_path("throttle-migrate-no-serialize");
        fs::write(
            &path,
            r#"{ "accounts": [], "throttle": { "burst": 4, "minSpacingMs": 350 } }"#,
        )
        .unwrap();
        let config = load(&path).unwrap();
        assert!(config.migrated_legacy_throttle);

        let out = tmp_path("throttle-migrate-no-serialize-out");
        save(&out, &config).unwrap();
        let value = read_json(&out);
        assert!(
            value.get("migratedLegacyThrottle").is_none()
                && value.get("migrated_legacy_throttle").is_none(),
            "migrated_legacy_throttle leaked into the saved file: {value}"
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&out).ok();
    }

    /// The auto-fix claim: persisting a migrated config must actually remove
    /// `throttle` from the FILE BYTES, not just the in-memory struct, and must
    /// write `accountThrottle` in its place.
    #[test]
    fn persisting_a_migrated_config_removes_throttle_from_the_file() {
        let path = tmp_path("throttle-migrate-persist-roundtrip");
        fs::write(
            &path,
            r#"{ "accounts": [], "throttle": { "burst": 4, "minSpacingMs": 350 } }"#,
        )
        .unwrap();
        let config = load(&path).unwrap();
        assert!(config.migrated_legacy_throttle);

        save(&path, &config).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains(r#""throttle""#),
            "persisted file still carries the legacy key: {raw}"
        );
        assert!(raw.contains("accountThrottle"), "persisted file: {raw}");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn load_or_init_creates_the_file_and_its_parent_dir() {
        let dir = tempfile::tempdir().expect("a scratch dir");
        // Two levels down: a fresh box has no `~/.config` at all.
        let path = dir.path().join("config").join(crate::paths::CONFIG_FILE);
        assert!(!path.exists(), "the test must start with no file");

        let (config, created) = load_or_init(&path).expect("a missing config must be created");
        assert!(
            created,
            "the first call must report that it created the file"
        );
        assert!(
            config.accounts.is_empty(),
            "a created config has no accounts"
        );
        assert_eq!(
            config.upstream, "https://api.anthropic.com",
            "the created config carries this struct's own serde defaults"
        );

        // On disk, and readable by the ordinary `load` — a first-run file is
        // not a special shape.
        let reloaded = load(&path).expect("the created file loads like any other");
        assert!(reloaded.accounts.is_empty());
        assert_eq!(
            read_json(&path)["accounts"],
            json!([]),
            "the created file spells out an empty accounts array"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path)
                .expect("stat the created config")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a created config is 0600 like every other save — it will hold tokens"
        );
    }

    #[test]
    fn load_or_init_leaves_an_existing_file_byte_for_byte() {
        let path = tmp_path("load-or-init-existing");
        fs::write(&path, SAMPLE).expect("write the fixture");
        let before = fs::read(&path).expect("read the fixture back");

        let (config, created) = load_or_init(&path).expect("an existing config loads");
        assert!(!created, "an existing file was not created by this call");
        assert_eq!(config.accounts.len(), 1, "the existing fleet is what loads");
        assert_eq!(
            fs::read(&path).expect("the file is still there"),
            before,
            "loading must never rewrite the file it read"
        );
        crate::log_err::LogErr::log_debug(fs::remove_file(&path), "removing a test temp file");
    }

    #[test]
    fn load_or_init_still_refuses_a_corrupt_file() {
        let path = tmp_path("load-or-init-corrupt");
        fs::write(&path, "{ not json").expect("write the broken fixture");

        let err = load_or_init(&path).expect_err("a corrupt config must stay an error");
        assert!(
            matches!(err, ConfigError::Parse(_)),
            "a corrupt file is a parse error, never a silent default: {err}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("the file is still there"),
            "{ not json",
            "a corrupt file must never be overwritten by the defaults"
        );
        crate::log_err::LogErr::log_debug(fs::remove_file(&path), "removing a test temp file");
    }

    #[test]
    #[cfg(unix)]
    fn load_or_init_errors_rather_than_panics_when_the_parent_is_unwritable() {
        let dir = tempfile::tempdir().expect("a scratch dir");
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).expect("create the parent");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500))
            .expect("make the parent unwritable");

        let err = load_or_init(&locked.join(crate::paths::CONFIG_FILE))
            .expect_err("an unwritable parent must be an error");
        assert!(
            matches!(err, ConfigError::Io(_)),
            "an unwritable parent surfaces as an i/o error: {err}"
        );

        // Restore so the TempDir can clean itself up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restore");
    }

    /// A usable row, `acct-good`, beside `acct-import`, which only names another file to
    /// import from and so is quarantined.
    const GOOD_ROW_AND_IMPORT_ROW: &str = r#"{ "accounts": [
                 { "name": "acct-good", "accessToken": "at-good" },
                 { "name": "acct-import", "importFrom": "/some/other/file.json" }
               ] }"#;

    /// Only the row named `ok` loaded, and the one row set aside is `quarantined`.
    fn assert_only_ok_loads_and_quarantined_is(config: &Config, quarantined: &str) {
        let names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["ok"], "only `ok` may be usable");
        assert_eq!(config.quarantined_accounts, vec![quarantined.to_string()]);
    }
}
