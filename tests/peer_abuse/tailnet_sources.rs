//! Which source addresses reach the knock path at all: a tailnet is not this LAN,
//! and the operator's own Macs may knock over it in app mode.

use super::*;

// ---------------------------------------------------------------------------
// A tailnet is not this LAN
// ---------------------------------------------------------------------------

/// **A node on a tailnet reaches the knock path on no setting, and neither
/// does a link-local address.**
///
/// `listener::is_lan_scope` used to count `100.64.0.0/10` (RFC 6598, which is
/// what Tailscale numbers a tailnet out of) and `169.254.0.0/16` (RFC 3927
/// link-local) as LAN, and `internet_admission` tests the SOURCE before it
/// looks at anything else. So a listener bound `0.0.0.0` with
/// `peer.internet` off answered a knock and a first pairing from any tailnet
/// node on earth, and the switch the operator set could not refuse it: the
/// pairing queue on the operator's screen was reachable from a network they
/// never meant to expose it to.
///
/// Watch it fail: put `a == 100 && (64..128).contains(&b)` back into
/// `is_lan_scope_v4` and the first two rows below return `Answer`.
#[test]
fn a_tailnet_or_link_local_source_never_reaches_the_knock_path() {
    let wide_open: std::net::IpAddr = "0.0.0.0".parse().expect("a literal address parses");

    for source in ["100.64.0.1", "100.127.255.254", "169.254.1.1"] {
        let from: std::net::IpAddr = source.parse().expect("a literal address parses");
        for pattern in [
            noise::Handshake::Knock,
            noise::Handshake::KnockPsk,
            noise::Handshake::Pair,
        ] {
            for internet in [false, true] {
                assert_eq!(
                    listener::internet_admission(wide_open, from, pattern, internet, &[]),
                    listener::InternetAdmission::Refuse,
                    "{source} is not on this LAN, so a {} must get nothing back from a \
                     listener bound to a globally routable address (internet={internet})",
                    pattern.pattern()
                );
            }
        }

        // And the path such a peer really uses is untouched: a return visit
        // against a pinned key is still answered, because the pin is what
        // decides it.
        assert_eq!(
            listener::internet_admission(wide_open, from, noise::Handshake::Return, false, &[]),
            listener::InternetAdmission::Answer,
            "{source} must still reach the pin check with an IK return visit"
        );
    }

    // Positive control: a real private-LAN source is answered exactly as
    // before, or the assertions above are about a gate that refuses everybody.
    let neighbour: std::net::IpAddr = "192.168.1.4".parse().expect("a literal address parses");
    assert_eq!(
        listener::internet_admission(wide_open, neighbour, noise::Handshake::Knock, true, &[]),
        listener::InternetAdmission::Answer,
        "a neighbour on the private LAN must still be able to knock"
    );
}

/// **A tailnet's IPv6 address gets the same answer as its IPv4 one.**
///
/// Tailscale numbers a tailnet out of `fd7a:115c:a1e0::/48` as well as
/// `100.64.0.0/10`, and that /48 sits inside the unique-local `fc00::/7`
/// that `is_lan_scope_v6` counts as LAN. So one tailnet node was refused a
/// knock at `100.x` and answered at `fd7a:`, and the IPv4 rule above was only
/// half a rule.
///
/// Watch it fail: delete the `is_tailscale_v6` early return from
/// `is_lan_scope_v6` and every `fd7a:` row below returns `Answer`.
#[test]
fn a_tailnet_ipv6_source_never_reaches_the_knock_path_either() {
    for bind in ["0.0.0.0", "::"] {
        let wide_open: std::net::IpAddr = bind.parse().expect("a literal address parses");
        for source in [
            "fd7a:115c:a1e0::1",
            "fd7a:115c:a1e0:ab12:4843:cd96:6258:b240",
        ] {
            let from: std::net::IpAddr = source.parse().expect("a literal address parses");
            for pattern in [
                noise::Handshake::Knock,
                noise::Handshake::KnockPsk,
                noise::Handshake::Pair,
            ] {
                for internet in [false, true] {
                    assert_eq!(
                        listener::internet_admission(wide_open, from, pattern, internet, &[]),
                        listener::InternetAdmission::Refuse,
                        "{source} is a tailnet address like 100.64.0.0/10, so a {} on a \
                         listener bound {bind} must get nothing back (internet={internet})",
                        pattern.pattern()
                    );
                }
            }
            assert_eq!(
                listener::internet_admission(wide_open, from, noise::Handshake::Return, false, &[]),
                listener::InternetAdmission::Answer,
                "{source} must still reach the pin check with an IK return visit"
            );
        }

        // Every other unique-local address is still this LAN: only the /48
        // moved. `fd7a:115c:a1e1::1` is one past the range's edge.
        for neighbour in ["fd00::1", "fc00::1", "fd7a:115c:a1e1::1"] {
            let from: std::net::IpAddr = neighbour.parse().expect("a literal address parses");
            assert_eq!(
                listener::internet_admission(wide_open, from, noise::Handshake::Knock, true, &[]),
                listener::InternetAdmission::Answer,
                "{neighbour} is unique-local outside Tailscale's /48, so it still counts as LAN"
            );
        }
    }
    assert!(listener::is_tailscale_v6(
        "fd7a:115c:a1e0::5"
            .parse()
            .expect("a literal address parses")
    ));
    assert!(!listener::is_tailscale_v6(
        "fd00::5".parse().expect("a literal address parses")
    ));
}

// ---------------------------------------------------------------------------
// Your own Macs may knock over the tailnet, in app mode (Gil, 2026-10-20)
// ---------------------------------------------------------------------------

/// A Tailscale status with fake values: this Mac is user 111 at
/// `100.64.0.1` / `fd7a:115c:a1e0::1`; laptop-2 is the operator's own Mac;
/// the rest are each refused for one reason.
const OWN_MAC_STATUS: &str = r#"{
  "BackendState": "Running",
  "Self": {"UserID": 111, "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]},
  "Peer": {
    "nodekey:01": {"HostName": "laptop-2", "OS": "macOS", "UserID": 111, "Online": true,
      "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"]},
    "nodekey:04": {"HostName": "coworker", "OS": "macOS", "UserID": 222, "Online": true,
      "TailscaleIPs": ["100.64.0.5", "fd7a:115c:a1e0::5"]},
    "nodekey:05": {"HostName": "server", "OS": "macOS", "UserID": 111, "Online": true,
      "TailscaleIPs": ["100.64.0.6"], "Tags": ["tag:tcr"]},
    "nodekey:06": {"HostName": "shared-in", "OS": "macOS", "UserID": 111, "Online": true,
      "TailscaleIPs": ["100.64.0.7"], "ShareeNode": true}
  }
}"#;

fn own_mac_status() -> tcr::peer::tailscale_app::Status {
    tcr::peer::tailscale_app::Status::parse(OWN_MAC_STATUS.as_bytes()).expect("the fixture parses")
}

fn ip(text: &str) -> std::net::IpAddr {
    text.parse().expect("a literal address parses")
}

const PAIRING_PATTERNS: [noise::Handshake; 3] = [
    noise::Handshake::Knock,
    noise::Handshake::KnockPsk,
    noise::Handshake::Pair,
];

/// **One of the operator's own Macs, knocking on this Mac's own tailnet
/// address in app mode, is answered for `Knock`, `KnockPsk` and `Pair`, in both
/// families**, and the same source is still refused by `internet_admission`
/// alone: the row above keeps holding for everyone else.
///
/// Watch it fail by making `own_mac_admission` return `Refuse` after its
/// checks (the code before this change): every row here flips.
#[test]
fn an_own_mac_on_the_tailnet_address_is_answered_in_app_mode() {
    use tcr::peer::config::TailnetMode;
    let status = own_mac_status();
    for (accepted_on, from) in [
        ("100.64.0.1", "100.64.0.2"),
        ("::ffff:100.64.0.1", "::ffff:100.64.0.2"),
        ("fd7a:115c:a1e0::1", "fd7a:115c:a1e0::2"),
    ] {
        for pattern in PAIRING_PATTERNS {
            assert_eq!(
                listener::own_mac_admission(
                    TailnetMode::App,
                    Some(ip(accepted_on)),
                    ip(from),
                    pattern,
                    Ok(&status)
                ),
                listener::InternetAdmission::Answer,
                "laptop-2 at {from}, arriving on {accepted_on}, a {}",
                pattern.pattern()
            );
            assert_eq!(
                listener::internet_admission(ip("0.0.0.0"), ip(from), pattern, false, &[]),
                listener::InternetAdmission::Refuse,
                "the blanket rule for a tailnet source is unchanged"
            );
        }
    }
}

/// **Everyone else on the tailnet keeps the refusal**: another user's Mac, a
/// tagged node, a shared-in node, a source that is not on the tailnet at all,
/// a connection that arrived on a non-tailnet local address (a hotspot
/// neighbour whose carrier-grade NAT address happens to be laptop-2's), mode
/// `off`, mode `node`, and a reading of the app that failed. Return and Enrol
/// are not this rule's to answer.
///
/// Watch each fail by deleting the clause it names in `own_mac_admission` or
/// `may_be_an_own_mac_knock`: `on_my_tailnet_address` for the hotspot rows,
/// the mode check for `off` and `node`, the `own_macs` lookup (any tailnet
/// peer instead) for the three other-machine rows.
#[test]
fn everyone_else_on_the_tailnet_is_refused_a_knock() {
    use tcr::peer::config::TailnetMode;
    use tcr::peer::tailscale_app::ReadError;
    let status = own_mac_status();
    let refused = |mode, accepted_on: Option<&str>, from: &str, reading, why: &str| {
        for pattern in PAIRING_PATTERNS {
            assert_eq!(
                listener::own_mac_admission(mode, accepted_on.map(ip), ip(from), pattern, reading),
                listener::InternetAdmission::Refuse,
                "{why}: a {} from {from} on {accepted_on:?}",
                pattern.pattern()
            );
        }
    };
    let on_me = Some("100.64.0.1");
    refused(
        TailnetMode::App,
        on_me,
        "100.64.0.5",
        Ok(&status),
        "another user's Mac",
    );
    refused(
        TailnetMode::App,
        Some("fd7a:115c:a1e0::1"),
        "fd7a:115c:a1e0::5",
        Ok(&status),
        "another user's Mac over IPv6",
    );
    refused(
        TailnetMode::App,
        on_me,
        "100.64.0.6",
        Ok(&status),
        "a tagged node",
    );
    refused(
        TailnetMode::App,
        on_me,
        "100.64.0.7",
        Ok(&status),
        "a shared-in node",
    );
    refused(
        TailnetMode::App,
        on_me,
        "100.64.0.99",
        Ok(&status),
        "an address no machine holds",
    );
    refused(
        TailnetMode::App,
        Some("100.80.1.1"),
        "100.64.0.2",
        Ok(&status),
        "a hotspot neighbour, arriving on this Mac's hotspot address",
    );
    refused(
        TailnetMode::App,
        Some("192.168.1.5"),
        "100.64.0.2",
        Ok(&status),
        "arriving on a LAN address",
    );
    refused(
        TailnetMode::App,
        None,
        "100.64.0.2",
        Ok(&status),
        "no accepted address known",
    );
    refused(
        TailnetMode::Off,
        on_me,
        "100.64.0.2",
        Ok(&status),
        "mode off",
    );
    refused(
        TailnetMode::Node,
        on_me,
        "100.64.0.2",
        Ok(&status),
        "mode node",
    );
    refused(
        TailnetMode::App,
        on_me,
        "100.64.0.2",
        Err(ReadError::TimedOut),
        "a failed read",
    );
    refused(
        TailnetMode::App,
        on_me,
        "100.64.0.2",
        Err(ReadError::NotInstalled),
        "no app",
    );

    for pattern in [noise::Handshake::Return, noise::Handshake::Enrol] {
        assert_eq!(
            listener::own_mac_admission(
                TailnetMode::App,
                on_me.map(ip),
                ip("100.64.0.2"),
                pattern,
                Ok(&status)
            ),
            listener::InternetAdmission::Refuse,
            "Return and Enrol are internet_admission's to answer, and it already does"
        );
        assert_eq!(
            listener::internet_admission(ip("0.0.0.0"), ip("100.64.0.5"), pattern, false, &[]),
            listener::InternetAdmission::Answer,
            "unchanged: a {} from any tailnet address reaches its own check",
            pattern.pattern()
        );
    }
}
