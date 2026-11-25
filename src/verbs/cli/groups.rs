//! `tcr group`: membership, reserve, park, control-group allowance and color, and the
//! `group ls` text and JSON renderings.

use std::path::Path;

use anyhow::{Context as _, bail};

use super::load_config;
use crate::base::config::{self, Account, Config};
use crate::base::log_err::LogErr as _;
use crate::base::provider::Provider;

/// Reject a group label whose CONTENT could corrupt header composition
/// (`main.rs`'s `compose_group_header`) or a config write — a literal newline,
/// any other control character, or a codepoint above U+00FF — and reject
/// empty/whitespace-only.
///
/// The Phase 1 validator (moved here from `main.rs` so `tcr group add`'s
/// surgical write and `tcr run --group`'s header composition share exactly one
/// definition — two validators drift and then disagree). Defense in depth, not
/// a fix for a live vulnerability: Claude Code's own header-value parser
/// already hard-errors on a header value containing a line break, NUL, or a
/// codepoint above U+00FF, so a bad label fails closed downstream today
/// regardless of this check.
///
/// Returns the failing character class in `Err` (never the raw character —
/// most of what this rejects is unprintable) so a call site can say exactly
/// what was wrong.
pub fn validate_group_label_chars(label: &str) -> Result<(), &'static str> {
    if label.trim().is_empty() {
        return Err("empty or whitespace-only");
    }
    for c in label.chars() {
        if c == '\n' || c == '\r' {
            return Err("contains a newline");
        }
        if c.is_control() {
            return Err("contains a control character");
        }
        if (c as u32) > 0xFF {
            return Err("contains a codepoint above U+00FF");
        }
    }
    Ok(())
}

/// Printed after every group mutation that actually changed the file.
///
/// Used to say "will not see this until it restarts" — true before
/// `Manager::reload_groups_if_changed` existed (`groups`/`groupSettings` were
/// read once, at boot, exactly like `disabled` before the live-control routes
/// existed for it). It is FALSE now: a running proxy hot-reloads group
/// membership, reservation and color from this same file's mtime on its next
/// natural cadence tick (a served request or a status/TUI read) — no restart.
/// Left in place rather than deleted because the CLI and the running proxy
/// are still two separate processes with no synchronous handshake, so
/// "already applied" is still not quite true either — this says what IS true
/// without over-promising instant propagation.
const GROUP_LIVE_RELOAD_NOTE: &str = "note: the running proxy picks this up automatically (next request or status check) — no restart needed";

/// The warning [`add_to_group`] prints when the account being labelled is the
/// control account. Split out as a pure function so a test can assert the text
/// without capturing stderr — the notice IS the fix here, so it needs a gate of
/// its own rather than riding along untested inside an `eprintln!`.
///
/// `None` when there is nothing to say, which is every ordinary add.
pub(super) fn control_account_group_notice(
    config: &Config,
    account: &str,
    group: &str,
) -> Option<String> {
    if config.control_account_name() != Some(account) {
        return None;
    }
    Some(format!(
        "warning: '{account}' is the control account, which inference requests never select. \
         A group whose only member is the control account routes nothing — add another account \
         to '{group}', or clear the control account with `tcr control --clear`."
    ))
}

/// The warning [`add_to_group`] prints when the account being labelled is a
/// Codex account. Modelled on [`control_account_group_notice`], and pure for the
/// same reason: the notice is the whole of this fix, so a test asserts its text
/// rather than capturing stderr.
///
/// The label is right to write: it is how `tcr group park` reaches the account,
/// and it shows in `tcr status`. What it never does is route, because no Codex
/// request carries a group, and while the group is reserved the account serves
/// nothing at all. `None` for a Claude row, which is every ordinary add.
pub(super) fn codex_group_add_notice(target: &Account, group: &str) -> Option<String> {
    match target.provider {
        Provider::Anthropic => None,
        Provider::Codex => Some(crate::verbs::provider_guard::codex_group_add_warning(
            &target.name,
            group,
        )),
    }
}

/// `tcr group add <group> <account>` — label `account` with `group`.
///
/// Resolves through [`Config::find_account_by_name`], which applies exactly the rule
/// `tcr enable`/`disable`/`token`/`remove`/`priority` use, and refuses an
/// ambiguous name rather than resolving it to whichever row comes first. That
/// refusal used to be reachable: on a fleet holding one email twice, this
/// labelled the first row and reported success, and the panel's group menu had
/// no way to say which row it meant. Unique names removed the ambiguity itself.
///
/// Idempotent: adding a label an account already carries succeeds and says so,
/// so the panel can call it without first checking state. The group label
/// itself is validated with [`validate_group_label_chars`] — the same Phase 1
/// validator `tcr run --group` uses — because `add` is the one path that can
/// put a fresh, attacker- or typo-controlled string into the config; `rm` must
/// NOT run this check (see [`remove_from_group`]'s doc-comment).
pub fn add_to_group(config_path: &Path, group: &str, account: &str) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        bail!("group {group:?}: invalid group label — {reason}");
    }
    // Deliberately NOT `load_for_edit` — that helper's warning ("a proxy is
    // already listening — it may overwrite this edit when it flushes the
    // config on exit") describes a hazard THIS write does not have.
    // `Manager::persist_now` flushes via `config::save_tokens`, not a whole
    // `config::save` of its boot-time snapshot (see persist_now's and
    // save_tokens' doc-comments) — it re-reads the CURRENT on-disk document
    // at shutdown and merges in only the token fields, so a group label this
    // surgical write already put on disk survives that flush untouched. The
    // warning would be false on every ordinary run, and TcrBar calls this
    // command with the proxy always up, which is exactly the case that makes
    // a false-but-constant warning turn into wallpaper.
    let config = load_config(config_path)?;
    let target = config.find_account_by_name(account)?;
    // The label itself is real and the write below is correct — but on the
    // CONTROL account it buys nothing, because `Manager::select_with_group`
    // excludes that account from every inference pick
    // (`select.rs`'s `control_excluded`). A group whose only member is the
    // control account therefore serves NOTHING: every request asking for it
    // falls back to the whole pool, silently, forever. Measured on the live
    // fleet 2026-08-23 — 0 of 3 probes landed in such a group, 3 of 3 landed in
    // an ordinary two-account one.
    //
    // Warn rather than refuse: the label is legitimate (the panel colours it,
    // `tcr status` reports it, and a second account joining the group makes it
    // route), and refusing would break this command's idempotence contract with
    // TcrBar. stderr is the channel TcrBar already surfaces verbatim — see
    // `GroupCommand::classify`'s `spoke` arm, which exists for exactly this
    // "exit 0 but you should read something" case.
    if let Some(notice) = control_account_group_notice(&config, &target.name, group) {
        eprintln!("{notice}");
    }
    // The same warn-don't-refuse rule for a Codex account, whose label parks it
    // but never routes to it. TcrBar shows the stderr of a successful add.
    if let Some(notice) = codex_group_add_notice(target, group) {
        eprintln!("{notice}");
    }
    let outcome = config::save_group_membership(config_path, target, group, true)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupWrite::Updated => {
            println!("Added '{account}' to group '{group}'.");
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupWrite::Unchanged => {
            println!("'{account}' is already in group '{group}'; nothing changed.");
        }
        config::GroupWrite::NoEntry => bail!(
            "'{account}' vanished from {} between load and write (concurrent edit?) — nothing changed",
            config_path.display()
        ),
        config::GroupWrite::Ambiguous => bail!(
            "more than one entry in {} carries '{account}''s identity — nothing changed",
            config_path.display()
        ),
    }
    Ok(())
}

/// `tcr group rm <group> <account>` / `tcr group rm <group> --all` — remove
/// `account`'s (or every member's, with `--all`) `group` label.
///
/// The single-account path resolves through [`Config::find_account_by_name`], same as
/// [`add_to_group`] and for the same reasons.
///
/// `account == None` iff `all` — clap's `ArgGroup` on `GroupRmArgs` refuses
/// "both" and "neither" at parse time before this ever runs, so the `expect`
/// below documents that invariant rather than guessing around it.
///
/// Deliberately does NOT run [`validate_group_label_chars`] on `group`: unlike
/// `add`, `rm` must be able to remove a label that somehow got into the config
/// with a bad character (hand-edited, or written by an older/buggy version) —
/// refusing to remove a bad label would make it permanent.
pub fn remove_from_group(
    config_path: &Path,
    group: &str,
    account: Option<&str>,
    all: bool,
) -> anyhow::Result<()> {
    // Same reasoning as `add_to_group`: no `load_for_edit` warning here either
    // — `save_group_membership`'s surgical write survives the proxy's
    // shutdown flush (`persist_now` -> `save_tokens`) for the same reason.
    let config = load_config(config_path)?;

    if all {
        let mut removed_from: Vec<String> = Vec::new();
        for acct in config.group_members(group) {
            let outcome = config::save_group_membership(config_path, acct, group, false)
                .with_context(|| format!("save config at {}", config_path.display()))?;
            match outcome {
                config::GroupWrite::Updated => removed_from.push(acct.name.clone()),
                config::GroupWrite::Unchanged => {}
                config::GroupWrite::NoEntry => bail!(
                    "'{}' vanished from {} mid-removal (concurrent edit?) — {} account(s) already updated: {}",
                    acct.name,
                    config_path.display(),
                    removed_from.len(),
                    removed_from.join(", ")
                ),
                config::GroupWrite::Ambiguous => bail!(
                    "more than one entry in {} carries '{}''s identity — {} account(s) already updated: {}",
                    config_path.display(),
                    acct.name,
                    removed_from.len(),
                    removed_from.join(", ")
                ),
            }
        }
        if removed_from.is_empty() {
            println!("Group '{group}' has no members; nothing changed.");
        } else {
            println!(
                "Removed group '{group}' from {} account(s): {}.",
                removed_from.len(),
                removed_from.join(", ")
            );
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        return Ok(());
    }

    // clap's ArgGroup on `GroupRmArgs` guarantees exactly one of
    // `account`/`--all` — see this function's doc-comment.
    let Some(account) = account else {
        bail!("name the account to remove from the group, or pass --all");
    };
    let target = config.find_account_by_name(account)?;
    let outcome = config::save_group_membership(config_path, target, group, false)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupWrite::Updated => {
            println!("Removed '{account}' from group '{group}'.");
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupWrite::Unchanged => {
            println!("'{account}' was not in group '{group}'; nothing changed.");
        }
        config::GroupWrite::NoEntry => bail!(
            "'{account}' vanished from {} between load and write (concurrent edit?) — nothing changed",
            config_path.display()
        ),
        config::GroupWrite::Ambiguous => bail!(
            "more than one entry in {} carries '{account}''s identity — nothing changed",
            config_path.display()
        ),
    }
    Ok(())
}

/// What `tcr group reserve` prints, decided before anything is written. Every
/// line is built by [`plan_reserve`], so a test reads the exact words without
/// capturing a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReservePlan {
    /// Printed on stderr before the success line when `--force` reserved a group
    /// that holds Codex accounts.
    pub(super) force_warning: Option<String>,
    /// The first stdout line when the reservation was written.
    pub(super) reserved_line: String,
    /// The one stdout line when the group was already reserved.
    pub(super) already_line: String,
    /// Printed on stderr after the success line when few accounts are left.
    pub(super) low_count_warning: Option<String>,
}

/// The safety rail and the lines of `tcr group reserve <group>`, or the refusal
/// to print instead. Pure: it reads `config` and decides; nothing is written.
///
/// A fleet with no Codex account ([`Config::holds_provider`]) runs the rail and
/// the words it always has: refuse when the reservation would leave zero
/// enabled, unreserved accounts, warn below three. `force` changes nothing
/// there.
///
/// A fleet holding a Codex account counts each provider apart, because neither
/// serves the other's traffic, and checks two things in order:
/// 1. **The Claude rail.** Refused when the reservation takes the enabled,
///    unreserved Claude accounts from at least one to none. A fleet that had no
///    such account has no Claude pool to protect, so it is not refused. `force`
///    never lifts this.
/// 2. **The Codex members.** Refused without `force` when the group holds a
///    Codex account, in any state: no Codex session can ask for a group, so a
///    reserved group's Codex members serve nothing (a disabled one is stranded
///    the moment it is enabled). This runs on an already-reserved group too, as
///    the rail does. With `force` it proceeds and warns.
///
/// The success lines then count what is left per provider, and warn when one
/// or two Claude accounts are left of a pool that had any.
pub(super) fn plan_reserve(
    config: &Config,
    group: &str,
    force: bool,
) -> Result<ReservePlan, String> {
    let mut reserved_after = config.reserved_group_names();
    reserved_after.insert(group.to_string());
    let sorted_reserved = || -> Vec<String> {
        config
            .reserved_group_names()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };

    if !config.holds_provider(Provider::Codex) {
        let remaining = config.unreserved_enabled_count(&reserved_after);
        if remaining == 0 {
            let currently_reserved = sorted_reserved();
            let advice = if currently_reserved.is_empty() {
                "every enabled account belongs to this group".to_string()
            } else {
                format!("unreserve one of: {} first", currently_reserved.join(", "))
            };
            return Err(format!(
                "reserving '{group}' would leave 0 unreserved enabled accounts — the fleet could serve no unrequested traffic. {advice}. Nothing changed."
            ));
        }
        return Ok(ReservePlan {
            force_warning: None,
            reserved_line: format!(
                "group '{group}' reserved. {remaining} unreserved enabled account(s) remain."
            ),
            already_line: format!(
                "group '{group}' is already reserved; nothing changed. {remaining} unreserved enabled account(s) remain."
            ),
            low_count_warning: (remaining < 3).then(|| {
                format!(
                    "warning: only {remaining} unreserved enabled account(s) remain — ordinary (unrequested) traffic may starve."
                )
            }),
        });
    }

    let claude_before =
        config.unreserved_enabled_count_for(&config.reserved_group_names(), Provider::Anthropic);
    let claude_after = config.unreserved_enabled_count_for(&reserved_after, Provider::Anthropic);
    let codex_after = config.unreserved_enabled_count_for(&reserved_after, Provider::Codex);

    if claude_before >= 1 && claude_after == 0 {
        let every_claude_in_group = config.group_holds_every_enabled(group, Provider::Anthropic);
        let advice = if every_claude_in_group {
            "every enabled Claude account belongs to this group".to_string()
        } else {
            format!("unreserve one of: {} first", sorted_reserved().join(", "))
        };
        return Err(crate::verbs::provider_guard::reserve_leaves_no_claude(
            group,
            codex_after,
            &advice,
        ));
    }

    let codex_members: Vec<&str> = config
        .group_members(group)
        .filter(|a| a.provider == Provider::Codex)
        .map(|a| a.name.as_str())
        .collect();
    if !codex_members.is_empty() && !force {
        return Err(crate::verbs::provider_guard::reserve_strands_codex(
            group,
            &codex_members,
        ));
    }

    let remaining = crate::verbs::provider_guard::reserve_remaining(claude_after, codex_after);
    Ok(ReservePlan {
        force_warning: (!codex_members.is_empty())
            .then(|| crate::verbs::provider_guard::reserve_force_warning(group, &codex_members)),
        reserved_line: format!("group '{group}' reserved. {remaining}"),
        already_line: format!("group '{group}' is already reserved; nothing changed. {remaining}"),
        low_count_warning: (claude_before >= 1 && claude_after < 3)
            .then(|| crate::verbs::provider_guard::reserve_few_claude_left(claude_after)),
    })
}

/// `tcr group reserve <group>` — mark `group` reserved: from the next
/// restart, an account carrying it is off-limits to traffic that did not ask
/// for one of its groups.
///
/// The safety rail runs BEFORE any write: reserving shrinks the general pool,
/// so this refuses outright — leaving the config byte-identical — if the
/// change would leave zero unreserved ENABLED accounts, and warns (but
/// proceeds) below three. Idempotent: reserving an already-reserved group
/// succeeds and says so.
///
/// The verb without `--force`: [`reserve_group_with`] with `force` false.
pub fn reserve_group(config_path: &Path, group: &str) -> anyhow::Result<()> {
    reserve_group_with(config_path, group, false)
}

/// `tcr group reserve <group> [--force]`: [`plan_reserve`], then the write,
/// then the lines the plan holds. A refusal is returned before the write, so it
/// leaves the config byte-identical.
pub fn reserve_group_with(config_path: &Path, group: &str, force: bool) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        bail!("group {group:?}: invalid group label — {reason}");
    }
    let config = load_config(config_path)?;
    let plan = plan_reserve(&config, group, force).map_err(anyhow::Error::msg)?;

    let outcome = config::save_group_reserved(config_path, group, true)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupReserveWrite::Updated => {
            if let Some(warning) = &plan.force_warning {
                eprintln!("{warning}");
            }
            println!("{}", plan.reserved_line);
            // Say what reserving actually does, at the one moment an operator is
            // guaranteed to be reading. The second half is the surprising one: it
            // can make a `--group` request WAIT or 429 where it used to be served
            // by someone else, and an operator who learns that from a stalled
            // session instead of from here will read it as a hang.
            println!(
                "  · pool traffic will no longer use its accounts, and '{group}' traffic will no longer leave it"
            );
            println!(
                "  · a --group {group} request with no member free now waits or returns 429 rather than using another account"
            );
            if let Some(warning) = &plan.low_count_warning {
                eprintln!("{warning}");
            }
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupReserveWrite::Unchanged => {
            if let Some(warning) = &plan.force_warning {
                eprintln!("{warning}");
            }
            println!("{}", plan.already_line);
        }
    }
    Ok(())
}

/// `tcr group unreserve <group>` — clear `group`'s `reserved` flag. Never
/// refused: unreserving only ever GROWS the unreserved pool, so the safety
/// rail `reserve` runs has nothing to check here.
pub fn unreserve_group(config_path: &Path, group: &str) -> anyhow::Result<()> {
    let outcome = config::save_group_reserved(config_path, group, false)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupReserveWrite::Updated => {
            println!("group '{group}' unreserved.");
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupReserveWrite::Unchanged => {
            println!("group '{group}' was not reserved; nothing changed.");
        }
    }
    Ok(())
}

/// `tcr group park <group>` — hold every member of `group` out of rotation
/// with one write (`groupSettings.<group>.parked = true`). A running proxy
/// picks it up live, no restart: parking is one `groupSettings` key, on the
/// same reload cadence `reserve` uses.
///
/// No safety rail like [`reserve_group`]'s unreserved-pool floor, deliberately:
/// parking every account is a legitimate thing to ask for (the whole fleet
/// down for the evening), and unlike a reservation it says so plainly in the
/// output rather than leaving the fleet looking healthy. Idempotent: parking an
/// already-parked group succeeds and says so. Parking a group nobody carries
/// writes the setting and says that too, exactly as `reserve` does.
pub fn park_group(config_path: &Path, group: &str) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        bail!("group {group:?}: invalid group label — {reason}");
    }
    let config = load_config(config_path)?;
    let members = config.group_members(group).count();

    let outcome = config::save_group_parked(config_path, group, true)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupParkWrite::Updated => {
            println!("group '{group}' parked. {members} account(s) held out of rotation.");
            println!(
                "  · no request reaches them, not even an explicit --group {group} ask, until you unpark"
            );
            println!("  · an account disabled on its own stays disabled after `tcr group unpark`");
            if members == 0 {
                eprintln!(
                    "warning: no account currently carries '{group}' — the setting is written and applies to whoever joins it."
                );
            }
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupParkWrite::Unchanged => {
            println!(
                "group '{group}' is already parked; nothing changed. {members} account(s) held out of rotation."
            );
        }
    }
    Ok(())
}

/// `tcr group unpark <group>` — clear `group`'s `parked` flag and put its
/// members back in rotation. Never refused: unparking only ever GROWS what can
/// serve. Deliberately does NOT run [`validate_group_label_chars`] — same
/// reasoning as [`unreserve_group`]: it must be able to clear a label that
/// somehow reached the config with a bad character, and refusing would make the
/// park permanent.
///
/// Touches only the group's own key: a member that carries its own `disabled`
/// stays disabled, because the two flags are independent.
pub fn unpark_group(config_path: &Path, group: &str) -> anyhow::Result<()> {
    let outcome = config::save_group_parked(config_path, group, false)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupParkWrite::Updated => {
            println!("group '{group}' unparked.");
            println!("  · a member disabled on its own is still disabled — `tcr enable <account>`");
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupParkWrite::Unchanged => {
            println!("group '{group}' was not parked; nothing changed.");
        }
    }
    Ok(())
}

/// `tcr group allow-control <group>` — opt `group` in to selecting the
/// control account (`groupSettings.<group>.allowControlAccount = true`): from
/// the next restart, an explicit `--group group` ask for inference may select
/// the control account (`select.rs`'s `control_excluded` carve-out), which
/// inference never does otherwise. Idempotent: opting in an already-opted-in
/// group succeeds and says so.
///
/// Same validation as [`reserve_group`] — `group` is a fresh label an
/// operator typed, so [`validate_group_label_chars`] runs before any write.
/// No safety rail like `reserve_group`'s unreserved-pool-floor check: opting
/// in only ever GROWS what a group can serve, never shrinks the general pool.
pub fn allow_control_group(config_path: &Path, group: &str) -> anyhow::Result<()> {
    if let Err(reason) = validate_group_label_chars(group) {
        bail!("group {group:?}: invalid group label — {reason}");
    }
    let outcome = config::save_group_allow_control(config_path, group, true)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupControlWrite::Updated => {
            println!("group '{group}' may now select the control account.");
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupControlWrite::Unchanged => {
            println!("group '{group}' already may select the control account; nothing changed.");
        }
    }
    Ok(())
}

/// `tcr group disallow-control <group>` — clear `group`'s
/// `allowControlAccount` flag. Never refused: disallowing only ever SHRINKS
/// what a group can serve, so this has nothing to check, mirroring
/// [`unreserve_group`]. Deliberately does NOT run
/// [`validate_group_label_chars`] — same reasoning as `unreserve_group`: it
/// must be able to clear a label that somehow got into the config with a bad
/// character, and refusing would make it permanent.
pub fn disallow_control_group(config_path: &Path, group: &str) -> anyhow::Result<()> {
    let outcome = config::save_group_allow_control(config_path, group, false)
        .with_context(|| format!("save config at {}", config_path.display()))?;
    match outcome {
        config::GroupControlWrite::Updated => {
            println!("group '{group}' may no longer select the control account.");
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupControlWrite::Unchanged => {
            println!(
                "group '{group}' was not allowed to select the control account; nothing changed."
            );
        }
    }
    Ok(())
}

/// `tcr group color <group> <hex>` / `tcr group color <group> --clear` — set
/// or clear `group`'s configured color. `hex = None` means `--clear`.
///
/// Unlike `reserve`/`unreserve`, this refuses a group with no current
/// member — matching what [`add_to_group`] does for an unknown ACCOUNT
/// (`Config::find_account_by_name`'s error), applied here to the group name instead:
/// a color for a label nothing carries could never appear on any account
/// row, so setting one would silently do nothing and look like success.
///
/// An explicit hex with no readable foreground in either text polarity
/// (`APCA |Lc| < 60` for both black and white text —
/// [`config::best_readable_apca`]) **warns, never refuses**: it is the
/// operator's chip to pick badly if they want to. `--clear` never warns — a
/// derived color always clears the floor by construction.
pub fn set_group_color(config_path: &Path, group: &str, hex: Option<&str>) -> anyhow::Result<()> {
    // Validate the hex BEFORE touching the config, same ordering `reserve_group`
    // uses for its safety rail: a rejected write must leave the file untouched.
    let normalized = hex
        .map(|h| {
            config::validate_hex_color(h)
                .map_err(|reason| anyhow::anyhow!("color {h:?}: invalid hex color — {reason}"))
        })
        .transpose()?;

    let config = load_config(config_path)?;
    let existing = config.all_group_names();
    if !existing.contains(group) {
        if existing.is_empty() {
            bail!(
                "group {group:?}: no account in the config carries any group label — nothing to color"
            );
        }
        bail!(
            "group {group:?}: not a configured group. Configured groups: {}",
            existing.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    let outcome = config::save_group_color(config_path, group, normalized.as_deref())
        .with_context(|| format!("save config at {}", config_path.display()))?;
    // Re-load is unnecessary for the message: `normalized`/`--clear` already
    // tells us what the resolved value will be on the next read, and printing
    // it from the just-loaded `config` for the clear case keeps this from
    // silently drifting if `group_color`'s derivation ever changes.
    let (resolved, source) = if let Some(hex) = &normalized {
        (hex.clone(), config::ColorSource::Set)
    } else {
        let (hex, _) = config.group_color(group);
        (hex, config::ColorSource::Derived)
    };
    match outcome {
        config::GroupColorWrite::Updated => {
            println!(
                "group '{group}' color set to {resolved} ({}).",
                source.as_str()
            );
            println!("{GROUP_LIVE_RELOAD_NOTE}");
        }
        config::GroupColorWrite::Unchanged => {
            println!(
                "group '{group}' color is already {resolved} ({}); nothing changed.",
                source.as_str()
            );
        }
    }
    // Warn (never refuse — it is the operator's call) when neither text
    // polarity clears the APCA readability floor. Only for an EXPLICIT
    // `--clear`d/set color: a `Derived` color always clears the floor by
    // construction (`derive_group_color`'s own worst-case-≥60 test), so
    // checking it here would be dead code, never firing.
    if source == config::ColorSource::Set
        && let Some(warning) = low_contrast_color_warning(&resolved, group)
    {
        eprintln!("{warning}");
    }
    Ok(())
}

/// The low-contrast warning [`set_group_color`] prints for an explicit hex
/// with no readable foreground in either text polarity — `None` when
/// `resolved` clears the APCA floor. Factored out as a pure function (input
/// -> `Option<String>`, no I/O) so the warning's TRIGGER condition and
/// CONTENT are both unit-testable without capturing stderr.
pub(super) fn low_contrast_color_warning(resolved: &str, group: &str) -> Option<String> {
    let apca = config::best_readable_apca(resolved);
    if apca >= 60.0 {
        return None;
    }
    let alternative = config::derive_group_color(group);
    Some(format!(
        "warning: '{resolved}' has no readable foreground for group '{group}' \
         (best achievable APCA |Lc| {apca:.1}, need >= 60 in either polarity). \
         Set anyway, per your choice. A derived alternative that clears the \
         floor: {alternative}."
    ))
}

/// `tcr group ls [--json]` — list every group and its members, plus the
/// ungrouped count.
///
/// Read-only: plain [`config::load`], no clobber-warning (we never save).
/// Text output is greppable, one line per group; an account in several groups
/// appears under each — same on both paths.
pub fn list_groups(config_path: &Path, json: bool) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    if json {
        println!("{}", render_groups_json(&config)?);
    } else {
        print!("{}", render_groups_text(&config));
    }
    Ok(())
}

/// The one reason a group can exist and still route nothing, spelled ONCE.
/// Both `ls` surfaces render this same token — a text `route_block=` field and a
/// JSON `routeBlock` value — so a script keying on one cannot be surprised by
/// the other. Two spellings of one fact is how they drift.
pub(super) const ROUTE_BLOCK_CONTROL_ONLY: &str = "control-account-only";

/// The other reason, spelled once the same way: every member is a Codex
/// account, and no Codex request carries a group, so a Claude session asking
/// for this group has nobody to be served by.
const ROUTE_BLOCK_CODEX_ONLY: &str = "codex-only";

/// How many of `members` are Codex accounts: the `codex_members=` field of
/// `tcr group ls`, and its `codexMembers` key.
fn codex_member_count(config: &Config, members: &[&str]) -> usize {
    members
        .iter()
        .filter(|member| config.provider_of(member) == Some(Provider::Codex))
        .count()
}

/// Why a group cannot serve an inference request at all, as opposed to merely
/// existing with members and a colour: `None` when it routes, or the token that
/// names the reason.
///
/// **This mirrors a runtime rule and must not drift from it.** The authority is
/// `Manager::classify_group_miss`'s `GroupMiss::OnlyControl` arm
/// (`src/manager/select/mod.rs`): an inference pick excludes the control account
/// unconditionally, so a group with no other member can never be selected —
/// UNLESS the group has opted in with `allowControlAccount`
/// (`Config::group_allows_control`), which lifts the exclusion for that group's
/// own `--group` asks (`select.rs`'s `control_excluded` carve-out). This is the
/// config-side view of that same fact — the only one `tcr group ls` can
/// compute, since it reads the file and not the running proxy. If the exclusion
/// rule changes there, this changes with it.
///
/// A Codex member never counts toward routing, because no Codex request carries
/// a group: the question here is whether a Claude session asking for the group
/// can be served, and only its Claude members can serve one. So the rule runs
/// over the Claude members alone, and a group with none reads
/// [`ROUTE_BLOCK_CODEX_ONLY`] (the runtime's `GroupMiss::OtherProviderOnly`). In
/// a fleet with no Codex account every member is a Claude member, and the
/// answer is the one this always gave.
///
/// Deliberately NOT a general health check: an account that is merely disabled or
/// rate-limited right now still counts as routable here, because that is
/// transient and this line is about the group's permanent shape.
fn group_route_block(config: &Config, group: &str, members: &[&str]) -> Option<&'static str> {
    let anthropic: Vec<&str> = members
        .iter()
        .copied()
        .filter(|member| config.provider_of(member) == Some(Provider::Anthropic))
        .collect();
    if anthropic.is_empty() {
        return Some(ROUTE_BLOCK_CODEX_ONLY);
    }
    let routes = match config.control_account_name() {
        Some(control) => {
            anthropic.iter().any(|member| *member != control) || config.group_allows_control(group)
        }
        None => true,
    };
    (!routes).then_some(ROUTE_BLOCK_CONTROL_ONLY)
}

/// One group as `tcr group ls` reports it. Both renderers read these rows, so
/// the text line and the JSON object are computed once and cannot disagree.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GroupRow<'a> {
    group: &'a str,
    accounts: usize,
    members: Vec<&'a str>,
    reserved: bool,
    parked: bool,
    allows_control: bool,
    routes: bool,
    route_block: Option<&'static str>,
    color: String,
    color_source: &'static str,
    /// Only when there is one, so a group with no Codex member, which is every
    /// group of a fleet with no Codex account, carries no new key.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_members: Option<usize>,
}

/// Every group's row, in name order, and the count of accounts in no group.
fn group_rows(config: &Config) -> (Vec<GroupRow<'_>>, usize) {
    let (groups, ungrouped) = config.group_membership();
    let rows = groups
        .into_iter()
        .map(|(group, members)| {
            let (color, source) = config.group_color(group);
            let route_block = group_route_block(config, group, &members);
            let codex_members = codex_member_count(config, &members);
            GroupRow {
                group,
                accounts: members.len(),
                reserved: config.is_group_reserved(group),
                parked: config.is_group_parked(group),
                allows_control: config.group_allows_control(group),
                routes: route_block.is_none(),
                route_block,
                color,
                color_source: source.as_str(),
                codex_members: (codex_members > 0).then_some(codex_members),
                members,
            }
        })
        .collect();
    (rows, ungrouped)
}

/// `tcr group ls` text: one greppable line per group, then `ungrouped`.
pub(super) fn render_groups_text(config: &Config) -> String {
    use std::fmt::Write as _;
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let (rows, ungrouped) = group_rows(config);
    let width = rows.iter().map(|r| r.group.len()).max().unwrap_or(0);
    let mut out = String::new();
    for row in &rows {
        writeln!(
            out,
            "group {:width$} accounts={} members={}{} reserved={} parked={} allows_control={} routes={}{} color={} color_source={}",
            row.group,
            row.accounts,
            row.members.join(","),
            row.codex_members
                .map_or_default(|n| format!(" codex_members={n}")),
            yes_no(row.reserved),
            yes_no(row.parked),
            yes_no(row.allows_control),
            yes_no(row.routes),
            row.route_block
                .map_or_default(|block| format!(" route_block={block}")),
            row.color,
            row.color_source,
        )
        .log_warn("rendering a group line");
    }
    writeln!(out, "ungrouped accounts={ungrouped}").log_warn("rendering the ungrouped line");
    out
}

/// `tcr group ls --json`: the structured equivalent of [`render_groups_text`]
/// for the TcrBar panel.
pub(super) fn render_groups_json(config: &Config) -> anyhow::Result<String> {
    let (rows, ungrouped) = group_rows(config);
    // Through `Value`, so the keys come out in the order the JSON map keeps
    // them, as they always have, rather than in field order.
    let out = serde_json::json!({ "groups": serde_json::to_value(&rows)?, "ungrouped": ungrouped });
    Ok(serde_json::to_string_pretty(&out)?)
}
