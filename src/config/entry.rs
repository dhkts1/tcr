//! Finding the one `accounts` entry in the raw document that carries an account's identity.

use serde::Deserialize;
use serde_json::{Map, Value};

use super::Account;

/// The identity fields of an on-disk account entry — the only part of a stored
/// account this layer needs to read. Deliberately narrower than [`Account`]: an
/// entry the user is mid-edit on (no `accessToken` yet) still gets matched
/// rather than skipped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DiskIdentity {
    pub(super) name: String,
    #[serde(default)]
    pub(super) account_uuid: Option<String>,
    #[serde(default)]
    pub(super) org_uuid: Option<String>,
    #[serde(default)]
    pub(super) org_name: Option<String>,
}

/// Why no single `accounts` entry can take a targeted write. Converted into
/// each writer's own outcome enum, which carries the same two arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryRefusal {
    /// No usable `accounts` array, or nothing in it carries that identity.
    NoEntry,
    /// More than one entry carries that identity and nothing breaks the tie.
    Ambiguous,
}

/// Where the ONE `accounts` entry carrying `target`'s identity lives — or why
/// there is no one entry to write into. Separate from [`find_account_entry`] so
/// the whole array can be scanned immutably (counting matches) before any mutable
/// borrow is taken; a single mutable pass cannot both count and hand back a
/// reference.
#[derive(Debug, PartialEq)]
pub(super) enum EntryMatch {
    /// Exactly one entry matches, at this index of the `accounts` array.
    One(usize),
    /// No usable `accounts` array, or nothing in it carries that identity.
    None,
    /// Two or more entries match.
    Many,
}

pub(super) fn locate_account_entry(doc: &Map<String, Value>, target: &Account) -> EntryMatch {
    let Some(entries) = doc.get("accounts").and_then(Value::as_array) else {
        return EntryMatch::None;
    };
    // Parse every entry's identity first, then resolve over the whole set at once.
    // Returning `Many` on the second `same_identity` hit — which is what a single
    // scanning pass can do — refuses the LEGACY TWO-ORG SHAPE, where entry
    // `{name, uuid U, orgUuid "org-a"}` sits beside a pre-org entry `{name, uuid
    // U}` written before org UUIDs were stored. Those are two real accounts, one
    // person in two orgs, and `same_identity` matches the pre-org entry against
    // both of them because an unknown org is deliberately tolerated. Refused that
    // way, NEITHER account can ever be durably benched.
    //
    // `resolve` breaks exactly that tie and only that tie: when one candidate
    // matches on fully known identity and the rest matched only because something
    // was missing, the known one wins. A tie with nothing stricter to prefer is
    // still `Many`, and still refused.
    let probes: Vec<(usize, Account)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let object = entry.as_object()?;
            // Rows of different providers are never the same account, whatever their
            // names and ids say: an Anthropic row and a Codex row for the same person
            // legitimately share an email, and the name fallback and the org-unknown
            // tolerance would otherwise let a Codex import overwrite the Anthropic
            // credential. The entry's provider rides on its probe, and
            // `identity::same_identity` refuses every pair of different providers.
            // Absent means Anthropic, as everywhere else. A present value is read
            // through `Provider`'s own serde spelling, the one `load` uses, so a value
            // this build does not know (a hand-edit typo, a later provider) matches
            // NOTHING: read as Anthropic, it would take an Anthropic credential and then
            // lose its key to `merge_account`'s provider stamp, a silent relabel.
            let entry_provider = provider_of_entry(object)?;
            let stored =
                serde_json::from_value::<DiskIdentity>(Value::Object(object.clone())).ok()?;
            Some((
                index,
                crate::identity::probe(
                    entry_provider,
                    &stored.name,
                    stored.account_uuid,
                    stored.org_uuid,
                    stored.org_name,
                ),
            ))
        })
        .collect();
    match crate::identity::resolve(probes.iter().map(|(index, probe)| (*index, probe)), target) {
        crate::identity::Resolved::One(index) => EntryMatch::One(index),
        crate::identity::Resolved::None => EntryMatch::None,
        crate::identity::Resolved::Many => EntryMatch::Many,
    }
}

/// The provider an on-disk account entry belongs to, read the way `load` reads it:
/// an absent key is Anthropic, and a present one goes through [`Provider`]'s own
/// serde spelling. `None` for a value this build does not know, which every caller
/// treats as matching NOTHING — see [`locate_account_entry`] for why it must never
/// read as Anthropic. One reading for every matcher over the raw document
/// ([`locate_account_entry`], [`plan_merge`]), so they cannot drift on it.
///
/// [`Provider`]: crate::provider::Provider
pub(super) fn provider_of_entry(entry: &Map<String, Value>) -> Option<crate::provider::Provider> {
    match entry.get("provider") {
        None => Some(crate::provider::Provider::Anthropic),
        Some(value) => serde_json::from_value(value.clone()).ok(),
    }
}

/// The on-disk `accounts` entry whose identity matches `target`, or the
/// [`EntryRefusal`] explaining why there is no single entry to write.
///
/// Matching reuses the [`DiskIdentity`] probe + [`crate::identity::resolve`]
/// pairing that [`merge_tokens`] uses, so a rotated credential and a disabled flag
/// can never land on two different entries.
///
/// An AMBIGUOUS identity is refused, not resolved to the first match. The old
/// first-match-wins rested on "the CLI already refuses an ambiguous query", but
/// the TUI is a caller that never goes through that check — it selects by row
/// index — and `config::load` validates no uniqueness, so nothing upstream makes
/// the identity unique. `same_identity` falls back to name equality when either
/// side lacks a uuid, so two entries sharing a name match either runtime row: the
/// flag would land on whichever is earlier in the file, benching a healthy account
/// while the TUI shows the other one disabled.
///
/// Refused, but only where the tie is real — which is not the same as "a second
/// entry satisfied `same_identity`". `resolve` first tries the org key, so the
/// legacy two-org shape (one entry with an org, one written before org UUIDs
/// existed and carrying none) resolves both of its rows instead of locking both
/// out of ever being benched.
pub(super) fn find_account_entry<'a>(
    doc: &'a mut Map<String, Value>,
    target: &Account,
) -> Result<&'a mut Map<String, Value>, EntryRefusal> {
    let index = match locate_account_entry(doc, target) {
        EntryMatch::One(index) => index,
        EntryMatch::None => return Err(EntryRefusal::NoEntry),
        EntryMatch::Many => return Err(EntryRefusal::Ambiguous),
    };
    // The immutable scan above proved this path resolves; `NoEntry` here is the
    // total-function tail, not a reachable outcome.
    doc.get_mut("accounts")
        .and_then(Value::as_array_mut)
        .and_then(|entries| entries.get_mut(index))
        .and_then(Value::as_object_mut)
        .ok_or(EntryRefusal::NoEntry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::account_fixture;

    /// The provider guard in `locate_account_entry`, on the one shape where
    /// nothing else separates the two rows: the same name, and no `accountUuid`
    /// on the stored row, so `same_identity` falls back to name equality and
    /// calls them one account. Only the provider can say otherwise.
    ///
    /// The Anthropic target is the control: the row still resolves for the
    /// provider it belongs to, so a `None` above is the guard and not a row the
    /// matcher could never see.
    #[test]
    fn locate_account_entry_never_matches_across_providers() {
        let doc: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [ { "name": "alice@example.com", "accessToken": "at-a" } ] }"#,
        )
        .unwrap();

        let mut codex = account_fixture("alice@example.com");
        codex.provider = crate::provider::Provider::Codex;
        codex.account_uuid = Some("11111111-1111-4111-8111-111111111111".to_string());
        assert_eq!(
            locate_account_entry(&doc, &codex),
            EntryMatch::None,
            "a Codex target must never resolve to an Anthropic row, whatever the names say"
        );

        let anthropic = account_fixture("alice@example.com");
        assert_eq!(
            locate_account_entry(&doc, &anthropic),
            EntryMatch::One(0),
            "the same row must still resolve for an Anthropic target"
        );
    }

    /// A `provider` value this build does not know matches NO target. Were it
    /// read as Anthropic, the hand-typed `"Codex"` row below would take the
    /// Anthropic credential and then lose its key to the Updated arm's stamp,
    /// turning a loud load failure into a silent relabel.
    ///
    /// The second document is the control: the identical row, spelled the way
    /// `Provider` serialises, still resolves.
    #[test]
    fn locate_account_entry_matches_nothing_for_an_unrecognised_provider() {
        let typo: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "alice@example.com", "provider": "Codex", "accessToken": "at-a" }
            ] }"#,
        )
        .unwrap();
        let control: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "alice@example.com", "provider": "anthropic", "accessToken": "at-a" }
            ] }"#,
        )
        .unwrap();

        let anthropic = account_fixture("alice@example.com");
        assert_eq!(
            locate_account_entry(&typo, &anthropic),
            EntryMatch::None,
            "an unrecognised provider must not read as Anthropic"
        );
        assert_eq!(
            locate_account_entry(&control, &anthropic),
            EntryMatch::One(0)
        );
    }
}
