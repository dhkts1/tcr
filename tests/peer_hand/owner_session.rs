//! The owner's half of the hand mode, over a real session: the owner fixture
//! and the gates that drive a hand grant through a live listener.

use super::*;

// ---------------------------------------------------------------------------
// The OWNER's half, over a real session
// ---------------------------------------------------------------------------

/// The owner's listener, its peers file and its manager, as one fixture.
///
/// Built by hand rather than borrowed from `tests/peer_lease.rs`'s `mesh`
/// module, which is private to that binary: what this file needs and that one
/// does not is a manager holding an account with a bearer, since a `Hand`
/// grant is the one path on which the lender reads its own access token.
struct Owner {
    addr: SocketAddr,
    public: [u8; 32],
    ledger: Arc<Mutex<Ledger>>,
    _home: tempfile::TempDir,
}

/// Stand an owner up: a peers file pinning `borrower` with one `hand` grant on
/// the seven-day window, a manager with one obviously fake account, and the
/// shipped accept loop.
async fn owner_serving(borrower: tcr_peer_wire::PeerId, mode: &str) -> Owner {
    owner_lending(
        borrower,
        &format!(
            r#"{{
                "mode": "{mode}",
                "window": "7d",
                "fraction": 0.2,
                "ttlS": 300,
                "maxInflight": 2
              }}"#
        ),
    )
    .await
}

/// The same owner, with the `lend` value written out in full: one grant, or a
/// JSON array of several, which the lease scope lets an operator hold and
/// what the ended-grant gate below needs.
async fn owner_lending(borrower: tcr_peer_wire::PeerId, lend: &str) -> Owner {
    owner_lending_expiring(borrower, lend, 1_893_456_000_000).await
}

/// [`owner_lending`], with the account's bearer expiry chosen.
///
/// The expiry is what decides whether a renewal is DUE
/// (`lease::handoff_renewal_due` against `HANDOFF_RENEW_LEAD_MS`), so a test
/// about renewals has to set it and every other test wants one far away.
async fn owner_lending_expiring(
    borrower: tcr_peer_wire::PeerId,
    lend: &str,
    expires_at_ms: i64,
) -> Owner {
    use tcr::peer::config::PeerStore;
    use tcr::peer::id::NodeKey;
    use tcr::peer::listener::{self, LeaseServing, SessionContext};

    let home = tempfile::tempdir().expect("a temp home");
    let peers = home.path().join("tcr-peers.json");
    std::fs::write(
        &peers,
        format!(
            r#"{{
          "peers": [
            {{
              "node": "{node}",
              "label": "borrowing-mac",
              "addedAt": 1,
              "allow": {{ "inspect": true }},
              "lend": {lend}
            }}
          ]
        }}"#,
            node = borrower.to_wire(),
        ),
    )
    .expect("write the owner's peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }

    let config: tcr::config::Config = serde_json::from_str(&format!(
        r#"{{
            "proxy": {{ "port": 0 }},
            "upstream": "http://127.0.0.1:1",
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            "accounts": [
                {{
                    "name": "alice@example.com",
                    "accessToken": "{OWNER_FAKE_BEARER}",
                    "expiresAt": {expires_at_ms},
                    "accountUuid": "11111111-1111-1111-1111-111111111111",
                    "orgUuid": "22222222-2222-2222-2222-222222222222"
                }}
            ]
        }}"#
    ))
    .expect("the inline owner config parses");
    let manager = tcr::manager::Manager::with_live_refresher(config, None);
    // THE OWNER'S OWN WINDOW, measured, because a `hand` grant hands a bearer
    // over only for an account whose window this Mac has read: the borrower
    // reports its spend as a rise above that figure, and there is no honest
    // baseline without it. Every lease here is on the seven-day window.
    {
        let mut measured = reqwest::header::HeaderMap::new();
        measured.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            OWNER_MEASURED_UTILIZATION
                .to_string()
                .parse()
                .expect("a header value"),
        );
        manager.update_quota(0, &measured, 200);
    }

    let key = NodeKey::load_or_mint(home.path()).expect("the owner's node key");
    let public = key.id().0;
    let store = PeerStore::open(&peers).expect("the owner's peers file reads");
    let state_path = home.path().join("peer-state.json");
    let ledger = Arc::new(Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        // Headroom the owner has actually noted, or `grant` has nothing to cut
        // a fraction out of and refuses before mode is ever consulted.
        held.note_owner_headroom(Window::SevenDay, 0.50);
    }
    let context = SessionContext::new(&key, store.path(), &state_path).with_lease_serving(Some(
        LeaseServing {
            ledger: Arc::clone(&ledger),
            upstream: "http://127.0.0.1:1".to_string(),
            utilization: Arc::new(tcr::peer::serve::NoFleetUtilization),
            manager,
        },
    ));

    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the owner's peer listener");
    let addr = listening.local_addr().expect("the owner's peer addr");
    tokio::spawn(async move {
        listener::serve_on_with(listening, context)
            .await
            .log_debug("serving a test peer listener");
    });

    Owner {
        addr,
        public,
        ledger,
        _home: home,
    }
}

/// Ask `owner` for one seven-day lease over a fresh `IK` session, and read
/// whatever the owner sends after the grant.
///
/// The second element is `None` when nothing arrived inside a LAN round trip,
/// which is what a serve-mode grant answers: the read waits for the close.
async fn ask_for_a_lease(owner: &Owner, secret: &[u8; 32]) -> (Lease, Option<Control>) {
    let (mut stream, mut session) = ask_owner_for_a_lease(owner, secret).await;
    let minted = read_the_grant(&mut stream, &mut session).await;
    // Whatever comes next, it must not take longer than a LAN round trip: with
    // nothing to hand over, nothing comes at all and this read waits for the
    // close.
    let next = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tcr::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
    )
    .await;
    let next = match next {
        Err(_elapsed) => None,
        Ok(read) => read.ok(),
    };
    (minted, next)
}

/// A borrower that asks through `lease::request_lease`, and the owner it asks.
///
/// The home is a field because the borrower's node key lives in it, beside the
/// peers file: `request_lease` mints the key there, so the id the owner pins is
/// read from it and not invented.
struct HandBorrower {
    _home: tempfile::TempDir,
    store: tcr::peer::config::PeerStore,
    owner: Owner,
}

/// Stand up an owner lending to a fresh borrower in `mode`, and write the
/// borrower's own row for it: `disclose` on, because the borrower's half of the
/// two opt-ins is checked before it asks.
async fn hand_borrower(mode: &str) -> HandBorrower {
    let home = tempfile::tempdir().expect("the borrower's temp home");
    let borrower_peers = home.path().join("tcr-peers.json");
    let borrower = tcr::peer::id::NodeKey::load_or_mint(home.path())
        .expect("the borrower's node key")
        .id();
    let owner = owner_serving(borrower, mode).await;

    std::fs::write(
        &borrower_peers,
        format!(
            r#"{{
      "peers": [
        {{
          "node": "{node}",
          "label": "lending-mac",
          "addedAt": 1,
          "allow": {{ "allowDisclose": true }},
          "endpoints": [
            {{
              "kind": "direct",
              "addr": "{addr}",
              "source": "paired",
              "observedAtMs": 1
            }}
          ]
        }}
      ]
    }}"#,
            node = tcr_peer_wire::PeerId(owner.public).to_wire(),
            addr = owner.addr,
        ),
    )
    .expect("write the borrower's peers file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&borrower_peers, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    let store = tcr::peer::config::PeerStore::open(&borrower_peers)
        .expect("the borrower's peers file reads");
    HandBorrower {
        _home: home,
        store,
        owner,
    }
}

/// Open a CONTROL stream to `owner` as the borrower holding `secret`, over a
/// pinned `Return` handshake, and send it one ask: a seven-day lease of 0.2,
/// two at a time, for 300 seconds. The stream and session come back unread.
async fn ask_owner_for_a_lease(
    owner: &Owner,
    secret: &[u8; 32],
) -> (tokio::net::TcpStream, tcr::peer::noise::PeerSession) {
    let mut stream = tokio::net::TcpStream::connect(owner.addr)
        .await
        .expect("connect to the owner");
    let mut session = tcr::peer::noise::dial_handshake(
        &mut stream,
        secret,
        tcr::peer::noise::Handshake::Return,
        Some(&owner.public),
        None,
    )
    .await
    .expect("the borrower is pinned, so the return visit completes");

    let header = tcr_peer_wire::StreamHeader {
        kind: tcr_peer_wire::StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: tcr::peer::lease::random_id().expect("a request id"),
    };
    tcr::peer::serve::send_control(&mut stream, &mut session, &header)
        .await
        .expect("the header goes");
    let ask = tcr_peer_wire::LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.2),
        ttl_s: 300,
        max_inflight: 2,
    };
    tcr::peer::serve::send_control(&mut stream, &mut session, &Control::LeaseRequest(ask))
        .await
        .expect("the ask goes");
    (stream, session)
}

/// The owner's first answer on an ask stream, which must be a grant that
/// minted a lease.
async fn read_the_grant(
    stream: &mut tokio::net::TcpStream,
    session: &mut tcr::peer::noise::PeerSession,
) -> Lease {
    let granted: Control = tcr::peer::serve::recv_control(stream, session)
        .await
        .expect("the owner answers the ask");
    let Control::LeaseGrant(grant) = granted else {
        panic!("the first answer is the grant, not {granted:?}");
    };
    grant
        .lease
        .unwrap_or_else(|| panic!("the owner granted a lease, refusal: {:?}", grant.refusal))
}

/// **The gate for the review's first HIGH**: the mode is the FUNDING grant's,
/// and a row whose `hand` grant has ended hands nothing over.
///
/// The row here is a failing input: an ended `hand` grant and
/// a live `serve` grant, both on the seven-day window. `PeerRow::grant_for`
/// skips the ended one, so the lease is funded by the serve grant and the
/// owner's bearer must stay on the owner's Mac.
///
/// Watched red by putting the window-only lookup back in `serve_control`'s
/// `Control::LeaseRequest` arm (`store.row(..).lend.iter().find(|g| g.window ==
/// minted.window)`): that lookup answers with the ENDED hand grant, a `Handoff`
/// carrying `OWNER_FAKE_BEARER` arrives, and the assertion below names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_hand_grant_beside_a_live_serve_grant_hands_nothing_over() {
    let (borrower_secret, borrower_public) =
        tcr::peer::noise::generate_static().expect("a borrower keypair");
    let borrower = tcr_peer_wire::PeerId(borrower_public);
    // `until` is unix SECONDS and this one is in 1970, so `LendGrant::has_ended`
    // is true for it on any clock this test can run on.
    let owner = owner_lending(
        borrower,
        r#"[
                {
                  "mode": "hand",
                  "window": "7d",
                  "fraction": 0.2,
                  "ttlS": 300,
                  "maxInflight": 2,
                  "until": 1000
                },
                {
                  "mode": "serve",
                  "window": "7d",
                  "scope": "all",
                  "fraction": 0.2,
                  "ttlS": 300,
                  "maxInflight": 2
                }
              ]"#,
    )
    .await;

    let (minted, next) = ask_for_a_lease(&owner, &borrower_secret).await;
    assert!(
        minted.expires_at_ms > 0,
        "the live serve grant funded a lease, or this gate is asserting about a refusal"
    );
    assert!(
        next.is_none(),
        "the lease was funded by the SERVE grant, so nothing is handed over; this session \
         read {next:?}"
    );
}

/// **The gate for the review's M2**: a poisoned lease ledger hands nothing
/// over.
///
/// The arm used to read the lease's scope back through a SECOND lock on the
/// same ledger and turn a poisoned one into `LendScope::All`, which resolves
/// the owner's bearer against every account on the Mac rather than the ones
/// the operator lent. The scope now leaves `Ledger::grant` in the same value as
/// the answer, so there is no second lock and a poisoned ledger refuses the
/// whole request at the first one: no grant, no `Handoff`, and a session that
/// closes.
///
/// Watched red by putting both halves of the defect back at once, which is the
/// state the review describes: `serving.ledger.lock()` in the
/// `Control::LeaseRequest` arm recovering with
/// `unwrap_or_else(|poisoned| poisoned.into_inner())`, and the scope read back
/// with `serving.ledger.lock().map(..).unwrap_or_default()`. The owner then
/// answers a poisoned ledger with a lease AND a `Handoff` carrying
/// `OWNER_FAKE_BEARER`, and the assertion below names the frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poisoned_lease_ledger_hands_nothing_over() {
    let (borrower_secret, borrower_public) =
        tcr::peer::noise::generate_static().expect("a borrower keypair");
    let borrower = tcr_peer_wire::PeerId(borrower_public);
    let owner = owner_serving(borrower, "hand").await;

    // Poison it the only way a `std::sync::Mutex` is poisoned: a panic while
    // the guard is held. The join handle's `Err` IS the panic, so it is read
    // rather than dropped.
    let ledger = Arc::clone(&owner.ledger);
    let panicked = std::thread::spawn(move || {
        let _held = ledger.lock().expect("the ledger is not poisoned yet");
        panic!("poisoning the owner's ledger on purpose");
    })
    .join();
    assert!(
        panicked.is_err(),
        "the helper thread has to panic for the ledger to be poisoned"
    );
    assert!(
        owner.ledger.lock().is_err(),
        "the ledger is poisoned, or this gate is asserting about a healthy one"
    );

    let (mut stream, mut session) = ask_owner_for_a_lease(&owner, &borrower_secret).await;

    // Every frame this session can still read, and there must be none: the
    // refusal closes the stream. Two reads, because the defect answered with a
    // grant FIRST and the bearer after it.
    let mut read = Vec::new();
    for _ in 0..2 {
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tcr::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
        )
        .await;
        match next {
            Ok(Ok(frame)) => read.push(frame),
            // A closed stream and a read that never answered are the same
            // fact here: nothing more is coming.
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(
        !read
            .iter()
            .any(|frame| matches!(frame, Control::Handoff { .. })),
        "a poisoned ledger hands no bearer over, and this session read {read:?}"
    );
    assert!(
        read.is_empty(),
        "a poisoned ledger answers nothing at all, and this session read {read:?}"
    );
}

/// **The gate for the owner half**: after granting a `hand` lease, the owner's
/// listener sends the bearer on the SAME session, unasked.
///
/// `lease::handoff_for` and `Ledger::apply_usage_hint` have had tests since
/// they were written, and both of those call the function directly. Nothing
/// called either from a session, so an owner could grant a hand-mode lease all
/// day and never hand anything over, which from the borrower's side is
/// indistinguishable from serve mode.
///
/// Driven frame by frame rather than through `lease::request_lease`, and it
/// stays that way now that the borrower half IS wired: this gate is about what
/// the OWNER puts on the session, and reading it frame by frame is what lets
/// the serve leg below assert that nothing arrives. The borrower's end of the
/// same exchange is
/// `a_borrowed_hand_lease_leaves_the_owners_bearer_in_this_process`.
///
/// The `serve`-mode leg in the same test is the positive control, and it is the
/// assertion with teeth: without it, "a Handoff arrived" would also pass
/// against a listener that hands its bearer to every borrower.
///
/// Watched red by dropping the `handoff_for` block from `serve_control`'s
/// `Control::LeaseRequest` arm: the hand-mode leg reads nothing after the
/// grant and the first assertion names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_grant_is_followed_by_the_owners_bearer_on_the_same_session() {
    for (mode, expect_handoff) in [("hand", true), ("serve", false)] {
        let (borrower_secret, borrower_public) =
            tcr::peer::noise::generate_static().expect("a borrower keypair");
        let borrower = tcr_peer_wire::PeerId(borrower_public);
        // The same fixture with the one word changed, which is the only
        // difference the two legs rest on.
        let owner = owner_serving(borrower, mode).await;

        let (mut stream, mut session) = ask_owner_for_a_lease(&owner, &borrower_secret).await;
        let minted = read_the_grant(&mut stream, &mut session).await;

        // Whatever comes next, it must not take longer than a LAN round trip:
        // in serve mode nothing comes at all and the read waits for the close.
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tcr::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
        )
        .await;

        if expect_handoff {
            let frame = next
                .expect("a hand-mode grant is followed by a frame, and this one timed out")
                .expect("and that frame reads as a Control");
            let Control::Handoff {
                lease_id,
                access_token,
                expires_at_ms,
                utilization,
            } = frame
            else {
                panic!("a hand grant is followed by the bearer, not by {frame:?}");
            };
            assert_eq!(
                lease_id, minted.lease_id,
                "the bearer is handed for the lease this session just minted"
            );
            assert_eq!(
                access_token.reveal(),
                OWNER_FAKE_BEARER,
                "and it is the owner's own bearer, read through the manager"
            );
            assert!(
                expires_at_ms > 0,
                "with an absolute expiry, so the borrower can stop without waiting for a 401"
            );
            assert_eq!(
                utilization,
                Some(OWNER_MEASURED_UTILIZATION),
                "and with the owner's own utilization on the lease's window, which is the \
                 baseline every spend this borrower reports is a rise above"
            );
            assert_eq!(
                owner
                    .ledger
                    .lock()
                    .expect("ledger lock")
                    .grantee_of(minted.lease_id),
                Some(borrower),
                "and the ledger still names the borrower as the only party that may spend it"
            );
        } else {
            let handed = match next {
                Err(_elapsed) => None,
                Ok(read) => read.ok(),
            };
            assert!(
                handed.is_none(),
                "the control: a serve-mode grant hands nothing over, and this session \
                 read {handed:?}"
            );
        }
    }
}

/// **The gate for the borrower half**: a hand-mode borrow driven through
/// `lease::request_lease` leaves the owner's bearer in THIS process's store.
///
/// Both halves existed and nothing connected them. The owner
/// sent the frame (the gate above) and `request_lease` read exactly one answer
/// and returned, so the session was dropped with the bearer still in flight and
/// `HandedTokens::put` had no production caller anywhere. From the borrower's
/// side that is indistinguishable from a lender running in serve mode.
///
/// Polled rather than read once: the reader lives in a task the ask spawns, so
/// the lease comes back first and the bearer lands a round trip later. The poll
/// has a ceiling and the failure below names what was never stored.
///
/// The serve-mode leg is the positive control and it is the assertion with
/// teeth: without it, "a bearer is in the store" would also pass against a
/// borrower that stored anything it was sent.
///
/// Watched red by deleting the `tokio::spawn(read_handed_bearers(..))` line
/// from `lease::request_lease` (`src/peer/lease.rs`): the hand leg polls out
/// with an empty store and the first assertion names the lease id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_borrowed_hand_lease_leaves_the_owners_bearer_in_this_process() {
    for (mode, expect_bearer) in [("hand", true), ("serve", false)] {
        let HandBorrower {
            _home,
            store,
            owner,
        } = hand_borrower(mode).await;

        let ask = tcr_peer_wire::LeaseRequest {
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.2),
            ttl_s: 300,
            max_inflight: 2,
        };
        let lease =
            tcr::peer::lease::request_lease(&store, &tcr_peer_wire::PeerId(owner.public), &ask)
                .await
                .expect("the ask reaches the owner")
                .expect("the owner grants a lease in both modes");

        let bearer_of = |lease_id: u128| {
            handed_tokens()
                .lock()
                .expect("the handed-token store's lock")
                .bearer(lease_id, crate_now_ms())
                .map(str::to_string)
        };
        let mut held = None;
        for _ in 0..100 {
            held = bearer_of(lease.lease_id);
            if held.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        if expect_bearer {
            assert_eq!(
                held.as_deref(),
                Some(OWNER_FAKE_BEARER),
                "a hand grant leaves the owner's bearer in this process, under the lease it \
                 was granted for ({:032x})",
                lease.lease_id
            );
        } else {
            assert!(
                held.is_none(),
                "the control: a serve-mode grant hands nothing over, and this process holds \
                 {held:?} for the lease it just took"
            );
        }
    }
}

/// **A hand-mode borrow reports what it spent, and the owner's ledger moves.**
///
/// The review's finding: nothing in production built a `Control::UsageHint`.
/// `usage_hint` existed, the owner's arm that applies one existed, and no
/// caller joined them, so a hand-mode lease was debited by nobody: the request
/// leaves the borrower's Mac, the owner never sees it, and `spent` stayed at
/// 0.0 for the life of the lease. The ceiling `may_relay` enforces was
/// therefore never reached and the owner's own `tcr peer ls` said a lease that
/// had spent the account's window was untouched.
///
/// The whole loop is driven here: the borrower asks for a lease, the owner
/// grants in `hand` mode and pushes its bearer on that session, the borrower
/// serves two requests from its own Mac against an origin whose utilization
/// rises between them, and the OWNER's own ledger is what is asserted.
///
/// **The first answer is charged too**, which is the review's first HIGH here.
/// A rise needs two readings and the second reading is the answer's own header,
/// so the FIRST one has to come from somewhere else: the owner puts its own
/// utilization on the `Handoff` frame and the borrower's meter starts there. It
/// used to start at `None`, so every lease's first answer was free and a
/// borrower that asked for a fresh lease per request was never debited at all.
///
/// Watch it fail by deleting the `report_handed_spend` call from
/// `serve_on_handed_bearer`: no hint is ever sent and `spent` stays at 0.0. Or
/// by seeding `HandedMeter.last` with `None` in `register_handed_meter` and
/// dropping the `note_handed_baseline` call: the ledger then carries 0.07, the
/// second answer's rise alone, and the first answer's 0.10 is never charged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_mode_borrow_reports_its_spend_to_the_owner() {
    let HandBorrower {
        _home,
        store,
        owner,
    } = hand_borrower("hand").await;

    let lease = tcr::peer::lease::request_lease(
        &store,
        &tcr_peer_wire::PeerId(owner.public),
        &tcr_peer_wire::LeaseRequest {
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.2),
            ttl_s: 300,
            max_inflight: 2,
        },
    )
    .await
    .expect("the ask reaches the owner")
    .expect("the owner grants a hand-mode lease");

    // The bearer arrives on the same session, unasked: the borrow cannot leave
    // from here until it has.
    for _ in 0..200 {
        let held = handed_tokens()
            .lock()
            .expect("the handed-token store's lock")
            .bearer(lease.lease_id, crate_now_ms())
            .is_some();
        if held {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let origin = FakeOrigin::start_rising().await;
    let base = format!("http://{ORIGIN_HOST}");
    let body = bytes::Bytes::from_static(b"{\"model\":\"fake\"}");
    for request in 1..=2 {
        let served = serve_on_handed_bearer(
            &base,
            &origin.client(),
            &ask(&body),
            lease.lease_id,
            Window::SevenDay,
            crate_now_ms(),
        )
        .await
        .unwrap_or_else(|err| panic!("hand-mode request {request} reaches the origin: {err}"))
        .unwrap_or_else(|| panic!("hand-mode request {request} is served from this Mac"));
        assert_eq!(served.status(), 200);
    }

    // The owner's OWN ledger, which is the only place a debit counts.
    //
    // BOTH answers are charged, and the first one is the point. The meter's
    // baseline used to be `None` until an answer had already been served, so
    // the first answer on every lease was free and a borrower that re-asked per
    // request paid for nothing at all; this waits for the full figure rather
    // than for "something above zero", which the first charge alone would
    // satisfy.
    let expected = (0.40 - OWNER_MEASURED_UTILIZATION) + 0.07;
    let mut spent = 0.0;
    for _ in 0..200 {
        spent = spent_of(
            &owner.ledger.lock().expect("the owner's ledger lock"),
            lease.lease_id,
            crate_now_ms(),
        );
        if (spent - expected).abs() < 1e-6 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        (spent - expected).abs() < 1e-6,
        "the owner's ledger must carry the rise above the baseline its own handoff carried \
         (0.40 - {OWNER_MEASURED_UTILIZATION}) plus the rise between the two answers \
         (0.47 - 0.40), which is {expected}, and it carries {spent}"
    );
}

/// **The owner hands one bearer over, does not re-send it, and stops when the
/// lease is revoked.**
///
/// The bearer travels on the SAME session, unasked, which is the whole shape:
/// the borrower has nothing to ask with, since the token is the only thing it
/// holds and a request for a new one carries no proof the old one is
/// legitimate.
///
/// The bearer here expires inside `HANDOFF_RENEW_LEAD_MS`, so every poll finds
/// a renewal DUE and the only thing stopping a frame is that the token has not
/// changed. That is the case this fixture can reach: the owner's credential is
/// a constant in it, and a refreshed one is measured where it can be, over a
/// sequence of polls, in
/// `exactly_one_push_per_new_bearer_over_the_last_five_minutes`.
///
/// The revoke leg is "revocation IS stop renewing". There is no recall frame,
/// so what the test can observe is the ABSENCE of a later frame, and the
/// absence is bounded by a wait several poll intervals long so that "not yet"
/// cannot pass for "never".
///
/// Watched red by dropping the fingerprint comparison from
/// `lease::handoff_push_is_due`: a second, identical bearer arrives within a
/// second and the count is 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owner_hands_a_bearer_once_and_stops_at_revoke() {
    let (borrower_secret, borrower_public) =
        tcr::peer::noise::generate_static().expect("a borrower keypair");
    let borrower = tcr_peer_wire::PeerId(borrower_public);
    // Inside the renewal lead, so every poll finds one due.
    let expires_at_ms = crate_now_ms() + HANDOFF_RENEW_LEAD_MS / 2;
    let owner = owner_lending_expiring(
        borrower,
        r#"{
            "mode": "hand",
            "window": "7d",
            "scope": "all",
            "fraction": 0.2,
            "ttlS": 300,
            "maxInflight": 2
          }"#,
        expires_at_ms,
    )
    .await;

    let (mut stream, mut session) = ask_owner_for_a_lease(&owner, &borrower_secret).await;
    let minted = read_the_grant(&mut stream, &mut session).await;

    // The bearer that rides the grant, and then nothing while it is unchanged.
    //
    // This assertion used to read "at least three", and that was the review's
    // third finding written down as a test: the owner's credential does not
    // change in this fixture, so those extra frames were the SAME token going
    // out once a second for the whole five-minute lead. The renewal mechanism
    // is measured over a sequence of polls by
    // `exactly_one_push_per_new_bearer_over_the_last_five_minutes`; what is
    // measured here, on the wire, is that the first one arrives and no
    // duplicate of it follows.
    let mut handoffs = 0_usize;
    for _ in 0..2 {
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tcr::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
        )
        .await;
        let Ok(Ok(Control::Handoff {
            lease_id,
            access_token,
            ..
        })) = read
        else {
            break;
        };
        assert_eq!(lease_id, minted.lease_id, "each one is for this lease");
        assert_eq!(
            access_token.reveal(),
            OWNER_FAKE_BEARER,
            "and carries the real bearer"
        );
        handoffs += 1;
    }
    assert_eq!(
        handoffs, 1,
        "the grant's own bearer arrives unasked, and the unchanged token is not re-sent on \
         every poll for the rest of its life"
    );

    // Revoke, as an operator does it: the grant leaves the peers file. There is
    // no ledger removal and no recall frame, so this IS the mechanism.
    let peers = owner._home.path().join("tcr-peers.json");
    let raw = std::fs::read_to_string(&peers).expect("the peers file reads");
    let stripped = raw.replace(r#""mode": "hand""#, r#""mode": "serve""#);
    assert_ne!(raw, stripped, "the fixture has a hand grant to revoke");
    std::fs::write(&peers, stripped).expect("rewrite the peers file");
    let after = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        tcr::peer::serve::recv_control::<_, Control>(&mut stream, &mut session),
    )
    .await;
    assert!(
        after.is_err(),
        "revocation IS stop renewing: nothing may arrive after it, and this session read \
         {after:?}"
    );
}
