//! Reach from off the LAN: `tcr peer reach`, what this Mac can be reached on, and `tcr peer internet`, the switch that lets pinned Macs reach it.

use tcr::peer;

use super::peer_cli;
use super::{
    ensure_listen_for_opt_in, lock_peers_file, masked_label, now_unix_secs, peer_state_path,
    print_listen_written,
};

/// `tcr peer reach`: the three ways a Mac off this LAN could reach this one.
///
/// Read-only by default, and synchronous, because every one of the three is a
/// local question: a route-table read, a route lookup on an unconnected socket,
/// and a UDP exchange with the router on the local link. Nothing here dials a
/// peer and nothing touches the running proxy.
///
/// # The three, and why the third has no number yet
///
/// A global IPv6 address needs no router cooperation at all. A NAT-PMP mapping
/// is the IPv4 answer, and a refusal is the ORDINARY outcome -- most routers
/// ship with port mapping off -- so every failure here prints and the verb
/// still exits 0. The third is the time-derived port
/// ([`tcr::peer::reach::derived_port`]), which needs the secret both
/// ends share, and that secret is the pair's Noise handshake hash: **it is not
/// persisted anywhere today**, so the per-peer rows print the slot and say the
/// port is unavailable rather than inventing a number from public key material,
/// which a scanner could compute too.
/// `tcr peer reach`'s tailnet line, from the setting in the peers file and
/// the record the serving process last wrote.
///
/// The record is what the line reports, and it is never refreshed here: the
/// Tailscale app is read by the server, not by this verb. A record older than
/// [`peer::state::TAILNET_RECORD_STALE_MS`] says so rather than passing for
/// the current state.
///
/// The setting wins over the record. A server records nothing while the mode
/// is off, so a record left from before the mode went off is not read at all:
/// off is off, and an unknown word is shown as written and treated as off.
fn reach_tailnet_line(
    setting: &peer::config::TailnetSetting,
    record: Option<&peer::state::TailnetRecord>,
    now_ms: i64,
) -> String {
    use peer::config::{TailnetMode, TailnetSetting};
    use peer::state::TailnetState;

    match setting {
        TailnetSetting::Known(TailnetMode::Off) => return "reach: tailnet: off".to_string(),
        TailnetSetting::Unknown(word) => {
            return format!(
                "reach: tailnet: off (\"{}\" is a mode this tcr does not know, so it is treated \
                 as off)",
                masked_label(word)
            );
        }
        TailnetSetting::Known(TailnetMode::App | TailnetMode::Node) => {}
    }
    let Some(record) = record else {
        return format!(
            "reach: tailnet: {}: no serving process has recorded a tailnet reading",
            setting.as_word()
        );
    };
    if record.is_stale(now_ms) {
        return format!(
            "reach: tailnet: {}: the serving process last recorded one {}s ago, so it is not \
             running or not reading the tailnet",
            setting.as_word(),
            now_ms.saturating_sub(record.recorded_at_ms).max(0) / 1_000
        );
    }
    match record.state {
        TailnetState::Off => "reach: tailnet: off".to_string(),
        TailnetState::Ready => format!(
            "reach: tailnet: {}: {}, {} of your Macs heard over it (recorded by the serving \
             process)",
            record.mode.as_word(),
            record
                .address
                .map_or_else(|| "no tailnet address".to_string(), |addr| addr.to_string()),
            record.found
        ),
        state => format!(
            "reach: tailnet: {}: {}: {}",
            record.mode.as_word(),
            serde_json::to_value(state)
                .ok()
                .and_then(|word| word.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{state:?}")),
            record.detail.as_deref().unwrap_or("no detail recorded")
        ),
    }
}

pub(super) fn run_peer_reach(args: peer_cli::PeerReachArgs) -> anyhow::Result<()> {
    use tcr::peer::reach;

    /// How long a probe asks a mapping to be held for. Two minutes: long
    /// enough for the operator to test a dial, short enough that a forgotten
    /// probe leaves nothing open.
    const PROBE_LIFETIME_SECS: u32 = 120;

    let peers_path = args
        .peers
        .clone()
        .unwrap_or_else(peer::config::default_path);
    let file = peer::config::read_or_default(&peers_path)?;
    let store = peer::config::PeerStore::open(&peers_path)?;
    let rows = store.peers();

    // This process has run no session, so its reflexive register is empty and
    // every readout below would say "not told" on a Mac whose peers file
    // plainly records where it is seen. The rows carry both facts and the
    // serving process restores them at boot for the same reason; a CLI that
    // reports on reach restores them here.
    reach::restore_from_peers(&file);

    let v6 = reach::global_v6_addresses();
    let internal_port = file.listen.map(|listen| listen.port());
    let slot = reach::current_slot(now_unix_secs());

    // The dead drop's own readout: no `drop-status` verb (the design rules
    // one out), the current answer goes here where an operator already
    // looks. `lastFetchedAt` is the most recent `EndpointSource::Drop`
    // endpoint this peer's row holds, the one thing a fetch that actually
    // wrote something leaves on disk: nothing local records a publish, that
    // write lands on the store and never on this file, so no publish time is
    // printed rather than one invented.
    let drop_slot = peer::config::drop::current_slot(now_unix_secs());
    let drop_rows: Vec<(String, tcr_peer_wire::PeerId, String, Option<i64>)> = rows
        .iter()
        .filter(|row| row.allow.control.drop)
        .filter_map(|row| {
            let secret = row.rendezvous_secret?;
            let keys = peer::config::drop::DropKeys::derive(&secret);
            let name =
                peer::config::drop::DropName::for_slot(&keys, &row.node, drop_slot).to_wire();
            let last_fetched_ms = row
                .endpoints
                .iter()
                .filter(|ep| ep.source == peer::config::EndpointSource::Drop)
                .map(|ep| ep.observed_at_ms)
                .max();
            Some((row.label.clone(), row.node, name, last_fetched_ms))
        })
        .collect();

    // What a SERVING process recorded, read off the state file and never asked
    // of the router: a probe would change the mapping table it is reporting
    // on, and the keeper inside a running `tcr` already knows the answer.
    //
    // A record whose deadline has passed reads as no mapping. It is what a
    // process that died without deleting its mapping leaves behind, and
    // reporting it would tell an operator a port is open that nothing renews.
    let now_ms = peer::state::now_ms();
    let state = peer::state::load(&peer_state_path(&peers_path), now_ms).ok();
    let held = state
        .as_ref()
        .and_then(|state| state.mapping.clone())
        .filter(|record| record.expires_at_ms > now_ms);
    // The tailnet, as the serving process last recorded it: the same rule as
    // the mapping, never asked of the Tailscale app here. A stale record is
    // shown as stale, not dropped, so "the server stopped writing" and "no
    // server ever wrote one" stay two answers.
    let tailnet = state.and_then(|state| state.tailnet);
    let tailnet_line = reach_tailnet_line(&file.tailnet, tailnet.as_ref(), now_ms);

    // Every NAT-PMP outcome is a string, including the failures, because the
    // reader of this verb wants to see WHICH refusal the router gave.
    let client = reach::NatPmp::on_default_gateway();
    // `no_mapping` is the one sentence that names what is left when the router
    // refuses both protocols; `None` whenever the router answered or was not
    // asked, so it never prints under a mapping that worked.
    let mut no_mapping: Option<String> = None;
    let (gateway, external, mapping) = match &client {
        Ok(client) => {
            let mapping = match (args.map, internal_port) {
                (false, _) => "not asked for (pass --map)".to_string(),
                (true, None) => {
                    "no peer listener is configured, so there is no port to map".to_string()
                }
                (true, Some(port)) => {
                    // The same fallback the keeper runs, so this verb reports
                    // what `peer.internet on` would actually get rather than
                    // what NAT-PMP alone would: a router that speaks only UPnP
                    // is exactly the one an operator runs this verb about.
                    // Every UPnP refusal keeps its own name here,
                    // `UntrustedLocation`, `ControlUrlElsewhere` and
                    // `Redirected` included, because those three say a device
                    // answered and THIS NODE refused it, which "router did not
                    // answer" would hide.
                    let mut keeper = reach::MappingKeeper::new(*client, port, PROBE_LIFETIME_SECS)
                        .with_upnp(reach::upnp_discoverer());
                    match keeper.map() {
                        Ok(granted) => {
                            let over = if keeper.granted_by() == Some(reach::MappedOver::Upnp) {
                                " (over upnp; nat-pmp did not answer)"
                            } else {
                                ""
                            };
                            format!(
                                "tcp {} -> {} for {}s{over}",
                                granted.external_port, granted.internal_port, granted.lifetime_secs
                            )
                        }
                        Err(err @ reach::ReachError::Silent { .. }) => {
                            no_mapping = reach::reach_no_mapping_line(&err, Some(port));
                            "router did not answer".to_string()
                        }
                        Err(err) => {
                            no_mapping = reach::reach_no_mapping_line(&err, Some(port));
                            format!("refused: {err}")
                        }
                    }
                }
            };
            // Computed AFTER the mapping attempt above: the same UPnP
            // discovery the mapping arm just ran, run again this line's own
            // way, so this probe never contends with the one that actually
            // grants the lease.
            let external = reach::external_address_report(client, &reach::upnp_discoverer());
            (format!("{}", client.gateway()), external, mapping)
        }
        Err(err) => (
            format!("unavailable: {err}"),
            "unavailable: no gateway".to_string(),
            "unavailable: no gateway".to_string(),
        ),
    };

    if args.json {
        let peers: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "label": masked_label(&row.label),
                    "node": row.node.display(),
                    "derivedPort": serde_json::Value::Null,
                    "derivedPortUnavailable": PORT_SECRET_NOT_PERSISTED,
                    // Where that Mac says it sees THIS one, and whether the
                    // two could punch. Both come out of `reach` rather than
                    // being derived here, so this readout and the dial cannot
                    // disagree about what "possible" means.
                    "seesUsAt": reach::observed_self_for(&row.node)
                        .map(|addr| addr.to_string()),
                    "punchPossible": reach::punch_target(&row.node, &row.endpoints).is_ok(),
                })
            })
            .collect();
        let drop_peers: Vec<serde_json::Value> = drop_rows
            .iter()
            .map(|(label, node, name, last_fetched_ms)| {
                serde_json::json!({
                    "label": masked_label(label),
                    "node": node.display(),
                    "dropName": name,
                    "lastFetchedAtMs": last_fetched_ms,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "ipv6": v6.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "gateway": gateway,
            "externalAddress": external,
            "mapping": mapping,
            "listenPort": internal_port,
            "heldMapping": held,
            "tailnet": tailnet,
            "slot": slot,
            "slotSeconds": reach::SLOT_SECONDS,
            "peers": peers,
            "deadDrop": {
                "on": file.dead_drop.is_live(),
                "slot": drop_slot,
                "slotSeconds": file.dead_drop.slot_seconds,
                "peers": drop_peers,
            },
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if v6.is_empty() {
        println!(
            "reach: ipv6: none: this Mac has no globally routable IPv6 address, so a peer \
             cannot reach it over IPv6"
        );
    } else {
        for addr in &v6 {
            println!("reach: ipv6: {addr}");
        }
    }
    println!("reach: gateway: {gateway}");
    println!("reach: external-address: {external}");
    println!("reach: mapping: {mapping}");
    if let Some(line) = &no_mapping {
        println!("{line}");
    }
    match internal_port {
        Some(port) => println!("reach: listen-port: {port}"),
        None => println!("reach: listen-port: none (the peer listener is off)"),
    }
    match &held {
        Some(record) => println!(
            "reach: held-mapping: {} -> {} until {} (recorded by the serving process, not \
             asked of the router)",
            record
                .external_address
                .clone()
                .unwrap_or_else(|| format!("port {}", record.external_port)),
            record.internal_port,
            record.expires_at_ms
        ),
        None => println!(
            "reach: held-mapping: none (no serving process on this Mac holds one right now)"
        ),
    }
    println!("{tailnet_line}");
    println!(
        "reach: slot: {slot} ({}s each, the slot before and after also accepted)",
        reach::SLOT_SECONDS
    );
    if rows.is_empty() {
        println!("reach: peers: none pinned");
    }
    for row in &rows {
        println!(
            "reach: peer: {} {}: derived-port: unavailable: {PORT_SECRET_NOT_PERSISTED}",
            masked_label(&row.label),
            row.node.display()
        );
        // The punch half, from `reach` and formatted there: one function so a
        // reader of this verb and a dial that punches cannot disagree.
        println!(
            "{}",
            reach::reach_punch_line(&masked_label(&row.label), &row.node, &row.endpoints)
        );
    }
    if file.dead_drop.is_live() {
        println!(
            "reach: dead-drop: on, slot {drop_slot} ({}s each)",
            file.dead_drop.slot_seconds
        );
        if drop_rows.is_empty() {
            println!(
                "reach: dead-drop: no peer has both `control-drop` and a completed session yet"
            );
        }
        for (label, node, name, last_fetched_ms) in &drop_rows {
            println!(
                "reach: dead-drop: peer: {} {}: name-this-slot: {name}",
                masked_label(label),
                node.display()
            );
            match last_fetched_ms {
                Some(ms) => println!("reach: dead-drop: last-fetch-wrote: {ms}"),
                None => {
                    println!("reach: dead-drop: last-fetch-wrote: never (or nothing new to write)");
                }
            }
        }
    } else {
        println!("reach: dead-drop: off");
    }
    Ok(())
}

/// Why `tcr peer reach` prints no derived port, in the one wording both the
/// text and the JSON use.
const PORT_SECRET_NOT_PERSISTED: &str = "the pair's handshake secret is not stored, and deriving a port from public key \
     material instead would give every scanner the same number";

/// `tcr peer internet`: let pinned Macs reach this one from off its own LAN, or stop.
pub(super) fn run_peer_internet(a: peer_cli::PeerInternetArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
    let on = matches!(a.state, peer_cli::Switch::On);
    // A port first, because what `internet on` asks the router for is a
    // mapping TO the listener's port, and with none it used to write
    // the flag, print "no listener is configured" and leave the
    // operator with a switch that does nothing.
    //
    // In its own scope: `reach::set_internet` takes the peers-file lock
    // itself, and the lock is a lockfile, not a reentrant one, so a
    // hold still open here would make the whole verb wait out
    // `LOCK_WAIT_MS` and then refuse.
    if on {
        let written = {
            let (_lock, mut file) = lock_peers_file(&peers_path)?;
            match ensure_listen_for_opt_in(&mut file) {
                Some(chosen) => {
                    peer::config::save(&peers_path, &file)?;
                    Some(chosen)
                }
                None => None,
            }
        };
        if let Some(chosen) = written {
            print_listen_written(chosen, &peers_path);
        }
    }
    // The whole verb. The flag, the mapping delete and the refusal
    // wording live in `peer::reach`, so this arm is argv and nothing
    // else; see `reach::set_internet`.
    match peer::reach::set_internet(&peers_path, on)? {
        peer::reach::InternetSwitch::On {
            listen_port: Some(port),
        } => println!(
            "peer.internet: on (the listener on port {port} is mapped at boot and \
             renewed every 30 minutes)"
        ),
        // Unreachable through this arm now, since the block above gives
        // this Mac a port before the switch is written. Kept because
        // the type says it can happen and a caller of `set_internet`
        // that is not this arm may still see it; it no longer tells
        // anybody to go and hand-edit a file.
        peer::reach::InternetSwitch::On { listen_port: None } => println!(
            "peer.internet: on, but this Mac has no peer port, so there is nothing \
             to map yet"
        ),
        peer::reach::InternetSwitch::Off { deleted: Some(_) } => {
            println!("peer.internet: off (the router mapping was deleted)");
        }
        peer::reach::InternetSwitch::Off { deleted: None } => {
            println!("peer.internet: off (there was no mapping to delete)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod reach_tailnet_line_tests {
    use super::*;
    use peer::config::{TailnetMode, TailnetSetting};
    use peer::state::{TailnetRecord, TailnetState};

    fn record(state: TailnetState, detail: Option<&str>) -> TailnetRecord {
        TailnetRecord {
            mode: TailnetMode::App.into(),
            state,
            address: Some(std::net::Ipv4Addr::new(100, 64, 0, 7)),
            found: 2,
            detail: detail.map(str::to_string),
            recorded_at_ms: 1_000,
            ..TailnetRecord::default()
        }
    }

    /// **The line says what the server recorded, says stale when it is, and
    /// says when nothing was recorded. The setting wins: off is off even
    /// with a fresh record left from app mode, and an unknown word is shown
    /// and treated as off.**
    ///
    /// Watch it fail by making the `Known(TailnetMode::Off)` arm fall through
    /// instead of returning: the first row then reads `off: no serving process
    /// has recorded a tailnet reading`, and the leftover-record row would
    /// report app/ready.
    #[test]
    fn the_tailnet_line_for_each_case() {
        let off = TailnetSetting::Known(TailnetMode::Off);
        let app = TailnetSetting::Known(TailnetMode::App);
        assert_eq!(reach_tailnet_line(&off, None, 1_000), "reach: tailnet: off");
        assert!(reach_tailnet_line(&app, None, 1_000).contains("no serving process has recorded"));
        let ready = record(TailnetState::Ready, None);
        assert_eq!(
            reach_tailnet_line(&app, Some(&ready), 2_000),
            "reach: tailnet: app: 100.64.0.7, 2 of your Macs heard over it (recorded by the \
             serving process)"
        );
        assert_eq!(
            reach_tailnet_line(&off, Some(&ready), 2_000),
            "reach: tailnet: off",
            "a fresh record left from app mode does not outrank the setting"
        );
        assert!(
            reach_tailnet_line(
                &TailnetSetting::Unknown("both".to_string()),
                Some(&ready),
                2_000
            )
            .contains("\"both\" is a mode this tcr does not know"),
        );
        assert!(
            reach_tailnet_line(
                &app,
                Some(&ready),
                1_001 + peer::state::TAILNET_RECORD_STALE_MS
            )
            .contains("not running or not reading"),
            "a stale record is reported as stale"
        );
        let failed = record(
            TailnetState::AppNotRunning,
            Some("The Tailscale app is not running on this Mac."),
        );
        assert_eq!(
            reach_tailnet_line(&app, Some(&failed), 2_000),
            "reach: tailnet: app: appNotRunning: The Tailscale app is not running on this Mac."
        );
    }
}
