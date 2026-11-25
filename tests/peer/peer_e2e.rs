//! The two-process end-to-end: two real `tcr` servers on one box, driven only
//! through the CLI and the peer port.
//!
//! Every other peer test in this tree reaches for a library function. This one
//! may not: it spawns the `tcr` this build produced (`CARGO_BIN_EXE_tcr`) twice,
//! points each copy at its own scratch `HOME`, and then types at them: `tcr
//! peer name`, `find`, `pair`, `pending`, `accept`, `invite`, `join`, `share`,
//! `lend`, `allow`, `forget`: reading the answer off stdout, off each process's
//! own log, and off the fake upstream. That is the surface an operator has, and
//! three of the findings below are invisible from inside the library.
//!
//! # Isolation
//!
//! Same rules as `tests/proxy/headless_sigterm.rs` and `tests/cli/first_run_no_config.rs`,
//! set on the spawned [`Command`] and never on this process: `HOME` is a fresh
//! [`tempfile::TempDir`], `XDG_CACHE_HOME` is a directory under it (so the
//! affinity pins, the usage ledger, the owner claim and `peer-state.json` all
//! land in the tempdir), and `TCR_CLAUDE_CODE_CREDENTIALS` names a file that
//! does not exist: a scratch `HOME` does not scratch the login Keychain, and
//! on a developer's Mac that read finds a REAL credential. Both proxies bind
//! `--port 0`, which also short-circuits the whole incumbent question in
//! `server::serve`, so the live proxy on `127.0.0.1:3456` is never probed, never
//! signalled and never connected to. The peer listeners bind two ports the
//! kernel just handed back on `127.0.0.1`. The upstream is an axum router in
//! this test binary. Nothing here reads the operator's config directory or
//! cache directory, and no
//! binary is copied anywhere.
//!
//! Accounts are obviously fake (`lender-fake`, `at-fake-lender`, uuids of one
//! repeated digit): this repository is public.
//!
//! # What this found that a library test could not
//!
//! Four gaps, each named at the assertion that pins today's behaviour. The
//! assertions are written so that the day
//! one is FIXED, this test goes red and says which line to flip.
//!
//! 1. `pair::confirm` (`src/peer/pair.rs:670`) pins with `None` for the
//!    address, so a Mac trusted through the six-digit compare has a row it can
//!    never dial: `fallback::peer_lease_provider` wants `!addrs.is_empty()` and
//!    `serve::dial_peer` has nothing to connect to. The invite/join path does
//!    record one, which is why the borrowing leg below enrols that way.
//! 2. `pair::mint_invite_as` (`:242`) copies `listen` into the token verbatim,
//!    so a peers file holding `127.0.0.1:0` (or `0.0.0.0:9600`) mints a join key
//!    whose address nobody can dial. This test writes a fixed loopback port for
//!    that reason, which is worth knowing before an operator is told to use
//!    `:0`.
//! 3. Closed: the peer-lease fallback used to install at boot only, so a Mac
//!    that paired while its proxy was up could not borrow until it restarted.
//!    `fallback::install_late_if_the_file_now_allows_it` now re-asks the same
//!    question the next time the dry-fleet arm needs an answer, so step 7
//!    below runs no restart at all: it grants `disclose` on a live process and
//!    the very next request is served through the provider that install call
//!    put there.
//! 4. `listener::serve_control` (`src/peer/listener.rs:1486`) answers `Hello`
//!    and `Ping` and refuses everything else, so a real `Control::LeaseRequest`
//!    (the first frame of every borrow), is refused by the lender with no log
//!    line of its own. Step 7 asserts the served 200 directly (the diagnostic
//!    probe this file used to gate behind `TCR_E2E_EXPECT_LEASE_ARM=1` is now
//!    the assertion, unconditional): until LEASE-WIRE's arm lands in this
//!    tree, this test is RED there, and the failure names the line above.
//! 5. `serve::say_hello` (`src/peer/serve.rs:1339`) is the whole client half of
//!    the moved-peer refresh: it dials a pinned peer, sends a `Control::Hello`,
//!    and the listener's own answering half already records the caller's
//!    address (`listener.rs:1734`, wired and green today). It had no caller
//!    anywhere in this tree from the day it was written until
//!    `tcr peer hello <peer>` got wired to it, so a Mac that moved had no way to tell a
//!    peer where it went without re-pairing.
//!    `a_moved_peer_is_reached_through_a_refreshed_hello_endpoint` below runs
//!    against that verb and is no longer `#[ignore]`d. The second path, "through
//!    the beacon bridge with Hello disabled," needs a second, larger piece
//!    that is not written, not even ignored: the
//!    instance-id-to-key binding `discovery::observe_beacons`
//!    (`discovery.rs:541`) needs comes only from a session that already
//!    proved the peer's CURRENT boot's key, and nothing in this tree holds
//!    one of those across the life of a moved peer's new boot.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tcr::log_err::LogErr as _;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::any;

/// The E2E-MATRIX scenarios' one shared line format. See that module's docs.
#[path = "../tools/e2e/matrix.rs"]
mod matrix;

/// What the real API refuses a request without, shared by every fake upstream
/// in this tree. See that module's docs for why a fake upstream is strict.
use crate::api_contract;

/// The pairing path between two Macs, and the N-node ring.
#[path = "peer_e2e/pairing.rs"]
mod pairing;

/// The borrowing path: one lender, a chained borrow, inflight caps across one and
/// two lenders, and the comparison measurements.
#[path = "peer_e2e/borrowing.rs"]
mod borrowing;

/// Reaching a peer whose address is not the recorded one: a moved peer, a
/// forwarded borrow, a lender nobody can dial and a join key's second address.
#[path = "peer_e2e/reaching.rs"]
mod reaching;

/// The E2E-MATRIX scenarios the ring and the forward tests do not cover.
#[path = "peer_e2e/matrix_scenarios.rs"]
mod matrix_scenarios;

/// How long any one "wait for this line" step may take. Generous because a
/// debug-profile `tcr` boots a manager, mints a CA and binds two sockets;
/// bounded because a step that never happens has to fail rather than hang.
const LINE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the borrowing leg keeps re-asking for a served answer: long
/// enough to cover the lender's 30-second headroom ticker
/// (`server::boot_peer_listener`), because a lender that has just learned its
/// own quota cannot grant anything until the next note.
const BORROW_SERVED_ATTEMPTS: u32 = 20;

/// One line per step, so a human watching `cargo test -- --nocapture` and
/// `scripts/peer-e2e-local.sh` see the same trace.
fn step(n: u32, what: &str) {
    println!("STEP {n}: {what}");
}

// ---------------------------------------------------------------------------
// The fake upstream
// ---------------------------------------------------------------------------

/// What the upstream was handed, per request: the path and the credential.
///
/// The credential is the whole point of the file: a borrowed request must be
/// served on the LENDER's token, and the borrower's own must never appear here
///: see [`Upstream::credentials`].
#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Upstream {
    fn credentials(&self) -> Vec<String> {
        match self.seen.lock() {
            Ok(seen) => seen.iter().map(|(_, cred)| cred.clone()).collect(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .map(|(_, cred)| cred.clone())
                .collect(),
        }
    }

    fn requests(&self) -> usize {
        self.seen.lock().map_or(0, |seen| seen.len())
    }

    /// Whether a request for `path` on `credential` arrived after the first
    /// `before` requests. The upstream records an arrival before it starts
    /// holding the answer, so for [`spawn_slow_upstream`] a `true` here means
    /// that request is in flight right now.
    fn saw_since(&self, before: usize, path: &str, credential: &str) -> bool {
        self.seen.lock().is_ok_and(|seen| {
            seen.iter()
                .skip(before)
                .any(|(seen_path, seen_cred)| seen_path == path && seen_cred == credential)
        })
    }
}

/// A fake `api.anthropic.com`: records the credential, answers a canned body and
/// reports a LOW utilization on all three windows, which is what gives the
/// lender a measured window to lend a fraction of.
///
/// It is [`spawn_upstream_with_trip`] with the trip never armed, so the two
/// answer every request the same way until a test arms one.
async fn spawn_upstream() -> (String, Upstream) {
    let (url, upstream, _never_armed) = spawn_upstream_with_trip(String::new()).await;
    (url, upstream)
}

// ---------------------------------------------------------------------------
// One Mac
// ---------------------------------------------------------------------------

/// How many times one Mac may lose the peer-port race before its boot is a
/// failure rather than a retry. See [`Mac::boot`] for why the race exists at
/// all and why it is retried instead of removed.
const PORT_RACE_ATTEMPTS: u32 = 5;

/// A port the kernel just handed back, released before it is written into a
/// peers file.
///
/// The peer listener needs its port in the file BEFORE the process boots
/// (`server::boot_peer_listener` returns early without `listen`), and
/// `tcr peer invite` copies that value into the token it mints: so `:0` is not
/// usable here even though the listener itself would accept it. See gap 2 in
/// this file's docs.
fn free_loopback_port() -> u16 {
    tcr::test_support::loopback::unused_port()
}

/// How many draws [`loopback_port_other_than`] makes before it calls the
/// runner broken rather than unlucky.
const PORT_REDRAW_ATTEMPTS: u32 = 20;

/// A port from `draw` that is not `excluded`.
///
/// The kernel may hand back a port the moment it is released, so a test
/// that shuts a peer down and draws a "fresh" port can get the old one again.
/// CI hit exactly that in `a_moved_peer_is_reached_through_a_refreshed_hello_endpoint`.
/// `draw` is a parameter so `a_port_redraw_skips_the_excluded_port` can force
/// that repeat every time instead of waiting for it.
fn loopback_port_other_than(excluded: u16, mut draw: impl FnMut() -> u16) -> u16 {
    for _ in 0..PORT_REDRAW_ATTEMPTS {
        let port = draw();
        if port != excluded {
            return port;
        }
    }
    panic!(
        "the kernel handed back the excluded port {excluded} {PORT_REDRAW_ATTEMPTS} times in a row; \
         that is a runner defect, not bad luck"
    );
}

#[test]
fn a_port_redraw_skips_the_excluded_port() {
    let mut draws = [4000, 4000, 4000, 5000].into_iter();
    let port = loopback_port_other_than(4000, || draws.next().expect("a stubbed draw"));
    assert_eq!(
        port, 5000,
        "the excluded port must never come back as the fresh one"
    );
}

#[test]
#[should_panic(expected = "handed back the excluded port 4000")]
fn a_port_redraw_that_only_sees_the_excluded_port_fails_loudly() {
    loopback_port_other_than(4000, || 4000);
}

/// One `tcr` install: its own HOME, config, peers file, node key and logs.
struct Mac {
    label: &'static str,
    home: tempfile::TempDir,
    peers: PathBuf,
    peer_port: u16,
    /// The running server, if this Mac is booted.
    child: Option<Child>,
    /// The log of the CURRENT boot. A new file per boot, never appended to,
    /// because a restart that re-used one log makes every "wait for the boot
    /// line" match the PREVIOUS boot's line and read a dead port.
    log: PathBuf,
    boots: u32,
    /// `host:port` of this boot's proxy.
    proxy: String,
}

impl Mac {
    /// A Mac with one fake account, pointed at `upstream`.
    ///
    /// `disabled` is how the borrower gets a DRY fleet with a non-empty
    /// `accounts[]`: the dry-fleet arm is the only place a fallback provider is
    /// consulted, and a fleet of one disabled account reaches it while still
    /// proving the account count in the 429 body.
    fn new(
        label: &'static str,
        upstream: &str,
        account: &str,
        token: &str,
        disabled: bool,
    ) -> Self {
        let home = tempfile::tempdir().expect("a scratch HOME");
        let config_dir = home.path().join(".config");
        std::fs::create_dir_all(&config_dir).expect("the scratch .config");
        let disabled = if disabled {
            r#", "disabled": true"#
        } else {
            ""
        };
        std::fs::write(
            config_dir.join("tcr.json"),
            format!(
                r#"{{
  "proxy": {{ "port": 0 }},
  "upstream": "{upstream}",
  "quotaProbeSeconds": 0,
  "warmupSeconds": 0,
  "accounts": [
    {{
      "name": "{account}",
      "accessToken": "{token}",
      "accountUuid": "11111111-1111-1111-1111-111111111111",
      "orgUuid": "22222222-2222-2222-2222-222222222222"{disabled}
    }}
  ]
}}
"#
            ),
        )
        .expect("the scratch config writes");

        let peer_port = free_loopback_port();
        let peers = config_dir.join("tcr-peers.json");
        std::fs::write(
            &peers,
            format!("{{\"listen\":\"127.0.0.1:{peer_port}\",\"peers\":[]}}\n"),
        )
        .expect("the scratch peers file writes");
        // `config::read_or_default` refuses a peers file it did not write:
        // "has mode 644, expected 0600". A test that skipped this got a proxy
        // with no peer port and no clue why.
        std::fs::set_permissions(&peers, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the peers file takes 0600");

        Self {
            label,
            peers,
            peer_port,
            child: None,
            log: home.path().join("boot-0.log"),
            boots: 0,
            proxy: String::new(),
            home,
        }
    }

    fn command(&self) -> Command {
        let mut command = crate::tcr_env::tcr(self.home.path());
        command
            // Colour off at the source. A child that inherits an unset
            // `NO_COLOR` writes escape codes into its boot log even though the
            // log is a file, because the subscriber never asks whether its sink
            // is a terminal (`tracing_subscriber::fmt::Layer::default`). The
            // searchers strip escapes too (`strip_ansi`), so neither half alone
            // is load-bearing; this half also keeps the logs readable when a
            // failing run prints one.
            .env("NO_COLOR", "1")
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"));
        command
    }

    /// Boot this Mac's proxy and wait until both of its sockets are up.
    ///
    /// # Why this retries
    ///
    /// [`free_loopback_port`] borrows a port from the kernel and gives it back
    /// before writing it into the peers file, so between that call and the
    /// child's own bind the number belongs to nobody, and under a full
    /// `cargo test` run, where other binaries are opening sockets at the same
    /// time, another process takes it. The proxy still comes up (it binds
    /// `:0`), the peer listener logs `could not bind the peer socket` and
    /// carries on, and this boot then waits thirty seconds for a
    /// `peer listener up` line that will never arrive. Measured at e64fd59
    /// on an otherwise-unmodified tree: two runs, two different victims
    /// (`a_chained_borrow_leaf_from_middle_middle_from_root`, then
    /// `n_macs_pair_in_a_ring`), the same panic at the same line.
    ///
    /// Reading the port off the child instead would be the better fix and is
    /// not available: the peer listener never binds at all unless `listen` is
    /// already in the peers file ([`free_loopback_port`]'s own docs), and
    /// `tcr peer invite` mints the number into a token, so `:0` cannot be
    /// deferred to the child. So the race is retried rather than removed, and
    /// the assertion below keeps it VISIBLE: a boot that needed all five
    /// attempts is not a passing test, it is a runner that has stopped having
    /// free ports.
    fn boot(&mut self) {
        assert!(
            self.child.is_none(),
            "{} is already booted; shut it down before booting again",
            self.label
        );
        let mut lost_the_port = 0;
        while !self.boot_once() {
            lost_the_port += 1;
            assert!(
                lost_the_port < PORT_RACE_ATTEMPTS,
                "{}: lost the peer port to another process {lost_the_port} times in a row. \
                 That is no longer a race, so this is a runner defect and not a retry: \
                 something on this box is taking loopback ports as fast as the kernel \
                 hands them out.",
                self.label
            );
            self.move_listen_port(free_loopback_port());
        }
    }

    /// Move this Mac's peer listener to `port`: point the peers file at it,
    /// leaving every other key, including any pinned rows a previous boot
    /// wrote, exactly as it was.
    fn move_listen_port(&mut self, port: u16) {
        self.peer_port = port;
        let body = std::fs::read_to_string(&self.peers).expect("the peers file reads back");
        let mut file: serde_json::Value =
            serde_json::from_str(&body).expect("the peers file is JSON");
        file["listen"] = serde_json::Value::String(format!("127.0.0.1:{}", self.peer_port));
        std::fs::write(
            &self.peers,
            format!(
                "{}\n",
                serde_json::to_string(&file).expect("it re-serializes")
            ),
        )
        .expect("the peers file rewrites");
        std::fs::set_permissions(
            &self.peers,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("the peers file takes 0600");
    }

    /// One boot attempt. `false` means the peer listener lost its port to
    /// another process and the child has been stopped again; every other
    /// failure still panics, because only this one is a race worth retrying.
    fn boot_once(&mut self) -> bool {
        self.boots += 1;
        self.log = self.home.path().join(format!("boot-{}.log", self.boots));
        let out = std::fs::File::create(&self.log).expect("the boot log opens");
        let err = out.try_clone().expect("the boot log clones for stderr");
        let mut command = self.command();
        command
            .args(["--headless", "--port", "0", "--no-replace"])
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        let child = command.spawn().unwrap_or_else(|err| {
            panic!(
                "spawning the tcr this build produced ({}) failed: {err}",
                env!("CARGO_BIN_EXE_tcr")
            )
        });
        self.child = Some(child);

        let listening = self.wait_for_log("listening on http://");
        self.proxy = listening
            .rsplit_once("http://")
            .map_or_default(|(_, rest)| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            });
        assert!(
            self.proxy.starts_with("127.0.0.1:"),
            "{}: could not read a loopback proxy address out of {listening:?}",
            self.label
        );
        // Re-read rather than assert while `peer_listen` is missing: the
        // waiters now skip a line the child has not finished writing, and a
        // line that arrives without the field would otherwise be read as a
        // listener on the wrong port. Bounded by the same deadline as every
        // other wait here, so a line that never grows the field still fails,
        // and fails saying which line it was.
        let deadline = Instant::now() + LINE_TIMEOUT;
        let announced = loop {
            let (which, peer_line) =
                self.wait_for_one_of(&["peer listener up", "could not bind the peer socket"]);
            if which == 1 {
                // The port went to somebody else between `free_loopback_port` and
                // this child's own bind. Stop the child and let `boot` try again on
                // a fresh number; the line is printed so a run that hit the race
                // says so in its output rather than only in a timing difference.
                println!(
                    "         {}: lost peer port {} to another process, retrying: {peer_line}",
                    self.label, self.peer_port
                );
                self.shutdown();
                return false;
            }
            if let Some(announced) = field(&peer_line, "peer_listen") {
                break announced;
            }
            assert!(
                Instant::now() < deadline,
                "{}: the peer listener line never carried peer_listen within {LINE_TIMEOUT:?}: {peer_line:?}",
                self.label
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(
            announced,
            format!("127.0.0.1:{}", self.peer_port),
            "{}: the peer listener bound an address other than the one its peers file names",
            self.label
        );
        true
    }

    /// The whole of this boot's log.
    fn log(&self) -> String {
        strip_ansi(&std::fs::read_to_string(&self.log).unwrap_or_default())
    }

    /// Block until this boot's log holds a line containing `needle`, and return
    /// that line. Fails with the log's tail, which is the only useful thing to
    /// look at when a step never happened.
    /// Block until this boot's log holds a line containing ONE of `needles`,
    /// and return which one matched with the line itself.
    ///
    /// Separate from [`Self::wait_for_log`] because a wait on a single success
    /// line cannot tell a slow boot from a boot that has already printed the
    /// reason it will never get there, and thirty seconds later the failure it
    /// reports is "no line", never the refusal that is sitting in the log.
    /// Earliest needle wins when a line matches two.
    fn wait_for_one_of(&self, needles: &[&str]) -> (usize, String) {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let body = self.log();
            for line in complete_lines(&body).rev() {
                if let Some(which) = needles.iter().position(|needle| line.contains(needle)) {
                    return (which, line.to_string());
                }
            }
            assert!(
                Instant::now() < deadline,
                "{}: no log line containing any of {needles:?} within {LINE_TIMEOUT:?}. Tail:\n{}",
                self.label,
                tail(&body, 12)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_log(&self, needle: &str) -> String {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let body = self.log();
            if let Some(line) = complete_lines(&body)
                .rev()
                .find(|line| line.contains(needle))
            {
                return line.to_string();
            }
            assert!(
                Instant::now() < deadline,
                "{}: no log line containing {needle:?} within {LINE_TIMEOUT:?}. Tail:\n{}",
                self.label,
                tail(&body, 12)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `tcr peer <args> --peers <this Mac's peers file>`, not yet run.
    fn peer_command(&self, args: &[&str]) -> Command {
        let mut command = self.command();
        command
            .arg("peer")
            .args(args)
            .args(["--peers", self.peers.to_str().expect("a utf-8 peers path")]);
        command
    }

    /// A file in this Mac's home directory.
    fn home_file(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    /// Run `tcr peer <args> --peers <this Mac's peers file>` and hand back
    /// (stdout, stderr, success).
    fn peer(&self, args: &[&str]) -> (String, String, bool) {
        let out = self
            .peer_command(args)
            .output()
            .unwrap_or_else(|err| panic!("{}: spawning `tcr peer {args:?}`: {err}", self.label));
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    }

    /// [`Self::peer`], refusing anything but exit 0. Returns stdout.
    fn peer_ok(&self, args: &[&str]) -> String {
        let (stdout, stderr, ok) = self.peer(args);
        assert!(
            ok,
            "{}: `tcr peer {args:?}` exited non-zero.\nstdout: {stdout}\nstderr: {stderr}",
            self.label
        );
        stdout
    }

    /// `tcr peer status --json` against THIS Mac's running proxy, parsed.
    ///
    /// The block the panel's live half reads, off the live process, which is a
    /// different fact from what the peers file says, and the only way to catch
    /// a block that is derived correctly and never assigned.
    fn status_json(&self) -> serde_json::Value {
        // This Mac's own config says `"port": 0`, the kernel picks one at
        // boot, so the port to ASK on is the one the boot line reported, not
        // the one on disk. A side config carrying it is written here rather
        // than rewriting the Mac's own, which the running server also owns.
        let asking = self.home.path().join("asking-config.json");
        let port = self.proxy.rsplit_once(':').map_or_default(|(_, port)| port);
        std::fs::write(
            &asking,
            format!("{{\n  \"proxy\": {{ \"port\": {port} }},\n  \"accounts\": []\n}}\n"),
        )
        .expect("the asking config writes");

        let mut command = self.command();
        command.args([
            "peer",
            "status",
            "--json",
            "--config",
            asking.to_str().expect("a utf-8 config path"),
        ]);
        let out = command
            .output()
            .unwrap_or_else(|err| panic!("{}: spawning `tcr status --json`: {err}", self.label));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{}: `tcr peer status --json` exited non-zero.\nstdout: {stdout}\nstderr: {}",
            self.label,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|err| panic!("{}: `peer status --json` is not JSON: {err}", self.label))
    }

    /// `tcr peer ls --json`, parsed.
    fn ls(&self) -> serde_json::Value {
        let stdout = self.peer_ok(&["ls", "--json"]);
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|err| panic!("{}: `peer ls --json` is not JSON: {err}", self.label))
    }

    /// The first pinned row's full wire node id: the form `allow` and `lend`
    /// take, never the `tcr-…` short form `tcr peer id` prints.
    fn first_pinned_node(&self) -> String {
        let ls = self.ls();
        ls["peers"][0]["node"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "{}: no pinned peer to read a node id from: {ls}",
                    self.label
                )
            })
            .to_string()
    }

    /// Every pinned row's full wire node id, in the order the peers file holds
    /// them.
    ///
    /// For a Mac pinned to more than one peer, where
    /// [`Self::first_pinned_node`] answers a question with two answers: the
    /// caller reads the list and picks by what it already knows (the id it
    /// recorded after the FIRST join is the one to skip), rather than trusting
    /// an order this test does not set.
    fn pinned_nodes(&self) -> Vec<String> {
        let ls = self.ls();
        let rows = ls["peers"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: `peer ls --json` has no peers array: {ls}", self.label));
        rows.iter()
            .map(|row| {
                row["node"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{}: a pinned row with no node: {ls}", self.label))
                    .to_string()
            })
            .collect()
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().log_debug("killing a test child");
            child.wait().log_debug("reaping a test child");
        }
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The lines of `body` the writer has FINISHED: everything up to its last
/// newline. Whatever follows that newline is still being written, so it is not
/// a line yet and no waiter here may match on it.
///
/// A tracing event reaches these logs as its message first and its fields
/// behind it, so a reader that catches one in flight sees
/// `peer listener up (a second socket; ...)` with no `peer_listen=` after it.
/// That half line is what the macOS CI job matched on 2026-09-19: all twelve
/// tests in this file panicked at the same assertion, every one of them
/// `left: ""` against the port its peers file names, while the same code
/// passed locally because a 50 ms poll never lands inside the write on an idle
/// machine. The emitters are not at fault: both `src/proxy/server/mod.rs` and
/// `src/peer/listener.rs` carry `peer_listen` on that event, so a matched line
/// without it was never a whole line. How the write came apart is not measured
/// here: polling a live boot log on a fast machine caught no mid-line read in
/// 8346 of them, which is why the waiters stop trusting the tail rather than
/// the writer.
fn complete_lines(body: &str) -> std::str::Lines<'_> {
    let finished = body.rfind('\n').map_or(0, |at| at + 1);
    body[..finished].lines()
}

#[test]
fn a_half_written_line_is_not_a_line() {
    let whole = "INFO server: peer listener up (a second socket) peer_listen=127.0.0.1:49894\n";
    let half = "INFO server: peer listener up (a second socket)";
    let body = format!("{whole}{half}");
    assert_eq!(
        complete_lines(&body).next_back(),
        Some(whole.trim_end()),
        "the last finished line is the one carrying peer_listen, not the half line under it"
    );
    assert_eq!(
        complete_lines(half).next(),
        None,
        "a body with no newline in it holds no finished line at all"
    );
    let announced = complete_lines(&body)
        .rev()
        .find(|line| line.contains("peer listener up"))
        .and_then(|line| field(line, "peer_listen"));
    assert_eq!(
        announced.as_deref(),
        Some("127.0.0.1:49894"),
        "a waiter that skips the half line reads the address the emitter announced"
    );
}

/// Every ANSI escape sequence out of some child output.
///
/// # Why the tests need this
///
/// `tracing_subscriber`'s `Layer::default` turns colour ON whenever `NO_COLOR`
/// is unset, and it never asks whether the sink is a terminal, so a child
/// whose stdout is a plain file still gets escapes. Every spawn here now sets
/// `NO_COLOR=1`, but a searcher that only works on plain text is one dropped
/// `env` call away from the failure that cost this file a CI round: locally
/// the tool shell exports `NO_COLOR=1`, the runner does not, and
/// `field(line, "peer_listen")` looked for a literal `peer_listen=` that had
/// become `\e[3mpeer_listen\e[0m\e[2m=\e[0m`. Stripping here makes the
/// searchers hold either way.
///
/// Handles the two forms the formatter emits: CSI (`\e[` … final byte in
/// `@`-`~`) and a two-byte escape such as `\e(B`.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        // A CSI sequence runs until its final byte, `@` through `~`. Anything
        // else is the two-byte form: the escape and the one character after it,
        // both already consumed.
        if chars.next() == Some('[') {
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

/// `key=value` out of one tracing line, where the value runs to the next space.
///
/// Colour-blind by construction: see [`strip_ansi`].
fn field(line: &str, key: &str) -> Option<String> {
    let line = strip_ansi(line);
    let marker = format!("{key}=");
    let start = line.find(&marker)? + marker.len();
    Some(
        line[start..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

/// The exact boot line a CI runner produced at 8668af1, where `NO_COLOR` is
/// unset and every field name and `=` sits inside an escape sequence. Red
/// before [`strip_ansi`]: `field` searched for a literal `peer_listen=` that
/// is not in this line at all, and all twelve tests in this file timed out
/// thirty seconds later saying the field never arrived.
#[test]
fn a_coloured_boot_line_still_yields_the_peer_address() {
    let coloured = "\u{1b}[2m2026-09-19T16:08:55Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mtcr::server\u{1b}[0m\u{1b}[2m:\u{1b}[0m peer listener up (a second socket; the local /_tcr/ gate is untouched) \u{1b}[3mpeer_listen\u{1b}[0m\u{1b}[2m=\u{1b}[0m127.0.0.1:34865 \u{1b}[3mnode\u{1b}[0m\u{1b}[2m=\u{1b}[0mtcr-EXAMPLENODE";
    assert_eq!(
        field(coloured, "peer_listen").as_deref(),
        Some("127.0.0.1:34865"),
        "the address reads out of a line whose field name and `=` are wrapped in escapes"
    );
    assert_eq!(
        field(coloured, "node").as_deref(),
        Some("tcr-EXAMPLENODE"),
        "the last field on a coloured line ends at the line, not at an escape"
    );
    assert!(
        !strip_ansi(coloured).contains('\u{1b}'),
        "stripping leaves no escape byte behind"
    );
    assert!(
        strip_ansi(coloured).contains("peer_listen=127.0.0.1:34865"),
        "a stripped line carries the plain `key=value` the searchers look for"
    );
    let plain = "INFO server: peer listener up peer_listen=127.0.0.1:34865";
    assert_eq!(
        strip_ansi(plain),
        plain,
        "a line with no escapes in it comes back unchanged"
    );
}

/// The six digits out of `code=NNNNNN` or `shows NNNNNN`.
fn six_digits(haystack: &str) -> Option<String> {
    let after_marker = |marker: &str| {
        haystack.rsplit_once(marker).map(|(_, rest)| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
    };
    after_marker("code=")
        .or_else(|| after_marker("this Mac shows "))
        .filter(|digits| digits.len() == 6)
}

fn tail(body: &str, lines: usize) -> String {
    let all: Vec<&str> = body.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// One POST at a proxy, answered or not. Returns (status, body).
async fn post_messages(proxy: &str) -> (u16, String) {
    let client = tcr::http_client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("a loopback client");
    let answered = client
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        // What a real client sends, and what the API refuses a message request
        // without: see `tests/tools/api_contract.rs`.
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap_or_else(|err| panic!("the proxy at {proxy} did not answer: {err}"));
    let status = answered.status().as_u16();
    let body = answered.text().await.unwrap_or_default();
    (status, body)
}

/// `tcr peer join --stdin`, which is the path the panel and the `tcr://` handler
/// use: the key is piped, so it never enters an argument vector.
fn join_with_stdin(mac: &Mac, key: &str) -> String {
    let mut command = mac.peer_command(&["join", "--stdin", "--label", "borrower-mac"]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("{}: spawning `tcr peer join`: {err}", mac.label));
    child
        .stdin
        .as_mut()
        .expect("the join process's stdin")
        .write_all(format!("{key}\n").as_bytes())
        .expect("the join key writes to stdin");
    let out = child.wait_with_output().expect("the join process exits");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{}: `tcr peer join --stdin` exited {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        mac.label,
        out.status.code()
    );
    assert!(
        stdout.contains("peer join: ok"),
        "the join must report the enrolment: {stdout}"
    );
    // The secret is a live bearer token for its ten minutes. Neither stream may
    // echo it, and the failure paths print more than the success path. The
    // field is taken off the END rather than by stripping a version prefix: the
    // prefix moves between builds, and a strip that quietly matched nothing
    // would leave this assertion comparing against the whole key, which no
    // surface prints anyway.
    let secret = key.rsplit(':').next().unwrap_or(key);
    assert!(
        !stdout.contains(secret) && !stderr.contains(secret),
        "the join key appeared in this command's own output"
    );
    stdout
}

/// Like [`spawn_upstream`], but any request bearing `trip_credential` answers
/// a DURABLE quota rejection (`anthropic-ratelimit-unified-status: rejected`
/// plus a scoped `5h-status: rejected`, which is the ONE 429 arm
/// `src/proxy/mod.rs` parks the account on via `Manager::mark_rate_limited`,
/// never the transient/no-guidance arm a few lines below it) the FIRST time
/// the returned flag is armed, then reverts to the canned 200 for every other
/// credential and every later hit. This is how the chained-borrow test forces
/// one Mac's own account into `AccountStatus::Throttled` without touching any
/// file that Mac did not already have a reason to read.
async fn spawn_upstream_with_trip(trip_credential: String) -> (String, Upstream, Arc<AtomicBool>) {
    let upstream = Upstream::default();
    let recorder = upstream.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let armed_for_handler = Arc::clone(&armed);
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let recorder = recorder.clone();
        let armed = Arc::clone(&armed_for_handler);
        let trip_credential = trip_credential.clone();
        async move {
            let path = req.uri().path().to_string();
            let credential = req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let api_key = req
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            // The origin's own refusal, decided before the body is drained and
            // answered after the arrival is recorded: see
            // `tests/tools/api_contract.rs`.
            let refusal = api_contract::refuse_if_incomplete(req.method(), req.headers());
            axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .log_debug("reading a test request body");
            let full_credential = format!("{credential}{api_key}");
            if let Ok(mut seen) = recorder.seen.lock() {
                seen.push((path, full_credential.clone()));
            }
            if let Some(refusal) = refusal {
                return refusal;
            }
            if full_credential == trip_credential && armed.swap(false, Ordering::SeqCst) {
                return Response::builder()
                    .status(429)
                    .header("content-type", "application/json")
                    .header("retry-after", "3600")
                    .header("anthropic-ratelimit-unified-status", "rejected")
                    .header("anthropic-ratelimit-unified-5h-status", "rejected")
                    .body(Body::from(
                        br#"{"type":"error","error":{"type":"rate_limit_error"}}"#.to_vec(),
                    ))
                    .expect("the canned 429 builds");
            }
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-status", "allowed")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
                .body(Body::from(
                    br#"{"type":"message","id":"msg_fake"}"#.to_vec(),
                ))
                .expect("the canned upstream answer builds")
        }
    }));
    (
        tcr::test_support::stub_upstream::http(app).url(),
        upstream,
        armed,
    )
}

/// Like [`spawn_upstream`], but every answer is held open for `delay` before
/// it is sent: long enough to make several requests fired at once genuinely
/// overlap in flight, which is what the concurrent-borrow test needs to prove
/// `max_inflight` binds DURING a request rather than only between them.
async fn spawn_slow_upstream(delay: Duration) -> (String, Upstream) {
    let upstream = Upstream::default();
    let recorder = upstream.clone();
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let recorder = recorder.clone();
        async move {
            let path = req.uri().path().to_string();
            let credential = req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let api_key = req
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            // The origin's own refusal, decided before the body is drained and
            // answered after the arrival is recorded: see
            // `tests/tools/api_contract.rs`.
            let refusal = api_contract::refuse_if_incomplete(req.method(), req.headers());
            axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .log_debug("reading a test request body");
            if let Ok(mut seen) = recorder.seen.lock() {
                seen.push((path, format!("{credential}{api_key}")));
            }
            tokio::time::sleep(delay).await;
            if let Some(refusal) = refusal {
                return refusal;
            }
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-status", "allowed")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
                .body(Body::from(
                    br#"{"type":"message","id":"msg_fake"}"#.to_vec(),
                ))
                .expect("the canned upstream answer builds")
        }
    }));
    (tcr::test_support::stub_upstream::http(app).url(), upstream)
}

/// Step 1 of every forwarded-borrow leg: three Macs on one upstream, each with
/// its own fake account (only `borrower` may borrow), booted in that order.
fn boot_lender_carrier_borrower(upstream: &str) -> (Mac, Mac, Mac) {
    let mut lender = Mac::new("lender", upstream, "lender-fake", "at-fake-lender", false);
    let mut carrier = Mac::new(
        "carrier",
        upstream,
        "carrier-fake",
        "at-fake-carrier",
        false,
    );
    let mut borrower = Mac::new(
        "borrower",
        upstream,
        "borrower-fake",
        "at-fake-borrower",
        true,
    );

    step(1, "boot three tcr processes: lender, carrier and borrower");
    lender.boot();
    carrier.boot();
    borrower.boot();
    (lender, carrier, borrower)
}

/// One ordinary request on `lender`'s own proxy, which must be served: it is
/// what gives the lender a measured window to lend a fraction of.
async fn warm(lender: &Mac) {
    let (warm, _) = post_messages(&lender.proxy).await;
    assert_eq!(
        warm, 200,
        "{} must serve its own request before it can lend a fraction of it",
        lender.label
    );
}

/// Five concurrent `POST /v1/messages` through `proxy`, every one back inside
/// [`LINE_TIMEOUT`]. Their statuses, in task order, printed for the trace.
async fn five_concurrent_borrows(proxy: &str) -> Vec<u16> {
    let deadline = Instant::now() + LINE_TIMEOUT;
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let proxy = proxy.to_string();
            tokio::spawn(async move { post_messages(&proxy).await })
        })
        .collect();
    let mut statuses = Vec::new();
    for handle in handles {
        let (status, _) = handle.await.expect("a borrow task panicked");
        statuses.push(status);
    }
    assert!(
        Instant::now() < deadline,
        "all five concurrent borrows must return within the borrow deadline"
    );
    println!("         statuses: {statuses:?}");
    statuses
}

/// Invite `borrower` onto `lender`, lend it a `max_inflight: 2` lease, and let
/// it disclose to that lender: the five CLI steps every borrowing leg in this
/// file repeats, in one place because the two-lender test performs them twice
/// and a second hand-written copy is a second chance to mistype a fraction.
fn lend_to(lender: &Mac, borrower: &Mac, label: &str) -> String {
    lend_to_with(lender, borrower, label, &[])
}

/// [`lend_to`]'s five steps, with `extra` appended to the `lend` verb's argv.
fn lend_to_with(lender: &Mac, borrower: &Mac, label: &str, extra: &[&str]) -> String {
    let pinned_before = borrower.pinned_nodes();
    let minted = lender.peer_ok(&["invite", "--label", label, "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(borrower, &key);
    let borrower_node = lender.first_pinned_node();
    lender.peer_ok(&["share", "on", "--window", "7d", "--fraction", "0.90"]);
    let mut lend = vec![
        "lend",
        &borrower_node,
        "--window",
        "7d",
        "--fraction",
        "0.90",
        "--ttl",
        "300",
        "--max-inflight",
        "2",
    ];
    lend.extend_from_slice(extra);
    lender.peer_ok(&lend);
    // The row the join ADDED, by difference rather than by position: which end
    // of the peers file a new row lands on is not this test's to know, and an
    // `allow` aimed at the wrong lender would read as a lender that refuses.
    let lender_node_at_borrower = borrower
        .pinned_nodes()
        .into_iter()
        .find(|node| !pinned_before.contains(node))
        .expect("the borrower pinned the lender it just joined");
    borrower.peer_ok(&["allow", &lender_node_at_borrower, "disclose", "on"]);
    lender_node_at_borrower
}

/// Delete every endpoint from one pinned row in a Mac's peers file, leaving
/// every other key and every other row exactly as they were.
///
/// This is the fact the whole case turns on and it is written directly rather
/// than acted out, because the ways a peer's addresses really go stale (it
/// moved, its router mapping expired, it came up on another network) all take
/// wall time this test has no reason to spend. What matters downstream is the
/// row's own content, and `PeerStore` re-reads the file when its mtime moves,
/// so the running borrower sees this within one request.
fn strip_endpoints(mac: &Mac, node: &str) {
    let body = std::fs::read_to_string(&mac.peers).expect("the peers file reads back");
    let mut file: serde_json::Value = serde_json::from_str(&body).expect("the peers file is JSON");
    let rows = file["peers"]
        .as_array_mut()
        .expect("the peers file has a peers array");
    let row = rows
        .iter_mut()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}", mac.label));
    row["endpoints"] = serde_json::Value::Array(Vec::new());
    std::fs::write(
        &mac.peers,
        format!(
            "{}\n",
            serde_json::to_string(&file).expect("it re-serializes")
        ),
    )
    .expect("the peers file rewrites");
    std::fs::set_permissions(
        &mac.peers,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("the peers file takes 0600");
}

/// The endpoints on one Mac's row for `node`, as `tcr peer ls --json` reports
/// them: the running process's own view, not this test's copy of the file.
fn endpoints_for(mac: &Mac, node: &str) -> Vec<serde_json::Value> {
    let ls = mac.ls();
    let rows = ls["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: no peers array: {ls}", mac.label));
    let row = rows
        .iter()
        .find(|row| row["node"].as_str() == Some(node))
        .unwrap_or_else(|| panic!("{}: no pinned row for {node}: {ls}", mac.label));
    row["endpoints"].as_array().cloned().unwrap_or_default()
}

/// Mint an invite on `host`, join `guest` to it, and answer with the node ids
/// each one now holds for the other.
///
/// By DIFFERENCE against the rows each side held first, never by position: a
/// third pairing in this test would otherwise read whichever row the file
/// happens to list first, and the grants below would land on the wrong Mac.
fn join_to(host: &Mac, guest: &Mac, label: &str) -> (String, String) {
    let host_had = host.pinned_nodes();
    let guest_had = guest.pinned_nodes();
    let minted = host.peer_ok(&["invite", "--label", label, "--ttl", "300"]);
    let key = minted
        .lines()
        .find(|line| line.starts_with("tcr-join:"))
        .unwrap_or_else(|| panic!("no join key on stdout: {minted}"))
        .to_string();
    join_with_stdin(guest, &key);
    let guest_at_host = host
        .pinned_nodes()
        .into_iter()
        .find(|node| !host_had.contains(node))
        .unwrap_or_else(|| panic!("{} pinned nothing new when {label} joined", host.label));
    let host_at_guest = guest
        .pinned_nodes()
        .into_iter()
        .find(|node| !guest_had.contains(node))
        .unwrap_or_else(|| panic!("{} pinned nothing new after joining", guest.label));
    (guest_at_host, host_at_guest)
}
