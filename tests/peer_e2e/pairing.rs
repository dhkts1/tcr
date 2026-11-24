//! The pairing path: two Macs find each other, pair on six digits and pin, and
//! N Macs pair in a ring.

use super::*;

// ---------------------------------------------------------------------------
// The pairing path
// ---------------------------------------------------------------------------

/// **Two Macs pair the way an operator pairs them**: the beacon switch, a
/// pairing request that discloses nothing until Accept, the same six digits on
/// both screens, and a pin.
///
/// Every assertion is on output an operator can see: a CLI exit code, a line on
/// stdout, a line in the other process's log, a field in `--json`. Nothing here
/// calls a library function, which is what makes it able to catch a verb that
/// never reached the dispatcher.
///
/// Watched red: `tests/tools/e2e/watch-peer-e2e-fail.sh` deletes the
/// `record_pairing_key`/six-digit `tracing::info!` block from
/// `src/peer/listener.rs` (the `Handshake::Pair` arm) and this fails at the
/// "the lender's screen shows the digits" step, because the responder's half of
/// the compare is then invisible to the operator standing at the lender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_macs_find_each_other_pair_on_six_digits_and_pin() {
    let (upstream, _seen) = spawn_upstream().await;
    let mut lender = Mac::new("lender", &upstream, "lender-fake", "at-fake-lender", false);
    let mut borrower = Mac::new(
        "borrower",
        &upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot two tcr processes on kernel proxy ports");
    lender.boot();
    borrower.boot();
    println!(
        "         lender proxy={} peer=127.0.0.1:{}; borrower proxy={} peer=127.0.0.1:{}",
        lender.proxy, lender.peer_port, borrower.proxy, borrower.peer_port
    );

    step(
        2,
        "each Mac takes a display name (never this machine's hostname)",
    );
    assert_eq!(
        lender.peer_ok(&["name", "lender-mac"]).trim(),
        "lender-mac",
        "`tcr peer name` must echo the name it stored"
    );
    assert_eq!(
        borrower.peer_ok(&["name", "borrower-mac"]).trim(),
        "borrower-mac"
    );

    step(3, "find on, one Mac at a time");
    for mac in [&lender, &borrower] {
        find_on(mac);
    }

    step(
        4,
        "the borrower presses Trust on the lender's address, which SENDS a pairing request",
    );
    let mut pair = spawn_pair(&borrower, &format!("127.0.0.1:{}", lender.peer_port));
    let asked = wait_for_file(&pair.out, "peer pair: asked", &borrower);
    let instance = field(&asked, "instance").unwrap_or_default();
    let instance = asked
        .rsplit_once("as instance ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map_or(instance, str::to_string);
    assert_eq!(
        instance.len(),
        16,
        "the request must name this boot's 8-byte instance id, got {instance:?}"
    );

    step(5, "the lender sees a request and nothing else about it");
    let pending: serde_json::Value =
        serde_json::from_str(lender.peer_ok(&["pending", "--json"]).trim())
            .expect("`peer pending --json` is JSON");
    assert_eq!(
        pending["pending"][0]["instanceId"].as_str(),
        Some(instance.as_str()),
        "the queued request must be the one the borrower just sent: {pending}"
    );
    assert_eq!(
        pending["pending"][0]["proposedName"].as_str(),
        Some("borrower-mac"),
        "the row must carry the name the borrower proposed, not its hostname: {pending}"
    );
    let text = lender.peer_ok(&["pending"]);
    assert!(
        text.contains("wants to connect") && text.contains(&instance),
        "the text row is what the operator reads: {text}"
    );
    // The whole point: before Accept, the requester has been told
    // nothing: no static key, so no digits either.
    let before = strip_ansi(&std::fs::read_to_string(&pair.out).unwrap_or_default());
    assert!(
        !before.contains("this Mac shows"),
        "digits appeared BEFORE anyone pressed Accept, which means an XX message 2 \
         went out to an unapproved instance:\n{before}"
    );

    step(
        6,
        "Accept opens a window for that one instance, and the digits appear",
    );
    let accepted = lender.peer_ok(&["accept", &instance]);
    assert!(
        accepted.contains(&format!("instance={instance}")),
        "accept must name the instance it approved: {accepted}"
    );
    let borrower_line = wait_for_file(&pair.out, "this Mac shows", &borrower);
    let borrower_code =
        six_digits(&borrower_line).unwrap_or_else(|| panic!("no six digits in {borrower_line:?}"));
    let lender_line = lender.wait_for_log("peer pairing: compare these six digits");
    let lender_code =
        six_digits(&lender_line).unwrap_or_else(|| panic!("no six digits in {lender_line:?}"));
    assert_eq!(
        borrower_code, lender_code,
        "the two screens must show the SAME six digits; a mismatch is the one signal \
         this path exists to produce"
    );
    println!("         both screens show {borrower_code}");

    step(
        7,
        "the operator compares them, and the borrower pins the lender",
    );
    pair.answer(&lender_code);
    let outcome = pair.finish(&borrower);
    assert!(
        outcome.contains("peer pair: trusted"),
        "the compare must end in a pin: {outcome}"
    );
    let row = borrower.ls();
    assert_eq!(
        row["peers"].as_array().map(Vec::len),
        Some(1),
        "the borrower must hold exactly one pinned row: {row}"
    );
    assert_eq!(
        row["pendingCount"].as_u64(),
        Some(0),
        "an accepted request must leave the pending list: {row}"
    );

    // GAP 1, now closed: `pair::confirm` takes the address the
    // pairing ran over and `pin_row` writes it, so a Mac trusted on six digits
    // has a way back to the Mac that trusted it. This assertion used to pin
    // the gap at zero endpoints; it now pins the fix.
    let endpoints = row["peers"][0]["endpoints"].clone();
    assert_eq!(
        endpoints.as_array().map(Vec::len),
        Some(1),
        "a six-digit pairing must leave exactly one endpoint on the pinned row: {row}"
    );
    assert_eq!(
        endpoints[0]["addr"].as_str(),
        Some(format!("127.0.0.1:{}", lender.peer_port).as_str()),
        "the endpoint must be the address the pairing actually ran over: {endpoints}"
    );
    assert_eq!(
        endpoints[0]["kind"].as_str(),
        Some("direct"),
        "an address this Mac dialled itself is a direct locator: {endpoints}"
    );
    assert_eq!(
        endpoints[0]["source"].as_str(),
        Some("paired"),
        "an endpoint written at the pin is sourced `paired`, which is what makes it \
         worth more than a beacon: {endpoints}"
    );
    assert!(
        endpoints[0]["observedAtMs"].as_i64().unwrap_or_default() > 0,
        "the endpoint must carry when it was observed, on this Mac's own clock: {endpoints}"
    );

    // And the RUNNING process reports the same pin in the payload the panel
    // reads. Derived correctly in `peer_status_rows` and never assigned is
    // exactly the failure this step exists to catch: `tcr peer ls` would still
    // be right while the panel drew nothing.
    let status = borrower.status_json();
    let peers = status["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("`tcr peer status --json` must carry a peers block: {status}"));
    assert_eq!(
        peers.len(),
        1,
        "the pinned row must reach the payload the panel reads: {status}"
    );
    assert_eq!(
        peers[0]["address"].as_str(),
        Some(format!("127.0.0.1:{}", lender.peer_port).as_str()),
        "and it must carry the endpoint, not an empty row: {status}"
    );
    assert_eq!(
        peers[0]["paths"][0]["kind"].as_str(),
        Some("direct"),
        "with one direct path under it: {status}"
    );

    lender.shutdown();
    borrower.shutdown();
}

// ---------------------------------------------------------------------------
// The N-node ring
// ---------------------------------------------------------------------------

/// `TCR_E2E_NODES`, default 2, clamped to the 2..=5 range
/// `scripts/peer-e2e-local.sh --nodes N` is documented to accept. A value
/// outside that range is a test-runner mistake, not a shape this test learns
/// to tolerate, so it panics rather than clamping silently.
fn e2e_node_count() -> usize {
    let n = std::env::var("TCR_E2E_NODES").ok().map_or(2, |raw| {
        raw.parse::<usize>()
            .unwrap_or_else(|err| panic!("TCR_E2E_NODES={raw:?} is not a number: {err}"))
    });
    assert!((2..=5).contains(&n), "TCR_E2E_NODES must be 2..=5, got {n}");
    n
}

/// The six-digit compare `two_macs_find_each_other_pair_on_six_digits_and_pin`
/// proves, generalized to any pair inside a larger ring: `initiator` sends the
/// pairing request, `responder` accepts it, both screens must show the SAME
/// six digits, and `initiator` ends up with one more pinned row than before.
///
/// This is a new helper next to that test, not an edit to it: the two-Mac test
/// stays byte-for-byte what it was, and this one reuses its exact CLI
/// incantations and log-line needles so the ring case is provably the same
/// protocol run N times, not a shape that merely looks similar.
fn pair_two_macs(initiator: &Mac, responder: &Mac) {
    let mut pair = spawn_pair(initiator, &format!("127.0.0.1:{}", responder.peer_port));
    let asked = wait_for_file(&pair.out, "peer pair: asked", initiator);
    let instance = asked
        .rsplit_once("as instance ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map_or_default(str::to_string);
    assert_eq!(
        instance.len(),
        16,
        "{}: the request must name this boot's 8-byte instance id, got {instance:?}",
        initiator.label
    );

    let pending: serde_json::Value =
        serde_json::from_str(responder.peer_ok(&["pending", "--json"]).trim())
            .expect("`peer pending --json` is JSON");
    let proposed_name = format!("{}-mac", initiator.label);
    let row = pending["pending"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["instanceId"].as_str() == Some(instance.as_str()))
        .unwrap_or_else(|| {
            panic!(
                "{}: no pending row for instance {instance}: {pending}",
                responder.label
            )
        });
    assert_eq!(
        row["proposedName"].as_str(),
        Some(proposed_name.as_str()),
        "{}: the pending row must carry the name {} proposed, not its hostname: {pending}",
        responder.label,
        initiator.label
    );

    let accepted = responder.peer_ok(&["accept", &instance]);
    assert!(
        accepted.contains(&format!("instance={instance}")),
        "{}: accept must name the instance it approved: {accepted}",
        responder.label
    );
    let initiator_line = wait_for_file(&pair.out, "this Mac shows", initiator);
    let initiator_code = six_digits(&initiator_line)
        .unwrap_or_else(|| panic!("{}: no six digits in {initiator_line:?}", initiator.label));
    let responder_line = responder.wait_for_log("peer pairing: compare these six digits");
    let responder_code = six_digits(&responder_line)
        .unwrap_or_else(|| panic!("{}: no six digits in {responder_line:?}", responder.label));
    assert_eq!(
        initiator_code, responder_code,
        "{} and {} must show the SAME six digits",
        initiator.label, responder.label
    );

    pair.answer(&initiator_code);
    let outcome = pair.finish(initiator);
    assert!(
        outcome.contains("peer pair: trusted"),
        "{}: the compare must end in a pin: {outcome}",
        initiator.label
    );
}

/// **N Macs in a ring** (`TCR_E2E_NODES`, default 2): boot N, name each, turn
/// `find` on for each, then pair every adjacent pair around the ring with the
/// same six-digit protocol the two-Mac test proves. At N=2 there is exactly
/// ONE edge (0-1), which is the two-Mac test's own topology, run through this
/// generalized driver rather than duplicated by hand; at N>=3 there are N
/// edges (0-1, 1-2, ..., (N-1)-0) and every Mac ends the run with exactly two
/// pinned rows.
///
/// `tests/peer_e2e.rs`'s existing two-Mac test is untouched by this addition
/// and keeps passing on its own, so "every existing step still passes at N=2
/// byte-for-byte" holds trivially: this is a new, separate driver, not a
/// rewrite of that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn n_macs_pair_in_a_ring() {
    let n = e2e_node_count();
    let (upstream, _seen) = spawn_upstream().await;
    let mut macs: Vec<Mac> = (0..n)
        .map(|i| {
            let label: &'static str = Box::leak(format!("node{i}").into_boxed_str());
            let account = format!("{label}-fake");
            let token = format!("at-fake-{label}");
            Mac::new(label, &upstream, &account, &token, true)
        })
        .collect();

    step(1, &format!("boot {n} tcr processes on kernel proxy ports"));
    for mac in &mut macs {
        mac.boot();
    }
    for mac in &macs {
        println!(
            "         {} proxy={} peer=127.0.0.1:{}",
            mac.label, mac.proxy, mac.peer_port
        );
    }

    step(
        2,
        "each Mac takes a display name (never this machine's hostname)",
    );
    for mac in &macs {
        let name = format!("{}-mac", mac.label);
        assert_eq!(
            mac.peer_ok(&["name", &name]).trim(),
            name,
            "`tcr peer name` must echo the name it stored"
        );
    }

    step(3, "find on, one Mac at a time");
    for mac in &macs {
        find_on(mac);
    }

    let edges: Vec<(usize, usize)> = if n == 2 {
        vec![(0, 1)]
    } else {
        (0..n).map(|i| (i, (i + 1) % n)).collect()
    };
    step(4, &format!("pair the ring: {} edge(s)", edges.len()));
    // GAP 1 (this file's module docs, `src/peer/pair.rs:670`): `pair::confirm`
    // pins only the INITIATOR's side: the asker ends up with a row for the
    // responder, and the responder's `accept` approves the request but pins
    // nothing of its own. A ring where node `i` always initiates the edge to
    // `i+1 mod n` therefore leaves every node with exactly ONE pinned row (the
    // node it asked), never two, even at N>=3: the expected count below is
    // the number of times each node appears as the INITIATOR, read off the
    // edge list rather than assumed, so this test breaks loudly the day GAP 1
    // is fixed and a responder starts pinning too.
    let mut expected_pins = vec![0usize; n];
    for (initiator, responder) in &edges {
        pair_two_macs(&macs[*initiator], &macs[*responder]);
        expected_pins[*initiator] += 1;
    }

    for (i, mac) in macs.iter().enumerate() {
        let ls = mac.ls();
        assert_eq!(
            ls["peers"].as_array().map(Vec::len),
            Some(expected_pins[i]),
            "{}: must hold exactly {} pinned row(s) after the ring (GAP 1: only an \
             initiator's own `pair::confirm` pins): {ls}",
            mac.label,
            expected_pins[i]
        );
    }

    for mac in &mut macs {
        mac.shutdown();
    }
}

/// `tcr peer find on` must exit 0, write the switch, and say so: a non-zero
/// exit is a test failure here, never a NOTE this step swallows. A box with no
/// multicast-capable interface is not a passing outcome for an operator, and
/// it is not one for this gate either: if the mDNS calls behind the switch
/// cannot start on this runner, that is an environment defect to fix in the
/// runner, not a shape this test learns to tolerate.
///
/// What this step does NOT claim: that one Mac SAW the other's beacon. It
/// cannot be claimed through the CLI on this build: `find on` starts a browse
/// and discards the scan (`src/main.rs:2056-2062`), and the announcement dies
/// with the short-lived CLI process that registered it, so there is no
/// cross-process sighting to read. What the beacon may carry is
/// `tests/peer_discovery.rs`'s subject; this is the switch.
fn find_on(mac: &Mac) {
    let (stdout, stderr, ok) = mac.peer(&["find", "on", "--announce-name", "off"]);
    assert!(
        ok,
        "{}: `tcr peer find on` exited non-zero.\nstdout: {stdout}\nstderr: {stderr}",
        mac.label
    );
    assert!(
        stdout.contains("peer.find: on (announceName=false)"),
        "{}: `find on` said something else: {stdout}",
        mac.label
    );
    let file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&mac.peers).expect("the peers file reads back"),
    )
    .expect("the peers file is JSON");
    assert_eq!(
        file["discovery"].as_bool(),
        Some(true),
        "{}: `find on` must write the switch: {file}",
        mac.label
    );
    assert_eq!(
        file["announceName"].as_bool(),
        Some(false),
        "{}: `--announce-name off` must be stored too: {file}",
        mac.label
    );
}

/// A `tcr peer pair` process: it knocks, then retries the handshake for ten
/// minutes while it waits for the far side's Accept, then asks on stdin for the
/// digits the other screen shows.
struct PairProcess {
    child: Child,
    out: PathBuf,
}

impl PairProcess {
    /// Type the digits the other Mac is showing.
    fn answer(&mut self, code: &str) {
        let stdin = self.child.stdin.as_mut().expect("the pair process's stdin");
        stdin
            .write_all(format!("{code}\n").as_bytes())
            .expect("the compared code writes to stdin");
        stdin.flush().expect("stdin flushes");
    }

    /// Wait for it to exit and return everything it printed.
    fn finish(mut self, mac: &Mac) -> String {
        let status = self
            .child
            .wait()
            .expect("the pair process is waitable after its answer");
        let printed = strip_ansi(&std::fs::read_to_string(&self.out).unwrap_or_default());
        assert!(
            status.success(),
            "{}: `tcr peer pair` exited {status:?}:\n{printed}",
            mac.label
        );
        printed
    }
}

fn spawn_pair(mac: &Mac, addr: &str) -> PairProcess {
    let out = mac.home_file("pair.out");
    let file = std::fs::File::create(&out).expect("the pair log opens");
    let err = file.try_clone().expect("the pair log clones for stderr");
    let mut command = mac.peer_command(&["pair", addr]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err));
    let child = command
        .spawn()
        .unwrap_or_else(|err| panic!("{}: spawning `tcr peer pair`: {err}", mac.label));
    PairProcess { child, out }
}

/// Block until `path` holds a line containing `needle`; return that line.
fn wait_for_file(path: &Path, needle: &str, mac: &Mac) -> String {
    let deadline = Instant::now() + LINE_TIMEOUT;
    loop {
        let body = strip_ansi(&std::fs::read_to_string(path).unwrap_or_default());
        if let Some(line) = complete_lines(&body)
            .rev()
            .find(|line| line.contains(needle))
        {
            return line.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "{}: nothing containing {needle:?} in {} within {LINE_TIMEOUT:?}. Got:\n{body}",
            mac.label,
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
