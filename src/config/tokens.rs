//! Writing credentials into the file without reverting anyone's settings, and
//! settings without reverting credentials.

use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};

use super::credential_version::{stamp_credential_version, with_codex_credential_versions};
use super::edit::CODEX_SYNC_PENDING_KEY;
use super::entry::{DiskIdentity, provider_of_entry};
use super::file::{read_document, save, write_atomic};
use super::{Account, Config, ConfigError};

/// Persist ONLY the per-account credential state from `memory` into the file at
/// `path`, leaving every user-owned setting on disk exactly as the user left it.
///
/// The running server's `Config` is a BOOT-TIME snapshot: it goes stale the
/// moment the user edits the file, and only the credential fields
/// (`access_token` / `refresh_token` / `expires_at`) are ever mutated in memory
/// afterwards. Writing that whole snapshot back — which is what a plain [`save`]
/// does — therefore reverts every edit made while the proxy runs (observed live
/// 2026-07-25: a deleted `pacing` key was restored by the shutdown flush and read
/// back by the next boot, three restarts running). So persisting is a
/// read-modify-write: the FILE is the authority for everything except tokens.
///
/// Accounts are matched by identity ([`crate::identity::same_identity`], which
/// reduces to name equality for the current config shape) and never by index —
/// indices shift when the user adds or removes an account. Iterating the on-disk
/// list and pulling tokens IN gives the two removal semantics for free: an
/// account the user deleted from the file is not resurrected, and an account on
/// disk that the server never loaded is left untouched.
///
/// An unreadable file, a malformed one, or one carrying no usable `accounts`
/// list falls back to writing the in-memory config: a just-rotated refresh token
/// is single-use, so dropping it strands that account on `invalid_grant`
/// forever, which is strictly worse than overwriting a file whose account list
/// we cannot find anyway. Every fallback logs a warning naming the cause.
///
/// A credential that cannot be placed on a SPECIFIC entry — the entry is
/// malformed, or its identity matches nothing the server loaded — is never a
/// reason to fail the whole write: every other account's token still lands, and
/// [`merge_tokens`] hands back what it could not place so each miss is warned
/// about by name here. Silence was the old defect: the merge skipped, the write
/// succeeded, `Ok(())` came back, and the caller's error branch never ran.
///
/// The merge runs on the file's raw JSON document, NOT on a `Config` round-trip:
/// deserializing would materialize every serde default back into the file, so a
/// key the user just DELETED would reappear as its default (`"pacing": {}`) and a
/// key they never wrote would appear for the first time. Editing the parsed
/// document leaves the file byte-identical apart from the credential fields.
/// The credential fields, named once.
///
/// [`merge_tokens`] writes exactly these from memory (via [`Credentials`]) and
/// [`carry_disk_credentials`] preserves exactly these from disk. A key in one
/// list and not the other is a field that silently stops being protected, which
/// is what `the_credential_key_list_matches_what_merge_tokens_writes` exists to
/// catch.
const CREDENTIAL_KEYS: [&str; 3] = ["accessToken", "refreshToken", "expiresAt"];

/// Write `config` back, taking every account's CREDENTIALS from the file rather
/// than from `config`.
///
/// The mirror of [`save_tokens`]: that one writes credentials and preserves
/// everything else, this one writes everything else and preserves credentials.
/// Between them no writer has to serialize a field it did not change.
///
/// Every mutating `tcr account` verb reaches here through `cli::save_after_edit`,
/// and none of them changes a credential: they set a priority, a threshold,
/// `disabled`, group membership, or remove an entry outright. Writing
/// credentials from their snapshot could therefore only ever revert someone
/// else's rotation, and a single-use refresh token does not survive being
/// reverted (see `a_settings_edit_does_not_revert_a_rotated_credential`). The
/// server's own `warn_if_server_running` already told users to "expect to
/// re-apply" after such a clash, which is not something you can do for a
/// rotated token you never saw.
///
/// Unknown per-account keys need no help here. `Account::extra` is
/// `#[serde(flatten)]`, so they already round-trip through the typed `Config`;
/// the modeled credential triple is the whole exposure.
pub fn save_settings(path: &Path, config: &Config) -> Result<(), ConfigError> {
    // No readable file means there are no on-disk credentials to preserve, and
    // writing the snapshot whole is exactly what this call did before.
    let Ok(disk) = read_document(path) else {
        return save(path, config);
    };
    let Ok(Value::Object(mut doc)) =
        serde_json::to_value(with_codex_credential_versions(config).as_ref())
    else {
        // `Config` is a struct, so this is the total-function tail rather than a
        // reachable outcome. Falling back rather than failing keeps a verb that
        // would have worked before working.
        return save(path, config);
    };
    carry_disk_credentials(&mut doc, &disk, &config.accounts);
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)
}

/// Copy each matched account's credential fields from `disk` into `doc`.
///
/// Matched with [`plan_merge`], the same unambiguous-in-both-directions pairing
/// [`merge_tokens`] uses, so the two cannot drift on what counts as the same
/// account. An entry the planner declines to pair keeps whatever the snapshot
/// held, which is what it would have had anyway.
fn carry_disk_credentials(
    doc: &mut Map<String, Value>,
    disk: &Map<String, Value>,
    memory: &[Account],
) {
    let Some(entries) = disk.get("accounts").and_then(Value::as_array) else {
        return;
    };
    let Some(out) = doc.get_mut("accounts").and_then(Value::as_array_mut) else {
        return;
    };
    for (entry, (_, plan)) in entries.iter().zip(plan_merge(entries, memory)) {
        let EntryPlan::Write(position) = plan else {
            continue;
        };
        let (Some(from), Some(Value::Object(into))) = (entry.as_object(), out.get_mut(position))
        else {
            continue;
        };
        for key in CREDENTIAL_KEYS {
            match from.get(key) {
                Some(value) => {
                    into.insert(key.to_string(), value.clone());
                }
                // Absent on disk, so absent here. Leaving the snapshot's value
                // would reinstate a credential the user or another process
                // deliberately removed.
                None => {
                    into.remove(key);
                }
            }
        }
    }
}

/// Which accounts' credentials a token write is allowed to touch.
///
/// The in-memory config is a boot-time snapshot. [`Manager::persist_tokens`]
/// already knew not to write SETTINGS from it ("writing it whole would stamp
/// stale settings over the user's live file") and then wrote every account's
/// CREDENTIALS from that same snapshot, which are stale in exactly the same way
/// and far more expensive to lose. Another process that refreshed or re-authed
/// an account in the meantime gets its single-use refresh token stamped back to
/// the snapshot's copy; that account then 400s `invalid_grant` on its next
/// refresh and is dead until re-authed by hand.
///
/// No lock fixes this. The staleness is in the writer's memory, not in its file
/// access, so two writes that are already strictly ordered still lose the token
/// (see `a_stale_snapshot_reverts_another_processs_rotated_token`). What fixes
/// it is that a rotation knows which account it rotated, and says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialScope {
    /// Every account in the snapshot. Correct only for a writer that genuinely
    /// speaks for the whole fleet, which is the shutdown flush and nothing else.
    All,
    /// Only the account at this index in `memory.accounts`. Every rotation:
    /// [`crate::identity::Resolved::One`] already carries this index.
    Only(usize),
}

impl CredentialScope {
    /// Whether a loaded account at `position` is this write's business.
    const fn covers(self, position: usize) -> bool {
        match self {
            Self::All => true,
            Self::Only(only) => only == position,
        }
    }
}

pub fn save_tokens(path: &Path, memory: &Config) -> Result<(), ConfigError> {
    save_tokens_for(path, memory, CredentialScope::All)
}

/// [`save_tokens`], restricted to the accounts `scope` names.
///
/// The fallback paths below are deliberately NOT scoped. They fire only when the
/// file is unreadable, malformed, or has no usable accounts list, and the
/// existing policy there is to write the snapshot whole rather than lose a
/// rotated token into a broken file. That is unchanged; a scope narrows the
/// normal merge, not the salvage.
pub fn save_tokens_for(
    path: &Path,
    memory: &Config,
    scope: CredentialScope,
) -> Result<(), ConfigError> {
    let mut doc = match read_document(path) {
        Ok(doc) => doc,
        Err(ConfigError::Io(err)) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "config unreadable at persist time; falling back to writing the in-memory config"
            );
            return save(path, memory);
        }
        Err(ConfigError::Parse(err)) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "config on disk is malformed JSON at persist time; falling back to writing the in-memory config"
            );
            return save(path, memory);
        }
    };
    let report = match merge_tokens(&mut doc, memory, scope) {
        Ok(report) => report,
        Err(reason) => {
            tracing::warn!(
                reason = %reason,
                path = %path.display(),
                "config on disk has no usable accounts list at persist time; falling back to writing the in-memory config so the rotated tokens are not lost"
            );
            return save(path, memory);
        }
    };
    report.warn_unpersisted(path);
    write_atomic(path, &serde_json::to_string_pretty(&doc)?)
}

/// The mutable credential triple, carrying the SAME serde renames as
/// [`Account`] so the merged keys are spelled exactly as the account struct
/// spells them — one source of truth for the wire names.
///
/// The `Option`s skip rather than clear: memory holds `None` only when the file
/// had no such field at boot, so an absent value means "nothing to say about
/// this key", never "delete what the user has since written there".
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Credentials<'a> {
    pub(super) access_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) refresh_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) expires_at: Option<i64>,
}

/// Why the on-disk `accounts` list could not be merged into AT ALL. Not one
/// account's problem but the whole document's, so [`save_tokens`] answers it with
/// the same whole-config fallback an unparseable file takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmergeable {
    /// No `accounts` key in the document.
    Missing,
    /// `accounts` is present but is not a JSON array.
    NotAnArray,
}

impl std::fmt::Display for Unmergeable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "the document has no accounts key",
            Self::NotAnArray => "the accounts key is not an array",
        })
    }
}

/// Why ONE on-disk account entry did not receive its rotated credentials. The
/// other entries in the same document are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The array element is not a JSON object (a string, a number, `null`).
    NotAnObject,
    /// The element is an object but carries no readable identity — no string
    /// `name`, which every match needs.
    NoIdentity,
    /// The entry is well-formed but no loaded account shares its identity. The
    /// signature of an account renamed on disk while the proxy was running,
    /// which is exactly the live-edit workflow [`save_tokens`] exists to support.
    NoMemoryMatch,
    /// The entry is well-formed and has a loaded account it could belong to, but
    /// the pairing is not unique: either several loaded accounts carry its
    /// identity, or another on-disk entry carries the same identity and nothing
    /// stored says which entry is which. Writing here means picking one of them at
    /// random and stamping a rotated credential over another account's own
    /// single-use refresh token, so nothing is written and the entry keeps what it
    /// had. See [`crate::identity::resolve`].
    Ambiguous,
    /// The credential triple would not serialize. Structurally unreachable —
    /// reported rather than swallowed precisely because reaching it would mean an
    /// assumption this module rests on has broken.
    CredentialEncoding,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAnObject => "the entry is not a JSON object",
            Self::NoIdentity => "the entry has no readable account name",
            Self::NoMemoryMatch => "no loaded account has that identity (renamed on disk?)",
            Self::Ambiguous => {
                "that identity does not pick out one loaded account and one entry, so no credential could be chosen"
            }
            Self::CredentialEncoding => "the credentials would not serialize",
        })
    }
}

/// One on-disk entry the merge could not write into. It keeps whatever
/// credential it already held — which, after a rotation, is a consumed one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedEntry {
    /// Position in the on-disk `accounts` array — the only handle the user has on
    /// an entry too malformed to carry a name.
    index: usize,
    /// The entry's `name` when one is readable, which it is for the common
    /// [`SkipReason::NoMemoryMatch`] case.
    name: Option<String>,
    reason: SkipReason,
}

impl SkippedEntry {
    /// How the entry is named in a log line: its `name`, else its position.
    fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("accounts[{}]", self.index))
    }
}

/// What a merge could not persist. Both lists are REPORTS, not failures: the
/// merge places every credential it can and hands the rest back, so
/// [`save_tokens`] logs each miss with the config path attached instead of the
/// helper logging blind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MergeReport {
    /// On-disk entries left holding whatever credential they already had.
    skipped: Vec<SkippedEntry>,
    /// Names of loaded accounts with no on-disk entry to write into. Benign when
    /// the user deleted the account; a token loss when they renamed it.
    absent_from_disk: Vec<String>,
    /// Entries left alone because [`CredentialScope`] did not name them. Counted
    /// rather than listed: this is the normal, expected outcome for every
    /// account a rotation did not rotate, and naming them would warn on every
    /// refresh for the life of the process.
    out_of_scope: usize,
}

impl MergeReport {
    /// Emit one line per credential that did NOT reach the file. A rotated
    /// refresh token is single-use, so a skip means that token is now consumed
    /// and unrecoverable — the account has to be re-authed. Nothing else in the
    /// stack can say this: the write itself succeeds and returns `Ok(())`.
    fn warn_unpersisted(&self, path: &Path) {
        for entry in &self.skipped {
            tracing::warn!(
                account = %entry.label(),
                index = entry.index,
                reason = %entry.reason,
                path = %path.display(),
                "rotated credential not persisted for this account; it may need `tcr login`"
            );
        }
        // A loaded account with no on-disk entry is USUALLY the user deleting it
        // from the file — correct, expected, and not worth a warning on every
        // persist for the rest of the process's life. Paired with an unmatched
        // on-disk entry in the same write it is instead the signature of a
        // RENAME, where a rotated credential really was dropped. The pairing is
        // what makes the two cases distinguishable, so only it escalates.
        let renamed = self
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::NoMemoryMatch);
        for name in &self.absent_from_disk {
            if renamed {
                tracing::warn!(
                    account = %name,
                    path = %path.display(),
                    "loaded account has no entry on disk while another entry matched nothing; a rename would drop its rotated credential, and it may need `tcr login`"
                );
            } else {
                tracing::debug!(
                    account = %name,
                    path = %path.display(),
                    "loaded account has no entry on disk; nothing persisted for it (removed from the file?)"
                );
            }
        }
    }
}

/// Overwrite the credential fields of every account present in BOTH `memory` and
/// the on-disk `doc`, matched by identity and never by position. Nothing else in
/// the document is touched.
///
/// Iterating the ON-DISK list and pulling tokens in gives both removal semantics
/// for free: an account the user deleted from the file is never resurrected (it
/// has no entry to write into), and an account on disk the server never loaded
/// is left alone (no memory match).
///
/// Every path that declines to write a credential is REPORTED, never silent: the
/// per-entry misses come back in the [`MergeReport`], and a document with no
/// usable `accounts` list comes back as [`Unmergeable`] so the caller can fall
/// back to writing the config whole rather than lose every rotated token in the
/// one write. A skipped entry is not an error for the others — the merge runs to
/// the end of the list either way.
fn merge_tokens(
    doc: &mut Map<String, Value>,
    memory: &Config,
    scope: CredentialScope,
) -> Result<MergeReport, Unmergeable> {
    // Plan against an IMMUTABLE view first. Deciding one entry at a time under a
    // mutable borrow is what made the old first-match resolution unfixable: the
    // assignment for entry N depends on which accounts the other entries claim,
    // which a single mutable pass cannot see. Same split, and for the same reason,
    // as `locate_account_entry` vs `find_account_entry`.
    let Some(accounts) = doc.get("accounts") else {
        return Err(Unmergeable::Missing);
    };
    let Some(entries) = accounts.as_array() else {
        return Err(Unmergeable::NotAnArray);
    };
    let plan = plan_merge(entries, &memory.accounts);

    let Some(accounts) = doc.get_mut("accounts").and_then(Value::as_array_mut) else {
        // The immutable read above already proved both of these; this is the
        // total-function tail, not a reachable outcome.
        return Err(Unmergeable::Missing);
    };

    let mut report = MergeReport::default();
    // Which loaded accounts found a home, so the ones that did not can be named
    // afterwards — a rename shows up here as well as in `skipped`.
    let mut placed = vec![false; memory.accounts.len()];

    for (index, (entry, (name, plan))) in accounts.iter_mut().zip(plan).enumerate() {
        let position = match plan {
            EntryPlan::Skip(reason) => {
                report.skipped.push(SkippedEntry {
                    index,
                    name,
                    reason,
                });
                continue;
            }
            EntryPlan::Write(position) => position,
        };
        // Not this write's account. NOT a `skipped` entry: that list means "a
        // credential we meant to write and could not", and every one of those is
        // warned about as a consumed single-use token. Declining to write an
        // account we never rotated is the opposite of a loss.
        if !scope.covers(position) {
            report.out_of_scope += 1;
            continue;
        }
        // A `Write` plan only comes from an entry the planner parsed, so both of
        // these hold by construction.
        let (Some(object), Some(fresh)) = (entry.as_object_mut(), memory.accounts.get(position))
        else {
            continue;
        };
        let credentials = Credentials {
            access_token: &fresh.access_token,
            refresh_token: fresh.refresh_token.as_deref(),
            expires_at: fresh.expires_at,
        };
        let Ok(Value::Object(fields)) = serde_json::to_value(&credentials) else {
            report.skipped.push(SkippedEntry {
                index,
                name,
                reason: SkipReason::CredentialEncoding,
            });
            continue;
        };
        object.extend(fields);
        match fresh.provider {
            crate::provider::Provider::Anthropic => {}
            crate::provider::Provider::Codex => {
                let key = CODEX_SYNC_PENDING_KEY;
                match fresh.extra.get(key) {
                    Some(value) => {
                        object.insert(key.to_string(), value.clone());
                    }
                    None => {
                        object.remove(key);
                    }
                }
            }
        }

        // Stamp the version on every entry this write actually touches, since
        // nothing else in the live path does — see [`CREDENTIAL_VERSION`]'s
        // doc-comment — at the version of the account's own provider: the
        // pairing above never crosses providers, so it is the entry's too.
        // Backfill only, except that a Codex entry below its version is raised
        // (see [`needs_credential_stamp`]); a number above the provider's is
        // never touched, so this can never paper over the refusal `load`
        // performs on an unknown future version.
        stamp_credential_version(object, fresh.provider);
        if let Some(seen) = placed.get_mut(position) {
            *seen = true;
        }
    }

    // In-scope only: an account this write was never going to touch is not
    // "absent from disk", it is none of this write's business.
    report.absent_from_disk = memory
        .accounts
        .iter()
        .enumerate()
        .zip(&placed)
        .filter(|((position, _), seen)| scope.covers(*position) && !**seen)
        .map(|((_, account), _)| account.name.clone())
        .collect();
    Ok(report)
}

/// What the read-only planning pass decided about one on-disk entry.
enum EntryPlan {
    /// Write the loaded account at this position into the entry.
    Write(usize),
    /// Leave the entry's credentials exactly as they are, for this reason.
    Skip(SkipReason),
}

/// Decide, for the whole `accounts` array at once, which loaded account owns each
/// on-disk entry — paired with the entry's readable `name` for the report.
///
/// An entry is written ONLY when the pairing is unambiguous in BOTH directions:
/// the entry resolves to exactly one loaded account, AND that account is claimed
/// by exactly one entry. One direction is not enough. Resolving each entry
/// independently — which is what a first-match search does — let two entries both
/// take the same loaded account, stamping that account's freshly rotated
/// credential onto an entry belonging to a DIFFERENT account and destroying that
/// account's own single-use refresh token. And an entry that has only one
/// candidate is still a guess when that candidate has two suitors: writing picks
/// one of two entries to be "the real one" on nothing.
///
/// Pairings are committed REPEATEDLY until no more can be, because each commitment
/// removes an account from one pool and an entry from the other, which can break a
/// tie that was unbreakable a moment ago. That is what keeps the legacy two-org
/// shape working — one person, two orgs, where the older entry predates org UUIDs
/// and so carries none. The org-carrying entry pairs off first (it is the only
/// *strict* match on either side), and the pre-org entry, which matched BOTH
/// accounts while they were both in the pool, is then left facing exactly one.
/// Resolved in a single pass it would tie forever and neither account would ever
/// have its rotated token persisted. Iterating also makes the result independent
/// of the order the entries happen to sit in the file.
///
/// Whatever is still unpaired when the fixed point is reached is reported, never
/// guessed.
///
/// Every pairing is confined to ONE provider: each entry's probe carries the entry's
/// provider and [`crate::identity::same_identity`] never pairs records of two
/// providers, so an entry is only ever weighed against loaded accounts of its own
/// provider, and an account only against entries of its own. An Anthropic row and a
/// Codex row may share a name, and when either side lacks an account UUID the name
/// fallback alone calls them one account — so, provider-blind, the two rows would tie
/// and neither rotation would be persisted, or an entry left with only the OTHER
/// provider's account would take that account's credential, a token the entry's
/// upstream will never accept.
fn plan_merge(entries: &[Value], memory: &[Account]) -> Vec<(Option<String>, EntryPlan)> {
    // Parse each entry's identity once. A `None` probe is an entry no match can
    // reach, and its plan is already final — so `plan[i].is_some()` is exactly
    // "this entry is settled", for both the unmatchable and the paired.
    let mut probes: Vec<Option<Account>> = Vec::with_capacity(entries.len());
    let mut names: Vec<Option<String>> = Vec::with_capacity(entries.len());
    let mut plan: Vec<Option<EntryPlan>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(object) = entry.as_object() else {
            probes.push(None);
            names.push(None);
            plan.push(Some(EntryPlan::Skip(SkipReason::NotAnObject)));
            continue;
        };
        // Read the name BEFORE the identity parse, so a half-edited entry that
        // fails to deserialize can still be named in the warning.
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        names.push(name);
        let Ok(stored) = serde_json::from_value::<DiskIdentity>(Value::Object(object.clone()))
        else {
            probes.push(None);
            plan.push(Some(EntryPlan::Skip(SkipReason::NoIdentity)));
            continue;
        };
        // A provider this build does not know matches nothing. No loaded account can
        // carry one (`load` refuses the value), so this is the no-match case. A known
        // one rides on the probe, and `identity::same_identity` never pairs it with an
        // account of another provider, in any of the resolves below.
        let Some(provider) = provider_of_entry(object) else {
            probes.push(None);
            plan.push(Some(EntryPlan::Skip(SkipReason::NoMemoryMatch)));
            continue;
        };
        probes.push(Some(crate::identity::probe(
            provider,
            &stored.name,
            stored.account_uuid,
            stored.org_uuid,
            stored.org_name,
        )));
        plan.push(None);
    }

    let mut claimed = vec![false; memory.len()];

    // Commit the mutually-unambiguous pairings until there are none left. Each
    // round settles at least one entry or ends the loop, so this terminates in at
    // most `entries.len()` rounds.
    loop {
        let mut progressed = false;
        for index in 0..probes.len() {
            let Some(probe) = probes[index].as_ref().filter(|_| plan[index].is_none()) else {
                continue;
            };
            // Which unclaimed account of its provider does this entry point at?
            let crate::identity::Resolved::One(position) = crate::identity::resolve(
                memory
                    .iter()
                    .enumerate()
                    .filter(|(position, _)| !claimed[*position]),
                probe,
            ) else {
                continue;
            };
            // …and does that account point back at this entry alone? Any other
            // unsettled entry of its provider with the same identity makes the write a
            // coin flip.
            let mutual = crate::identity::resolve(
                probes
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| plan[*other].is_none())
                    .filter_map(|(other, probe)| probe.as_ref().map(|probe| (other, probe))),
                &memory[position],
            ) == crate::identity::Resolved::One(index);
            if mutual {
                plan[index] = Some(EntryPlan::Write(position));
                claimed[position] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    // Everything the fixed point could not settle. An entry with no candidate left
    // at all is the rename/removal case the report already had a name for; an entry
    // that still has candidates is one this function refused to guess between.
    for index in 0..probes.len() {
        let Some(probe) = probes[index].as_ref().filter(|_| plan[index].is_none()) else {
            continue;
        };
        let unmatched = crate::identity::resolve(
            memory
                .iter()
                .enumerate()
                .filter(|(position, _)| !claimed[*position]),
            probe,
        ) == crate::identity::Resolved::None;
        plan[index] = Some(EntryPlan::Skip(if unmatched {
            SkipReason::NoMemoryMatch
        } else {
            SkipReason::Ambiguous
        }));
    }

    names
        .into_iter()
        .zip(plan)
        // Every slot was assigned above: unmatchable at parse, paired in the fixed
        // point, or classified in the sweep. This is the total-function tail.
        .map(|(name, plan)| {
            (
                name,
                plan.unwrap_or(EntryPlan::Skip(SkipReason::NoMemoryMatch)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load;
    use crate::config::tests::{read_json, tmp_path};
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    /// One account, tokens as given, written as the file the server booted from.
    fn one_account_file(access: &str, refresh: &str, expires: i64) -> String {
        format!(
            r#"{{ "accounts": [ {{ "name": "acct-a", "accessToken": "{access}", "refreshToken": "{refresh}", "expiresAt": {expires} }} ] }}"#
        )
    }

    /// THE regression guard. Reproduces what happened live on 2026-07-25: the
    /// server booted with `pacing.maxInFlightPerAccount = 3`, the user deleted
    /// the key while it ran, and the next persist stamped the boot-time snapshot
    /// back over the file — so the deleted setting returned and the next boot
    /// read it. Persisting must write the rotated tokens and NOTHING else.
    #[test]
    fn persist_does_not_clobber_a_user_edit() {
        let path = tmp_path("persist-user-edit");
        fs::write(
            &path,
            r#"{ "pacing": { "maxInFlightPerAccount": 3 },
                 "accounts": [ { "name": "acct-a", "accessToken": "at-old", "refreshToken": "rt-old", "expiresAt": 1 } ] }"#,
        )
        .unwrap();

        // The server's boot-time snapshot still carries the setting…
        let mut memory = load(&path).unwrap();
        assert_eq!(memory.pacing.max_in_flight_per_account, Some(3));
        // …the user deletes it while the proxy runs…
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        // …and a token rotates, triggering a persist.
        memory.accounts[0].access_token = "at-new".to_string();
        memory.accounts[0].refresh_token = Some("rt-new".to_string());
        memory.accounts[0].expires_at = Some(2);
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert!(
            value.get("pacing").is_none(),
            "the server restored a key the user deleted: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(2));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_preserves_unknown_top_level_keys() {
        let path = tmp_path("persist-unknown");
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        let mut memory = load(&path).unwrap();

        // The user adds keys the server does not model — including one it has
        // never seen — then a token rotates.
        fs::write(
            &path,
            r#"{ "quotaProbeSeconds": 120,
                 "routes": [{ "name": "r1", "match": "*fable*" }],
                 "accounts": [ { "name": "acct-a", "accessToken": "at-old", "refreshToken": "rt-old", "expiresAt": 1, "models": ["claude-fable-5"] } ] }"#,
        )
        .unwrap();
        memory.accounts[0].access_token = "at-new".to_string();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["quotaProbeSeconds"], json!(120));
        assert!(value["routes"].is_array());
        assert_eq!(value["accounts"][0]["models"], json!(["claude-fable-5"]));
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_falls_back_when_file_is_unreadable() {
        let path = tmp_path("persist-missing");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // No file at all (deleted under the running server, or a first-boot path
        // that has yet to create it): the rotated tokens must still land.
        assert!(!path.exists());
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(7));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_falls_back_when_file_is_malformed() {
        let path = tmp_path("persist-malformed");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // A single-use refresh token is worth more than an unparseable file.
        fs::write(&path, "{ this is not json").unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][0]["refreshToken"],
            json!("rt-new")
        );

        // Valid JSON that is not an object takes the same path.
        fs::write(&path, "[]").unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][0]["refreshToken"],
            json!("rt-new")
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_matches_accounts_by_name_not_index() {
        let path = tmp_path("persist-by-name");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 },
                   { "name": "acct-b", "accessToken": "at-b-new", "refreshToken": "rt-b-new", "expiresAt": 22 } ] }"#,
        )
        .unwrap();
        // The user reorders the accounts on disk while the proxy runs. Index 0 in
        // memory is acct-a; index 0 on disk is now acct-b.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-b", "accessToken": "at-b-old" },
                   { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["name"], json!("acct-b"));
        assert_eq!(
            value["accounts"][0]["accessToken"],
            json!("at-b-new"),
            "tokens landed by position, not identity"
        );
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-b-new"));
        assert_eq!(value["accounts"][1]["name"], json!("acct-a"));
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-a-new"));
        assert_eq!(value["accounts"][1]["expiresAt"], json!(11));
        fs::remove_file(&path).ok();
    }

    /// **THE token-clobber guard.** Two on-disk entries whose identities both match
    /// one loaded account, with nothing stored to tell them apart. The old
    /// first-match resolution walked the array and stamped the SAME account's
    /// freshly rotated credential onto both, so the second entry's own single-use
    /// refresh token was destroyed on disk — its next refresh 400s
    /// (`invalid_grant`) and the account is dead until re-authed by hand.
    ///
    /// Nothing may be written into either entry: an unbreakable tie is reported,
    /// never guessed. The entries keep the credentials they already held, which is
    /// recoverable; a foreign account's token in their place is not.
    #[test]
    fn persist_refuses_to_write_a_credential_into_an_ambiguous_entry() {
        let path = tmp_path("persist-ambiguous");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-new", "refreshToken": "rt-new", "expiresAt": 99 } ] }"#,
        )
        .unwrap();
        // Two entries share the name and carry no UUID, so `same_identity` matches
        // the loaded account against both. Entry 1 holds its OWN refresh token.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-one-old", "refreshToken": "rt-one-old" },
                   { "name": "acct-a", "accessToken": "at-two-old", "refreshToken": "rt-two-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-two-old"),
            "the second entry's own single-use refresh token was overwritten: {value}"
        );
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-two-old"));
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-one-old"),
            "a tie is refused on BOTH sides — picking the earlier entry is still a guess: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-one-old"));
        fs::remove_file(&path).ok();
    }

    /// The refusal above is reported, not silent: both entries come back as
    /// skipped, and the loaded account comes back as having found no home — which
    /// is what makes `save_tokens` warn by name that a rotated credential did not
    /// reach the file.
    #[test]
    fn an_ambiguous_entry_is_reported_as_skipped() {
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-new" } ] }"#,
        )
        .unwrap();
        let mut doc: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a" }, { "name": "acct-a" } ] }"#,
        )
        .unwrap();

        let report = merge_tokens(&mut doc, &memory, CredentialScope::All).unwrap();

        assert_eq!(
            report.skipped,
            vec![
                SkippedEntry {
                    index: 0,
                    name: Some("acct-a".to_string()),
                    reason: SkipReason::Ambiguous,
                },
                SkippedEntry {
                    index: 1,
                    name: Some("acct-a".to_string()),
                    reason: SkipReason::Ambiguous,
                },
            ]
        );
        assert_eq!(report.absent_from_disk, vec!["acct-a".to_string()]);
    }

    /// The legacy two-org shape must keep working: one person, two orgs, where the
    /// older entry predates org UUIDs and so carries none. `same_identity` matches
    /// the pre-org entry against BOTH accounts, so resolving each entry
    /// independently ties forever and neither account's rotated token is ever
    /// persisted. The strict pairing settles it: each side has exactly one partner
    /// whose org key it actually equals.
    ///
    /// Asserted in BOTH disk orders — the real config has the older (pre-org) entry
    /// first, which is precisely the order a single forward pass gets wrong.
    #[test]
    fn persist_places_both_tokens_on_the_legacy_two_org_shape() {
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "me@example.com", "accountUuid": "u1", "orgUuid": "org-a",
                     "accessToken": "at-a-new", "refreshToken": "rt-a-new" },
                   { "name": "me@example.com", "accountUuid": "u1",
                     "accessToken": "at-legacy-new", "refreshToken": "rt-legacy-new" } ] }"#,
        )
        .unwrap();

        for (label, disk) in [
            (
                "org-carrying entry first",
                r#"{ "accounts": [
                       { "name": "me@example.com", "accountUuid": "u1", "orgUuid": "org-a", "accessToken": "at-a-old" },
                       { "name": "me@example.com", "accountUuid": "u1", "accessToken": "at-legacy-old" } ] }"#,
            ),
            (
                "pre-org entry first",
                r#"{ "accounts": [
                       { "name": "me@example.com", "accountUuid": "u1", "accessToken": "at-legacy-old" },
                       { "name": "me@example.com", "accountUuid": "u1", "orgUuid": "org-a", "accessToken": "at-a-old" } ] }"#,
            ),
        ] {
            let path = tmp_path("persist-two-org");
            fs::write(&path, disk).unwrap();
            save_tokens(&path, &memory).unwrap();

            let value = read_json(&path);
            let by_org = |org: Option<&str>| {
                value["accounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e.get("orgUuid").and_then(Value::as_str) == org)
                    .unwrap_or_else(|| panic!("no entry with orgUuid {org:?} ({label}): {value}"))
                    .clone()
            };
            assert_eq!(
                by_org(Some("org-a"))["refreshToken"],
                json!("rt-a-new"),
                "the org-carrying entry missed its rotated token ({label}): {value}"
            );
            assert_eq!(
                by_org(None)["refreshToken"],
                json!("rt-legacy-new"),
                "the pre-org entry missed its rotated token ({label}): {value}"
            );
            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn persist_does_not_resurrect_an_account_the_user_removed() {
        let path = tmp_path("persist-removed");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a" },
                   { "name": "acct-gone", "accessToken": "at-gone" } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        let accounts = value["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1, "a removed account came back: {value}");
        assert_eq!(accounts[0]["name"], json!("acct-a"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_leaves_an_account_the_server_never_loaded_untouched() {
        let path = tmp_path("persist-added");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-new" } ] }"#,
        )
        .unwrap();
        // The user added a second account by hand after boot.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-old" },
                   { "name": "acct-new", "accessToken": "at-new", "refreshToken": "rt-new" } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-a-new"));
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][1]["refreshToken"], json!("rt-new"));
        fs::remove_file(&path).ok();
    }

    /// A document that parses but carries no `accounts` list at all — a
    /// truncated hand-edit, or a file another tool rewrote. The merge used to
    /// return before writing ANYTHING while `save_tokens` still reported `Ok`, so
    /// one write consumed and dropped the freshly rotated refresh token of every
    /// account at once. It is a malformed document, and takes the same
    /// whole-config fallback an unparseable one does.
    #[test]
    fn merge_with_missing_accounts_array_falls_back_and_warns() {
        let path = tmp_path("persist-no-accounts-key");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        let malformed = r#"{ "proxy": { "port": 3456 } }"#;
        fs::write(&path, malformed).unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(malformed).unwrap();
        assert_eq!(
            merge_tokens(&mut doc, &memory, CredentialScope::All),
            Err(Unmergeable::Missing),
            "the caller must be told, not handed a silently untouched document"
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-new"),
            "a single-use rotated token was dropped: {value}"
        );
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        assert_eq!(value["accounts"][0]["expiresAt"], json!(7));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the fallback must not loosen permissions on a token file"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn merge_with_non_array_accounts_falls_back() {
        let path = tmp_path("persist-accounts-not-array");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // `accounts` present but the wrong JSON type: nothing to merge into, and
        // writing the document back verbatim would drop the rotated token.
        let malformed = r#"{ "accounts": {} }"#;
        fs::write(&path, malformed).unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(malformed).unwrap();
        assert_eq!(
            merge_tokens(&mut doc, &memory, CredentialScope::All),
            Err(Unmergeable::NotAnArray)
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(value["accounts"][0]["refreshToken"], json!("rt-new"));
        assert_eq!(value["accounts"][0]["accessToken"], json!("at-new"));
        fs::remove_file(&path).ok();
    }

    /// The live-edit workflow `save_tokens` exists to support, in its one losing
    /// shape: renaming an account leaves its on-disk entry matching nothing, so
    /// its rotated credential cannot be placed. That is unavoidable — being
    /// silent about it is not. Both halves of the rename must be reported, and
    /// the OTHER account's token must still land.
    #[test]
    fn renamed_account_is_reported_not_silently_skipped() {
        let path = tmp_path("persist-renamed");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 },
                   { "name": "acct-b", "accessToken": "at-b-new", "refreshToken": "rt-b-new", "expiresAt": 22 } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a-renamed", "accessToken": "at-a-old", "refreshToken": "rt-a-old" },
                   { "name": "acct-b", "accessToken": "at-b-old", "refreshToken": "rt-b-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory, CredentialScope::All)
            .expect("the document has an accounts array");
        assert_eq!(
            report.skipped,
            vec![SkippedEntry {
                index: 0,
                name: Some("acct-a-renamed".to_string()),
                reason: SkipReason::NoMemoryMatch,
            }],
            "the unmatched on-disk entry must come back named"
        );
        assert_eq!(
            report.absent_from_disk,
            vec!["acct-a".to_string()],
            "the memory side of the rename must be visible too — that is what makes it a rename and not a deletion"
        );

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-a-old"),
            "a token landed on an entry that is not its own: {value}"
        );
        assert_eq!(value["accounts"][1]["accessToken"], json!("at-b-new"));
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-b-new"),
            "one skipped entry must not cost the other accounts their tokens"
        );
        assert_eq!(value["accounts"][1]["expiresAt"], json!(22));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn undeserializable_entry_does_not_block_its_siblings() {
        let path = tmp_path("persist-junk-entry");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 33 } ] }"#,
        )
        .unwrap();
        // Two entries a mid-edit file can hold — a bare string and an object with
        // no name — ahead of the real account.
        fs::write(
            &path,
            r#"{ "accounts": [
                   "acct-a",
                   { "accessToken": "at-orphan" },
                   { "name": "acct-a", "accessToken": "at-a-old", "refreshToken": "rt-a-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory, CredentialScope::All)
            .expect("the document has an accounts array");
        assert_eq!(
            report.skipped,
            vec![
                SkippedEntry {
                    index: 0,
                    name: None,
                    reason: SkipReason::NotAnObject,
                },
                SkippedEntry {
                    index: 1,
                    name: None,
                    reason: SkipReason::NoIdentity,
                },
            ]
        );
        assert!(
            report.absent_from_disk.is_empty(),
            "the loaded account did find its entry"
        );
        // A nameless entry is still addressable by the user: they can count rows.
        assert_eq!(report.skipped[0].label(), "accounts[0]");

        save_tokens(&path, &memory).unwrap();
        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0],
            json!("acct-a"),
            "a malformed entry was rewritten instead of left alone"
        );
        assert_eq!(value["accounts"][1], json!({ "accessToken": "at-orphan" }));
        assert_eq!(value["accounts"][2]["accessToken"], json!("at-a-new"));
        assert_eq!(
            value["accounts"][2]["refreshToken"],
            json!("rt-a-new"),
            "junk ahead of a good entry blocked its rotated token: {value}"
        );
        assert_eq!(value["accounts"][2]["expiresAt"], json!(33));
        fs::remove_file(&path).ok();
    }

    /// The benign twin of the rename: an account deleted from the file is
    /// reported as absent, with NO unmatched on-disk entry beside it. That
    /// pairing is the only thing distinguishing a correct deletion from a
    /// rename that just cost an account its refresh token.
    #[test]
    fn account_removed_from_disk_is_reported_without_a_skip() {
        let path = tmp_path("persist-removed-report");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new" },
                   { "name": "acct-gone", "accessToken": "at-gone-new" } ] }"#,
        )
        .unwrap();
        fs::write(
            &path,
            r#"{ "accounts": [ { "name": "acct-a", "accessToken": "at-a-old" } ] }"#,
        )
        .unwrap();

        let mut doc = read_document(&path).unwrap();
        let report = merge_tokens(&mut doc, &memory, CredentialScope::All)
            .expect("the document has an accounts array");
        assert!(
            report.skipped.is_empty(),
            "a deletion leaves no unmatched on-disk entry: {report:?}"
        );
        assert_eq!(report.absent_from_disk, vec!["acct-gone".to_string()]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn save_tokens_writes_owner_only_permissions() {
        let path = tmp_path("persist-perm");
        let memory: Config =
            serde_json::from_str(&one_account_file("at-new", "rt-new", 7)).unwrap();
        // Both paths through save_tokens must land 0600: the merge…
        fs::write(&path, one_account_file("at-old", "rt-old", 1)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // …and the fallback.
        fs::remove_file(&path).unwrap();
        save_tokens(&path, &memory).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).ok();
    }

    /// T1: a steady-state persist must stamp `credentialVersion` on the entry it
    /// actually writes into, since nothing else in the live path did — see
    /// [`CREDENTIAL_VERSION`]'s doc-comment. An entry the merge does not touch
    /// (the account the server never loaded) must be left byte-identical,
    /// `credentialVersion` included: this is not the version check, which lives
    /// in `load` and is unaffected.
    #[test]
    fn persist_backfills_credential_version_on_the_touched_entry_only() {
        let path = tmp_path("persist-credential-version");
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-new", "refreshToken": "rt-a-new", "expiresAt": 11 } ] }"#,
        )
        .unwrap();
        // acct-a has no credentialVersion and is the one this save touches;
        // acct-b is never loaded into memory, so the merge must not write it at
        // all.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a-old" },
                   { "name": "acct-b", "accessToken": "at-b", "credentialVersion": 1 } ] }"#,
        )
        .unwrap();
        save_tokens(&path, &memory).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["credentialVersion"],
            json!(1),
            "a touched entry with no credentialVersion must be stamped: {value}"
        );
        assert_eq!(
            value["accounts"][1],
            json!({ "name": "acct-b", "accessToken": "at-b", "credentialVersion": 1 }),
            "an untouched entry must be left byte-identical: {value}"
        );
        fs::remove_file(&path).ok();
    }

    /// The token merge never pairs an entry with another provider's account. The
    /// Anthropic entry below is the only one named like the loaded Codex account and
    /// carries no `accountUuid`, so identity alone pairs them — stamping the Codex
    /// account's rotated credential over the Anthropic row's own single-use refresh
    /// token, with a token Anthropic will never accept. It is reported as matching
    /// nothing instead, which is also what lets the warning name it as a possible
    /// rename rather than an ambiguity.
    ///
    /// The Anthropic memory is the control: the same entry still takes a rotated
    /// credential from an account of its own provider.
    #[test]
    fn a_token_merge_never_writes_a_codex_credential_onto_an_anthropic_entry() {
        let disk = r#"{ "accounts": [
            { "name": "alice@example.com", "accessToken": "at-anthropic", "refreshToken": "rt-anthropic" } ] }"#;
        let codex_memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "alice@example.com", "provider": "codex",
                     "accessToken": "at-codex-rotated", "refreshToken": "rt-codex-rotated" } ] }"#,
        )
        .unwrap();

        let mut doc: Map<String, Value> = serde_json::from_str(disk).unwrap();
        let report = merge_tokens(&mut doc, &codex_memory, CredentialScope::All).unwrap();
        assert_eq!(
            doc["accounts"][0]["refreshToken"],
            json!("rt-anthropic"),
            "a Codex credential landed on an Anthropic entry: {doc:?}"
        );
        assert_eq!(doc["accounts"][0]["accessToken"], json!("at-anthropic"));
        assert_eq!(
            report.skipped,
            vec![SkippedEntry {
                index: 0,
                name: Some("alice@example.com".to_string()),
                reason: SkipReason::NoMemoryMatch,
            }],
            "an entry with no account of its own provider matches nothing"
        );

        let anthropic_memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "alice@example.com",
                     "accessToken": "at-anthropic-rotated", "refreshToken": "rt-anthropic-rotated" } ] }"#,
        )
        .unwrap();
        let mut doc: Map<String, Value> = serde_json::from_str(disk).unwrap();
        merge_tokens(&mut doc, &anthropic_memory, CredentialScope::All).unwrap();
        assert_eq!(
            doc["accounts"][0]["refreshToken"],
            json!("rt-anthropic-rotated"),
            "control: an account of the entry's own provider must still be written"
        );
    }

    /// An entry whose `provider` this build does not know matches nothing in the token
    /// merge either, by the same reading `locate_account_entry` uses: read as
    /// Anthropic, the hand-typed `"Codex"` row below would take the Anthropic
    /// account's rotated credential.
    ///
    /// The second document is the control: the identical row, spelled the way
    /// `Provider` serialises, takes it.
    #[test]
    fn a_token_merge_matches_nothing_to_an_entry_with_an_unrecognised_provider() {
        let memory: Config = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "alice@example.com", "accessToken": "at-rotated", "refreshToken": "rt-rotated" } ] }"#,
        )
        .unwrap();

        let mut typo: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "alice@example.com", "provider": "Codex",
                     "accessToken": "at-old", "refreshToken": "rt-old" } ] }"#,
        )
        .unwrap();
        let report = merge_tokens(&mut typo, &memory, CredentialScope::All).unwrap();
        assert_eq!(
            typo["accounts"][0]["refreshToken"],
            json!("rt-old"),
            "an unrecognised provider must not read as Anthropic: {typo:?}"
        );
        assert_eq!(
            report.skipped,
            vec![SkippedEntry {
                index: 0,
                name: Some("alice@example.com".to_string()),
                reason: SkipReason::NoMemoryMatch,
            }]
        );

        let mut control: Map<String, Value> = serde_json::from_str(
            r#"{ "accounts": [
                   { "name": "alice@example.com", "provider": "anthropic",
                     "accessToken": "at-old", "refreshToken": "rt-old" } ] }"#,
        )
        .unwrap();
        merge_tokens(&mut control, &memory, CredentialScope::All).unwrap();
        assert_eq!(control["accounts"][0]["refreshToken"], json!("rt-rotated"));
    }

    /// Two accounts, both credentials at their original value.
    fn two_account_file() -> &'static str {
        r#"{ "accounts": [
               { "name": "acct-a", "accessToken": "at-a", "refreshToken": "rt-a", "expiresAt": 1 },
               { "name": "acct-b", "accessToken": "at-b", "refreshToken": "rt-b", "expiresAt": 1 } ] }"#
    }

    /// THE regression guard for the scoped write.
    ///
    /// The story, all of it real: the server boots and snapshots the config.
    /// The user then re-auths `acct-b` in a separate `tcr` process, which puts
    /// a fresh single-use refresh token on disk. The server's snapshot still
    /// holds b's OLD credential. The server then rotates `acct-a` and persists.
    ///
    /// Before `CredentialScope`, that persist wrote the snapshot's credentials
    /// for EVERY matched account, so it stamped b's stale token back over the
    /// re-auth. b then 400s `invalid_grant` on its next refresh and is dead
    /// until re-authed by hand, again.
    ///
    /// There is no concurrency here. The two writes are strictly ordered, one
    /// finishing before the other starts, which is exactly why a file lock
    /// cannot fix this and `CredentialScope` can.
    #[test]
    fn a_rotation_does_not_revert_another_processs_credential() {
        let path = tmp_path("scoped-rotation");
        fs::write(&path, two_account_file()).unwrap();

        // The server's boot-time snapshot.
        let mut server = load(&path).unwrap();

        // Another process re-auths acct-b. Completes entirely.
        let mut relogin = load(&path).unwrap();
        relogin.accounts[1].access_token = "at-b-relogin".to_string();
        relogin.accounts[1].refresh_token = Some("rt-b-relogin".to_string());
        save_tokens(&path, &relogin).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][1]["refreshToken"],
            json!("rt-b-relogin"),
            "precondition: the re-auth must land, or the assertion below proves nothing"
        );

        // The server rotates acct-a and persists, scoped as `persist_tokens` does.
        server.accounts[0].access_token = "at-a-rotated".to_string();
        server.accounts[0].refresh_token = Some("rt-a-rotated".to_string());
        save_tokens_for(&path, &server, CredentialScope::Only(0)).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["refreshToken"],
            json!("rt-a-rotated"),
            "the rotation this write exists for must land"
        );
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-b-relogin"),
            "a rotation of acct-a must not revert acct-b's credential"
        );

        fs::remove_file(&path).ok();
    }

    /// `CredentialScope::All` is still destructive from a stale snapshot, and
    /// that is pinned here on purpose rather than left to be rediscovered.
    ///
    /// The fix is not that `All` became safe. It is that the rotation path no
    /// longer uses it. The one remaining `All` caller is `Manager::persist_now`,
    /// the shutdown flush, which is also the case `cli.rs`'s
    /// `warn_if_server_running` already warns users about. If a third caller
    /// ever wants `All`, this test is what it has to justify.
    #[test]
    fn the_all_scope_still_reverts_another_processs_credential() {
        let path = tmp_path("all-scope-reverts");
        fs::write(&path, two_account_file()).unwrap();

        let mut stale = load(&path).unwrap();

        let mut other = load(&path).unwrap();
        other.accounts[1].refresh_token = Some("rt-b-relogin".to_string());
        save_tokens(&path, &other).unwrap();

        stale.accounts[0].access_token = "at-a-new".to_string();
        save_tokens_for(&path, &stale, CredentialScope::All).unwrap();

        assert_eq!(
            read_json(&path)["accounts"][1]["refreshToken"],
            json!("rt-b"),
            "documenting the hazard `All` still carries: it reverted the re-auth"
        );

        fs::remove_file(&path).ok();
    }

    /// The accounts a scoped write left alone are COUNTED, never reported as
    /// unpersisted credentials. Every entry in `skipped` is warned about as a
    /// consumed single-use token, and an account we never rotated is the
    /// opposite of a loss: listing it there would warn about every other account
    /// on every single refresh.
    #[test]
    fn a_scoped_write_counts_the_untouched_rather_than_calling_them_skipped() {
        let mut doc: Map<String, Value> =
            serde_json::from_str(two_account_file()).expect("fixture parses");
        let memory: Config = serde_json::from_str(two_account_file()).expect("fixture loads");

        let report = merge_tokens(&mut doc, &memory, CredentialScope::Only(0))
            .expect("the document has an accounts array");

        assert_eq!(
            report.out_of_scope, 1,
            "acct-b was not this write's business"
        );
        assert!(
            report.skipped.is_empty(),
            "an out-of-scope account must not be reported as an unpersisted credential: {:?}",
            report.skipped.iter().map(|s| s.index).collect::<Vec<_>>()
        );
        assert!(
            report.absent_from_disk.is_empty(),
            "acct-b is on disk; it was simply out of scope"
        );
    }

    /// THE regression guard for `save_settings`.
    ///
    /// A `tcr account` verb loads the config, changes a SETTING, and writes the
    /// whole `Config` back via `config::save`. Every account's credentials go
    /// with it, from a snapshot taken before the edit. The server rotating a
    /// token in between has that rotation reverted, and a single-use refresh
    /// token does not survive being reverted.
    #[test]
    fn a_settings_edit_does_not_revert_a_rotated_credential() {
        let path = tmp_path("settings-edit-revert");
        fs::write(&path, two_account_file()).unwrap();

        // The CLI loads the config for an edit.
        let mut cli = load(&path).unwrap();

        // The server rotates acct-b. Completes before the CLI writes.
        let mut server = load(&path).unwrap();
        server.accounts[1].access_token = "at-b-rotated".to_string();
        server.accounts[1].refresh_token = Some("rt-b-rotated".to_string());
        save_tokens_for(&path, &server, CredentialScope::Only(1)).unwrap();
        assert_eq!(
            read_json(&path)["accounts"][1]["refreshToken"],
            json!("rt-b-rotated"),
            "precondition: the rotation must land, or the assertion below proves nothing"
        );

        // The CLI applies its edit. A priority change, nothing to do with credentials.
        cli.accounts[0].priority = Some(9);
        save_settings(&path, &cli).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["priority"],
            json!(9),
            "the edit must land"
        );
        assert_eq!(
            value["accounts"][1]["refreshToken"],
            json!("rt-b-rotated"),
            "a settings edit reverted a rotated refresh token"
        );

        fs::remove_file(&path).ok();
    }

    /// A settings edit over a same-named Anthropic and Codex pair keeps each row's
    /// OWN credential from disk. Neither row carries a uuid, so the name fallback
    /// alone calls both of them either loaded account; only the provider tells them
    /// apart. Provider-blind, the planner refused both pairings as ambiguous and the
    /// write kept the snapshot's stale credentials for BOTH rows: two single-use
    /// refresh tokens reverted in one edit.
    ///
    /// Watched red: drop the provider guard from `identity::same_identity` and both
    /// rows are written with the stale snapshot's credentials.
    #[test]
    fn a_settings_edit_keeps_each_row_of_a_same_named_cross_provider_pair_on_its_own_credential() {
        let path = tmp_path("settings-edit-cross-provider");
        fs::write(
            &path,
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a-disk", "refreshToken": "rt-a-disk" },
                { "name": "alice@example.com", "provider": "codex",
                  "accessToken": "at-c-disk", "refreshToken": "rt-c-disk" }
            ] }"#,
        )
        .unwrap();
        // The editor's snapshot predates both rotations. Built directly, not through
        // `load`, which would rename the second row.
        let mut stale: Config = serde_json::from_str(
            r#"{ "accounts": [
                { "name": "alice@example.com", "accessToken": "at-a-stale", "refreshToken": "rt-a-stale" },
                { "name": "alice@example.com", "provider": "codex",
                  "accessToken": "at-c-stale", "refreshToken": "rt-c-stale" }
            ] }"#,
        )
        .unwrap();
        stale.accounts[0].priority = Some(9);

        save_settings(&path, &stale).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["priority"],
            json!(9),
            "the edit must land"
        );
        assert_eq!(
            (
                &value["accounts"][0]["refreshToken"],
                &value["accounts"][1]["refreshToken"]
            ),
            (&json!("rt-a-disk"), &json!("rt-c-disk")),
            "each row keeps its own credential from disk: {value}"
        );
        assert_eq!(value["accounts"][1]["provider"], json!("codex"));

        fs::remove_file(&path).ok();
    }

    /// Plain `save` is still destructive from a stale snapshot, pinned here
    /// rather than left to be rediscovered.
    ///
    /// The fix is not that `save` became safe. It is that the CLI edit path
    /// stopped using it. `save` remains correct for its other callers, which
    /// write a config they just built rather than one they loaded earlier: the
    /// first-account-after-login path in `oauth.rs` (reached only when the file
    /// does not exist yet) and the one-time legacy-throttle migration in
    /// `main.rs`. A new caller has to argue with this test.
    #[test]
    fn plain_save_still_reverts_a_rotated_credential() {
        let path = tmp_path("plain-save-reverts");
        fs::write(&path, two_account_file()).unwrap();

        let mut stale = load(&path).unwrap();

        let mut server = load(&path).unwrap();
        server.accounts[1].refresh_token = Some("rt-b-rotated".to_string());
        save_tokens_for(&path, &server, CredentialScope::Only(1)).unwrap();

        stale.accounts[0].priority = Some(9);
        save(&path, &stale).unwrap();

        assert_eq!(
            read_json(&path)["accounts"][1]["refreshToken"],
            json!("rt-b"),
            "documenting the hazard `save` still carries: it reverted the rotation"
        );

        fs::remove_file(&path).ok();
    }

    /// `CREDENTIAL_KEYS` must name exactly the fields `merge_tokens` writes.
    ///
    /// Two lists, one truth. `merge_tokens` writes credentials from memory by
    /// serializing [`Credentials`]; `carry_disk_credentials` preserves them from
    /// disk by walking `CREDENTIAL_KEYS`. A field added to one and not the other
    /// is a credential that is written but never protected, and nothing else in
    /// the stack would say so.
    #[test]
    fn the_credential_key_list_matches_what_merge_tokens_writes() {
        let creds = Credentials {
            access_token: "at",
            refresh_token: Some("rt"),
            expires_at: Some(1),
        };
        let Ok(Value::Object(fields)) = serde_json::to_value(&creds) else {
            panic!("Credentials must serialize to an object");
        };
        let mut written: Vec<&str> = fields.keys().map(String::as_str).collect();
        written.sort_unstable();
        let mut protected: Vec<&str> = CREDENTIAL_KEYS.to_vec();
        protected.sort_unstable();
        assert_eq!(
            written, protected,
            "merge_tokens writes {written:?} but carry_disk_credentials protects {protected:?}"
        );
    }

    /// A credential the FILE no longer carries must not be reinstated from the
    /// snapshot.
    ///
    /// "Credentials come from disk" has to mean absence too. A `claude
    /// setup-token` account legitimately has no refresh token, and a user can
    /// delete one by hand. Carrying only the keys the file HAS would leave the
    /// snapshot's copy in place and silently resurrect it, which is the same
    /// class of bug as the revert this whole change exists to stop, pointed the
    /// other way.
    #[test]
    fn a_credential_absent_on_disk_is_removed_rather_than_reinstated() {
        let path = tmp_path("absent-credential");
        fs::write(&path, two_account_file()).unwrap();

        // The CLI's snapshot carries acct-b's refresh token.
        let mut cli = load(&path).unwrap();
        assert!(
            cli.accounts[1].refresh_token.is_some(),
            "precondition: the snapshot holds a refresh token to reinstate"
        );

        // The file loses it: a setup-token re-auth, or a hand edit.
        fs::write(
            &path,
            r#"{ "accounts": [
                   { "name": "acct-a", "accessToken": "at-a", "refreshToken": "rt-a", "expiresAt": 1 },
                   { "name": "acct-b", "accessToken": "at-b", "expiresAt": 1 } ] }"#,
        )
        .unwrap();

        cli.accounts[0].priority = Some(9);
        save_settings(&path, &cli).unwrap();

        let value = read_json(&path);
        assert_eq!(
            value["accounts"][0]["priority"],
            json!(9),
            "the edit must still land"
        );
        assert!(
            value["accounts"][1].get("refreshToken").is_none(),
            "a refresh token the file no longer carries was reinstated from the snapshot: {}",
            value["accounts"][1]
        );

        fs::remove_file(&path).ok();
    }
}
