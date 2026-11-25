//! `tcr peer id` and `tcr peer ls`: this node's own id, and the pinned peers with what each may do and what is in flight.

use std::path::Path;

use anyhow::Context as _;
use tcr::config;
use tcr::peer;
use tcr::status;

use super::peer_cli;
use super::{masked_label, peer_state_path, pending_row_for_readers};

/// `tcr peer id`: print this node's own peer id, minting the keypair on first use.
pub(super) fn run_peer_id(a: peer_cli::PeerIdArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
    let config_dir = peers_path
        .parent()
        .map_or_else(peer::id::default_config_dir, Path::to_path_buf);

    if a.regenerate {
        if !a.yes {
            anyhow::bail!(
                "--regenerate mints a NEW node key and evicts every peer that pinned \
                 this one; pass --yes to confirm"
            );
        }

        // The peers about to be orphaned, read against the OLD key.
        // An unreadable peers file stops the regenerate outright, a
        // malformed or unowned file must not be silently read as "no
        // peers pinned" right before the key underneath them changes.
        let evicted = peer::config::PeerStore::open(&peers_path)?.peers();
        if evicted.is_empty() {
            println!("no peers are pinned; regenerating evicts nobody");
        } else {
            println!("regenerating evicts {} pinned peer(s):", evicted.len());
            for row in &evicted {
                println!("  {} ({})", masked_label(&row.label), row.node.display());
            }
        }

        for path in [
            peer::id::private_key_path(&config_dir),
            peer::id::public_key_path(&config_dir),
        ] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
    }

    let key = peer::id::NodeKey::load_or_mint(&config_dir)?;
    println!("{}", key.id().display());
    Ok(())
}

/// `tcr peer ls`: list pinned peers, what each may do, and what is in flight.
pub(super) fn run_peer_ls(a: peer_cli::PeerLsArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.path();
    // A peers file that exists but does not parse is reported IN the
    // JSON document (`peers_error`) rather than as a command failure:
    // the same reasoning `status::peers_block` gives for why an
    // unreadable file is an empty block and not a refusal applies
    // here too, and `--json` is what the panel and scripts parse.
    // The human-readable branch keeps refusing outright, since there
    // is no document for it to carry the error in.
    let store = match peer::config::PeerStore::open(&peers_path) {
        Ok(store) => store,
        Err(err) if a.json => {
            let out = status::PeerLsJson::unreadable(err.to_string());
            println!("{}", serde_json::to_string(&out)?);
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    let rows = store.peers();
    let state_path = peer_state_path(&peers_path);
    let state = peer::state::load(&state_path, peer::state::now_ms())?;

    // The main config, read once for both forms: the JSON form keys its
    // `lentTo` block by account label and reads each account's exit
    // lock, and the text form lists the Codex exit pins.
    //
    // A missing or unreadable config is an EMPTY list and not a
    // failure: `tcr peer ls` is about the peers file, and an operator
    // running it on a Mac with no accounts configured still wants the
    // pinned rows.
    let config_path = a.config.unwrap_or_else(config::default_path);
    let config_accounts: Vec<config::Account> = match config::load(&config_path) {
        Ok(config) => config.accounts,
        Err(err) => {
            tracing::debug!(target: "tcr",
                error = %err,
                path = %config_path.display(),
                "peer ls: no account labels to key `lentTo` by"
            );
            Vec::new()
        }
    };

    if a.json {
        // The clock the two derived keys are answered against, taken
        // ONCE for the whole listing: two rows read a second apart
        // could otherwise disagree about whether the same 18:00 has
        // arrived.
        let now_s = u64::try_from(peer::state::now_ms().max(0) / 1_000).unwrap_or(0);

        // The claimed name on a pending row is text a stranger on this
        // network chose. It was already sanitized on arrival; it is
        // masked again on the way out for the reason `masked_label`
        // gives, this repository is public and the state file is
        // hand-editable JSON.
        //
        // `visible_pending` and never `state.pending`: a reservation
        // placeholder holds an address's slot against the cap before
        // any handshake has earned it a name or an instance id, and
        // rendering it shows the operator a pairing request from
        // nobody that no Accept can complete. The row still counts
        // toward the cap in the listener. See
        // `PeerState::visible_pending`, which is why this is a second
        // reader and not a narrower field.
        let pending: Vec<peer::state::Knock> = state
            .visible_pending()
            .into_iter()
            .map(pending_row_for_readers)
            .collect();
        // The account labels every lease is measured against, and
        // their groups, because a `group:` scope is resolved by name
        // against the account's own group list.
        //
        // EVERY account is here, and that is the fix. This read
        // was `filter_map(|a| sanitize_label(&a.name).ok())`, so an
        // account whose own name is not a label (an email, a uuid)
        // was DROPPED from the map entirely and its card never showed
        // its "Lent to …" line, even under an `all` lease
        // that plainly lends it. Being un-nameable in a scope and
        // being un-lent are two different facts and the filter
        // conflated them.
        //
        // What is handed over is the name `tcr status` prints, which
        // is what the panel matches its account card against; `lent_to`
        // keys the map on it and sanitizes it itself for the scope
        // match, so an `account:<label>` scope still reaches exactly
        // the accounts a `--scope` could name and nothing else. The
        // provider rides along so `lent_to` can leave a Codex account
        // out: no lease draws on one.
        let accounts: Vec<(String, Vec<String>, tcr::provider::Provider)> = config_accounts
            .iter()
            .map(|account| {
                (
                    account.name.clone(),
                    account.groups.clone().unwrap_or_default(),
                    account.provider,
                )
            })
            .collect();
        let lent_to = peer::lease::lent_to(&store, &accounts);
        let masked: Vec<status::PeerLsRow> = rows
            .into_iter()
            .map(|mut row| {
                // Derived per grant and never read off the file, the
                // rule `ended` already follows: a bearer's deadline is
                // a fact about a credential and a clock.
                for grant in &mut row.lend {
                    grant.handed_key_until =
                        peer_ls_handed_key_until(grant, &config_accounts, now_s);
                }
                let (until, ended) = peer_ls_ends(&row, &state, now_s);
                status::PeerLsRow::from_row(row, until, ended)
            })
            .collect();
        // Trusted rows first, found rows after, newest first within
        // the found group: `visible_found` returns them in the order
        // `discovery::FoundList` already sorts in, and no rewriting
        // happens here.
        let found_rows: Vec<status::PeerFoundRow> = state
            .visible_found()
            .iter()
            .map(found_row_for_readers)
            .collect();
        let peers: Vec<status::PeerLsPeer> = masked
            .iter()
            .cloned()
            .map(status::PeerLsPeer::Trusted)
            .chain(found_rows.iter().cloned().map(status::PeerLsPeer::Found))
            .collect();
        let file = store.file();
        let exits = peer_ls_exits(&config_accounts, &file.peers, &state, peer::state::now_ms());
        let out = status::PeerLsJson {
            supported: true,
            peers,
            lent_to,
            internet: file.internet,
            network: status::network_fact::network_present(),
            exits,
            pending_count: pending.len(),
            pending,
            blocked_count: state.blocked_count(),
            blocked: state.banned.clone(),
            muted_count: state.muted_count(),
            muted: state.muted.clone(),
            limited: state.found_not_shown,
            caps: status::PeerCapsJson {
                found_rows: peer::discovery::MAX_FOUND_ROWS,
                found_per_address: peer::discovery::MAX_FOUND_PER_ADDRESS,
                pending: peer::state::MAX_PENDING_KNOCKS,
                knock_interval_ms: peer::listener::KNOCK_INTERVAL_MS,
                knock_burst: peer::listener::KNOCK_BURST,
                unauthenticated_sockets: peer::listener::MAX_UNAUTHENTICATED_SOCKETS,
            },
            peers_error: None,
        };
        println!("{}", serde_json::to_string(&out)?);
    } else {
        if rows.is_empty() {
            println!("no peers pinned");
        }
        for row in &rows {
            println!("{}  {}", row.node.display(), masked_label(&row.label));
        }
        // Newest first, the same order `discovery::FoundList` already
        // sorts `visible_found` in, so a human reading the terminal
        // sees the Mac they just turned on first, the same rule the
        // JSON array follows.
        for row in state.visible_found() {
            let found = found_row_for_readers(&row);
            let age_secs = (peer::state::now_ms()
                .saturating_sub(row.last_seen_ms)
                .max(0)
                / 1_000)
                .max(0);
            let name = found
                .name
                .as_deref()
                .unwrap_or("no name announced")
                .to_string();
            println!(
                "{}  {}  found {age_secs}s ago, not trusted",
                found.address, name
            );
        }
        // A Codex account's exit pin, after the rows and before the
        // summary. Only a fleet holding a Codex account with a pin that
        // names a peer prints one, so every other listing is unchanged.
        for line in peer_ls_inert_exit_lines(&config_accounts) {
            println!("{line}");
        }
        // Greppable, `key: value`-shaped, and printed even at zero:
        // "pending=0" is a fact an operator can act on and a missing
        // line is one they have to go and check. `found=` and
        // `not-shown=` are the same rule extended to the list this
        // change adds.
        println!(
            "peer ls: pending={} blocked={} muted={} found={} not-shown={}",
            state.visible_pending().len(),
            state.blocked_count(),
            state.muted_count(),
            state.visible_found().len(),
            state.found_not_shown
        );
    }
    Ok(())
}

/// When the bearer THIS grant would hand over stops working, in unix seconds.
///
/// The CLI's answer to the question `Manager::handoff_bearer` answers in the
/// running proxy, and deliberately the narrower of the two: it applies the
/// filters the config can see (the grant's scope, a disabled account, a
/// strictly pinned account, which is never handed over) and cannot apply the
/// one only a running proxy has, an account its own health checks have marked
/// errored. So a figure here is "the key this grant is entitled to hand over",
/// which is what a panel drawing a countdown needs; the proxy may still refuse
/// to hand it for a reason no file records.
///
/// `None` for every serve grant and every ended grant, which is `handoff_for`'s
/// own first two refusals, in the same order and for the same reasons.
fn peer_ls_handed_key_until(
    grant: &peer::config::LendGrant,
    accounts: &[config::Account],
    now_s: u64,
) -> Option<u64> {
    if grant.mode != peer::config::LendMode::Hand {
        return None;
    }
    if grant.has_ended(now_s) {
        return None;
    }
    accounts
        .iter()
        .filter(|account| !account.disabled.unwrap_or(false))
        .filter(|account| !account.cannot_be_handed())
        .filter(|account| {
            let label = tcr_peer_wire::sanitize_label(&account.name).unwrap_or_default();
            peer::lease::scope_covers(
                &grant.scope,
                &label,
                &account.groups.clone().unwrap_or_default(),
            )
        })
        .find_map(|account| account.expires_at)
        .and_then(|expires_at_ms| u64::try_from(expires_at_ms / 1_000).ok())
}

/// The text form of `tcr peer ls`: one line per Codex account whose exit lock
/// names a peer, saying what that pin really does.
///
/// A Codex request always leaves from this Mac, because no Mac carries one in
/// this version, and `tcr peer account` no longer writes such a pin. One written
/// before it refused can still be on the row, though, and it does one of two
/// things. A pin that is not strict is inert: the exit path warns and sends the
/// request from here. A strict one is not inert at all: the exit path refuses
/// every request routed to the account (`src/peer/egress.rs`, its Codex arm),
/// so its line says so and names the command that clears it.
///
/// A Claude account's pin is a real one and is not listed here, and nothing is
/// listed for a Codex account left on `local`. `--json` is unchanged: its
/// `exits` map already carries every pin.
fn peer_ls_inert_exit_lines(accounts: &[config::Account]) -> Vec<String> {
    accounts
        .iter()
        .filter(|account| account.provider == tcr::provider::Provider::Codex)
        .filter_map(|account| {
            let pin = account.egress_pin();
            let peer = pin.egress.peer()?;
            let why = if pin.strict {
                tcr::provider_guard::refused_exit_suffix(&account.name)
            } else {
                tcr::provider_guard::INERT_EXIT_SUFFIX.to_string()
            };
            Some(format!("{}  exits={}{why}", account.name, peer.display()))
        })
        .collect()
}

fn peer_ls_exits(
    accounts: &[config::Account],
    peers: &[peer::config::PeerRow],
    state: &peer::state::PeerState,
    now_ms: i64,
) -> std::collections::BTreeMap<String, status::PeerExitJson> {
    let mut out = std::collections::BTreeMap::new();
    for account in accounts {
        let pin = account.egress_pin();
        if pin.egress.is_local() && !pin.strict {
            continue;
        }
        let Ok(label) = tcr_peer_wire::sanitize_label(&account.name) else {
            // An account whose name is not a label still has an exit lock, but
            // there is no key to file it under and the panel matches its cards
            // by this name. Dropping it here is the one honest option; the pin
            // itself is unaffected, and `tcr status` still shows the account.
            continue;
        };
        let (peer_down, waiting_seconds) = match pin.egress.peer() {
            None => (false, None),
            Some(peer) => {
                let reachable = peers
                    .iter()
                    .any(|row| row.node == peer && row.has_endpoint());
                let waited = state
                    .last_seen_of(peer)
                    .and_then(|at| u64::try_from(now_ms.saturating_sub(at).max(0) / 1_000).ok());
                (!reachable, if reachable { None } else { waited })
            }
        };
        out.insert(
            label,
            status::PeerExitJson {
                egress: pin.egress.to_string(),
                egress_strict: pin.strict,
                peer_down,
                waiting_seconds,
            },
        );
    }
    out
}

/// The row-level `until` and `ended`, over every lease this row is
/// party to: the operator's grants, the leases the ledger has actually minted
/// for it (`state.leases`), and the leases that Mac has granted THIS one
/// (`state.borrowed`), both sections of the same state file.
///
/// The ledger's rows are in here and not only the grants because the two can
/// disagree: a lease minted before the operator shortened the grant runs to
/// its own `until`, and a borrower that is still being served must not be
/// drawn as ended.
fn peer_ls_ends(
    row: &peer::config::PeerRow,
    state: &peer::state::PeerState,
    now_s: u64,
) -> (Option<u64>, bool) {
    // (end, has it passed) for every lease this row is party to.
    let mut ends: Vec<(Option<u64>, bool)> = row
        .lend
        .iter()
        .map(|grant| (grant.until, grant.has_ended(now_s)))
        .collect();
    ends.extend(
        state
            .leases_lent_to(row.node)
            .map(|lease| (lease.until, lease.until.is_some_and(|until| until <= now_s))),
    );
    // The BORROWED side, on the same clock and the same `until`: a lease this
    // Mac holds FROM that row's Mac ends when the lender said it ends, and the
    // row renders "ends in 1 h" off the same number either way. Read from the
    // state file rather than from any running process, which is what makes it
    // answerable by a CLI invocation at all.
    ends.extend(
        state
            .leases_borrowed_from(row.node)
            .map(|lease| (lease.until, lease.until.is_some_and(|until| until <= now_s))),
    );

    let live: Vec<Option<u64>> = ends
        .iter()
        .filter(|(_, ended)| !ended)
        .map(|(until, _)| *until)
        .collect();
    if !live.is_empty() {
        // The soonest END among the live leases, and `None` when every live
        // lease is open-ended: a row with one lease that ends at 18:00 and one
        // that never ends has not ended at 18:00, but 18:00 is still the next
        // thing that happens to it.
        return (live.into_iter().flatten().min(), false);
    }
    if ends.is_empty() {
        // No lease at all is not an ended lease: `Add a lease…` is the zero
        // state, and a Mac that was never lent to must not be greyed.
        return (None, false);
    }
    (ends.iter().filter_map(|(until, _)| *until).max(), true)
}

/// One found row as a READER of it wants it: [`pending_row_for_readers`]'s
/// twin. The dial address is built here for the same reason, an operator or
/// the panel's Trust button hands this string straight to `tcr peer pair`,
/// and the announced name is masked again for the same reason: it is text a
/// stranger's Mac chose, already whitelisted on arrival, and this repository
/// is public.
fn found_row_for_readers(row: &peer::state::Found) -> status::PeerFoundRow {
    status::PeerFoundRow {
        name: row.announced_name.as_deref().map(masked_label),
        address: row.dial_address(),
        trusted: false,
        last_seen_ms: row.last_seen_ms,
        sources: row.sources.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::test_support::account_with_groups;

    /// `tcr peer ls` lists a Codex account's exit pin with what it really does:
    /// a loose pin is inert, because a Codex request always leaves from this Mac,
    /// and a strict one skips the account during selection, so its line
    /// names the command that clears it. A Claude pin, and a Codex account left
    /// local, get no line.
    #[test]
    fn peer_ls_marks_a_codex_exit_pin_inert() {
        use tcr::provider::Provider;
        let peer = tcr_peer_wire::PeerId([7_u8; 32]);
        let pinned = |name: &str, provider: Provider, strict: bool| config::Account {
            provider,
            egress: config::Egress::Via(peer),
            egress_strict: strict,
            ..account_with_groups(name, None)
        };
        let accounts = vec![
            pinned("bob@example.com/codex", Provider::Codex, false),
            pinned("carol@example.com/codex", Provider::Codex, true),
            pinned("alice@example.com", Provider::Anthropic, false),
            config::Account {
                provider: Provider::Codex,
                ..account_with_groups("dana@example.com/codex", None)
            },
        ];
        assert_eq!(
            peer_ls_inert_exit_lines(&accounts),
            vec![
                format!(
                    "bob@example.com/codex  exits={} (inert: Codex accounts always leave from this Mac)",
                    peer.display()
                ),
                format!(
                    "carol@example.com/codex  exits={} (refused: a Codex account cannot exit from another Mac, so tcr skips this account; clear it with `tcr peer account carol@example.com/codex --exits-from local --no-must`)",
                    peer.display()
                ),
            ]
        );
    }
}
