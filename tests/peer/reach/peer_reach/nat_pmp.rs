//! NAT-PMP against a fake gateway: the three opcodes, refusals, the route table
//! and the IPv6 probe, then a router with no NAT-PMP service, the retry ladder
//! and what a keeper says about it.

use super::*;

/// **The gate for item 1**: a fake responder answers the three opcodes, and the
/// client reads the GATEWAY's numbers back out of each one.
///
/// The external port the fake grants (`41_234`) is not the internal port asked
/// for (`3_456`), so an implementation that echoed its own request would fail
/// here rather than pass and advertise a dead port.
#[test]
fn the_three_opcodes_round_trip_against_a_fake_gateway() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let client = NatPmp::at(fake.addr);

    let external = client
        .external_address()
        .expect("opcode 0 against a cooperative fake");
    assert_eq!(
        external.addr,
        Ipv4Addr::new(203, 0, 113, 7),
        "opcode 0 must report the gateway's external address"
    );
    assert_eq!(
        external.epoch_secs, 7_777,
        "bytes 4..8 are the epoch counter"
    );

    for protocol in [MapProtocol::Udp, MapProtocol::Tcp] {
        let mapping = client
            .map(protocol, 3_456, 3_456, 600)
            .unwrap_or_else(|err| panic!("mapping {} failed: {err}", protocol.label()));
        assert_eq!(mapping.protocol, protocol);
        assert_eq!(mapping.internal_port, 3_456);
        assert_eq!(
            mapping.external_port, 41_234,
            "the external port must come from the gateway, not from the request"
        );
        assert_eq!(mapping.lifetime_secs, 600);

        let renewed = client
            .renew(&mapping, 1_200)
            .unwrap_or_else(|err| panic!("renewing {} failed: {err}", protocol.label()));
        assert_eq!(
            renewed.external_port, mapping.external_port,
            "a renewal must not move the port every peer was told about"
        );
        assert_eq!(renewed.lifetime_secs, 1_200);

        let deleted = client
            .delete(protocol, 3_456)
            .unwrap_or_else(|err| panic!("deleting {} failed: {err}", protocol.label()));
        assert_eq!(deleted.external_port, 0);
        assert_eq!(deleted.lifetime_secs, 0);
    }

    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        7,
        "one request per call and no retries: one opcode 0, then map, renew and \
         delete for each of the two protocols"
    );
}

/// A refusal is a value. **The normal outcome on most routers**, so the one
/// thing it must never be is a panic or an `unwrap` on a zeroed field.
#[test]
fn a_refusal_is_an_error_value_naming_the_code() {
    let fake = FakeGateway::start(Behaviour::Refuse(2));
    let client = NatPmp::at(fake.addr);

    let refusal = client
        .external_address()
        .expect_err("a result code of 2 is not a success");
    assert!(
        matches!(refusal, ReachError::Refused(ResultCode::NotAuthorized)),
        "expected a typed NotAuthorized refusal, got {refusal:?}"
    );
    assert!(
        refusal.to_string().contains("port mapping is off"),
        "the refusal must say what an operator should change: {refusal}"
    );

    let unknown = NatPmp::at(FakeGateway::start(Behaviour::Refuse(9)).addr)
        .map(MapProtocol::Tcp, 1, 1, 60)
        .expect_err("a result code of 9 is not a success");
    assert!(
        matches!(unknown, ReachError::Refused(ResultCode::Unknown(9))),
        "an unregistered code is carried, not flattened: {unknown:?}"
    );
}

/// A silent gateway costs three sends with the RFC's doubling backoff, then
/// yields [`ReachError::Silent`].
///
/// Both halves matter. The count proves the retry loop ran rather than one send
/// happening to time out, and the elapsed floor proves the timeout DOUBLES: a
/// loop that kept 250 ms for all three tries would finish in 0.75 s and pass a
/// count-only assertion.
#[test]
fn a_silent_gateway_costs_three_tries_with_a_doubling_timeout() {
    let fake = FakeGateway::start(Behaviour::Silent);
    let client = NatPmp::at(fake.addr);

    let started = Instant::now();
    let outcome = client
        .external_address()
        .expect_err("a gateway that never answers cannot succeed");
    let elapsed = started.elapsed();

    match outcome {
        ReachError::Silent { gateway, tries } => {
            assert_eq!(gateway, fake.addr);
            assert_eq!(tries, TRIES, "RFC 6886 § 3.1, bounded at three sends here");
        }
        other => panic!("expected a Silent gateway, got {other:?}"),
    }
    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        TRIES,
        "the gateway must have been asked exactly three times"
    );
    assert!(
        elapsed >= Duration::from_millis(1_750),
        "250 + 500 + 1000 ms is the RFC's backoff; this took {elapsed:?}"
    );
}

/// Every way a reply can be well-formed enough to read and still be the wrong
/// reply. Driven through the pure header check, so each shape is one line.
#[test]
fn a_reply_for_something_else_is_refused_rather_than_read() {
    // A mapping response for the opcode that was actually sent: the control.
    let good = vec![
        VERSION, 0x81, 0, 0, 0, 0, 0, 1, 0x0d, 0x80, 0xa1, 0x12, 0, 0, 2, 0x58,
    ];
    reach::interpret(&good, 1, 16).expect("the positive control must pass");

    let short = good[..12].to_vec();
    assert!(
        matches!(
            reach::interpret(&short, 1, 16),
            Err(ReachError::Malformed(_))
        ),
        "a 12-byte mapping response is short by four"
    );

    let mut wrong_version = good.clone();
    wrong_version[0] = 1;
    assert!(
        matches!(
            reach::interpret(&wrong_version, 1, 16),
            Err(ReachError::Malformed(_))
        ),
        "version 1 does not define this layout"
    );

    // The answer to opcode 2 arriving where opcode 1's answer was expected,
    // which is what a late reply to an earlier request looks like.
    assert!(
        matches!(
            reach::interpret(&good, 2, 16),
            Err(ReachError::Malformed(_))
        ),
        "an answer carrying another opcode must not be read as this one's"
    );
}

/// The gateway is found from the route table, and the route table has three
/// shapes worth pinning.
///
/// The `route -n get default` case with NO `gateway:` line is the measured one:
/// on a Mac with a VPN up, the kernel's default route is a point-to-point
/// `utun` interface and carries no gateway address, so a client that only read
/// this tool would find nothing on a machine that has a perfectly good NAT
/// router on `en0`.
#[test]
fn the_route_table_parsers_find_the_gateway_and_skip_the_ones_that_are_not_one() {
    let with_gateway = "   route to: default\ndestination: default\n       mask: default\n \
                        gateway: 192.168.1.1\n  interface: en0\n";
    assert_eq!(
        reach::parse_route_get(with_gateway),
        Some(Ipv4Addr::new(192, 168, 1, 1))
    );

    let point_to_point =
        "   route to: default\ndestination: default\n       mask: default\n  interface: utun4\n";
    assert_eq!(
        reach::parse_route_get(point_to_point),
        None,
        "a point-to-point default route has no gateway address to find"
    );

    let netstat = "Routing tables\n\nInternet:\nDestination        Gateway            Flags\n\
                   default            link#24            UCSg\n\
                   default            192.168.1.1        UGScIg\n";
    assert_eq!(
        reach::parse_netstat_inet(netstat),
        Some(Ipv4Addr::new(192, 168, 1, 1)),
        "the link# row is not an address a UDP request can be sent to"
    );

    // Linux. The hex is in the HOST's byte order, so 0101A8C0 is 192.168.1.1;
    // read big-endian it would be 1.1.168.192, an address that looks fine and
    // points nowhere.
    let proc_route = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                      eth0\t0000FEA9\t00000000\t0001\t0\t0\t1000\t0000FFFF\n\
                      eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n";
    assert_eq!(
        reach::parse_proc_net_route(proc_route),
        Some(Ipv4Addr::new(192, 168, 1, 1))
    );
    assert_eq!(
        reach::parse_proc_net_route(
            "Iface\tDestination\tGateway\n\
                                     eth0\t0000FEA9\t00000000\t0001\n"
        ),
        None,
        "a table with no default route names no gateway"
    );
}

/// The port number is the RFC's, stated once.
#[test]
fn the_gateway_port_is_the_registered_one() {
    assert_eq!(GATEWAY_PORT, 5351, "RFC 6886 § 3");
    assert_eq!(VERSION, 0, "the only version RFC 6886 defines");
}

/// **The gate for item 2, first half**: which IPv6 addresses count as reachable
/// from the open internet.
///
/// A table rather than a range check, and the rejected rows are the point. The
/// one that matters most is `fd7a:115c:a1e0::/48`: that shape is what a VPN
/// hands this Mac, it looks like a global address, it answers a bind, and no
/// other machine on the internet can reach it. A filter written as "starts with
/// 2 or 3" would pass every row here by accident and still be wrong, which is
/// why the accepted rows include `3fff::`, inside global unicast but outside
/// `2000::/4`.
#[test]
fn only_a_globally_routable_v6_address_counts_as_one() {
    let reachable = [
        "2606:4700:4700::1111",
        "2a00:1450:4001:82f::200e",
        "3fff:1:2::5",
    ];
    for text in reachable {
        let addr: Ipv6Addr = text.parse().expect("a test address must parse");
        assert!(
            reach::is_global_v6(&addr),
            "{text} is global unicast and must be reported"
        );
    }

    let not_reachable = [
        ("::", "unspecified"),
        ("::1", "loopback"),
        ("ff02::1", "multicast"),
        (
            "fe80::1",
            "link-local, and its scope cannot be sent to a peer",
        ),
        (
            "fd7a:115c:a1e0::1",
            "unique-local: the VPN shape, unreachable from off the box",
        ),
        ("fc00::1", "unique-local, the other half of fc00::/7"),
        ("2001:db8::1", "reserved for documentation"),
        ("::ffff:192.0.2.1", "an IPv4 address wearing a v6 shape"),
        ("100::1", "the discard-only prefix"),
    ];
    for (text, why) in not_reachable {
        let addr: Ipv6Addr = text.parse().expect("a test address must parse");
        assert!(
            !reach::is_global_v6(&addr),
            "{text} must be rejected ({why})"
        );
    }
}

/// **The gate for item 2, second half**: the probe answers, and every address it
/// answers with survives the filter above.
///
/// An empty list is a pass, and on this machine it is the real answer: its only
/// non-link-local v6 addresses are two unique-local ones from VPN interfaces.
/// So the assertion is the one that holds either way, nothing unreachable may
/// appear in the list, plus the property that the probe is pure enough to give
/// the same answer twice.
#[test]
fn the_v6_probe_reports_only_addresses_that_pass_the_filter() {
    let found = reach::global_v6_addresses();
    for addr in &found {
        assert!(
            reach::is_global_v6(addr),
            "the probe reported {addr}, which its own filter rejects"
        );
    }
    assert_eq!(
        found,
        reach::global_v6_addresses(),
        "two probes one after the other must agree"
    );
    println!("global v6 addresses on this machine: {found:?}");
}

// ---------------------------------------------------------------------------
// A router with no NAT-PMP service on it
// ---------------------------------------------------------------------------

/// An address on loopback with nothing bound on it, so a datagram sent there is
/// refused with an ICMP port unreachable.
///
/// A port is bound and released rather than picked out of the air, because a
/// number nobody has ever bound is a number some other test in this binary may
/// bind while this one runs, and then the "refusal" would be a fake gateway's
/// silence.
fn refusing_gateway() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind a port to learn a free one");
    let addr = socket.local_addr().expect("its own address");
    drop(socket);
    addr
}

/// **A refused probe is the ROUTER's answer, not a fault of this Mac's
/// socket.**
///
/// Measured on a real router: `tcr peer reach` printed `external-address:
/// unavailable: peer reach: the local UDP socket failed: Connection refused
/// (os error 61)`. The gateway answers the port 5351 probe with an ICMP port
/// unreachable, which says only that nothing there speaks NAT-PMP, and the one
/// thing the operator was told was that their own socket had failed. Worse,
/// `MappingKeeper::map` asks UPnP about answers that mean "this router may not
/// speak NAT-PMP", and a socket fault is not one of them, so a router with UPnP
/// on it was never asked over UPnP at all.
///
/// Watch it fail: return `ReachError::Socket(err)` from `NatPmp::probe_error`
/// for every error kind, and this reads
/// `the local UDP socket failed: Connection refused`, with
/// `no_natpmp_service()` false.
#[test]
fn a_refused_natpmp_probe_names_the_router_and_not_the_local_socket() {
    let gateway = refusing_gateway();

    let failure = NatPmp::at(gateway)
        .external_address()
        .expect_err("nothing is bound on that port, so the probe has to be refused");

    assert!(
        matches!(failure, ReachError::NoNatPmp { .. }),
        "a refused probe is an answer about the router, and every caller that routes on the \
         error reads the variant: {failure:?}"
    );
    assert!(
        failure.no_natpmp_service(),
        "and it means the same thing as silence to the UPnP fallback, or the one router that \
         needs the fallback is the one that never gets it: {failure:?}"
    );
    let said = failure.to_string();
    assert!(
        said.contains("refused") && said.contains(&gateway.to_string()),
        "the words an operator reads have to say who refused and that it was a refusal: {said}"
    );
    assert!(
        !said.contains("the local UDP socket failed"),
        "and must not blame this Mac's socket for what the router did: {said}"
    );
}

/// **A gateway that ANSWERS and says no is still not asked over UPnP.**
///
/// The positive control for the widening above: `no_natpmp_service` has to
/// separate "there is no service there" from "the service there refused", or
/// the fallback routes around a decision the router made on purpose.
#[test]
fn a_result_code_refusal_is_not_a_missing_natpmp_service() {
    let refused = ReachError::Refused(ResultCode::NotAuthorized);
    assert!(
        !refused.no_natpmp_service(),
        "a gateway that read the request and said no has decided, and a second protocol is not \
         this node's answer to that"
    );
    let malformed = ReachError::Malformed("not a NAT-PMP response".to_string());
    assert!(
        !malformed.no_natpmp_service(),
        "and an unreadable answer is still an answer from something that is listening"
    );
}

/// **The retry ladder is 5 s, 30 s, 2 min, 10 min, and then 10 min forever.**
///
/// Asserted as the sequence of intervals rather than by sleeping through them:
/// the ladder is a pure function of how many refusals are behind it, which is
/// the reason [`reach::MappingRetry`] takes its elapsed time as an argument.
///
/// Watch it fail: make `MappingRetry::interval` return
/// `MAPPING_RETRY_LADDER[0]` always, and the sequence reads 5 s four times.
#[test]
fn the_retry_ladder_grows_and_then_holds_at_its_cap() {
    use reach::{MappingAnswer, MappingRetry};

    let mut retry = MappingRetry::new();
    assert_eq!(
        retry.interval(),
        Duration::ZERO,
        "nothing has been refused yet, so the first request goes out at once"
    );

    let mut rungs = Vec::new();
    for _ in 0..6 {
        retry.answered(MappingAnswer::Refused);
        rungs.push(retry.interval());
    }
    assert_eq!(
        rungs,
        vec![
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(120),
            Duration::from_secs(600),
            Duration::from_secs(600),
            Duration::from_secs(600),
        ],
        "a router that keeps saying no is asked less and less often, and then at the cap, so a \
         router replaced tomorrow is still noticed within ten minutes"
    );

    // The wait itself: due only once the rung has elapsed.
    let mut retry = MappingRetry::new();
    retry.answered(MappingAnswer::Refused);
    retry.asked();
    retry.tick(Duration::from_secs(4));
    assert!(
        !retry.due(),
        "four seconds into a five-second rung, the router is left alone"
    );
    retry.tick(Duration::from_secs(1));
    assert!(retry.due(), "and at five seconds it may be asked again");

    // A mapping puts the ladder back where it started.
    retry.answered(MappingAnswer::Mapped);
    assert_eq!(
        retry.interval(),
        Duration::ZERO,
        "a router that mapped is not a router on a ladder: the next refusal, weeks from now, \
         waits five seconds and not ten minutes"
    );
}

/// **A Mac carried to another router asks it straight away.**
///
/// The ladder is about ONE router's answer. Waiting out ten minutes built up
/// behind a router in another building is a Mac that is unreachable for ten
/// minutes for no reason.
///
/// Watch it fail: drop the `self.reset()` from `gateway_is_now`, and the
/// interval after the move is still the cap.
#[test]
fn a_different_default_gateway_puts_the_ladder_back_on_its_first_rung() {
    use reach::{MappingAnswer, MappingRetry};

    let old: Ipv4Addr = "192.0.2.1".parse().expect("a literal address");
    let new: Ipv4Addr = "198.51.100.1".parse().expect("a literal address");

    let mut retry = MappingRetry::new();
    assert!(
        !retry.gateway_read_due(),
        "a node that is being answered has no ladder to reset, so its route table is not read \
         at all"
    );
    for _ in 0..4 {
        retry.answered(MappingAnswer::Refused);
    }
    assert_eq!(retry.interval(), Duration::from_secs(600));

    assert!(
        !retry.gateway_is_now(Some(old)),
        "the first reading is not a move: there was nothing to have moved from"
    );
    assert!(
        !retry.gateway_is_now(Some(old)),
        "and the same router twice is the same router"
    );
    assert_eq!(
        retry.interval(),
        Duration::from_secs(600),
        "so the ladder this Mac built behind that router still stands"
    );

    assert!(
        retry.gateway_is_now(Some(new)),
        "another gateway address is another router"
    );
    assert_eq!(
        retry.interval(),
        Duration::ZERO,
        "and the new one is asked on the next wake, not in ten minutes"
    );

    // The route table is read on a clock of its own, since reading it runs a
    // route-table tool.
    let mut retry = MappingRetry::new();
    retry.answered(MappingAnswer::Refused);
    retry.tick(
        reach::GATEWAY_RECHECK_INTERVAL
            .checked_sub(Duration::from_secs(1))
            .unwrap(),
    );
    assert!(!retry.gateway_read_due());
    retry.tick(Duration::from_secs(1));
    assert!(
        retry.gateway_read_due(),
        "and after the recheck interval it is read once, which resets that clock"
    );
    retry.gateway_is_now(None);
    assert!(!retry.gateway_read_due());
}

/// **The one sentence `tcr peer reach` prints when nothing will map a port.**
///
/// The operator's evening ended with a `reach` readout that said what had
/// failed and nothing about what was left to do. Two things are: forward the
/// port by hand, or put both Macs on one private network.
///
/// Watch it fail: return `None` unconditionally from `reach_no_mapping_line`.
#[test]
fn reach_names_the_two_ways_left_when_no_protocol_will_map() {
    let refused = ReachError::NoNatPmp {
        gateway: "192.0.2.1:5351".parse().expect("a literal address"),
        source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
    };

    let line = reach::reach_no_mapping_line(&refused, Some(7_755))
        .expect("a router with no mapping service on it is exactly what this line is for");
    assert!(
        line.starts_with("reach: no-mapping: "),
        "in the shape the rest of the verb prints, so it greps with them: {line}"
    );
    assert!(
        line.contains("tcp port 7755"),
        "naming the port to forward, since the operator has to type it into a router page: \
         {line}"
    );
    assert!(
        line.contains("by hand") && line.contains("private network"),
        "and both ways left, or this is a line that says only that something failed: {line}"
    );

    let silent = ReachError::Silent {
        gateway: "192.0.2.1:5351".parse().expect("a literal address"),
        tries: TRIES,
    };
    assert!(
        reach::reach_no_mapping_line(&silent, None)
            .is_some_and(|line| line.contains("the peer listener's port")),
        "a Mac with no listener configured still gets the advice, without inventing a port for \
         it"
    );

    assert!(
        reach::reach_no_mapping_line(&ReachError::Refused(ResultCode::NotAuthorized), Some(7_755))
            .is_none(),
        "but a device that ANSWERED and was refused by name is a different problem, and the \
         thing to do about it is read the name"
    );
}

/// **A keeper's answer is one line when it changes and a quiet one when it
/// repeats.**
///
/// The live log had 169 `would not map` warnings in a day, one every five
/// seconds, all of them the same sentence about the same router.
///
/// Driven at the decision rather than at the two log sites, which are three
/// hundred lines apart and one of them needs a real router to reach.
///
/// Watch it fail: have `mapping_answer_voice` answer `News` unconditionally.
#[test]
fn the_same_answer_from_one_router_is_said_once() {
    use reach::{AnswerVoice, MappingAnswer};

    // Its own address, because the record is per gateway and per process and
    // the other tests in this binary share the process.
    let gateway: SocketAddr = "192.0.2.61:5351".parse().expect("a literal address");

    assert_eq!(
        reach::mapping_answer_voice(gateway, MappingAnswer::Refused),
        AnswerVoice::News,
        "the first refusal from a router is a thing the operator has not been told"
    );
    assert_eq!(
        reach::mapping_answer_voice(gateway, MappingAnswer::Refused),
        AnswerVoice::Repeat,
        "the second is the same sentence about the same router"
    );
    assert_eq!(
        reach::mapping_answer_voice(gateway, MappingAnswer::Mapped),
        AnswerVoice::News,
        "a router that was refusing and now maps is news: what they were told last is no \
         longer true"
    );
    assert_eq!(
        reach::mapping_answer_voice(gateway, MappingAnswer::Mapped),
        AnswerVoice::Repeat,
        "and an ordinary mapping is the ordinary record of a keeper starting"
    );

    let other: SocketAddr = "192.0.2.62:5351".parse().expect("a literal address");
    assert_eq!(
        reach::mapping_answer_voice(other, MappingAnswer::Refused),
        AnswerVoice::News,
        "another router's first answer is its own first line"
    );
}

/// **The reverse-carry decision is the two facts and whether carriers are
/// parked**, with no router and no friend.
#[test]
fn the_reverse_carry_decision_parks_releases_or_does_nothing() {
    use tcr::peer::tunnel::{ReverseNeed, ReverseNotWanted, ReverseStep, reverse_step};

    let dialable = ReverseNeed::NotWanted(ReverseNotWanted::HoldsMapping);
    assert_eq!(reverse_step(ReverseNeed::Wanted, false), ReverseStep::Park);
    assert_eq!(reverse_step(dialable, true), ReverseStep::Release);
    assert_eq!(reverse_step(ReverseNeed::Wanted, true), ReverseStep::Idle);
    assert_eq!(reverse_step(dialable, false), ReverseStep::Idle);
}

/// **A Mac whose router answers a second late lets its friends go.**
///
/// The decision was taken once, two statements after the mapping keeper thread
/// was spawned and before its first NAT-PMP round trip could land. So
/// `external_socket()` was `None` on every boot and a Mac its router would map
/// perfectly well parked `MAX_PARKED_PER_PEER` carriers at every friend it
/// had, seconds before it became dialable, and kept them there until it
/// restarted.
///
/// The router here is the fake on loopback, answering after 600 ms, and a real
/// `MappingKeeper` asks it. The IPv6 half of the decision is held at "none" on
/// purpose: this test is about the mapping arriving late, and the machine
/// running the suite may have a global address of its own.
///
/// Watch it fail: make `watch_reverse_need` ask `need_now` once before its
/// loop instead of on every wake, and the carriers are never released.
#[tokio::test]
async fn reverse_carriers_are_released_when_the_router_answers_late() {
    use tcr::peer::tunnel;

    let fake = FakeGateway::start(Behaviour::SlowCooperative {
        external_port: 41_235,
        after: Duration::from_millis(600),
    });
    let mapped: Arc<Mutex<Option<reach::Mapping>>> = Arc::new(Mutex::new(None));

    let writing = Arc::clone(&mapped);
    let gateway = fake.addr;
    let keeper = std::thread::spawn(move || {
        let mut keeper = reach::MappingKeeper::new(NatPmp::at(gateway), 7_755, 600);
        let mapping = keeper.map().expect("the cooperative fake grants a mapping");
        *writing.lock().expect("the mapping this keeper holds") = Some(mapping);
    });

    // The control: nothing is mapped yet, so the first wake has to park.
    assert!(
        mapped.lock().expect("the mapping cell").is_none(),
        "positive control: this Mac must start with no mapping, or the park below is about a \
         router that had already answered"
    );

    let reading = Arc::clone(&mapped);
    let watched = tunnel::watch_reverse_need(
        Duration::from_millis(50),
        Some(60),
        move || {
            tunnel::reverse_carry_is_wanted(reading.lock().expect("the mapping cell").as_ref(), &[])
        },
        || Some(()),
        |()| {},
        None,
    )
    .await;

    keeper.join().expect("the keeper thread finishes");
    assert_eq!(
        (watched.parked, watched.released),
        (1, 1),
        "a Mac that cannot be dialled parks carriers at its friends, and the moment its \
         router answers it has to let them go: {watched:?}"
    );
    assert!(
        fake.requests.load(Ordering::SeqCst) > 0,
        "positive control: the fake router really was asked"
    );
}
