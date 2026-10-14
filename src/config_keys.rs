//! Every top-level config key tcr knows, and the ones a config file carries
//! that it does not.
//!
//! # Why this reports and never rejects
//!
//! tcr keeps a key it does not model verbatim across a load and save round
//! trip ([`crate::config::Config::extra`]), on purpose: the file is shared with
//! teamclaude, and refusing a key teamclaude writes would break that drop-in
//! promise. The cost of that tolerance is that a misspelled key
//! (`quotaProbeSecs` for `quotaProbeSeconds`) is kept on disk, has no effect,
//! and nothing says so. This registry is what lets `tcr doctor` and the boot
//! log name such a key, with the spelling it most likely meant, while the file
//! itself is left exactly as it is.
//!
//! # What counts as known
//!
//! Four kinds, see [`KeyKind`]: a field of [`crate::config::Config`], a key
//! read by name out of the unmodelled `extra` map, a teamclaude key tcr keeps
//! but does not use, and the legacy key `config::load` migrates. Two tests at
//! the bottom keep the first two kinds in step with the code: one reads the
//! `Config` struct's own fields, the other every key `src/` reads out of the
//! `extra` map.
//!
//! Top level only. Per-account and `proxy` keys have their own `extra` maps
//! and are out of scope here.

use serde::Serialize;
use serde_json::Value;

/// Why tcr recognises a top-level key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyKind {
    /// A field of [`crate::config::Config`].
    Modelled,
    /// Not a `Config` field, but read by name out of `Config::extra`
    /// (`src/manager/state.rs`, `src/manager/throttle.rs`).
    ReadFromExtra,
    /// Written by teamclaude, the JS proxy this config is shared with. tcr
    /// keeps it and does not read it.
    Teamclaude,
    /// tcr's own pre-split `throttle` key, which `config::load` migrates to
    /// `accountThrottle` and `fleetThrottle`.
    Legacy,
}

/// One key this build recognises at the top level of the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KnownKey {
    pub name: &'static str,
    pub kind: KeyKind,
}

const fn key(name: &'static str, kind: KeyKind) -> KnownKey {
    KnownKey { name, kind }
}

/// The registry. Order is for reading only; nothing depends on it.
const KNOWN_KEYS: &[KnownKey] = &[
    key("proxy", KeyKind::Modelled),
    key("upstream", KeyKind::Modelled),
    key("switchThreshold", KeyKind::Modelled),
    key("fableWeeklyThreshold", KeyKind::Modelled),
    key("pacing", KeyKind::Modelled),
    key("accountThrottle", KeyKind::Modelled),
    key("fleetThrottle", KeyKind::Modelled),
    key("lockAccount", KeyKind::Modelled),
    key("controlAccount", KeyKind::Modelled),
    key("controlReserve", KeyKind::Modelled),
    key("controlPooled", KeyKind::Modelled),
    key("controlIdentity", KeyKind::Modelled),
    key("resetUrgencyTierHours", KeyKind::Modelled),
    key("http1Only", KeyKind::Modelled),
    key("pricing", KeyKind::Modelled),
    key("usageRetentionDays", KeyKind::Modelled),
    key("accounts", KeyKind::Modelled),
    key("groupSettings", KeyKind::Modelled),
    key("quotaProbeSeconds", KeyKind::ReadFromExtra),
    key("warmupSeconds", KeyKind::ReadFromExtra),
    key("sessionAffinity", KeyKind::ReadFromExtra),
    key("revalidationServe", KeyKind::ReadFromExtra),
    key("loadBalanceMigration", KeyKind::ReadFromExtra),
    key("cacheTtlRewrite", KeyKind::ReadFromExtra),
    key("throttleExemptNoise", KeyKind::ReadFromExtra),
    key("divertBudget", KeyKind::ReadFromExtra),
    key("claudeCodeImport", KeyKind::ReadFromExtra),
    key("routes", KeyKind::Teamclaude),
    key("sx", KeyKind::Teamclaude),
    key("logDir", KeyKind::Teamclaude),
    key("autoUpdate", KeyKind::Teamclaude),
    key("throttle", KeyKind::Legacy),
];

/// Every key this build recognises at the top level of the config.
pub fn known_keys() -> &'static [KnownKey] {
    KNOWN_KEYS
}

/// Whether `name` is a top-level key this build recognises. Exact and
/// case-sensitive, because serde's matching is: `SessionAffinity` is ignored.
pub fn is_known(name: &str) -> bool {
    KNOWN_KEYS.iter().any(|known| known.name == name)
}

/// A top-level key in a config document that this build does not recognise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnknownKey {
    /// The key as the file spells it.
    pub key: String,
    /// The known key it is closest to, or `None` when nothing is close enough
    /// to be a likely typo (see [`nearest_known`]).
    pub nearest: Option<&'static str>,
}

/// The top-level keys of `document` that this build does not recognise,
/// sorted by key, each with its nearest known spelling. Empty for a document
/// with none, and for anything that is not a JSON object.
///
/// Pass the whole file or just `Config::extra` as an object: modelled keys are
/// known either way, so both give the same answer.
pub fn unknown_keys(document: &Value) -> Vec<UnknownKey> {
    let Some(object) = document.as_object() else {
        return Vec::new();
    };
    let mut unknown: Vec<UnknownKey> = object
        .keys()
        .filter(|name| !is_known(name))
        .map(|name| UnknownKey {
            key: name.clone(),
            nearest: nearest_known(name),
        })
        .collect();
    unknown.sort_by(|a, b| a.key.cmp(&b.key));
    unknown
}

/// The known key closest to `name` by edit distance, compared without case,
/// when it is close enough to be a typo: at most a third of the longer
/// spelling, and never less than 2. `quotaProbeSecs` gets `quotaProbeSeconds`
/// (3 edits in 17); `myComment` gets nothing, rather than an unrelated key
/// that happens to be closest.
pub fn nearest_known(name: &str) -> Option<&'static str> {
    let lowered = name.to_lowercase();
    KNOWN_KEYS
        .iter()
        .map(|known| {
            let distance = edit_distance(&lowered, &known.name.to_lowercase());
            (distance, known.name)
        })
        .filter(|(distance, known)| {
            let longest = name.chars().count().max(known.chars().count());
            *distance <= (longest / 3).max(2)
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, known)| known)
}

/// The one line the server logs at boot when the config carries keys this
/// build does not recognise, or `None` when it carries none.
pub fn boot_line(unknown: &[UnknownKey]) -> Option<String> {
    if unknown.is_empty() {
        return None;
    }
    let listed: Vec<String> = unknown.iter().map(describe).collect();
    Some(format!(
        "config has keys this build does not recognise and ignores: {} (kept in the file as written; run `tcr doctor` for details)",
        listed.join(", ")
    ))
}

/// `key` alone, or `key (did you mean nearest?)`.
pub fn describe(unknown: &UnknownKey) -> String {
    match unknown.nearest {
        Some(nearest) => format!("{} (did you mean {nearest}?)", unknown.key),
        None => unknown.key.clone(),
    }
}

/// Levenshtein distance over chars, one row at a time. Hand-rolled because the
/// crate has no string-distance dependency and this is the only caller.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = Vec::with_capacity(b.len() + 1);
        current.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != *cb);
            let insert = current[j] + 1;
            let delete = previous[j + 1] + 1;
            current.push(substitute.min(insert).min(delete));
        }
        previous = current;
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn names(kind: KeyKind) -> BTreeSet<&'static str> {
        KNOWN_KEYS
            .iter()
            .filter(|known| known.kind == kind)
            .map(|known| known.name)
            .collect()
    }

    /// The lane's own gate case: a misspelled probe cadence is named, with the
    /// key it meant.
    #[test]
    fn a_misspelled_key_is_named_with_its_nearest_spelling() {
        let unknown = unknown_keys(&json!({"accounts": [], "quotaProbeSecs": 60}));
        assert_eq!(
            unknown,
            vec![UnknownKey {
                key: "quotaProbeSecs".to_string(),
                nearest: Some("quotaProbeSeconds"),
            }]
        );
    }

    #[test]
    fn the_correct_spelling_is_not_reported() {
        assert_eq!(
            unknown_keys(&json!({"accounts": [], "quotaProbeSeconds": 60})),
            Vec::new()
        );
    }

    /// teamclaude's keys and the legacy `throttle` are kept on purpose, so a
    /// config written by teamclaude must read as clean.
    #[test]
    fn teamclaude_and_legacy_keys_are_not_reported() {
        let document = json!({
            "proxy": {"port": 3456}, "upstream": "https://api.anthropic.com",
            "switchThreshold": 0.9, "accounts": [], "routes": [], "sx": {},
            "logDir": "/tmp", "autoUpdate": true, "quotaProbeSeconds": 300,
            "warmupSeconds": 0, "throttle": {}
        });
        assert_eq!(unknown_keys(&document), Vec::new());
    }

    #[test]
    fn a_key_far_from_every_known_one_gets_no_suggestion() {
        assert_eq!(
            unknown_keys(&json!({"myComment": "hello"})),
            vec![UnknownKey {
                key: "myComment".to_string(),
                nearest: None,
            }]
        );
    }

    /// Case is a real miss (serde matches exactly), and the suggestion fixes it.
    #[test]
    fn a_wrong_case_key_is_reported_and_the_suggestion_is_the_right_case() {
        assert_eq!(
            unknown_keys(&json!({"SessionAffinity": false})),
            vec![UnknownKey {
                key: "SessionAffinity".to_string(),
                nearest: Some("sessionAffinity"),
            }]
        );
    }

    #[test]
    fn unknown_keys_come_back_sorted_and_a_non_object_has_none() {
        let unknown = unknown_keys(&json!({"zzz": 1, "cacheTtlRewrit": true}));
        let keys: Vec<&str> = unknown.iter().map(|u| u.key.as_str()).collect();
        assert_eq!(keys, vec!["cacheTtlRewrit", "zzz"]);
        assert_eq!(unknown_keys(&json!([1, 2])), Vec::new());
    }

    #[test]
    fn the_boot_line_is_absent_without_unknown_keys_and_names_each_one() {
        assert_eq!(boot_line(&[]), None);
        let line = boot_line(&unknown_keys(
            &json!({"quotaProbeSecs": 60, "myComment": 1}),
        ))
        .expect("a line when keys are unknown");
        assert!(
            line.contains("myComment, quotaProbeSecs (did you mean quotaProbeSeconds?)"),
            "{line}"
        );
    }

    #[test]
    fn no_key_is_registered_twice() {
        let unique: BTreeSet<&str> = KNOWN_KEYS.iter().map(|known| known.name).collect();
        assert_eq!(unique.len(), KNOWN_KEYS.len());
    }

    /// The camelCase name serde gives a snake_case field under
    /// `rename_all = "camelCase"`.
    fn camel_case(snake: &str) -> String {
        let mut out = String::new();
        let mut upper = false;
        for c in snake.chars() {
            if c == '_' {
                upper = true;
            } else if upper {
                out.extend(c.to_uppercase());
                upper = false;
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The JSON keys of `Config`, read from its source: every `pub` field of
    /// `pub struct Config` that is neither `#[serde(skip)]` nor
    /// `#[serde(flatten)]`, under its `rename` if it has one. Read from source,
    /// not from a serialized value, because a field with
    /// `skip_serializing_if` is absent from a default `Config`'s JSON and would
    /// slip past a serialization check.
    fn config_struct_keys() -> BTreeSet<String> {
        let source = include_str!("config.rs");
        let start = source
            .find("\npub struct Config {\n")
            .expect("config.rs declares `pub struct Config {`");
        let body = &source[start..];
        let end = body.find("\n}\n").expect("the Config struct closes");
        let mut keys = BTreeSet::new();
        let mut attributes = String::new();
        for line in body[..end].lines().skip(2) {
            let line = line.trim();
            if line.starts_with("#[") {
                attributes.push_str(line);
                continue;
            }
            let Some(field) = line.strip_prefix("pub ") else {
                continue;
            };
            let Some((name, _)) = field.split_once(':') else {
                continue;
            };
            let serde_words: Vec<&str> = attributes
                .split(['(', ')', ',', '[', ']'])
                .map(str::trim)
                .collect();
            let skipped = serde_words.iter().any(|w| *w == "skip" || *w == "flatten");
            if !skipped {
                let renamed = serde_words
                    .iter()
                    .find_map(|w| w.strip_prefix("rename = \""))
                    .map(|w| w.trim_end_matches('"').to_string());
                keys.insert(renamed.unwrap_or_else(|| camel_case(name.trim())));
            }
            attributes.clear();
        }
        keys
    }

    /// The drift test: the registry's modelled keys are exactly `Config`'s
    /// fields. A new field without a registry line fails here, and so does a
    /// registry line for a field that was removed or renamed.
    #[test]
    fn the_modelled_keys_match_the_config_struct() {
        let from_struct = config_struct_keys();
        let registered: BTreeSet<String> = names(KeyKind::Modelled)
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            from_struct, registered,
            "src/config.rs `Config` and config_keys.rs KNOWN_KEYS (Modelled) disagree"
        );
    }

    /// A cross-check on the source reading above: every key a default
    /// `Config` serializes to is registered as modelled.
    #[test]
    fn every_key_a_default_config_serializes_is_modelled() {
        let config: crate::config::Config =
            serde_json::from_str("{}").expect("an empty object is a valid config");
        let value = serde_json::to_value(&config).expect("a config serializes");
        let modelled = names(KeyKind::Modelled);
        let object = value.as_object().expect("a config serializes to an object");
        assert!(!object.is_empty(), "positive control: some keys serialized");
        for name in object.keys() {
            assert!(
                modelled.contains(name.as_str()),
                "`{name}` is a Config key missing from KNOWN_KEYS"
            );
        }
    }

    /// Every `src/` file, recursively.
    fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read a src directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The drift test for the second kind: every key the code reads out of the
    /// unmodelled map (`.extra` then `.get("key")`, across a line break or
    /// not) is registered as read from it, and every key registered that way
    /// is still read somewhere.
    #[test]
    fn the_read_from_extra_keys_match_the_code() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        let mut read = BTreeSet::new();
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read a source file");
            let compact: String = text.split_whitespace().collect();
            for (index, _) in compact.match_indices(".extra.get(\"") {
                let rest = &compact[index + ".extra.get(\"".len()..];
                if let Some((name, _)) = rest.split_once('"') {
                    read.insert(name.to_string());
                }
            }
        }
        assert!(
            read.contains("quotaProbeSeconds"),
            "positive control: the scan finds the probe cadence read in state.rs"
        );
        let registered: BTreeSet<String> = names(KeyKind::ReadFromExtra)
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            read, registered,
            "keys read from Config::extra and config_keys.rs KNOWN_KEYS (ReadFromExtra) disagree"
        );
    }
}
