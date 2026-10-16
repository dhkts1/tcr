//! The test proxy's world: a scratch HOME with a config of 18 fake accounts, a bound-token
//! file seeded to its cap, the real `tcr` binary started on a free port with a connect
//! guard injected, and the checks that prove it stayed inside that world.

use std::fs;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use serde_json::{json, Value};
use tcr::bound_tokens::{self, BoundTokenKind, Snapshot, StoredAccount, BOUND_TOKEN_CAP};
use tcr::paths::{self, Roots};

/// How many fake accounts the scratch config holds: the size of the fleet the throttle
/// defaults were tuned on.
pub const ACCOUNTS: usize = 18;

/// The live proxy's port. The connect guard refuses it even on loopback.
pub const LIVE_PORT: u16 = 3456;
/// The live proxy's peer listener. The test proxy must not bind it.
pub const LIVE_PEER_PORT: u16 = 7755;

pub fn account_name(i: usize) -> String {
    format!("loadtest{i:02}@example.com")
}

pub fn account_uuid(i: usize) -> String {
    format!("{i:08}-1111-4111-8111-111111111111")
}

pub fn org_uuid(i: usize) -> String {
    format!("{i:08}-2222-4222-8222-222222222222")
}

/// The scratch config: 18 fake OAuth accounts (one org each, tokens valid for a year),
/// `upstream` on the stub, and every timer that could reach elsewhere switched off.
pub fn config_json(stub_port: u16, throttles_off: bool, now_ms: i64) -> Value {
    let upstream = format!("http://127.0.0.1:{stub_port}");
    let accounts: Vec<Value> = (0..ACCOUNTS)
        .map(|i| {
            json!({
                "name": account_name(i),
                "type": "oauth",
                "accountUuid": account_uuid(i),
                "orgUuid": org_uuid(i),
                "accessToken": format!("sk-ant-oat01-fake-load-test-{i:02}"),
                "refreshToken": format!("sk-ant-ort01-fake-load-test-{i:02}"),
                "expiresAt": now_ms + 365 * 24 * 3_600 * 1_000,
                "priority": 0,
                // Known up front, so the profile backfill has nothing to ask for.
                "organizationType": "claude_max",
                "rateLimitTier": "default_claude_max_20x",
            })
        })
        .collect();
    let mut config = json!({
        "proxy": {"port": 0},
        "upstream": upstream,
        "codexUpstream": upstream,
        // The quota probe and the plan backfill call a hard-coded api.anthropic.com
        // (`probe::USAGE_URL`, `oauth::PROFILE_URL`), so they are off, not redirected.
        "quotaProbeSeconds": 0,
        // The warmer would spend quota; it is off by default and stays off.
        "warmupSeconds": 0,
        "accounts": accounts,
    });
    if throttles_off {
        config["accountThrottle"] = json!({});
        config["fleetThrottle"] = json!({});
    }
    config
}

/// The scratch roots one phase's proxy resolves every path from.
pub struct Scratch {
    pub home: PathBuf,
}

impl Scratch {
    pub fn create(root: &Path) -> anyhow::Result<Self> {
        let home = root.to_path_buf();
        fs::create_dir_all(home.join(".config"))?;
        fs::create_dir_all(home.join(".cache"))?;
        Ok(Self { home })
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join(".config").join(paths::CONFIG_FILE)
    }

    pub fn roots(&self) -> Roots {
        Roots {
            home: Some(self.home.clone()),
            xdg_cache_home: Some(self.home.join(".cache")),
        }
    }

    /// The bound-token file the proxy restores at boot, by the crate's own path rule.
    pub fn bound_token_path(&self) -> PathBuf {
        bound_tokens::path_beside(&paths::cache_dir_in(&self.roots()).affinity_file())
    }
}

/// Fill the bound-token file to [`BOUND_TOKEN_CAP`] with fake tokens spread over the 18
/// accounts and the last 20 hours, through the crate's own writer so the format is always
/// the one the proxy reads.
pub fn seed_bound_tokens(path: &Path, now_ms: i64) -> anyhow::Result<usize> {
    fs::create_dir_all(path.parent().context("the bound-token file has a parent")?)?;
    let mut snapshot = Snapshot::default();
    let slots: Vec<_> = (0..ACCOUNTS)
        .map(|i| {
            snapshot.account_slot(StoredAccount {
                name: account_name(i),
                account_uuid: Some(account_uuid(i)),
                org_uuid: Some(org_uuid(i)),
                org_name: None,
                provider: Default::default(),
            })
        })
        .collect();
    let mut rng = crate::stats::Rng::new(0x5eed);
    for i in 0..BOUND_TOKEN_CAP {
        let kind = if i % 2 == 0 {
            BoundTokenKind::ThinkingSignature
        } else {
            BoundTokenKind::MessageId
        };
        let age = (rng.unit() * 20.0 * 3_600_000.0) as i64;
        snapshot.push(
            bound_tokens::hash_token(&format!("seed-token-{i}")),
            kind,
            slots[i % ACCOUNTS],
            now_ms - age,
        );
    }
    Ok(bound_tokens::save(path, snapshot, now_ms)?)
}

/// The environment the test proxy runs under: cleared, then only scratch paths, the
/// connect guard, and nothing that names a proxy.
fn scratch_env(
    command: &mut Command,
    scratch: &Scratch,
    guard: &Path,
    attempts: &Path,
    denied_port: u16,
) {
    let home = &scratch.home;
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TMPDIR", home.join("tmp"))
        .env("CODEX_HOME", home.join(".codex"))
        // An override path that does not exist is how a run says "this machine has no
        // Claude Code or Codex login" (see `tests/headless_sigterm.rs`).
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-claude-code-login.json"),
        )
        .env("TCR_CODEX_AUTH", home.join("no-codex-login.json"))
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info")
        .env("DYLD_INSERT_LIBRARIES", guard)
        .env("TCR_TEST_DENIED_PORT", denied_port.to_string())
        .env("TCR_TEST_CONNECT_LOG", attempts)
        .current_dir(home);
}

/// Prove the connect guard interposes on THIS binary before trusting it: run `tcr status`
/// against a listener this process owns, with that port denied, and check the guard
/// refused the connection and logged it, and the listener saw nothing.
pub fn guard_positive_control(tcr: &Path, guard: &Path, dir: &Path) -> anyhow::Result<()> {
    let scratch = Scratch::create(&dir.join("guard-control"))?;
    fs::create_dir_all(scratch.home.join("tmp"))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let config = scratch.home.join("control.json");
    fs::write(
        &config,
        json!({"proxy": {"port": port}, "accounts": []}).to_string(),
    )?;
    let attempts = scratch.home.join("attempts");
    let mut command = Command::new(tcr);
    command.args(["status", "--json", "--config"]).arg(&config);
    scratch_env(&mut command, &scratch, guard, &attempts, port);
    let output = command
        .output()
        .context("running the guard's positive control")?;
    listener.set_nonblocking(true)?;
    let accepted = listener.accept().is_ok();
    let logged = fs::read_to_string(&attempts).unwrap_or_default();
    if accepted || !logged.contains("connect") {
        bail!(
            "the connect guard did not interpose on {} (accepted={accepted}, logged={logged:?}, \
             status={:?}); refusing to run a proxy it cannot fence",
            tcr.display(),
            output.status
        );
    }
    Ok(())
}

/// The running test proxy. Dropping it kills the process, so a failed phase never leaves
/// one behind.
pub struct Proxy {
    child: Child,
    /// The proxy's own durable log directory, inside the scratch cache.
    pub log_dir: PathBuf,
    pub stderr: PathBuf,
    pub attempts: PathBuf,
}

impl Proxy {
    /// Start the proxy the way TcrBar does (`ServerController.swift` sends stdout to
    /// `/dev/null`): the durable log under the scratch cache is the only copy of each line,
    /// so the harness pays the same logging cost the shipped app pays, not a second file write.
    pub fn spawn(
        tcr: &Path,
        scratch: &Scratch,
        guard: &Path,
        stderr: &Path,
    ) -> anyhow::Result<Self> {
        fs::create_dir_all(scratch.home.join("tmp"))?;
        let attempts = scratch.home.join("connect-attempts");
        fs::write(&attempts, "")?;
        let mut command = Command::new(tcr);
        command
            .args(["--headless", "--port", "0", "--config"])
            .arg(scratch.config_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(stderr)?);
        scratch_env(&mut command, scratch, guard, &attempts, LIVE_PORT);
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", tcr.display()))?;
        Ok(Self {
            child,
            log_dir: paths::cache_dir_in(&scratch.roots()).log_dir(),
            stderr: stderr.to_path_buf(),
            attempts,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Every line of the durable log so far, the daily files in name (date) order. Empty
    /// before the proxy has created its log directory.
    pub fn log_text(&self) -> anyhow::Result<String> {
        let entries = match fs::read_dir(&self.log_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(err) => return Err(err).context("listing the proxy's log directory"),
        };
        let mut files: Vec<PathBuf> = entries
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        files.sort();
        let mut text = String::new();
        for file in files {
            text.push_str(&fs::read_to_string(&file)?);
        }
        Ok(text)
    }

    /// Wait for the `listening on http://` line and return the port it names.
    pub fn wait_listening(&mut self, timeout: Duration) -> anyhow::Result<u16> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(port) = listening_port(&self.log_text()?) {
                if port == LIVE_PORT {
                    bail!("the test proxy bound the live port {LIVE_PORT}");
                }
                return Ok(port);
            }
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "the test proxy exited before listening ({status}); see {} and {}",
                    self.log_dir.display(),
                    self.stderr.display()
                );
            }
            if Instant::now() > deadline {
                bail!(
                    "the test proxy did not listen within {timeout:?}; see {}",
                    self.log_dir.display()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Connections the guard refused, one line each.
    pub fn refused_connects(&self) -> anyhow::Result<usize> {
        let text = fs::read_to_string(&self.attempts)
            .with_context(|| format!("reading the guard's log {}", self.attempts.display()))?;
        Ok(text.lines().count())
    }

    /// SIGTERM, then wait for the drain; SIGKILL only if it hangs.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        let status = Command::new("/bin/kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()?;
        if !status.success() {
            bail!("could not signal the test proxy");
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.child.kill()?;
        bail!("the test proxy did not exit within 20 s of SIGTERM and was killed")
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            if let Err(err) = self.child.kill() {
                eprintln!(
                    "[load-test] could not kill test proxy {}: {err}",
                    self.child.id()
                );
            }
            if let Err(err) = self.child.wait() {
                eprintln!(
                    "[load-test] could not reap test proxy {}: {err}",
                    self.child.id()
                );
            }
        }
    }
}

/// The port in `tcr listening on http://127.0.0.1:PORT (headless)`.
pub fn listening_port(log: &str) -> Option<u16> {
    let marker = "listening on http://";
    let line = log.lines().find(|l| l.contains(marker))?;
    let after = &line[line.find(marker)? + marker.len()..];
    after
        .split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// The boot lines that name a path, for the report's proof of where the proxy looked.
pub fn boot_path_lines(log: &str) -> Vec<String> {
    log.lines()
        .take_while(|l| !l.contains("listening on http://"))
        .filter(|l| l.contains('/'))
        .map(str::to_string)
        .collect()
}

/// Boot lines that name the operator's own config or cache. Must be empty.
pub fn live_path_mentions(lines: &[String], real_home: &str) -> Vec<String> {
    let live = [
        format!("{real_home}/.config"),
        format!("{real_home}/.cache"),
        format!("{real_home}/Library"),
    ];
    lines
        .iter()
        .filter(|l| live.iter().any(|p| l.contains(p.as_str())))
        .cloned()
        .collect()
}

/// `restored=N` on the `bound-token pins restored` boot line.
pub fn restored_bound_tokens(log: &str) -> Option<usize> {
    let line = log
        .lines()
        .find(|l| l.contains("bound-token pins restored"))?;
    let start = line.find(" restored=")? + " restored=".len();
    line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// What `lsof -nP -a -p <pid> -i` showed: every internet endpoint, and the ones that are
/// not loopback or that touch the live proxy's ports.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SocketCheck {
    pub endpoints: usize,
    pub foreign: Vec<String>,
}

pub fn check_sockets(lsof: &str) -> SocketCheck {
    let mut check = SocketCheck::default();
    for line in lsof.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME [(STATE)]
        let Some(name) = cols.get(8) else {
            continue;
        };
        for endpoint in name.split("->") {
            check.endpoints += 1;
            let Some((host, port)) = endpoint.rsplit_once(':') else {
                check.foreign.push(line.to_string());
                break;
            };
            let loopback = host == "127.0.0.1" || host == "[::1]" || host == "localhost";
            let live = port == LIVE_PORT.to_string() || port == LIVE_PEER_PORT.to_string();
            if !loopback || live {
                check.foreign.push(line.to_string());
                break;
            }
        }
    }
    check
}

/// The process's lifetime peak physical footprint in MiB, from `vmmap --summary`: the figure
/// macOS charges the process for, compressed pages included, which resident size undercounts
/// on a box under memory pressure.
pub fn peak_footprint_mb(pid: u32) -> anyhow::Result<f64> {
    let output = Command::new("/usr/bin/vmmap")
        .args(["--summary", &pid.to_string()])
        .output()
        .context("running vmmap")?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_peak_footprint(&text).with_context(|| {
        format!(
            "vmmap printed no peak footprint ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// `Physical footprint (peak):  49.1M` in MiB.
pub fn parse_peak_footprint(vmmap: &str) -> Option<f64> {
    let line = vmmap
        .lines()
        .find(|l| l.trim_start().starts_with("Physical footprint (peak):"))?;
    let value = line.split(':').nth(1)?.trim();
    let (number, unit) = value.split_at(value.find(|c: char| c.is_ascii_alphabetic())?);
    let number: f64 = number.trim().parse().ok()?;
    let scale = match unit.trim() {
        "K" | "KB" => 1.0 / 1024.0,
        "M" | "MB" => 1.0,
        "G" | "GB" => 1024.0,
        _ => return None,
    };
    Some(number * scale)
}

pub fn lsof(pid: u32) -> anyhow::Result<String> {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-i"])
        .output()
        .context("running lsof")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listening_line_names_the_port() {
        let log = "x\n2026-10-16T10:00:00Z  INFO tcr: tcr listening on http://127.0.0.1:51234 (headless)\n";
        assert_eq!(listening_port(log), Some(51234));
        assert_eq!(listening_port("nothing yet"), None);
    }

    #[test]
    fn restored_count_is_read_from_the_boot_line() {
        let log =
            "INFO tcr::server: bound-token pins restored path=/s/x.json restored=65536 expired=0\n";
        assert_eq!(restored_bound_tokens(log), Some(65_536));
    }

    #[test]
    fn loopback_sockets_pass_and_anything_else_is_named() {
        let lsof = "\
COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
tcr 1 u 9u IPv4 0x1 0t0 TCP 127.0.0.1:50001 (LISTEN)
tcr 1 u 10u IPv4 0x2 0t0 TCP 127.0.0.1:50002->127.0.0.1:50003 (ESTABLISHED)
tcr 1 u 11u IPv4 0x3 0t0 TCP 10.0.0.5:50004->160.79.104.10:443 (ESTABLISHED)
tcr 1 u 12u IPv4 0x4 0t0 UDP *:5353
tcr 1 u 13u IPv4 0x5 0t0 TCP 127.0.0.1:50005->127.0.0.1:3456 (SYN_SENT)
tcr 1 u 14u IPv4 0x6 0t0 TCP 127.0.0.1:7755 (LISTEN)
";
        let check = check_sockets(lsof);
        assert_eq!(check.foreign.len(), 4, "{:?}", check.foreign);
        assert!(check.foreign[0].contains("160.79.104.10"));
        assert!(check.foreign[1].contains("*:5353"));
        assert!(check.foreign[2].contains(":3456"));
        assert!(check.foreign[3].contains(":7755"));
        let clean = check_sockets(&lsof.lines().take(3).collect::<Vec<_>>().join("\n"));
        assert_eq!(
            clean,
            SocketCheck {
                endpoints: 3,
                foreign: vec![]
            }
        );
    }

    #[test]
    fn the_peak_footprint_is_read_in_mebibytes() {
        let vmmap = "Physical footprint:         49.1M\nPhysical footprint (peak):  1.5G\n";
        assert_eq!(parse_peak_footprint(vmmap), Some(1536.0));
        assert_eq!(
            parse_peak_footprint("Physical footprint (peak):  2048K"),
            Some(2.0)
        );
        assert_eq!(parse_peak_footprint("Physical footprint:  49.1M"), None);
    }

    #[test]
    fn a_boot_line_naming_the_live_config_is_caught() {
        let lines = vec![
            "INFO path=/var/folders/x/tcr-load/steady/.cache/tcr/server-tool-pins.json".to_string(),
            "INFO ca=/Users/alice/.config/tcr-ca.pem".to_string(),
        ];
        assert_eq!(
            live_path_mentions(&lines, "/Users/alice"),
            vec![lines[1].clone()]
        );
    }

    #[test]
    fn the_config_switches_off_what_reaches_out_and_points_upstream_at_the_stub() {
        let c = config_json(4321, true, 0);
        assert_eq!(c["upstream"], "http://127.0.0.1:4321");
        assert_eq!(c["codexUpstream"], "http://127.0.0.1:4321");
        assert_eq!(c["quotaProbeSeconds"], 0);
        assert_eq!(c["warmupSeconds"], 0);
        assert_eq!(c["accountThrottle"], json!({}));
        assert_eq!(c["accounts"].as_array().map(Vec::len), Some(ACCOUNTS));
        assert!(c["accounts"][0]["name"]
            .as_str()
            .is_some_and(|n| n.ends_with("@example.com")));
        let throttled = config_json(4321, false, 0);
        assert!(
            throttled.get("accountThrottle").is_none(),
            "defaults stay on"
        );
    }

    #[test]
    fn the_seeded_file_loads_back_at_the_cap_against_the_scratch_accounts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server-tool-pins.json");
        let now = tcr::now_ms();
        assert_eq!(
            seed_bound_tokens(&path, now).expect("seed"),
            BOUND_TOKEN_CAP
        );
        let config_path = dir.path().join("tcr.json");
        fs::write(&config_path, config_json(1, false, now).to_string()).expect("write config");
        // The proxy's own loader, so a quarantined account would show up here as a short list.
        let config = tcr::config::load(&config_path).expect("the scratch config loads");
        assert_eq!(config.accounts.len(), ACCOUNTS);
        let report = bound_tokens::load(
            &path,
            &config.accounts,
            now,
            bound_tokens::BOUND_TOKEN_TTL_MS,
        );
        assert_eq!(report.degraded, None);
        assert_eq!(report.tokens.len(), BOUND_TOKEN_CAP);
        assert_eq!(
            (report.unresolved, report.ambiguous, report.expired),
            (0, 0, 0)
        );
    }
}
