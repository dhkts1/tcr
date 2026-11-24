// ---------------------------------------------------------------------------
// The CLI, through the binary this build produced. `--peers` points the whole
// peer surface at a temp file, so nothing here reads the operator's config directory or touches
// the proxy on 127.0.0.1:3456.
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// Run `tcr peer <args> --peers <path>` and return `(stdout, stderr, success)`.
fn run_tcr_peer(peers: &Path, args: &[&str]) -> (String, String, bool) {
    let output = tcr_beside(peers)
        .arg("peer")
        .args(args)
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .output()
        .unwrap_or_else(|err| panic!("spawn tcr peer {args:?}: {err}"));
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// A peers file with one pinned Mac and nothing lent, plus that peer's id in
/// the wire form `tcr peer lend` takes.
fn cli_peers_file(dir: &Path) -> (PathBuf, String) {
    let peers = dir.join("tcr-peers.json");
    let node = PeerId([31_u8; 32]);
    let mut row = lender_row_for(node, Vec::new());
    row.label = "attic-nuc".to_string();
    write_peers(&peers, vec![row]);
    (peers, node.to_wire())
}

/// **`tcr peer lend` and `tcr peer share on` refuse a scope that names a Codex
/// account, exit 1, and write nothing**, through the binary an operator and the
/// panel run.
///
/// Codex accounts stay on this Mac: no lease draws on one, so a grant scoped to
/// one would advertise a capability that is not there. The match is the one
/// the serve path uses for `account:` scopes, the sanitized label. The Codex
/// row's name is a label, the way `tcr login --codex --name work` writes one:
/// an email name never reaches this check, because the scope parser refuses an
/// `@` first.
///
/// Clearing stays allowed, so an old lease that names one can still go: a zero
/// fraction on both verbs, and `--list`. A config that cannot be read skips the
/// check, as `tcr peer ls` does. The control is the Claude row, which lends as
/// it always did.
///
/// Watch it fail by dropping the guard call: the run exits 0 and writes the
/// peers file.
#[test]
fn peer_lend_and_share_on_refuse_a_codex_scope() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("tcr.json");
    std::fs::write(
        &config,
        r#"{"accounts":[
             {"name":"work-fake","accessToken":"not-a-real-token"},
             {"name":"work","provider":"codex","accessToken":"not-a-real-token",
              "credentialVersion":2}
           ]}"#,
    )
    .expect("the config fixture writes");
    let refusal = "'work' is a Codex account, and Codex accounts stay on this Mac: only a Claude account can be lent. Nothing was changed.";
    let run = |config: &Path, args: &[&str]| -> std::process::Output {
        tcr_beside(&peers)
            .arg("peer")
            .args(args)
            .args(["--peers", peers.to_str().expect("a utf-8 path")])
            .args(["--config", config.to_str().expect("a utf-8 path")])
            .output()
            .unwrap_or_else(|err| panic!("spawn tcr peer {args:?}: {err}"))
    };
    let peers_before = std::fs::read(&peers).expect("the peers file reads");
    let config_before = std::fs::read(&config).expect("the config reads");

    for args in [
        &["lend", &node, "--scope", "account:work"][..],
        &["lend", &node, "--scope", "account:work-fake,work"],
        &["share", "on", "--scope", "account:work"],
    ] {
        let out = run(&config, args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?} is refused: {stderr}{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            stderr.contains(refusal),
            "{args:?} says why, in the refusal's own words: {stderr}"
        );
        assert_eq!(
            std::fs::read(&peers).expect("the peers file reads"),
            peers_before,
            "{args:?} wrote nothing to the peers file"
        );
        assert_eq!(
            std::fs::read(&config).expect("the config reads"),
            config_before,
            "{args:?} wrote nothing to the config"
        );
    }

    // Clearing an old grant that names it stays allowed on both verbs.
    for args in [
        &["lend", &node, "--scope", "account:work", "--fraction", "0"][..],
        &["share", "on", "--scope", "account:work", "--fraction", "0"],
        &["lend", &node, "--list"],
    ] {
        let out = run(&config, args);
        assert!(
            out.status.success(),
            "{args:?} still works: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A config that cannot be read skips the check rather than failing the verb.
    let missing = dir.path().join("no-such-config.json");
    let out = run(&missing, &["lend", &node, "--scope", "account:work"]);
    assert!(
        out.status.success(),
        "a missing config skips the check: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!missing.exists(), "and creates no config either");

    // The control: the Claude row lends as before.
    let out = run(&config, &["lend", &node, "--scope", "account:work-fake"]);
    assert!(
        out.status.success(),
        "a Claude account is lent: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// **`tcr peer lend --scope group:work` adds a SECOND lease rather than
/// replacing the first**, and prints the id that names it.
///
/// A trusted Mac may hold several leases at once, one per scope.
/// So the replace key is (window, scope) and not the window alone: the failure
/// this guards is an operator who lends `group:work` and silently loses the
/// `all` lease they were already lending on the same window.
///
/// Watch it fail by retaining on `existing.window != window` alone in the
/// `Lend` arm of `run_peer`: the file then holds one lease instead of two.
#[test]
fn lend_with_a_scope_adds_a_second_lease_on_the_same_window() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &["lend", &node, "--window", "7d", "--fraction", "0.2"],
    );
    assert!(ok, "plain lend: {out}{err}");
    assert!(out.contains("scope=all"), "{out}");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.3",
            "--scope",
            "group:work",
        ],
    );
    assert!(ok, "scoped lend: {out}{err}");
    assert!(out.contains("scope=group:work"), "{out}");
    assert!(out.contains("leases=2"), "{out}");

    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    let lend = &file.peers[0].lend;
    assert_eq!(lend.len(), 2, "one lease per scope: {lend:?}");
    assert_eq!(lend[0].scope, tcr_peer_wire::LendScope::All);
    assert_eq!(
        lend[1].scope,
        tcr_peer_wire::LendScope::Group("work".to_string())
    );
    assert_ne!(lend[0].id, lend[1].id, "two leases, two handles");

    // And a scope that is not a scope changes nothing at all: the refusal
    // comes before the write.
    let (_, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--scope", "7"]);
    assert!(!ok, "a bare number is not a scope");
    assert!(err.contains("scope"), "{err}");
    let after = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(after.peers[0].lend.len(), 2, "a refused lend wrote nothing");
}

/// **`--list` prints one greppable line per lease with its id, and `--revoke
/// <lease-id>` removes exactly that one.**
///
/// The id is the whole point: an operator with two leases on one window has no
/// other way to name the one they want gone, and "the 7d one" is not a
/// selector.
///
/// Watch it fail by making `--revoke` clear `row.lend` instead of retaining on
/// the id: the surviving-lease assertion then finds nothing.
#[test]
fn lend_list_prints_ids_and_revoke_removes_exactly_one_lease() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    run_tcr_peer(
        &peers,
        &["lend", &node, "--window", "7d", "--fraction", "0.2"],
    );
    run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "5h",
            "--fraction",
            "0.1",
            "--scope",
            "group:work",
        ],
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--list"]);
    assert!(ok, "list: {out}{err}");
    let lines: Vec<&str> = out.lines().filter(|line| line.contains("lease=")).collect();
    assert_eq!(lines.len(), 2, "two leases, two lines: {out}");
    assert!(lines[0].contains("window=7d"), "{out}");
    assert!(lines[1].contains("scope=group:work"), "{out}");
    assert!(lines[1].contains("until=none"), "{out}");

    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    let doomed = tcr::peer::config::lease_id_string(file.peers[0].lend[0].id);
    let kept = file.peers[0].lend[1].id;
    assert!(
        out.contains(&doomed),
        "the printed line carries the id: {out}"
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--revoke", &doomed]);
    assert!(ok, "revoke: {out}{err}");
    assert!(out.contains("leases_left=1"), "{out}");
    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(file.peers[0].lend.len(), 1);
    assert_eq!(
        file.peers[0].lend[0].id, kept,
        "the lease that survived is the one that was not named"
    );

    // An id nothing holds is a refusal that names it, never a silent no-op.
    let (_, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--revoke", &doomed]);
    assert!(!ok, "revoking a lease that is already gone is an error");
    assert!(err.contains(&doomed), "{err}");
}

/// **`--for 2h` writes an end, `--list` shows it, and `--relend` clears it.**
///
/// Through the surface an operator actually types, the end is
/// absolute unix seconds on disk, the lease is ended once it passes, and
/// re-lending is one command rather than deleting and re-creating the lease
/// (which would change the id every surface is holding).
///
/// A clock time that has already gone is REFUSED here, and the grant keeps the
/// end it had. That is the retired roll-to-tomorrow: an operator who types a
/// time that has passed is told so rather than lent another day they did not
/// ask for.
///
/// Watch it fail by dropping `grant.until = end` from the `Lend` arm: the file
/// then has no end and the `ended` assertion below never fires.
#[test]
fn lend_for_a_duration_writes_an_end_and_relend_clears_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let before = u64::try_from(tcr::now_ms() / 1_000).expect("a positive clock");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--for",
            "2h",
        ],
    );
    assert!(ok, "lend --for: {out}{err}");
    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    let grant = &file.peers[0].lend[0];
    let until = grant.until.expect("`--for 2h` wrote an end");
    assert!(
        until >= before + 7_200 && until <= before + 7_260,
        "two hours ahead of now, in absolute unix seconds: {until} vs {before}"
    );
    assert!(!grant.ended, "two hours ahead has not passed");
    let id = tcr::peer::config::lease_id_string(grant.id);

    // A clock that has gone is REFUSED through the same CLI, and the end the
    // grant already had is left alone. It used to roll to tomorrow, which is
    // the behaviour this build retired: an operator who types a time that has
    // passed gets told so rather than silently lent another day.
    //
    // `00:00:01` has gone at every instant of the day but its first second,
    // which is the same window any bare clock has and the reason the refusal
    // names what it resolved to.
    let (out, err, ok) = run_tcr_peer(
        &peers,
        &["lend", &node, "--relend", &id, "--until", "00:00:01"],
    );
    assert!(
        !ok,
        "a clock that has passed is not an end this build will write: {out}{err}"
    );
    assert!(
        err.contains("already passed today"),
        "and the refusal says why, naming what it resolved to: {out}{err}"
    );
    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(
        file.peers[0].lend[0].until,
        Some(until),
        "a refused relend changes nothing: the two-hour end is still the one on disk"
    );
    let (out, _, _) = run_tcr_peer(&peers, &["lend", &node, "--list"]);
    assert!(
        out.contains("ended=false"),
        "two hours ahead still has not passed: {out}"
    );

    // And `--relend` with no end at all clears it: "re-lend with
    // one click", on the same lease id every surface is already holding.
    let (out, err, ok) = run_tcr_peer(&peers, &["lend", &node, "--relend", &id]);
    assert!(ok, "relend with no end: {out}{err}");
    assert!(out.contains("until=none"), "{out}");
    let file = tcr::peer::config::read_or_default(&peers).expect("the file reads");
    assert_eq!(file.peers[0].lend[0].until, None);
    assert_eq!(
        tcr::peer::config::lease_id_string(file.peers[0].lend[0].id),
        id,
        "re-lending keeps the id, because deleting and re-creating would strand every \
         surface holding the old one"
    );
}

/// **`tcr peer ls --json` carries `lentTo`, keyed by account label, with the
/// lease's real scope and end**, through the binary a
/// panel actually runs.
///
/// The library gates above prove `lent_to` computes the map. This proves the
/// CLI emits it: the block was absent from this output entirely before this
/// fix, and a panel cannot render a line the JSON does not carry. The account
/// labels come from the main config, which is why `--config` exists on this
/// verb.
///
/// `spare-fake` is the control: it is in no group, so a `group:work` lease must
/// not reach it, and its absence from a map that DOES have a `work-fake` key is
/// the assertion that means something.
///
/// Watch it fail by removing `lent_to` from `PeerLsJson`: the block is then
/// absent and the first assertion fails.
#[test]
fn peer_ls_json_carries_the_lent_to_line_keyed_by_account_label() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("tcr.json");
    // Obviously fake accounts: no real email, no org uuid. One in group `work`,
    // one in none.
    std::fs::write(
        &config,
        r#"{"accounts":[
             {"name":"work-fake","accessToken":"not-a-real-token","groups":["work"]},
             {"name":"spare-fake","accessToken":"not-a-real-token"}
           ]}"#,
    )
    .expect("the config fixture writes");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--scope",
            "group:work",
            "--for",
            "2h",
        ],
    );
    assert!(ok, "lend: {out}{err}");

    let json = ls_json(&peers, &config);

    let lent = &json["lentTo"];
    let row = &lent["work-fake"][0];
    assert_eq!(
        row["scope"]["group"], "work",
        "the grouped account's line names the scope it was lent under: {lent}"
    );
    assert_eq!(
        row["ended"], false,
        "two hours ahead has not passed: {lent}"
    );
    assert!(
        row["until"].is_u64(),
        "the end is absolute unix seconds: {lent}"
    );
    assert!(
        row["id"].is_string(),
        "the lease id is a string, never a number no double can hold: {lent}"
    );
    assert_eq!(row["peer"], "attic-nuc");

    assert!(
        lent.get("spare-fake").is_none(),
        "a `group:work` lease does not reach an account outside the group: {lent}"
    );
}

/// **`tcr peer ls --json` carries the four readouts the panel's Peers tab
/// draws: a grant's `mode`, its `handedKeyUntil`, the `internet` switch, and
/// the `exits` map.**
///
/// Every one of them was absent from this output, and a panel cannot draw a
/// key the JSON does not carry. They are asserted together and from one
/// invocation on purpose: they are one screen, and a reader that had to run
/// the binary four times would be reading four different instants.
///
/// The controls are the assertions that mean something. `mode` is read off the
/// SERVE grant as well as the hand one, because the serve value used to be
/// omitted as a serde default and an absent key reads as "unknown" to a
/// decoder that does not know the default. `handedKeyUntil` is absent on the
/// serve grant, because a serve grant hands nothing over whatever else is
/// true. And `spare-fake`, an account with no exit lock at all, is absent from
/// `exits` beside a `pinned-fake` that is present.
///
/// Watched red four ways, one per key: restore `skip_serializing_if =
/// "LendMode::is_serve"` on `LendGrant::mode`, return `None` from
/// `peer_ls_handed_key_until`, drop `internet` from `PeerLsJson`, and return
/// an empty map from `peer_ls_exits`.
#[test]
fn peer_ls_json_carries_the_mode_the_handed_key_the_switch_and_the_exits() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("tcr.json");
    // Obviously fake accounts, no real email and no org uuid. `work-fake` is
    // the one a hand grant can reach; `pinned-fake` carries the exit lock;
    // `spare-fake` has neither and is the control on both maps.
    //
    // The expiry is a fixed instant far ahead rather than a clock read, so the
    // assertion below is an equality and not a range.
    let expires_at_ms: i64 = 1_893_456_000_000;
    std::fs::write(
        &config,
        format!(
            r#"{{"accounts":[
                 {{"name":"work-fake","accessToken":"not-a-real-token","groups":["work"],
                   "expiresAt":{expires_at_ms}}},
                 {{"name":"pinned-fake","accessToken":"not-a-real-token",
                   "egress":"via {node}","egressStrict":true}},
                 {{"name":"spare-fake","accessToken":"not-a-real-token"}}
               ]}}"#
        ),
    )
    .expect("the config fixture writes");

    // A hand grant on the group the unpinned account is in, and a serve grant
    // beside it on the other window: two grants, two modes, one row.
    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--scope",
            "group:work",
            "--mode",
            "hand",
            "--config",
            config.to_str().expect("a utf-8 path"),
        ],
    );
    assert!(ok, "the hand lend: {out}{err}");
    let (out, err, ok) = run_tcr_peer(
        &peers,
        &["lend", &node, "--window", "5h", "--fraction", "0.2"],
    );
    assert!(ok, "the serve lend: {out}{err}");
    let (out, err, ok) = run_tcr_peer(&peers, &["internet", "on"]);
    assert!(ok, "internet on: {out}{err}");

    let json = ls_json(&peers, &config);

    let lend = json["peers"][0]["lend"]
        .as_array()
        .unwrap_or_else(|| panic!("the row carries its grants: {json}"))
        .clone();
    let hand = lend
        .iter()
        .find(|grant| grant["window"] == "7d")
        .unwrap_or_else(|| panic!("the hand grant is on the 7d window: {json}"));
    let serve = lend
        .iter()
        .find(|grant| grant["window"] == "5h")
        .unwrap_or_else(|| panic!("the serve grant is on the 5h window: {json}"));

    assert_eq!(hand["mode"], "hand", "the hand grant says so: {json}");
    assert_eq!(
        serve["mode"], "serve",
        "and so does the serve grant, rather than omitting the key and leaving a \
         decoder to guess the default: {json}"
    );
    assert_eq!(
        hand["handedKeyUntil"],
        serde_json::json!(expires_at_ms / 1_000),
        "the handed key's deadline is the covered account's own expiry, in unix \
         SECONDS: {json}"
    );
    assert!(
        serve.get("handedKeyUntil").is_none(),
        "a serve grant hands nothing over, so it carries no deadline: {json}"
    );

    assert_eq!(
        json["internet"], true,
        "the switch `tcr peer internet on` just set: {json}"
    );

    let exits = &json["exits"];
    let pinned = &exits["pinned-fake"];
    assert_eq!(
        pinned["egress"],
        format!("via {node}"),
        "the exit lock is reported in the file's own words: {json}"
    );
    assert_eq!(
        pinned["egressStrict"], true,
        "and with its strictness, which is the half that decides what an \
         unreachable route costs: {json}"
    );
    assert_eq!(
        pinned["peerDown"], true,
        "the pinned Mac has no endpoint on its row, so there is no way to reach \
         it right now: {json}"
    );
    assert!(
        pinned.get("waitingSeconds").is_none(),
        "this Mac has never heard from it, and `waiting since never` is not a \
         duration worth printing: {json}"
    );
    assert!(
        exits.get("spare-fake").is_none(),
        "an account with no pin and no strictness is absent rather than present \
         saying `local`: {json}"
    );
}

/// **An account whose own name is not a label still gets the "Lent
/// to …" line, through the binary a panel runs.**
///
/// `tcr peer ls --json` built its account list with
/// `filter_map(|a| sanitize_label(&a.name).ok())`, so an account named by
/// email (which is what an OAuth account is usually named here), was dropped
/// from `lentTo` outright. Its card then showed no line however much of it was
/// lent, and the two facts the filter conflated are different ones: that
/// account cannot be NAMED by `--scope account:<label>`, and it is very much
/// lent under an `all` grant.
///
/// The key is the name `tcr status` prints, because that is what a panel
/// matches its account card against; `lease::lent_to` sanitizes it itself for
/// the scope match, so `spare-fake` below stays out of a `group:work` lease
/// exactly as before.
///
/// Watch it fail by restoring the `filter_map(... .ok())` in `main.rs`'s
/// `peer ls --json` arm: the `alice@example.com` key disappears while every
/// other assertion here still passes.
#[test]
fn peer_ls_json_keeps_an_account_whose_name_is_not_a_label() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (peers, node) = cli_peers_file(dir.path());
    let config = dir.path().join("tcr.json");
    // `alice@example.com` is the obviously-fake email this repository uses in
    // every fixture, and an `@` is exactly what `sanitize_label` refuses.
    std::fs::write(
        &config,
        r#"{"accounts":[
             {"name":"alice@example.com","accessToken":"not-a-real-token"},
             {"name":"spare-fake","accessToken":"not-a-real-token"}
           ]}"#,
    )
    .expect("the config fixture writes");

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "lend",
            &node,
            "--window",
            "7d",
            "--fraction",
            "0.2",
            "--scope",
            "all",
        ],
    );
    assert!(ok, "lend: {out}{err}");

    let json = ls_json(&peers, &config);

    let lent = &json["lentTo"];
    // The positive control first: the account whose name IS a label is there,
    // so an absence below is about the un-labelled one and not about an empty
    // `lentTo`.
    assert_eq!(
        lent["spare-fake"][0]["peer"], "attic-nuc",
        "the control account is lent under `all`: {lent}"
    );
    assert_eq!(
        lent["alice@example.com"][0]["peer"], "attic-nuc",
        "an `all` lease lends every account, including the one whose name is not a label: \
         {lent}"
    );
    assert_eq!(
        lent["alice@example.com"][0]["scope"], "all",
        "and the line names the scope it was lent under: {lent}"
    );
}

/// **A grant whose scope this Mac cannot hold a request inside is SKIPPED, and
/// the next grant on the same window mints instead.**
///
/// The grant list is positional (several leases, one per scope),
/// so `grant_for` returned the first row on the window whatever its scope. A
/// `--scope account:bob` row naming an account this lender does not carry
/// therefore shadowed the `all` row beneath it: the lease minted against the
/// dead scope and `handle_serve_on` refused every request on it with
/// `Unsupported`, while a grant that could have served sat one line below,
/// never reached.
///
/// The enforceability answer is the production one: `Manager`'s own
/// `scope_restriction`, the same function the serving leg asks: so a grant
/// skipped here and a request refused there cannot disagree.
///
/// Watch it fail by making the `enforceable` closure in `Ledger::grant` return
/// `true` unconditionally: the lease then mints at 0.05 from the `account:bob`
/// row, which is the fraction no request can spend.
#[test]
fn an_unenforceable_grant_is_skipped_and_the_next_one_on_the_window_mints() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let borrower = PeerId([24_u8; 32]);

    // Two grants on ONE window. The first names an account this fleet does not
    // have; the second is the `all` grant the operator also wrote.
    let mut named = LendGrant::new(Window::SevenDay, 0.05, 300, 1);
    named.scope = tcr_peer_wire::LendScope::Accounts(vec!["bob".to_string()]);
    let everything = LendGrant::new(Window::SevenDay, 0.20, 300, 2);
    let store = lender_store(&peers, borrower, vec![named, everything]);

    // The lender's real fleet: one account, and it is not `bob`.
    let manager = fleet::lending_manager("http://127.0.0.1:1", Default::default());

    let ask = LeaseRequest {
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 300,
        max_inflight: 2,
    };
    let mut ledger = Ledger::new();
    ledger.note_owner_headroom(Window::SevenDay, 0.40);
    let lease = ledger
        .grant(&borrower, &ask, &store, manager.as_ref())
        .answer
        .lease
        .expect("the `all` grant one row down mints a lease");

    assert_eq!(
        lease.unit,
        LeaseUnit::Fraction(0.20),
        "the lease came from the `all` grant (0.20), not from the unenforceable \
         `account:bob` row above it (0.05)"
    );
    assert_eq!(
        ledger.scope_of(lease.lease_id),
        tcr_peer_wire::LendScope::All,
        "and the scope recorded beside it is the one that can actually be served"
    );

    // The control, on the SAME fleet: a scope this Mac CAN hold is not skipped.
    // Without this, a `grant_for` that skipped every scoped grant would pass
    // the assertions above.
    let mut serveable = LendGrant::new(Window::FiveHour, 0.07, 300, 1);
    serveable.scope = tcr_peer_wire::LendScope::Accounts(vec!["lender-fake".to_string()]);
    let five_hour_all = LendGrant::new(Window::FiveHour, 0.30, 300, 2);
    let store = lender_store(&peers, borrower, vec![serveable, five_hour_all]);
    let five_hour_ask = LeaseRequest {
        window: Window::FiveHour,
        unit: LeaseUnit::Fraction(0.50),
        ttl_s: 300,
        max_inflight: 2,
    };
    ledger.note_owner_headroom(Window::FiveHour, 0.40);
    let kept = ledger
        .grant(&borrower, &five_hour_ask, &store, manager.as_ref())
        .answer
        .lease
        .expect("an enforceable scoped grant mints");
    assert_eq!(
        kept.unit,
        LeaseUnit::Fraction(0.07),
        "`account:lender-fake` names an account this fleet carries, so it is the grant that \
         mints: the skip is about enforceability and not about scopes in general"
    );
}

/// **A malformed grant inside a `lend` LIST is refused with its own line and
/// column**, which is what `read_peer_file` promises the operator.
///
/// `one_or_many_grants` was `#[serde(untagged)]`, and an untagged enum buffers
/// its input before trying each arm. The position it then reports is where the
/// BUFFER ran out, not where the bad field is: for this fixture it named line
/// 11 (where the list closes), for a defect on line 9, under the message
/// "data did not match any variant of untagged enum `OneOrMany`", which names
/// neither the field nor anything an operator can act on.
///
/// Watch it fail by putting the untagged enum back: measured, the refusal then
/// reads `... untagged enum OneOrMany at line 11 column 5`, and the first
/// assertion below names it.
#[test]
fn a_malformed_grant_inside_a_list_is_refused_with_its_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let node = PeerId([31_u8; 32]).to_wire();
    // The second grant's `fraction` is a string. Line 9 of this fixture, and
    // the reader has to say so.
    write_raw_peers(
        &peers,
        &format!(
            r#"{{
  "peers": [
    {{
      "node": "{node}",
      "label": "attic-nuc",
      "addedAt": 1,
      "lend": [
        {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }},
        {{ "window": "5h", "fraction": "a lot", "ttlS": 300, "maxInflight": 1 }}
      ]
    }}
  ]
}}"#
        ),
    );

    let refusal = tcr::peer::config::read_or_default(&peers)
        .expect_err("a grant whose fraction is a string is not a readable peers file")
        .to_string();

    assert!(
        refusal.contains("line 9"),
        "the refusal names the line the bad grant is on: {refusal}"
    );
    assert!(
        !refusal.contains("OneOrMany"),
        "a buffered untagged enum's refusal names its own enum and not the operator's \
         field, which is the message this stopped producing: {refusal}"
    );
    assert!(
        refusal.contains("fraction") || refusal.contains("invalid type"),
        "and it says what was wrong with it, in serde's own words: {refusal}"
    );

    // The positive control: the same file with a valid second grant reads, so
    // the refusal above is about the bad field and not about the list shape.
    assert_the_list_shape_reads(dir.path(), &node);
}
