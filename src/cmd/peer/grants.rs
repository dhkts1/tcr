//! What a trusted Mac may do with this one: `tcr peer share`, `tcr peer allow` and `tcr peer lend`, and the lend grant they write.

use std::path::Path;

use anyhow::Context as _;
use tcr::config;
use tcr::peer;

use super::peer_cli;
use super::{lock_peers_file, parse_peer_arg, peer_window_name, print_listen_written};

/// `tcr peer share`: let trusted Macs serve requests with this Mac's accounts, or stop.
pub(super) fn run_peer_share(a: peer_cli::PeerSwitchArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.path();
    let (_lock, mut file) = lock_peers_file(&peers_path)?;
    if file.peers.is_empty() {
        println!(
            "peer share: no peers are pinned, so there is nobody to share with \
             (`tcr peer pair <host:port>` or `tcr peer invite` first)"
        );
        return Ok(());
    }

    let window = tcr_peer_wire::Window::from(a.window);
    match a.state {
        peer_cli::Switch::On => {
            let scope = tcr_peer_wire::LendScope::parse(&a.scope)
                .map_err(|refusal| anyhow::anyhow!("peer share: {refusal}"))?;
            // Zero is a removal here for the same reason it is in `peer
            // lend`: a grant that mints leases the first request
            // overdraws is worse than no grant, because it advertises a
            // capability. `inspect` is left exactly as it was, since
            // removing a grant is not the moment to grant a disclosure.
            if a.fraction <= 0.0 {
                let mut removed = 0_usize;
                for row in &mut file.peers {
                    let before = row.lend.len();
                    row.lend
                        .retain(|existing| !(existing.window == window && existing.scope == scope));
                    removed += before - row.lend.len();
                }
                if file
                    .default_lend
                    .as_ref()
                    .is_some_and(|default| default.window == window && default.scope == scope)
                {
                    file.default_lend = None;
                }
                peer::config::save(&peers_path, &file)?;
                println!(
                    "peer share: removed leases={removed} scope={scope} window={}",
                    peer_window_name(window)
                );
                println!(
                    "peer share: every peer that still holds `inspect` may still OPEN \
                     a serve; each request on it is refused for want of a grant \
                     (`tcr peer share off` closes the streams too)"
                );
                return Ok(());
            }
            // A scope that names a Codex account is refused here, before
            // anything is written, as `peer lend` refuses it. The zero
            // fraction above stays allowed, so an old grant that names
            // one can still be removed.
            refuse_a_lend_scope_naming_codex(&scope, a.config.as_deref())?;
            // AN END THAT HAS ALREADY PASSED IS REFUSED, BEFORE
            // ANYTHING IS WRITTEN. This verb keeps the end, the mode
            // and the schedule an operator set per peer, which is
            // right; what it did with a `--until` that has already gone
            // by was carry it onto the new grant and print `on`. Every
            // request against that grant is then refused for want of a
            // live lending, and the only surface saying so is
            // `peer lend --list`'s `ended=true`. `peer lend` cannot
            // produce one at all (`parse_lend_end` rolls a time of day
            // to tomorrow and refuses a duration that is not into the
            // future), so this verb is the one that has to.
            //
            // Refused rather than silently cleared: the end is a
            // decision the operator took, and a verb that quietly
            // removes it lends past a deadline somebody meant.
            let now_s =
                u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
            let closed: Vec<String> = file
                .peers
                .iter()
                .filter(|row| {
                    row.lend.iter().any(|existing| {
                        existing.window == window
                            && existing.scope == scope
                            && existing.has_ended(now_s)
                    })
                })
                .map(|row| row.node.display())
                .collect();
            if !closed.is_empty() {
                anyhow::bail!(
                    "peer share: the grant this would keep the end of has already \
                     ended on {} (scope={scope} window={}), so turning sharing on \
                     would write a grant that lends nothing and print success. Give \
                     it a new end first (`tcr peer lend <peer> --relend --for 2h`), \
                     or drop the end (`tcr peer lend <peer> --for none`)",
                    closed.join(", "),
                    peer_window_name(window)
                );
            }
            let mut grant = peer_lend_grant(window, a.fraction, a.ttl, a.max_inflight)?;
            grant.scope = scope.clone();
            for row in &mut file.peers {
                row.allow.inspect = true;
                // What this row already granted on the same (window,
                // scope), read BEFORE the retain below drops it.
                //
                // A hand grant, an end date and a daily window are
                // decisions the operator took per peer, and this verb
                // used to overwrite all three with `LendGrant::new`'s
                // defaults: mode serve, no end, no schedule. Turning a
                // hand grant into a serve grant is a different
                // disclosure than the one that was taken, and it
                // happened silently. `peer lend` already carries the
                // same rule and says why.
                let replacing = row
                    .lend
                    .iter()
                    .find(|existing| existing.window == window && existing.scope == scope)
                    .map(|existing| {
                        (
                            existing.mode,
                            existing.until,
                            existing.between,
                            existing.days.clone(),
                        )
                    });
                // By (window, SCOPE), for the reason `peer lend` gives:
                // a Mac may hold one lease per scope and turning
                // sharing on with a scope must not delete the others.
                row.lend
                    .retain(|existing| !(existing.window == window && existing.scope == scope));
                // A fresh id per row: a lease id is the handle for ONE
                // peer's lease, and two peers sharing one id would make
                // `--revoke` ambiguous.
                let mut per_peer = grant.clone();
                if let Some((mode, until, between, days)) = replacing {
                    per_peer.mode = mode;
                    per_peer.until = until;
                    per_peer.between = between;
                    per_peer.days = days;
                }
                per_peer.id = 0;
                per_peer.ensure_id()?;
                row.lend.push(per_peer);
            }
            // What the Sharing defaults sheet shows. A RECORD of the
            // defaults, never a second enforcement point. See
            // `PeerFile::default_lend`.
            let mut default = grant.clone();
            default.id = 0;
            file.default_lend = Some(default);
            // Sharing means those Macs reach this one, which needs a
            // port. On this arm only: the removal above and the `off`
            // arm below are not opt-ins and must open nothing.
            let listen_written = file.ensure_listen_for_opt_in();
            peer::config::save(&peers_path, &file)?;
            if let Some(chosen) = listen_written {
                print_listen_written(chosen, &peers_path);
            }
            println!(
                "peer share: on peers={} scope={} window={} fraction={} ttl_s={} \
                 max_inflight={}",
                file.peers.len(),
                grant.scope,
                peer_window_name(window),
                grant.fraction,
                grant.ttl_s,
                grant.max_inflight
            );
            println!(
                "peer share: those Macs may now serve requests on this Mac's accounts, \
                 which means they READ those requests in full, prompts included"
            );
            // The other direction is a separate act on purpose. This verb
            // says "my accounts may serve their requests"; `disclose` says
            // "my requests may be read by them", and turning both on from
            // one switch would grant a disclosure the operator did not ask
            // for. Granting `inspect` alone is deliberate.
            println!(
                "peer share: to borrow the other way, grant it per peer: \
                 `tcr peer allow <peer> disclose on`"
            );
        }
        peer_cli::Switch::Off => {
            for row in &mut file.peers {
                row.allow.inspect = false;
                row.lend.clear();
            }
            peer::config::save(&peers_path, &file)?;
            println!("peer share: off peers={}", file.peers.len());
            println!(
                "peer share: a live borrowed request dies within one frame, the \
                 listener re-reads this file and re-runs the gate before each one"
            );
        }
    }
    Ok(())
}

/// `tcr peer allow`: grant or revoke one thing for one peer.
pub(super) fn run_peer_allow(a: peer_cli::PeerAllowArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.path();
    let (_lock, mut file) = lock_peers_file(&peers_path)?;
    let peer_id = parse_peer_arg("peer allow", &a.peer)?;
    let on = matches!(a.state, peer_cli::Switch::On);

    let Some(row) = file.peers.iter_mut().find(|row| row.node == peer_id) else {
        anyhow::bail!(
            "peer allow: {} is not pinned here, and a grant for an unpinned peer \
             would be a row nothing enforces (`tcr peer ls` lists the pinned ones)",
            a.peer
        );
    };
    let grant = match a.grant {
        peer_cli::PeerGrant::Gateway => {
            row.allow.gateway = on;
            "gateway"
        }
        peer_cli::PeerGrant::Carry => {
            row.allow.carry = on;
            "carry"
        }
        peer_cli::PeerGrant::Forward => {
            row.allow.relay = on;
            "forward"
        }
        peer_cli::PeerGrant::Inspect => {
            row.allow.inspect = on;
            "inspect"
        }
        peer_cli::PeerGrant::Disclose => {
            row.allow.allow_disclose = on;
            "disclose"
        }
        peer_cli::PeerGrant::AcceptMove => {
            row.allow.accept_move = on;
            "accept-move"
        }
        peer_cli::PeerGrant::ControlBriefs => {
            row.allow.control.briefs = on;
            "control-briefs"
        }
        peer_cli::PeerGrant::ControlLendable => {
            row.allow.control.lendable = on;
            "control-lendable"
        }
        peer_cli::PeerGrant::ControlDiag => {
            row.allow.control.diag = on;
            "control-diag"
        }
        peer_cli::PeerGrant::ControlDrop => {
            row.allow.control.drop = on;
            "control-drop"
        }
    };
    peer::config::save(&peers_path, &file)?;
    println!(
        "peer allow: ok peer={} grant={grant} state={}",
        peer_id.display(),
        if on { "on" } else { "off" }
    );
    println!(
        "peer allow: in effect now, the listener re-reads {} whenever its mtime \
         moves, so no restart and no cold prompt cache",
        peers_path.display()
    );
    Ok(())
}

/// `tcr peer lend`: set what one peer may borrow on one window, or take it away.
pub(super) fn run_peer_lend(a: peer_cli::PeerLendArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.path();
    let (_lock, mut file) = lock_peers_file(&peers_path)?;
    let peer_id = parse_peer_arg("peer lend", &a.peer)?;
    let window = tcr_peer_wire::Window::from(a.window);

    // The scope FIRST, so a refused `--scope` spelling changes nothing
    // on disk: a lend that half-applied is worse than one that did not
    // run.
    let scope = tcr_peer_wire::LendScope::parse(&a.scope)
        .map_err(|refusal| anyhow::anyhow!("peer lend: {refusal}"))?;
    // The end, from whichever of the two flags was given.
    // One parser for both spellings, and the clock is the real one
    // here, `parse_lend_end`'s own gates inject theirs.
    let end_spec = a.for_.as_deref().or(a.until.as_deref());
    let end = match end_spec {
        Some(spec) => peer::lease::parse_lend_end(spec, time::OffsetDateTime::now_utc())?,
        None => None,
    };
    // The window and days, parsed here for the same
    // reason the scope is: a refused spelling must change nothing on
    // disk. Both are refused with the value named, a typo that
    // silently became "every hour" would read to the operator as a
    // schedule that does not work.
    let between = match a.between.as_deref() {
        Some(raw) => Some(
            raw.parse::<peer::schedule::Between>()
                .map_err(|refusal| anyhow::anyhow!("peer lend --between: {refusal}"))?,
        ),
        None => None,
    };
    let days = match a.days.as_deref() {
        Some(raw) => Some(
            raw.parse::<peer::schedule::Days>()
                .map_err(|refusal| anyhow::anyhow!("peer lend --days: {refusal}"))?,
        ),
        None => None,
    };

    let Some(row) = file.peers.iter_mut().find(|row| row.node == peer_id) else {
        anyhow::bail!(
            "peer lend: {} is not pinned here, so a grant for it would be a row \
             nothing enforces (`tcr peer ls` lists the pinned ones)",
            a.peer
        );
    };

    // --list, --revoke and --relend read or edit the leases that are
    // already there and never mint one, so each returns before the
    // lend below.
    if a.list {
        if row.lend.is_empty() {
            println!("peer lend: none peer={}", peer_id.display());
        }
        for grant in &row.lend {
            println!(
                "peer lend: lease={} peer={} scope={} window={} fraction={} ttl_s={} \
                 max_inflight={} until={} between={} days={} ended={}",
                peer::config::lease_id_string(grant.id),
                peer_id.display(),
                grant.scope,
                peer_window_name(grant.window),
                grant.fraction,
                grant.ttl_s,
                grant.max_inflight,
                grant
                    .until
                    .map_or_else(|| "none".to_string(), |at| at.to_string()),
                grant
                    .between
                    .map_or_else(|| "any".to_string(), |window| window.to_string()),
                grant
                    .days
                    .as_ref()
                    .map_or_else(|| "any".to_string(), ToString::to_string),
                grant.ended
            );
        }
        return Ok(());
    }

    if let Some(raw) = a.revoke.as_deref() {
        let id = peer::config::parse_lease_id(raw)
            .context("peer lend --revoke: that is not a lease id")?;
        let before = row.lend.len();
        row.lend.retain(|grant| grant.id != id);
        if row.lend.len() == before {
            anyhow::bail!(
                "peer lend: no lease {} on peer {} (`tcr peer lend {} --list`)",
                peer::config::lease_id_string(id),
                peer_id.display(),
                a.peer
            );
        }
        let left = row.lend.len();
        peer::config::save(&peers_path, &file)?;
        println!(
            "peer lend: revoked lease={} peer={} leases_left={left}",
            peer::config::lease_id_string(id),
            peer_id.display()
        );
        println!(
            "peer lend: a live borrowed request on it dies within one frame, the \
             listener re-reads {} whenever its mtime moves",
            peers_path.display()
        );
        return Ok(());
    }

    if let Some(raw) = a.relend.as_deref() {
        let id = peer::config::parse_lease_id(raw)
            .context("peer lend --relend: that is not a lease id")?;
        let Some(grant) = row.lend.iter_mut().find(|grant| grant.id == id) else {
            anyhow::bail!(
                "peer lend: no lease {} on peer {} (`tcr peer lend {} --list`)",
                peer::config::lease_id_string(id),
                peer_id.display(),
                a.peer
            );
        };
        // The new end, or none at all. `ended` is derived on read and
        // never written by hand, so clearing `until` is the whole of
        // putting a lease back to work.
        grant.until = end;
        grant.ended = false;
        let (window, until) = (grant.window, grant.until);
        peer::config::save(&peers_path, &file)?;
        println!(
            "peer lend: relent lease={} peer={} window={} until={}",
            peer::config::lease_id_string(id),
            peer_id.display(),
            peer_window_name(window),
            until.map_or_else(|| "none".to_string(), |at| at.to_string())
        );
        return Ok(());
    }

    // A SCOPE THAT NAMES A CODEX ACCOUNT IS REFUSED, before anything is
    // written: no lease draws on one (`refuse_a_lend_scope_naming_codex`).
    // A zero fraction is a removal and stays allowed, as --list,
    // --revoke and --relend above do, so an old lease that names one can
    // still be cleared.
    if a.fraction > 0.0 {
        refuse_a_lend_scope_naming_codex(&scope, a.config.as_deref())?;
    }

    // A HAND grant is refused when every account it could draw on is
    // strictly pinned away from this Mac. The bearer would be handed to
    // a borrower that must send on its own IP, and a strict pin says
    // this account's requests leave through a named peer or not at all,
    // so every borrowed request would be refused by the exit lock. The
    // grant would look granted and buy nothing.
    //
    // Read BEFORE any write, for the same reason the scope spelling is:
    // a refusal must change nothing on disk.
    //
    // Not strict, or not pinned at all, is allowed and says so: a
    // non-strict pin falls back to local, which is exactly what a
    // hand-mode borrower does.
    if matches!(a.mode, Some(peer_cli::PeerLendMode::Hand)) {
        let config_path = a.config.clone().unwrap_or_else(config::default_path);
        let main = config::load(&config_path).with_context(|| {
            format!(
                "peer lend: --mode hand reads {} to check this scope's exit locks",
                config_path.display()
            )
        })?;
        let covered: Vec<&config::Account> = main
            .accounts
            .iter()
            .filter(|account| {
                let label = tcr_peer_wire::sanitize_label(&account.name).unwrap_or_default();
                peer::lease::scope_covers(&scope, &label, account.groups.as_deref().unwrap_or(&[]))
            })
            .collect();
        if !covered.is_empty() && covered.iter().all(|account| account.cannot_be_handed()) {
            anyhow::bail!(
                "peer lend: every account this scope covers has egressStrict on, so a \
                 hand-mode grant would hand over a bearer the exit lock refuses to \
                 send with: the borrower sends from ITS own Mac, which a strict pin \
                 forbids whether it names this Mac or another. Lend it as `--mode \
                 serve`, or clear the pin on an account in {} first",
                a.scope
            );
        }
    }

    // The mode of the grant being replaced, read BEFORE the retain
    // below drops it. Editing a hand grant's fraction must not quietly
    // turn it back into a serve grant: that is a different disclosure
    // decision than the one the operator took, and the CLI would have
    // taken it for them silently.
    let replacing = row
        .lend
        .iter()
        .find(|existing| existing.window == window && existing.scope == scope)
        .map(|existing| existing.mode);
    let mode = a
        .mode
        .map(peer::config::LendMode::from)
        .or(replacing)
        .unwrap_or_default();

    // A lease is replaced by (window, SCOPE) and not by window alone:
    // one Mac may hold several leases at once, one per
    // scope, so lending `group:work` must not silently delete the
    // `all` lease on the same window.
    row.lend
        .retain(|existing| !(existing.window == window && existing.scope == scope));
    // Zero is a removal, not a lease of nothing: a row that mints
    // leases the first request overdraws is worse than no row, because
    // it advertises a capability. `clamp_to_grant` refuses anything
    // under MIN_DEBIT anyway, so this says the same thing on disk.
    if a.fraction <= 0.0 {
        let inspect_still_on = row.allow.inspect;
        peer::config::save(&peers_path, &file)?;
        println!(
            "peer lend: removed peer={} window={}",
            peer_id.display(),
            peer_window_name(window)
        );
        if inspect_still_on {
            println!(
                "peer lend: that peer still holds `inspect`, so it may still OPEN a \
                 SERVE, every request on it is refused for want of a grant \
                 (`tcr peer allow {} inspect off` closes the stream itself)",
                peer_id.display()
            );
        }
        return Ok(());
    }
    let mut grant = peer_lend_grant(window, a.fraction, a.ttl, a.max_inflight)?;
    grant.mode = mode;
    grant.scope = scope;
    grant.until = end;
    grant.between = between;
    grant.days = days;
    // Minted here rather than left to `save`, so the line below can
    // print the handle `--revoke` takes.
    grant.ensure_id()?;
    let printed = grant.clone();
    row.lend.push(grant);
    let leases = row.lend.len();
    peer::config::save(&peers_path, &file)?;
    println!(
        "peer lend: ok lease={} peer={} scope={} window={} fraction={} ttl_s={} \
         max_inflight={} until={} between={} days={} leases={leases}",
        peer::config::lease_id_string(printed.id),
        peer_id.display(),
        printed.scope,
        peer_window_name(window),
        printed.fraction,
        printed.ttl_s,
        printed.max_inflight,
        printed
            .until
            .map_or_else(|| "none".to_string(), |at| at.to_string()),
        printed
            .between
            .map_or_else(|| "any".to_string(), |window| window.to_string()),
        printed
            .days
            .as_ref()
            .map_or_else(|| "any".to_string(), ToString::to_string)
    );
    println!(
        "peer lend: a grant is a CEILING on one lease, not a promise, the lender's \
         own guard band clamps it again at grant time and can leave nothing"
    );
    Ok(())
}

/// Every account that HAS an exit lock, keyed by the label a scope names it by.
///
/// An account with neither a pin nor strictness is absent rather than present
/// with `local`, for the reason the `lentTo` map gives: the panel hides the
/// line when there is nothing to say, and a map of every account saying
/// "local" is a list the reader has to filter before it means anything.
/// `tcr peer lend` and `tcr peer share on`: refuse a scope that names a Codex
/// account, before anything is written.
///
/// Codex accounts stay on this Mac: no lease draws on one
/// (`Manager::lend_candidates` skips them), so a grant scoped to one would
/// advertise a capability that is not there. A row is named when a scope label
/// equals `sanitize_label` of its name, the match the serve path's
/// `scope_restriction` makes for an `account:` scope. Only such a scope names
/// rows, so nothing is read for `all` or `group:`. A config that is missing or
/// unreadable skips the check, as `tcr peer ls` treats one, rather than failing
/// a verb about the peers file. The refusal is
/// [`tcr::provider_guard::provider_guard`]'s, in the words every
/// Claude-only refusal uses.
fn refuse_a_lend_scope_naming_codex(
    scope: &tcr_peer_wire::LendScope,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let tcr_peer_wire::LendScope::Accounts(labels) = scope else {
        return Ok(());
    };
    let path = config_path.map_or_else(config::default_path, Path::to_path_buf);
    let Ok(main) = config::load(&path) else {
        return Ok(());
    };
    for account in &main.accounts {
        let named =
            tcr_peer_wire::sanitize_label(&account.name).is_ok_and(|label| labels.contains(&label));
        if named {
            tcr::provider_guard::provider_guard(account, tcr::provider_guard::ClaudeOnlyVerb::Lend)
                .map_err(anyhow::Error::msg)?;
        }
    }
    Ok(())
}

/// One lend grant, with the operator's numbers clamped and the clamp SAID.
///
/// The fraction ceiling is `0.5`, mirroring the clamp the main config already
/// applies to `controlReserve` (`src/config.rs:855-856`) rather than inventing a
/// second number for the same kind of quantity. A clamp is printed, never
/// silent: an operator who typed `0.9` and got `0.5` has to be able to see that
/// happen, or they will believe the file says something it does not.
fn peer_lend_grant(
    window: tcr_peer_wire::Window,
    fraction: f64,
    ttl_s: u32,
    max_inflight: u8,
) -> anyhow::Result<peer::config::LendGrant> {
    // REFUSED, not clamped, and this is the only fraction that is. `NaN`
    // compares false against both of `clamp`'s bounds, so it passed through the
    // clamp below unchanged and printed nothing; `serde_json` then writes a
    // non-finite float as `null`, and `null` is not an `f64`, so the peers file
    // this command had just written would not load again on any build.
    if !fraction.is_finite() {
        anyhow::bail!(
            "peer lend: --fraction {fraction} is not a number this can lend; give a fraction \
             of the window between 0 and {MAX_LEND_FRACTION}"
        );
    }
    let clamped = peer::config::lend_fraction(fraction);
    if (clamped - fraction).abs() > f64::EPSILON {
        println!(
            "peer lend: clamped fraction {fraction} to {clamped} (the ceiling on one lease is \
             {MAX_LEND_FRACTION}, the same clamp the main config applies to its own reserve)"
        );
    }
    Ok(peer::config::LendGrant::new(
        window,
        clamped,
        ttl_s,
        max_inflight,
    ))
}

/// The ceiling on one lease's fraction, and it is the LIBRARY's constant: the
/// flag that accepts a fraction and the reader that parses one off disk answer
/// to one number, or a hand-edited file carries what this flag would have
/// refused.
const MAX_LEND_FRACTION: f64 = peer::config::MAX_LEND_FRACTION;

#[cfg(test)]
mod peer_lend_fraction_tests {
    use super::{MAX_LEND_FRACTION, peer_lend_grant};
    use tcr::peer::config::LendGrant;

    /// **A fraction that is not a number is refused at the flag**, and the
    /// ordinary over-large one is still clamped and still written.
    ///
    /// `--fraction nan` passed the clamp in silence, because `NaN` compares
    /// false against both bounds and `(NaN - NaN).abs() > EPSILON` is false
    /// too, so nothing was printed and nothing was capped. The second half
    /// below is what that cost: `serde_json` writes a non-finite float as
    /// `null`, `LendGrant::fraction` is not an `Option`, so the peers file the
    /// command had just written would not load again, on this build or any
    /// other.
    ///
    /// Watch it fail by deleting the `is_finite` refusal in `peer_lend_grant`.
    #[test]
    fn a_fraction_that_is_not_a_number_is_refused_at_the_flag() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                peer_lend_grant(tcr_peer_wire::Window::SevenDay, bad, 300, 2).is_err(),
                "--fraction {bad} has to be refused, not written"
            );
        }

        let clamped = peer_lend_grant(tcr_peer_wire::Window::SevenDay, 0.9, 300, 2)
            .expect("an over-large fraction is an ordinary clamp and not a refusal");
        assert_eq!(clamped.fraction, MAX_LEND_FRACTION);

        // What the refusal above prevents, measured rather than asserted from
        // the documentation.
        let planted = LendGrant {
            fraction: f64::NAN,
            ..LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.2, 300, 2)
        };
        let written = serde_json::to_string(&planted).expect("a grant serializes");
        assert!(
            written.contains("\"fraction\":null"),
            "a non-finite fraction lands on disk as null: {written}"
        );
        assert!(
            serde_json::from_str::<LendGrant>(&written).is_err(),
            "and a file carrying it does not load again: {written}"
        );
    }
}

#[cfg(test)]
mod peer_grant_tests {
    use super::*;
    use crate::cmd::peer::run_peer;
    use std::path::PathBuf;

    /// One pinned peer and nothing else: what `share`, `allow` and `lend` all
    /// need before they will write anything, and what the operator has after
    /// `tcr peer pair`.
    ///
    /// Obviously fake node id; this repository is public and a fixture is the
    /// easiest place to leak a real one.
    const PINNED: [u8; 32] = [4_u8; 32];

    fn pinned_peers_file(dir: &Path) -> PathBuf {
        let peers_path = dir.join("tcr-peers.json");
        let file = peer::config::PeerFile {
            peers: vec![peer::config::PeerRow {
                node: tcr_peer_wire::PeerId(PINNED),
                label: "studio-mac".to_string(),
                endpoints: vec![peer::config::Endpoint::direct(
                    "127.0.0.1:1"
                        .parse()
                        .expect("a literal socket address parses"),
                    0,
                    peer::config::EndpointSource::Paired,
                )],
                added_at: 0,
                rendezvous_secret: None,
                sees_us_at: None,
                allow: peer::config::Allow::default(),
                lend: Vec::new(),
            }],
            ..peer::config::PeerFile::default()
        };
        peer::config::save(&peers_path, &file).expect("seed the peers file");
        peers_path
    }

    fn read_back(peers_path: &Path) -> peer::config::PeerRow {
        peer::config::read_or_default(peers_path)
            .expect("the peers file reads back")
            .peers
            .into_iter()
            .next()
            .expect("the seeded row is still there")
    }

    /// **`tcr peer share on` persists `inspect` and a lend grant**, and
    /// `share off` takes both away.
    ///
    /// Persistence is the whole claim: the verb printed a confident line and
    /// the listener reads the FILE, so a write that did not land is a switch
    /// that says "on" and serves nobody. Asserted by re-reading through
    /// `peer::config::read_or_default` (the same reader the listener uses)
    /// rather than by trusting the in-memory value the verb just set.
    ///
    /// Watch it fail by deleting the `peer::config::save` call in the `Share`
    /// arm's `On` branch: the row comes back with `inspect` false and no lend
    /// grant. Measured that way before this test was kept.
    #[tokio::test]
    async fn peer_share_persists_inspect_and_a_lend_grant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Share(peer_cli::PeerSwitchArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                state: peer_cli::Switch::On,
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.10,
                ttl: 600,
                max_inflight: 2,
                scope: "all".to_string(),
                config: None,
            }),
        })
        .await
        .expect("peer share on");

        let row = read_back(&peers_path);
        assert!(row.allow.inspect, "share on grants `inspect`");
        assert!(
            !row.allow.allow_disclose,
            "share on is one direction only: it does NOT grant `disclose`, which is what \
             would let the other Mac read THIS Mac's requests"
        );
        assert_eq!(row.lend.len(), 1, "one grant, for the window asked for");
        assert_eq!(row.lend[0].window, tcr_peer_wire::Window::SevenDay);
        assert!((row.lend[0].fraction - 0.10).abs() < f64::EPSILON);
        assert_eq!(row.lend[0].ttl_s, 600);
        assert_eq!(row.lend[0].max_inflight, 2);

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Share(peer_cli::PeerSwitchArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                state: peer_cli::Switch::Off,
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.10,
                ttl: 600,
                max_inflight: 2,
                scope: "all".to_string(),
                config: None,
            }),
        })
        .await
        .expect("peer share off");

        let row = read_back(&peers_path);
        assert!(!row.allow.inspect, "share off revokes `inspect`");
        assert!(
            row.lend.is_empty(),
            "share off clears the grants too, or a re-enabled `inspect` would resurrect a \
             ceiling the operator never re-typed"
        );
    }

    /// **`tcr peer share on` refuses to keep an end that has already gone
    /// by.**
    ///
    /// This verb keeps the end, the mode and the schedule an operator set per
    /// peer rather than overwriting them with defaults, which is right. What it
    /// did with an `until` that had already passed was carry it onto the new
    /// grant and print `on`: every request against that grant is then refused
    /// for want of a live lending, and the only surface that says so is
    /// `peer lend --list`'s `ended=true`. `peer lend` cannot write one of those
    /// at all, so this verb is the one that has to refuse.
    ///
    /// The file is re-read afterwards, because "refused" and "refused without
    /// writing" are different claims and only the second one is safe: a
    /// half-applied share leaves `inspect` granted with no grant behind it.
    ///
    /// Watch it fail by deleting the `closed` block from the `Share` arm's `On`
    /// branch: the command answers `Ok`, the grant comes back with yesterday's
    /// `until` and a fraction of 0.10, and the operator is told sharing is on.
    #[tokio::test]
    async fn peer_share_on_refuses_a_lending_window_that_has_already_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        let now_s = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())
            .expect("a unix timestamp after 1970");

        // A grant whose lending ended an hour ago, which is what a `--until
        // 18:00` looks like the next morning.
        {
            let mut file = peer::config::read_or_default(&peers_path).expect("the seeded file");
            let mut ended =
                peer::config::LendGrant::new(tcr_peer_wire::Window::SevenDay, 0.05, 300, 2);
            ended.until = Some(now_s - 3_600);
            file.peers[0].lend = vec![ended];
            peer::config::save(&peers_path, &file).expect("the ended grant is seeded");
        }

        let share_on = |path: PathBuf| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Share(peer_cli::PeerSwitchArgs {
                peers: peer_cli::PeersFile { peers: Some(path) },
                state: peer_cli::Switch::On,
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.10,
                ttl: 600,
                max_inflight: 2,
                scope: "all".to_string(),
                config: None,
            }),
        };
        let refusal = run_peer(share_on(peers_path.clone()))
            .await
            .expect_err("an end that has passed is refused, not printed as success")
            .to_string();
        assert!(
            refusal.contains("already"),
            "the refusal says the lending has ended: {refusal}"
        );
        assert!(
            refusal.contains("--relend") || refusal.contains("--for"),
            "and it says what to type instead, or an operator reads it as a bug: {refusal}"
        );

        let row = read_back(&peers_path);
        assert_eq!(
            row.lend.len(),
            1,
            "a refusal writes nothing, so the one ended grant is still the only one"
        );
        assert!(
            (row.lend[0].fraction - 0.05).abs() < f64::EPSILON,
            "and it is untouched: the refused fraction of 0.10 is not on disk"
        );
        assert!(
            !row.allow.inspect,
            "nor is the `inspect` this verb would have granted, which would be a disclosure \
             bought by a command that failed"
        );

        // The same verb on a grant whose end is ahead still works, which is the
        // control: without it this test passes on a verb that refuses always.
        {
            let mut file = peer::config::read_or_default(&peers_path).expect("the seeded file");
            file.peers[0].lend[0].until = Some(now_s + 3_600);
            peer::config::save(&peers_path, &file).expect("the live grant is seeded");
        }
        run_peer(share_on(peers_path.clone()))
            .await
            .expect("an end still ahead is shared on as before");
        let row = read_back(&peers_path);
        assert!(
            (row.lend[0].fraction - 0.10).abs() < f64::EPSILON,
            "the new fraction lands"
        );
        assert_eq!(
            row.lend[0].until,
            Some(now_s + 3_600),
            "and the operator's own end is still kept"
        );
    }

    /// **`tcr peer allow <peer> disclose on` persists that one grant and no
    /// other.**
    ///
    /// `disclose` is the one the borrowing seam reads
    /// (`fallback::peer_lease_provider`), and it is the opposite direction from
    /// `inspect`, so a verb that set both, or the wrong one, would turn on a
    /// disclosure the operator never asked for. Both halves are asserted.
    ///
    /// Watch it fail by deleting the `peer::config::save` call in the `Allow`
    /// arm: `disclose` comes back false.
    #[tokio::test]
    async fn peer_allow_persists_one_grant_and_leaves_the_others_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        // The WIRE form, not `display()`: the display form is deliberately
        // one-way and `PeerId::parse` refuses it (`crates/tcr-peer-wire`'s type
        // docs). `tcr peer ls --json` emits this form; the text output emits
        // the display form, which neither of these verbs can take: a surface
        // gap, not papered over here.
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Allow(peer_cli::PeerAllowArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                peer: peer_id.clone(),
                grant: peer_cli::PeerGrant::Disclose,
                state: peer_cli::Switch::On,
            }),
        })
        .await
        .expect("peer allow disclose on");

        let row = read_back(&peers_path);
        assert!(row.allow.allow_disclose, "the named grant is on");
        assert!(
            !row.allow.inspect && !row.allow.gateway && !row.allow.relay && !row.allow.accept_move,
            "one verb, one grant: {:?}",
            row.allow
        );

        run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Allow(peer_cli::PeerAllowArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                peer: peer_id,
                grant: peer_cli::PeerGrant::Disclose,
                state: peer_cli::Switch::Off,
            }),
        })
        .await
        .expect("peer allow disclose off");
        assert!(
            !read_back(&peers_path).allow.allow_disclose,
            "off is persisted too, a revoke that does not land is the dangerous direction"
        );

        // An UNPINNED peer is refused rather than written: a grant for a peer
        // nothing enforces is a row that reads like access.
        let stranger = tcr_peer_wire::PeerId([9_u8; 32]).to_wire();
        let refused = run_peer(peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Allow(peer_cli::PeerAllowArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                peer: stranger,
                grant: peer_cli::PeerGrant::Disclose,
                state: peer_cli::Switch::On,
            }),
        })
        .await;
        assert!(refused.is_err(), "an unpinned peer gets no grant");
        assert_eq!(
            read_back(&peers_path).node,
            tcr_peer_wire::PeerId(PINNED),
            "and the refusal wrote nothing"
        );
    }

    /// A main config with one account, pinned or not, for `--mode hand`'s
    /// exit-lock check to read.
    ///
    /// Written as JSON text rather than through `Config`, for the reason the
    /// peers fixtures give: a field added to `Config` later must not turn this
    /// into a compile error.
    fn config_with_account(dir: &Path, egress: &str) -> PathBuf {
        let path = dir.join(tcr::paths::CONFIG_FILE);
        std::fs::write(
            &path,
            format!(
                r#"{{
                  "proxy": {{ "port": 0 }},
                  "upstream": "https://api.anthropic.com",
                  "accounts": [
                    {{ "name": "alice@example.com", "accessToken": "at-alice"{egress} }}
                  ]
                }}"#
            ),
        )
        .expect("write the main config");
        path
    }

    /// **`tcr peer lend --mode hand` mints a hand grant, and a replace without
    /// `--mode` keeps it.**
    ///
    /// The CLI could not mint a hand grant at all: `LendMode` existed on the
    /// grant and on the wire, and no argv reached it, so the whole mode was
    /// unreachable from the surface an operator uses. Worse,
    /// the replace-by-(window, scope) path rewrote an existing hand grant as
    /// serve, so editing a hand grant's fraction silently took a disclosure
    /// decision back.
    ///
    /// The second half is the one with teeth: an omitted `--mode` has to mean
    /// "keep what is there", not "default to serve".
    ///
    /// Watched red by dropping the `.or(replacing)` from the mode resolution:
    /// the fraction edit turns the grant back into serve.
    #[tokio::test]
    async fn peer_lend_mints_a_hand_grant_and_a_replace_keeps_the_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        let config_path = config_with_account(dir.path(), "");
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();

        let lend = |fraction: f64, mode: Option<peer_cli::PeerLendMode>| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Lend(peer_cli::PeerLendArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                peer: peer_id.clone(),
                window: peer_cli::PeerWindow::SevenDay,
                fraction,
                ttl: 300,
                max_inflight: 3,
                scope: "all".to_string(),
                mode,
                config: Some(config_path.clone()),
                for_: None,
                until: None,
                between: None,
                days: None,
                list: false,
                revoke: None,
                relend: None,
            }),
        };

        run_peer(lend(0.20, Some(peer_cli::PeerLendMode::Hand)))
            .await
            .expect("peer lend --mode hand");
        let row = read_back(&peers_path);
        assert_eq!(
            row.lend[0].mode,
            peer::config::LendMode::Hand,
            "the operator asked for hand mode and the file has to say so: {:?}",
            row.lend
        );

        // The same grant, a different fraction, no `--mode`.
        run_peer(lend(0.30, None))
            .await
            .expect("peer lend, fraction only");
        let row = read_back(&peers_path);
        assert_eq!(
            row.lend.len(),
            1,
            "replaced, never appended: {:?}",
            row.lend
        );
        assert_eq!(
            row.lend[0].mode,
            peer::config::LendMode::Hand,
            "an omitted --mode keeps the mode that was there, or editing a fraction takes \
             a disclosure decision back without saying so: {:?}",
            row.lend
        );

        // And a NEW grant, on a window nothing covers yet, is serve.
        let mut fresh = lend(0.10, None);
        if let peer_cli::PeerAction::Lend(args) = &mut fresh.action {
            args.window = peer_cli::PeerWindow::FiveHour;
        }
        run_peer(fresh).await.expect("peer lend 5h");
        let row = read_back(&peers_path);
        let five_hour = row
            .lend
            .iter()
            .find(|grant| grant.window == tcr_peer_wire::Window::FiveHour)
            .expect("the 5h grant is there");
        assert_eq!(
            five_hour.mode,
            peer::config::LendMode::Serve,
            "a grant with nothing to inherit from is serve, which is what every grant \
             written before modes existed already meant"
        );
    }

    /// **`--mode hand` is refused when every account in the scope is strictly
    /// pinned away from this Mac.**
    ///
    /// A hand grant hands the borrower a bearer to spend from its OWN IP, and a
    /// strict pin says this account's requests leave through a named peer or
    /// not at all. The grant would look granted and every borrowed request
    /// would be refused by the exit lock, which is the worst of both: a
    /// capability advertised and not there.
    ///
    /// The non-strict leg is the control, and it is the assertion that makes
    /// this about STRICTNESS rather than about pinning: a pin without
    /// `egressStrict` falls back to local, which is exactly what a hand-mode
    /// borrower does, so it is allowed.
    ///
    /// Watched red by deleting the refusal block: the strict case writes a hand
    /// grant and the first assertion reads `Ok`.
    #[tokio::test]
    async fn peer_lend_mode_hand_is_refused_when_the_scope_is_pinned_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();
        let carrier = tcr_peer_wire::PeerId([0x44; 32]).to_wire();

        let lend_against = |config_path: PathBuf| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Lend(peer_cli::PeerLendArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                peer: peer_id.clone(),
                window: peer_cli::PeerWindow::SevenDay,
                fraction: 0.20,
                ttl: 300,
                max_inflight: 3,
                scope: "all".to_string(),
                mode: Some(peer_cli::PeerLendMode::Hand),
                config: Some(config_path),
                for_: None,
                until: None,
                between: None,
                days: None,
                list: false,
                revoke: None,
                relend: None,
            }),
        };

        let strict = config_with_account(
            dir.path(),
            &format!(r#", "egress": "via {carrier}", "egressStrict": true"#),
        );
        let refusal = run_peer(lend_against(strict))
            .await
            .expect_err("a hand grant on a strictly pinned scope buys nothing");
        let said = refusal.to_string();
        assert!(
            said.contains("egressStrict") && said.contains("--mode serve"),
            "the refusal has to name the lock AND what to do instead, or an operator reads \
             it as a bug in the CLI: {said}"
        );
        assert!(
            read_back(&peers_path).lend.is_empty(),
            "and it wrote nothing: a refusal that half-applied would leave a grant the \
             operator was told they did not get"
        );

        // The control: pinned but NOT strict falls back to local, which is what
        // a hand-mode borrower does anyway, so it is allowed.
        let soft = dir.path().join("soft");
        std::fs::create_dir_all(&soft).expect("a second scratch directory");
        let soft_config = config_with_account(&soft, &format!(r#", "egress": "via {carrier}""#));
        run_peer(lend_against(soft_config))
            .await
            .expect("a pin without egressStrict is not a refusal");
        assert_eq!(
            read_back(&peers_path).lend[0].mode,
            peer::config::LendMode::Hand,
            "and it wrote the hand grant"
        );
    }

    /// **`tcr peer lend` persists one grant per window, replaces rather than
    /// duplicates, clamps the fraction, and removes at zero.**
    ///
    /// Four claims in one test because they are one write: a second grant for
    /// the same window that appended instead of replacing would leave
    /// `Ledger::grant`'s `find` reading whichever row came first, which is not
    /// the one the operator just typed.
    ///
    /// Watch it fail by deleting the `row.lend.retain(...)` line in the `Lend`
    /// arm: the second call leaves two `7d` grants and the length assertion
    /// goes red.
    #[tokio::test]
    async fn peer_lend_persists_one_grant_per_window_and_removes_at_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = pinned_peers_file(dir.path());
        // The WIRE form, not `display()`: the display form is deliberately
        // one-way and `PeerId::parse` refuses it (`crates/tcr-peer-wire`'s type
        // docs). `tcr peer ls --json` emits this form; the text output emits
        // the display form, which neither of these verbs can take: a surface
        // gap, not papered over here.
        let peer_id = tcr_peer_wire::PeerId(PINNED).to_wire();

        let lend = |fraction: f64, window: peer_cli::PeerWindow| peer_cli::PeerArgs {
            action: peer_cli::PeerAction::Lend(peer_cli::PeerLendArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                peer: peer_id.clone(),
                window,
                fraction,
                ttl: 300,
                max_inflight: 3,
                scope: "all".to_string(),
                mode: None,
                config: None,
                for_: None,
                until: None,
                between: None,
                days: None,
                list: false,
                revoke: None,
                relend: None,
            }),
        };

        run_peer(lend(0.20, peer_cli::PeerWindow::SevenDay))
            .await
            .expect("peer lend 7d 0.20");
        let row = read_back(&peers_path);
        assert_eq!(row.lend.len(), 1);
        assert!((row.lend[0].fraction - 0.20).abs() < f64::EPSILON);
        assert_eq!(row.lend[0].ttl_s, 300);
        assert_eq!(row.lend[0].max_inflight, 3);

        // A second grant for the SAME window replaces it, and the fraction is
        // clamped to the ceiling with the clamp printed (see `peer_lend_grant`).
        run_peer(lend(0.90, peer_cli::PeerWindow::SevenDay))
            .await
            .expect("peer lend 7d 0.90");
        let row = read_back(&peers_path);
        assert_eq!(
            row.lend.len(),
            1,
            "replaced, never appended: {:?}",
            row.lend
        );
        assert!(
            (row.lend[0].fraction - MAX_LEND_FRACTION).abs() < f64::EPSILON,
            "0.90 is clamped to the {MAX_LEND_FRACTION} ceiling, got {}",
            row.lend[0].fraction
        );

        // A DIFFERENT window is a second row, not a replacement.
        run_peer(lend(0.10, peer_cli::PeerWindow::FiveHour))
            .await
            .expect("peer lend 5h 0.10");
        assert_eq!(read_back(&peers_path).lend.len(), 2);

        // Zero is a removal, and only of the window named.
        run_peer(lend(0.0, peer_cli::PeerWindow::SevenDay))
            .await
            .expect("peer lend 7d 0");
        let row = read_back(&peers_path);
        assert_eq!(row.lend.len(), 1, "the 7d grant is gone: {:?}", row.lend);
        assert_eq!(
            row.lend[0].window,
            tcr_peer_wire::Window::FiveHour,
            "and the 5h one is untouched"
        );
    }
}
