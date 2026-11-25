use std::net::{IpAddr, Ipv6Addr};

use anyhow::Result;

use crate::peer::config::{self as config, PeerFile};
use crate::peer::noise::Handshake;
use crate::peer::reach::{is_carrier_grade_nat, is_lan_scope, is_tailscale_v6};

// ---------------------------------------------------------------------------
// The accept gate `peer.internet` turns on
// ---------------------------------------------------------------------------

/// What the accept gate says about one connection, before a byte is written.
///
/// A typed answer rather than a `bool`, because the two outcomes are not
/// symmetric: [`Self::Refuse`] means the connection is closed with NOTHING
/// written, which is a different act from letting the handshake decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetAdmission {
    /// Carry on to the pattern's own admission, unchanged.
    Answer,
    /// Close with zero bytes written.
    Refuse,
}

/// Whether a connection to `bind`, from `from`, offering `pattern`, is
/// answered at all.
///
/// The rule is: "over the internet only the `IK` handshake against a
/// pinned key is answered, never a knock". The bind decides whether that row
/// applies at all: `bind` is the address THIS listener is bound to, and a
/// source outside LAN scope only meets row 14 when the listener itself can be
/// reached from outside the LAN. On a bind that is not loopback a knock
/// (`NN`, `NNpsk0`) and a first pairing (`XX`) from off the LAN get nothing
/// back, and `IK` proceeds to the pin check that has always decided it. A
/// stranger on the internet therefore cannot reach the operator's pairing
/// queue at all, and the one thing it can reach refuses every key that is not
/// already pinned.
///
/// # Only loopback short-circuits, and a private bind does not
///
/// This test used to be `is_lan_scope(bind)`, on the reasoning that "nothing
/// off the LAN can reach a private bind without a mapping this node did not
/// make". This node makes exactly that mapping:
/// [`crate::peer::reach::mapping_boot`] asks the router for one on ANY
/// non-loopback bind, so `listen: 10.0.0.5:7755` with `internet on` is a
/// world-reachable socket and the short-circuit answered a knock and a first
/// pairing from the internet on it. Loopback is the one bind class that
/// carries no such mapping and cannot acquire one, so it is the only one that
/// keeps the short-circuit.
///
/// # The switch is read for ONE case, and it is the narrow one
///
/// `peer.internet` decides whether THIS node asks its own router for a mapping
/// and dials another Mac's public address
/// ([`crate::peer::reach::spawn_mapping_keeper`], read where `serve` starts
/// it). It does not decide whether a stranger's knock, arriving at a socket
/// this node chose to bind wide open, gets an answer: `listen: 0.0.0.0:7755`
/// with `peer.internet: false` is still a world-reachable socket, and reading
/// the switch as permission answered it anyway. So the bind class carries row
/// 14, and the switch narrows ONE case the bind class alone got wrong.
///
/// That case is a home LAN with ISP-delegated IPv6. Both Macs hold `2001:...`
/// addresses out of one `/64`, both bind `[::]`, and neither address is LAN
/// scope by [`is_lan_scope`] (they are globally routable, and correctly so).
/// Row 14 then refused the FIRST PAIRING and the knock between two Macs
/// sitting on one desk, which is not what row 14 is for and is a regression
/// against the same pair on IPv4.
///
/// The narrowing: with the switch OFF, a source that shares one of this host's
/// own global `/64` prefixes is answered like any LAN neighbour.
/// `own_global_v6` is this host's own globally routable addresses, and sharing
/// a `/64` with one of them means the source sits on the same delegated
/// prefix, which on a home network means the same link. It is not proof of
/// anything (a `/64` may be wider than one link, and nothing here is
/// authentication), which is why it only reopens the pattern's OWN admission,
/// the operator's pairing window and the knock bucket, exactly as on the LAN.
///
/// With the switch ON the operator has asked to be reachable from the
/// internet, the mapping is live, and row 14 applies in full: no knock, no
/// first pairing, whatever prefix the source is in.
///
/// A source inside LAN scope is always answered, whatever `bind` is: nothing
/// about row 14 restricts this node from its own network.
pub fn internet_admission(
    bind: IpAddr,
    from: IpAddr,
    pattern: Handshake,
    internet: bool,
    own_global_v6: &[Ipv6Addr],
) -> InternetAdmission {
    if is_lan_scope(from) || is_loopback_bind(bind) {
        return InternetAdmission::Answer;
    }
    match pattern {
        // `IK` and `IKpsk1`: a pinned key, or a join token minted here. Both
        // prove something this node issued before a stream exists.
        Handshake::Return | Handshake::Enrol => InternetAdmission::Answer,
        // `XX` proves a key nobody has pinned yet, and `NN`/`NNpsk0` prove
        // nothing at all.
        Handshake::Pair | Handshake::Knock | Handshake::KnockPsk => {
            if !internet && shares_a_global_v6_prefix(from, own_global_v6) {
                InternetAdmission::Answer
            } else {
                InternetAdmission::Refuse
            }
        }
    }
}

/// Whether `addr` is a tailnet address in either family: `100.64.0.0/10` or
/// `fd7a:115c:a1e0::/48`, a v4-mapped form read as its IPv4 self.
fn is_tailnet_address(addr: IpAddr) -> bool {
    match addr.to_canonical() {
        IpAddr::V4(v4) => is_carrier_grade_nat(v4),
        IpAddr::V6(v6) => is_tailscale_v6(v6),
    }
}

/// The cheap half of [`own_mac_admission`]: everything it asks that needs no
/// reading of the Tailscale app. Checked first, so a connection that could
/// never be an own-Mac knock (a LAN host, a hotspot neighbour arriving on a
/// non-tailnet address, any mode but `app`) never starts the Tailscale CLI.
fn may_be_an_own_mac_knock(
    mode: config::TailnetMode,
    accepted_on: Option<IpAddr>,
    from: IpAddr,
    pattern: Handshake,
) -> bool {
    matches!(
        pattern,
        Handshake::Pair | Handshake::Knock | Handshake::KnockPsk
    ) && mode == config::TailnetMode::App
        && accepted_on.is_some_and(is_tailnet_address)
        && is_tailnet_address(from)
}

/// A knock or a first pairing from one of the operator's own Macs over the
/// tailnet, which [`internet_admission`] refuses: answered when ALL hold.
///
/// 1. The tailnet mode is `app`.
/// 2. The connection arrived on one of this Mac's own tailnet addresses
///    (`Self.TailscaleIPs` in `reading`). `accepted_on` is the accepted
///    socket's OWN local address, the one the packet was sent to, never the
///    listener's `0.0.0.0` bind. That rules out a hotspot neighbour whose
///    carrier-grade NAT address falls in `100.64.0.0/10`: its packet comes in
///    on Wi-Fi, to this Mac's Wi-Fi address.
/// 3. The source is an address of one of the operator's own Macs in
///    `reading` ([`crate::peer::tailscale_app::own_macs`]: online, untagged,
///    not shared in, the same Tailscale user, macOS or no OS). WireGuard drops
///    a packet whose source is not the sending peer's own address, so another
///    machine on the tailnet cannot borrow it.
///
/// `reading` is a failed read when the app could not be read, and a failed
/// read refuses. Return and Enrol are not decided here: [`internet_admission`]
/// already answers them from anywhere.
pub fn own_mac_admission(
    mode: config::TailnetMode,
    accepted_on: Option<IpAddr>,
    from: IpAddr,
    pattern: Handshake,
    reading: Result<&crate::peer::tailscale_app::Status, crate::peer::tailscale_app::ReadError>,
) -> InternetAdmission {
    if !may_be_an_own_mac_knock(mode, accepted_on, from, pattern) {
        return InternetAdmission::Refuse;
    }
    let (Some(accepted_on), Ok(status)) = (accepted_on, reading) else {
        return InternetAdmission::Refuse;
    };
    let accepted_on = accepted_on.to_canonical();
    let from = from.to_canonical();
    let on_my_tailnet_address = status
        .self_node
        .as_ref()
        .is_some_and(|me| me.tailscale_ips.contains(&accepted_on));
    let from_my_own_mac = crate::peer::tailscale_app::own_macs(status)
        .iter()
        .any(|mac| mac.tailscale_ips.contains(&from));
    if on_my_tailnet_address && from_my_own_mac {
        InternetAdmission::Answer
    } else {
        InternetAdmission::Refuse
    }
}

/// Is this listener bound to an address nothing off this machine can dial?
///
/// The one bind class [`internet_admission`] answers unconditionally. An
/// IPv4-mapped form is unwrapped first, because a dual-stack listener reports
/// `::ffff:127.0.0.1` for a socket bound to `127.0.0.1` and reading that as a
/// global address would refuse the machine's own traffic.
const fn is_loopback_bind(bind: IpAddr) -> bool {
    match bind {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}

/// Does `from` sit in the same global `/64` as one of this host's own
/// addresses?
///
/// # Why `/64` and not the whole address
///
/// A `/64` is the unit an ISP delegates and the unit SLAAC numbers hosts
/// inside, so "the same `/64` as one of mine" is the closest thing IPv6 has to
/// "the same home network" that a host can answer without asking the router.
/// The host part is the one thing it must NOT compare: every Mac on the link
/// has its own, and a privacy extension changes it by the hour.
///
/// Only global addresses on both sides. A link-local or unique-local source is
/// already LAN scope and never reaches here; an IPv4-mapped source is the IPv4
/// question and is answered before this. An empty `own_global_v6` (a Mac with
/// no global v6 at all, which is the common case measured on this machine)
/// makes this `false` for every source, which leaves row 14 exactly as it was.
pub fn shares_a_global_v6_prefix(from: IpAddr, own_global_v6: &[Ipv6Addr]) -> bool {
    let IpAddr::V6(from) = from else {
        return false;
    };
    if from.to_ipv4_mapped().is_some() {
        return false;
    }
    let prefix = &from.segments()[..4];
    own_global_v6
        .iter()
        .any(|own| own.segments()[..4] == *prefix)
}

/// [`internet_admission`], with this host's own global IPv6 addresses read off
/// the kernel.
///
/// The read costs two route lookups ([`crate::peer::reach::global_v6_addresses`]
/// binds a UDP socket and connects it, which sends nothing), so it is done
/// only when the pure answer would otherwise be a refusal. That is the rare
/// path by construction: every LAN connection, and every `IK` from anywhere,
/// has already been answered by the time this asks.
///
/// Read per connection rather than cached at boot for the reason the peers
/// file is: a Mac that moves between networks gets a new prefix, and a cached
/// one would answer about the network it used to be on.
///
/// Then, for what is still refused, [`own_mac_admission`]: a knock from one
/// of the operator's own Macs over the tailnet in app mode, decided on a
/// reading of the Tailscale app no older than
/// [`crate::peer::tailnet_find::OWN_MAC_READING_MAX_AGE`] from `readings`,
/// re-read with `read` when it is older. The read happens only after
/// [`may_be_an_own_mac_knock`] says it could matter.
pub(super) async fn admission_for_this_host<F>(
    bind: IpAddr,
    accepted_on: Option<IpAddr>,
    from: IpAddr,
    pattern: Handshake,
    file: &PeerFile,
    readings: &crate::peer::tailnet_find::ReadingCache,
    read: F,
) -> InternetAdmission
where
    F: FnOnce() -> std::result::Result<
            crate::peer::tailscale_app::Status,
            crate::peer::tailscale_app::ReadError,
        > + Send
        + 'static,
{
    let internet = file.internet;
    if internet_admission(bind, from, pattern, internet, &[]) == InternetAdmission::Answer {
        return InternetAdmission::Answer;
    }
    if internet_admission(
        bind,
        from,
        pattern,
        internet,
        &crate::peer::reach::global_v6_addresses(),
    ) == InternetAdmission::Answer
    {
        return InternetAdmission::Answer;
    }
    if !may_be_an_own_mac_knock(file.tailnet_mode(), accepted_on, from, pattern) {
        return InternetAdmission::Refuse;
    }
    let reading = readings
        .recent(crate::peer::tailnet_find::OWN_MAC_READING_MAX_AGE, read)
        .await;
    own_mac_admission(
        file.tailnet_mode(),
        accepted_on,
        from,
        pattern,
        reading.as_deref().map_err(|err| *err),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::peer::config::{PeerFile, TailnetMode};
    use crate::peer::tailnet_find::ReadingCache;
    use crate::peer::tailscale_app::{ReadError, Status};

    const STATUS: &str = r#"{
      "BackendState": "Running",
      "Self": {"UserID": 111, "TailscaleIPs": ["100.64.0.1"]},
      "Peer": {"nodekey:01": {"HostName": "laptop-2", "OS": "macOS", "UserID": 111,
        "Online": true, "TailscaleIPs": ["100.64.0.2"]}}
    }"#;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("a literal address parses")
    }

    fn app_mode() -> PeerFile {
        PeerFile {
            tailnet: TailnetMode::App.into(),
            ..Default::default()
        }
    }

    /// A reader that counts its runs and answers `answer`.
    fn counting(
        runs: &Arc<AtomicUsize>,
        answer: Result<(), ReadError>,
    ) -> impl FnOnce() -> Result<Status, ReadError> + Send + 'static {
        let runs = Arc::clone(runs);
        move || {
            runs.fetch_add(1, Ordering::SeqCst);
            answer.and_then(|()| Status::parse(STATUS.as_bytes()))
        }
    }

    /// **The listener answers an own-Mac knock through the cache, reads the
    /// app once for two knocks, never reads it for a connection that could
    /// not be one, and refuses on a failed read.**
    ///
    /// Watch it fail by dropping the `may_be_an_own_mac_knock` early return
    /// in `admission_for_this_host`: the LAN-address knock, which comes first
    /// against an empty cache, then runs the reader and the first count is 1.
    #[tokio::test]
    async fn the_listener_decides_an_own_mac_knock_on_a_recent_reading() {
        let cache = ReadingCache::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let file = app_mode();
        let knock = |accepted_on: &str, from: &str| (Some(ip(accepted_on)), ip(from));

        // First, against an empty cache: a knock that arrived on a LAN
        // address can never be an own-Mac knock, so it must not start the
        // Tailscale CLI at all.
        let (on, from) = knock("192.168.1.5", "100.64.0.2");
        let lan_side = admission_for_this_host(
            ip("0.0.0.0"),
            on,
            from,
            Handshake::Knock,
            &file,
            &cache,
            counting(&runs, Ok(())),
        )
        .await;
        assert_eq!(lan_side, InternetAdmission::Refuse);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "no read for a LAN-side knock"
        );

        let (on, from) = knock("100.64.0.1", "100.64.0.2");
        let first = admission_for_this_host(
            ip("0.0.0.0"),
            on,
            from,
            Handshake::Knock,
            &file,
            &cache,
            counting(&runs, Ok(())),
        )
        .await;
        assert_eq!(first, InternetAdmission::Answer);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "one read so far");

        let (on, from) = knock("100.64.0.1", "100.64.0.2");
        let second = admission_for_this_host(
            ip("0.0.0.0"),
            on,
            from,
            Handshake::Pair,
            &file,
            &cache,
            counting(&runs, Ok(())),
        )
        .await;
        assert_eq!(second, InternetAdmission::Answer);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the reading was reused");

        let failing = ReadingCache::new();
        let refused = admission_for_this_host(
            ip("0.0.0.0"),
            Some(ip("100.64.0.1")),
            ip("100.64.0.2"),
            Handshake::Knock,
            &file,
            &failing,
            counting(&runs, Err(ReadError::TimedOut)),
        )
        .await;
        assert_eq!(refused, InternetAdmission::Refuse, "a failed read refuses");
    }
}
