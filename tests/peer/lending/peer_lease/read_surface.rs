// ---------------------------------------------------------------------------
// The READ surface: `until` and `ended` on a `tcr peer ls --json`
// row, and the peers file opened once per accepted connection
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **A `tcr peer ls --json` row carries `until` and `ended`.**
///
/// Through the BINARY this build produced, because the writer is in
/// `src/main.rs` and the thing being gated is the JSON a panel decodes: not a
/// function a test could call. Three shapes, in the order an operator meets
/// them: a lease with no end, a lease given one by `--for 2h` on the real CLI,
/// and a lease whose end has passed.
///
/// The last one is the assertion row 13 exists for: an ended lease is still in
/// the list. A reader that dropped it would take the "re-lend with one click"
/// row off the screen at the exact moment it becomes the only useful control.
///
/// Watch it fail by returning `(None, false)` from `peer_ls_ends`: the `--for
/// 2h` assertion reads `null` and the ended one reads `false`.
#[test]
fn a_peer_ls_json_row_carries_until_and_ended() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    // A missing config, deliberately: `lentTo` is keyed by account label and
    // this test must never read the operator's real config directory.
    let config_path = home.path().join("tcr.json");
    let borrower = PeerId([7_u8; 32]);

    write_peers(
        &peers_path,
        vec![lender_row_for(
            borrower,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );

    let row = ls_json_row(&peers_path, &config_path);
    assert_eq!(
        row["until"],
        serde_json::Value::Null,
        "a lease with no end has no end to print"
    );
    assert_eq!(row["ended"], serde_json::Value::Bool(false));

    // `--for 2h` on the real CLI, so this covers argv → file → JSON and not
    // only the writer.
    let before = unix_seconds();
    let lent = tcr_beside(&peers_path)
        .args(["peer", "lend", &borrower.to_wire()])
        .args(["--peers", &peers_path.display().to_string()])
        .args(["--window", "7d", "--fraction", "0.2", "--for", "2h"])
        .output()
        .expect("`tcr peer lend` runs");
    assert!(
        lent.status.success(),
        "`tcr peer lend --for 2h`: {}",
        String::from_utf8_lossy(&lent.stderr)
    );
    let after = unix_seconds();

    let row = ls_json_row(&peers_path, &config_path);
    let until = row["until"].as_u64().expect("`until` is a number");
    assert!(
        until >= before + 7_200 && until <= after + 7_200,
        "`--for 2h` prints the end it wrote: {until} is not two hours after \
         {before}..={after}"
    );
    assert_eq!(
        row["ended"],
        serde_json::Value::Bool(false),
        "a lease that ends in two hours has not ended"
    );

    // An end that has passed. Written directly because `--for` and `--until`
    // can only name a FUTURE instant, which is correct and leaves the ended
    // state unreachable through argv.
    let ended_at = unix_seconds() - 60;
    let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    grant.until = Some(ended_at);
    write_peers(&peers_path, vec![lender_row_for(borrower, vec![grant])]);

    let listing = ls_json(&peers_path, &config_path);
    let rows = listing["peers"].as_array().expect("`peers` is an array");
    assert_eq!(
        rows.len(),
        1,
        "an ended lease STAYS in the list, greyed, so it can be re-lent"
    );
    assert_eq!(rows[0]["ended"], serde_json::Value::Bool(true));
    assert_eq!(
        rows[0]["until"].as_u64(),
        Some(ended_at),
        "the ended row prints WHEN it ended, which is what its row reads"
    );
}

/// The single row [`ls_json`] printed, with the count asserted rather than
/// indexed blindly: `peers[0]` on an empty array is a panic that reads like a
/// harness bug instead of the missing row it is.
fn ls_json_row(peers_path: &Path, config_path: &Path) -> serde_json::Value {
    let listing = ls_json(peers_path, config_path);
    let rows = listing["peers"].as_array().expect("`peers` is an array");
    assert_eq!(rows.len(), 1, "one pinned row");
    rows[0].clone()
}

/// The wall clock in unix SECONDS: the unit `until` is in.
fn unix_seconds() -> u64 {
    u64::try_from(tcr::now_ms().max(0) / 1_000).unwrap_or(0)
}

/// **An accepted connection opens the peers file ONCE**: the review's L2.
///
/// It opened it twice: `serve_connection` read it before taking the socket
/// slot, and `serve_stream` read the same bytes again for the stream gate a
/// few microseconds later. Both reads are on the pre-authentication path of
/// every connection a stranger can open, so the duplicate is paid by the
/// machine being knocked at.
///
/// Counted, not reasoned about: `config::peers_file_opens` counts at
/// `read_peer_file`, which is the one place the bytes are read and the place
/// every route into the file passes through: so a regression through
/// `read_or_default`, `PeerStore::open` or `reload_if_changed` is counted the
/// same.
///
/// Per PATH, so the count is this test's temp file and not whatever the other
/// tests in this binary are doing on their own at the same moment.
///
/// Watch it fail by putting `read_or_default(&context.peers_path)?` back in
/// `serve_stream` in place of `store.file()`: the delta is 2.
#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_connection_opens_the_peers_file_once() {
    let (upstream, _hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let ServeRig {
        pair,
        lease,
        store: borrower_store,
        ..
    } = serve_rig(
        LendGrant::new(Window::SevenDay, 0.20, 300, 2),
        lender_proxy.clone(),
    )
    .await;

    // AFTER the listener is up: standing one up reads the file once, and that
    // read is not per connection.
    let before = tcr::peer::config::peers_file_opens(&pair.lender_peers);
    // A positive control for the counter itself, on a path that is not the
    // lender's: an empty result here would otherwise read as "one open" no
    // matter what the listener did.
    assert_eq!(
        tcr::peer::config::peers_file_opens(&pair.borrower_peers),
        1,
        "the borrower's own store opened its own file once, so this counter \
         counts opens rather than always answering zero"
    );

    let ask = ask_for("/v1/messages");
    serve::open_serve(
        &pair.lender_id,
        &lease,
        &ask,
        &HeaderMap::new(),
        &borrower_store,
    )
    .await
    .expect("the SERVE stream ran")
    .served()
    .expect("the lender served it");

    let opened = tcr::peer::config::peers_file_opens(&pair.lender_peers) - before;
    assert_eq!(
        opened, 1,
        "one accepted connection opens the lender's peers file {opened} times; \
         the pre-authentication path must read it once and pass the value on"
    );
}

/// **The borrower's clock has a home on disk**: a lease this Mac is granted is
/// written to `peer-state.json`, and a FRESH `tcr peer ls --json` reads its
/// `until` back off the row for the Mac we borrow from.
///
/// `PeerLeaseProvider::leases` was a process-lifetime
/// cache and nothing else, so the one surface an operator reads :
/// `tcr peer ls --json`, which the panel renders: printed `until: null` on
/// every borrower while a lender was actively serving it. The two halves are
/// asserted separately on purpose: the state file (this process wrote it) and
/// the CLI's JSON (a DIFFERENT process read it), because a cache that persists
/// into a file no reader consults is the same defect with an extra step.
///
/// Watch it fail by deleting the `self.persist_borrowed();` line after the
/// cache insert in `PeerLeaseProvider::lease_for`: the state file reads zero
/// borrowed rows and the row's `until` is `null`.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrowed_lease_is_written_and_a_fresh_ls_reads_its_until() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    // The operator's own end, two hours out: the number the borrower's row has
    // to be able to print without asking anybody a second time.
    let ends_at = unix_seconds() + 7_200;
    let mut grant = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    grant.until = Some(ends_at);
    let LeasePair {
        lender_home,
        borrower_home,
        lender_peers,
        borrower_peers,
        lender_id,
        borrower_id: _,
    } = lease_pair(grant);

    let peer_addr = spawn_unprobed_lender(
        lender_home.path(),
        lender_peers.clone(),
        shared_ledger(None),
        lender_proxy.clone(),
    )
    .await;
    point_borrower_at(&borrower_peers, lender_id, peer_addr);

    // The production provider, asking for a lease it does not hold and then
    // spending it: no lease is handed to it, so what lands on disk is what the
    // borrow path itself learned.
    let provider = PeerLeaseProvider::new(borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let response = provider
        .try_serve(&ask)
        .await
        .expect("the lender granted a lease and served the borrowed request");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the request really was served on the lender's account"
    );

    let state_path = borrower_home.path().join("peer-state.json");
    let state = tcr::peer::state::load(&state_path, tcr::now_ms())
        .expect("the borrower's state file reads back");
    assert_eq!(
        state.borrowed.len(),
        1,
        "one lease was granted, so one borrowed row is on disk"
    );
    assert_eq!(
        state.borrowed[0].lender, lender_id,
        "the row names the Mac that granted it"
    );
    assert_eq!(
        state.borrowed[0].lease.until,
        Some(ends_at),
        "the lender's end crossed the wire and was written down"
    );
    assert!(
        state.leases.is_empty(),
        "a borrowed lease is not a lease this Mac lent: the two sections are \
         separate facts about opposite Macs"
    );

    // A DIFFERENT process, reading the same file: the whole point of item 1.
    // A missing config deliberately, so nothing reads the operator's real one.
    let config_path = borrower_home.path().join("tcr.json");
    let row = ls_json_row(&borrower_peers, &config_path);
    assert_eq!(
        row["until"].as_u64(),
        Some(ends_at),
        "the row for a Mac we borrow FROM prints when the lending ends"
    );
    assert_eq!(
        row["ended"],
        serde_json::Value::Bool(false),
        "a lease that ends in two hours has not ended"
    );
}

/// **A borrowed request reaches the API with the headers the API requires.**
///
/// The seam used to hand `PeerLeaseProvider` an `Ask` with no headers at all,
/// and the provider handed `open_serve` an empty map, so the lender put a
/// bearer on the request and nothing else. `api.anthropic.com` answers a
/// message request with no `anthropic-version` with a 400, and the borrower
/// returned that 400 to its client as a served answer. Every test passed,
/// because the fake upstream answered 200 to anything with a Bearer.
///
/// The instrument is the LENDER's own outbound request, recorded by the fake
/// upstream, and the fake upstream now refuses what the real one refuses
/// (`tests/tools/api_contract.rs`). Both halves are asserted: the headers the
/// API needs arrived, and the client's own credential did not.
///
/// Watch it fail by putting `&HeaderMap::new()` back in
/// `PeerLeaseProvider::try_serve`'s `open_serve` call: the lender's request is
/// answered 400 and `try_serve` returns `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrow_through_the_provider_carries_the_headers_the_api_requires() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.20, 300, 2));
    let peer_addr = spawn_unprobed_lender(
        pair.lender_home.path(),
        pair.lender_peers.clone(),
        shared_ledger(None),
        lender_proxy.clone(),
    )
    .await;
    point_borrower_at(&pair.borrower_peers, pair.lender_id, peer_addr);

    // The production provider, handed the `Ask` the proxy's own seam builds
    // from a client request: `Ask::scrubbed` has already removed the credentials.
    let provider = PeerLeaseProvider::new(pair.borrower_peers.clone());
    let ask = ask_for("/v1/messages");
    let response = provider
        .try_serve(&ask)
        .await
        .expect("the lender granted a lease and served the borrowed request");
    assert_eq!(
        response.status().as_u16(),
        200,
        "a borrow the API would refuse is not a served answer"
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    let seen = served.lock().expect("served lock").clone();
    assert_eq!(seen.len(), 1);
    for required in ["anthropic-version", "content-type"] {
        assert!(
            seen[0].header(required).is_some(),
            "the lender's outbound request must carry {required}: {:?}",
            seen[0].headers
        );
    }
    assert_eq!(
        seen[0].token,
        fleet::LENDER_TOKEN,
        "and it is served on the LENDER's own credential"
    );
    // Over VALUES, so a client credential arriving under a name nobody thought
    // to check is caught too.
    assert!(
        !seen[0]
            .values()
            .iter()
            .any(|value| value.contains("not-a-real-client-token")),
        "the client's own credential must not cross to the lender: {:?}",
        seen[0].headers
    );
}

/// A borrowed lease whose END has passed reads `ended: true`, while its
/// RENEWAL ttl is still ahead, the two clocks `Lease::until` and
/// `Lease::expires_at_ms` keep apart.
///
/// Written straight into the state file because no operator act on a BORROWER
/// can set an end in the past: the end is the lender's, and `--for`/`--until`
/// can only name a future instant.
///
/// Watch it fail by dropping the `borrowed` arm from `peer_ls_ends`
/// (`src/main.rs`): `ended` reads `false` and `until` reads `null`.
#[test]
fn a_borrowed_lease_whose_end_has_passed_reads_ended() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let config_path = home.path().join("tcr.json");
    let lender = PeerId([9_u8; 32]);
    write_peers(
        &peers_path,
        vec![borrower_row_for(
            lender,
            true,
            vec!["127.0.0.1:1".to_string()],
        )],
    );

    let ended_at = unix_seconds() - 60;
    let now_ms = tcr::now_ms();
    let state = tcr::peer::state::PeerState {
        borrowed: vec![tcr::peer::state::BorrowedRow {
            lease: Lease {
                lease_id: 0x5ea5e,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.20),
                granted_at_ms: now_ms - 120_000,
                // AHEAD: the renewal ttl has not run out, so `load` keeps the
                // row. What has passed is the LENDING, which is the fact the
                // greyed row renders.
                expires_at_ms: now_ms + 300_000,
                spent: 0.0,
                max_inflight: 2,
                until: Some(ended_at),
            },
            lender,
        }],
        ..tcr::peer::state::PeerState::default()
    };
    tcr::peer::state::save(&home.path().join("peer-state.json"), &state)
        .expect("the borrower's state file writes");

    let row = ls_json_row(&peers_path, &config_path);
    assert_eq!(
        row["ended"],
        serde_json::Value::Bool(true),
        "a borrowed lease whose end has passed greys its row"
    );
    assert_eq!(
        row["until"].as_u64(),
        Some(ended_at),
        "the ended row prints WHEN it ended"
    );
}

/// Borrowed leases are restored at boot, and one whose ttl ran out while the
/// process was down is not.
///
/// The restore is in `PeerLeaseProvider::new` rather than a second call,
/// because a borrower answering one request off an empty cache asks its lender
/// for a lease it already holds.
///
/// Watch it fail by returning `HashMap::new()` from the restore arm in
/// `PeerLeaseProvider::new`: `borrowed()` is empty and the live lease is lost.
#[test]
fn a_live_borrowed_lease_is_restored_at_boot_and_an_expired_one_is_not() {
    let home = tempfile::tempdir().expect("a temp dir");
    let peers_path = home.path().join("tcr-peers.json");
    let live_lender = PeerId([3_u8; 32]);
    let dead_lender = PeerId([4_u8; 32]);
    let now_ms = tcr::now_ms();

    let row_for = |lender: PeerId, expires_at_ms: i64| tcr::peer::state::BorrowedRow {
        lease: Lease {
            lease_id: 0x11,
            window: Window::SevenDay,
            unit: LeaseUnit::Fraction(0.20),
            granted_at_ms: now_ms - 600_000,
            expires_at_ms,
            spent: 0.0,
            max_inflight: 2,
            until: None,
        },
        lender,
    };
    let state = tcr::peer::state::PeerState {
        borrowed: vec![
            row_for(live_lender, now_ms + 300_000),
            row_for(dead_lender, now_ms - 1_000),
        ],
        ..tcr::peer::state::PeerState::default()
    };
    tcr::peer::state::save(&home.path().join("peer-state.json"), &state)
        .expect("the borrower's state file writes");

    let restored = PeerLeaseProvider::new(peers_path).borrowed();
    assert_eq!(
        restored.len(),
        1,
        "the expired lease is dropped on the way in, the live one is not"
    );
    assert_eq!(
        restored[0].lender, live_lender,
        "and the one that survived is the one whose ttl is still ahead"
    );
}
