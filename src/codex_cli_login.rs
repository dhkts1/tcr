//! `tcr login --codex`: a Codex (ChatGPT) login of tcr's own.
//!
//! tcr runs `codex login` with `CODEX_HOME` pointed at a throwaway folder under tcr's config
//! directory, reads the `auth.json` codex leaves there, deletes the folder, and imports the login
//! through the same path `tcr login --from-codex` takes ([`crate::oauth::import_codex_login_as`]).
//! codex does the OAuth, so tcr builds no auth of its own, and the login is tcr's alone: the
//! codex CLI keeps the login it has, and the two never spend each other's single-use refresh
//! token.
//!
//! # What `codex login` does under tcr
//!
//! Read in openai/codex's source at f82e365, chiefly `codex-rs/cli/src/login.rs` and
//! `codex-rs/login/src/server.rs`, and in the installed codex-cli 0.162.1's `--help`, not measured
//! on a real sign-in:
//!
//! - It signs in without a TTY and never reads stdin, so the `--non-interactive` form runs it
//!   with stdin closed and both streams piped. It waits for the browser with no timeout of its
//!   own.
//! - It prints the authorize URL on stderr only, alone on its line, after "navigate to this URL
//!   to authenticate:" and a blank line; the URL starts `https://auth.openai.com/oauth/authorize`.
//! - It opens the browser itself, before it prints the URL, and nothing turns that off
//!   ([`CODEX_OPENS_ITS_BROWSER`]).
//! - It holds `127.0.0.1:1455` (else 1457), and a second `codex login` on the same host cancels
//!   the first.
//! - It exits 0 once the browser has loaded its success page, with the tokens already saved, and
//!   1 on every failure; a signal ends it by that signal. With `CODEX_HOME` set, the folder must
//!   already exist, and the login is written to `$CODEX_HOME/auth.json`.
//! - Before it binds, it revokes and deletes any login already in that `CODEX_HOME`. tcr's folder
//!   is new and empty, so there is nothing there to lose.
//!
//! # The folder
//!
//! `tcr-codex-login.*` beside the config, mode `0700`, holding the sign-in's `auth.json` until tcr
//! has read it. Deletion is attempted on every path tcr can observe: success, every refusal,
//! codex's non-zero exit, and SIGTERM, SIGINT or SIGHUP, which also stop `codex login` and everything it
//! started. It is closed as soon as the login is in memory, before the identity check, the
//! import's route probe and its workspace check, which wait on the network. A forced kill of tcr
//! (SIGKILL) runs no handler, and can leave the folder behind, holding a live login; nothing
//! sweeps old folders. A failed cancellation cleanup names the remaining folder and asks the
//! user to delete it by hand.
//!
//! # Signals
//!
//! codex is spawned in tcr's own process group (no `setsid`, no `process_group`), so TcrBar's
//! Cancel (`Process.terminate()`, a SIGTERM to tcr's whole process group), a terminal's Ctrl-C
//! and a hang-up reach codex even when tcr dies before its handler runs. tcr's handler covers the
//! other case, a signal sent to tcr alone: it sends the same signal to codex and every process
//! codex started (an npm `codex` is a `node` launcher running the native binary), waits up to two
//! seconds, kills whatever is left, deletes the folder and exits with 128 plus the signal's
//! number. It signals through sysinfo, one pid at a time, because this crate forbids the unsafe
//! code a `killpg` would need. Every target retains its PID and start time; each signal
//! refreshes and verifies that identity, including the final SIGKILL after the grace wait.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::config::Account;
use crate::oauth::{self, LoginEvent, LoginRefusal, LoginUi};

/// Whether `codex login` opens the browser itself, which makes the `browser` event carry
/// `opened: true` so the caller opens nothing.
///
/// Read in openai/codex at f82e365, `codex-rs/login/src/server.rs` and `codex-rs/cli/src/login.rs`:
/// codex calls `webbrowser::open` on the authorize URL before it prints the URL, ignores the
/// result, and no flag or variable of `codex login` turns that off.
/// No output line can show whether a window actually opened: codex prints "If your browser did
/// not open, navigate to this URL to authenticate:" either way, and the result it ignored is
/// never printed. So this is a fact about codex, fixed at build time, not something read off a
/// run.
pub const CODEX_OPENS_ITS_BROWSER: bool = true;

/// How codex's authorize URL starts: its issuer is `auth.openai.com`, the host of
/// [`crate::codex_oauth::TOKEN_ENDPOINT`].
const AUTHORIZE_URL_PREFIX: &str = "https://auth.openai.com/oauth/authorize";

/// How long a signalled `codex login` and its children get to end before they are killed.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// How long codex's output gets to drain after codex has exited. A process codex left running
/// can hold its pipes open forever, so the read gives up rather than waiting on it.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The terminal's first line, before codex's own output.
pub const CODEX_LOGIN_START_LINE: &str = "[tcr] signing in a Codex account: `codex login` runs \
    in a folder of tcr's own, so the codex CLI keeps the login it has";

/// The terminal's line once the account is saved: its name, its plan as `tcr status` names it
/// (left out when unknown), and how to use it.
pub fn codex_login_saved_line(name: &str, plan: Option<&str>) -> String {
    let plan = plan.map_or_else(String::new, |plan| format!(" ({plan})"));
    format!(
        "[tcr] '{name}' is a Codex account{plan}: Codex sessions started with `tcr run --codex` \
         can use it"
    )
}

/// The refusal of a `codex login` that ended without leaving a login behind. `status` is its
/// exit code, or `signal <n>`; `said` is the last line codex wrote, when the caller has one
/// (the `--non-interactive` form, where codex's output went to a pipe rather than the screen).
pub fn codex_login_failed_refusal(status: &str, said: Option<&str>) -> LoginRefusal {
    let said = said.map_or_else(String::new, |line| format!(" (codex said: {line})"));
    LoginRefusal {
        code: LoginRefusal::CODEX_LOGIN_FAILED,
        reason: format!(
            "`codex login` exited with {status} before a login was saved{said}. Nothing was \
             written."
        ),
    }
}

/// `tcr login --codex`. Returns the name the account was saved under.
///
/// In order, and nothing is written and nothing is spawned before the folder is made:
/// the UI; `--account`'s row ([`oauth::codex_relogin_target`]); the route, as every login checks
/// it, refused before codex runs ([`oauth::codex_login_route_check`]); `codex` itself
/// ([`crate::codex_local::find_codex`]); the signal handlers; the folder; `codex login`; the
/// login it left; the identity check; the import; the notices.
pub async fn login(
    config_path: &Path,
    force: bool,
    account: Option<&str>,
    name: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let target = match account {
        Some(query) => {
            let config = oauth::load_or_default(config_path)?;
            let idx = oauth::codex_relogin_target(&config, query)?;
            Some(config.accounts[idx].clone())
        }
        None => None,
    };
    oauth::codex_login_route_check(config_path, force).await?;
    let codex = crate::codex_local::find_codex().map_err(|miss| LoginRefusal {
        code: LoginRefusal::CODEX_MISSING,
        reason: miss.refusal(),
    })?;
    if ui == LoginUi::Terminal {
        eprintln!("{CODEX_LOGIN_START_LINE}");
    }
    let mut signals = Signals::arm()?;
    let session = Arc::new(Session::new(throwaway_folder(config_path)?));
    let flow = sign_in(
        Arc::clone(&session),
        codex.path,
        config_path,
        force,
        account,
        name,
        target,
        ui,
    );
    tokio::pin!(flow);
    let result = tokio::select! {
        biased;
        signal = signals.recv() => match stop_and_exit(&session, signal).await {},
        result = &mut flow => result,
    };
    let closed = session.close_folder();
    match (result, closed) {
        (Ok(saved), Ok(())) => Ok(saved),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => {
            // The machine protocol extracts LoginRefusal's reason, so attach cleanup to that
            // reason as well as preserving its code instead of hiding it in anyhow context.
            if let Some(refusal) = error.downcast_ref::<LoginRefusal>() {
                Err(LoginRefusal {
                    code: refusal.code,
                    reason: format!("{}; {cleanup:#}", refusal.reason),
                }
                .into())
            } else {
                Err(error.context(format!("{cleanup:#}")))
            }
        }
    }
}

/// The flow from `codex login` to the notices, with the folder already made.
#[allow(clippy::too_many_arguments)]
async fn sign_in(
    session: Arc<Session>,
    codex: PathBuf,
    config_path: &Path,
    force: bool,
    account: Option<&str>,
    name: Option<&str>,
    target: Option<Account>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    let folder = session.folder_path();
    let ran = run_codex_login(&session, &codex, &folder, ui).await?;
    let auth = crate::paths::codex_auth_file(&folder);
    let status = describe(ran.status);
    let said = ran.said.as_deref();
    if !ran.status.success() {
        return Err(codex_login_failed_refusal(&status, said).into());
    }
    let Some(login) = crate::codex_login::read_codex_login(&auth)? else {
        return Err(codex_login_failed_refusal(&status, said).into());
    };
    // The login is in memory: nothing below needs the folder, and a cancel during the network
    // waits ahead must not find it still on disk.
    session.close_folder()?;
    if let Some(row) = &target {
        refuse_another_identity(row, &login)?;
    }
    let first = oauth::holds_no_codex_row(config_path)?;
    let plan = tcr_status_wire::codex_plan_label(login.plan_type.as_deref());
    let saved = oauth::import_codex_login_as(
        oauth::LOGIN_CODEX_COMMAND,
        config_path,
        force,
        name,
        account,
        login,
        None,
    )
    .await?;
    let first_account = if first {
        Some(oauth::read_first_codex_account_notice(config_path).await)
    } else {
        None
    };
    let notices = crate::codex_local::codex_notices(first_account);
    match ui {
        LoginUi::Machine => {
            oauth::say_codex_notices(ui, &notices);
            ui.emit(LoginEvent::SavedCodex {
                account: &saved,
                plan: plan.as_deref(),
            });
        }
        LoginUi::Terminal => {
            eprintln!("{}", codex_login_saved_line(&saved, plan.as_deref()));
            oauth::say_codex_notices(ui, &notices);
        }
    }
    Ok(saved)
}

/// Refuse a login that is not `row`'s account: a different ChatGPT user, or the row's user in
/// another workspace, or a login that does not name both. Keyed on the user id, never the
/// email: a Codex row stores no email, and its name may be a `--name`.
fn refuse_another_identity(
    row: &Account,
    login: &crate::codex_login::CodexLogin,
) -> anyhow::Result<()> {
    let names_both = login.user_id.is_some() && login.account_id.is_some();
    if names_both && oauth::codex_login_is_row(row, login) {
        return Ok(());
    }
    let same_user = login.user_id.is_some()
        && crate::identity::uuid_key_of(login.user_id.as_deref())
            == crate::identity::uuid_key_of(row.account_uuid.as_deref());
    let refusal = match (&login.account_id, &row.org_uuid) {
        (Some(got), Some(want)) if same_user => oauth::codex_relogin_workspace_refusal(
            login.email.as_deref(),
            got,
            login.plan_type.as_deref(),
            &row.name,
            want,
            row.organization_type.as_deref(),
        ),
        _ => oauth::codex_identity_refusal(
            login.email.as_deref().unwrap_or("a login with no email"),
            &row.name,
        ),
    };
    Err(refusal.into())
}

/// What a finished `codex login` left: how it ended, and the last line it said (the
/// `--non-interactive` form only).
struct Ran {
    status: std::process::ExitStatus,
    said: Option<String>,
}

/// `status` as the failed-login refusal names it: the exit code, or `signal <n>`.
fn describe(status: std::process::ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => code.to_string(),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "an unknown status".to_string(),
    }
}

/// Run `codex login` with `CODEX_HOME` at `folder`, in tcr's process group, and wait for it.
///
/// The terminal keeps codex's own stdio, so its output shows as codex wrote it. The
/// `--non-interactive` form closes stdin and pipes both streams: every line goes on to tcr's
/// stderr unchanged, the first authorize URL becomes the `browser` and `waiting` events, and the
/// last non-empty line of stderr, else of stdout, is kept for the refusal.
async fn run_codex_login(
    session: &Arc<Session>,
    codex: &Path,
    folder: &Path,
    ui: LoginUi,
) -> anyhow::Result<Ran> {
    let mut command = std::process::Command::new(codex);
    command
        .arg("login")
        .env(crate::codex_login::CODEX_HOME_ENV, folder)
        .env("PATH", crate::codex_local::codex_login_child_path(codex));
    if ui == LoginUi::Machine {
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to launch `{}`", codex.display()))?;
    let identity = process_identity(child.id()).filter(|identity| identity.start_time > 0);
    if identity.is_none() {
        tracing::warn!(
            pid = child.id(),
            "could not record codex login process identity; cancellation cannot signal an unverified PID"
        );
    }
    *session.codex_process.lock().expect("process lock poisoned") = identity;

    let output = Arc::new(Output::default());
    let (done, drained) = std::sync::mpsc::channel::<()>();
    let mut readers = 0;
    if let Some(stdout) = child.stdout.take() {
        readers += 1;
        spawn_reader(stdout, Stream::Out, ui, session, &output, done.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        readers += 1;
        spawn_reader(stderr, Stream::Err, ui, session, &output, done.clone());
    }
    drop(done);

    let waiting = Arc::clone(session);
    let status = tokio::task::spawn_blocking(move || {
        let status = child.wait();
        waiting.reaped.store(true, Ordering::SeqCst);
        status
    })
    .await
    .context("waiting for `codex login`")?
    .context("waiting for `codex login`")?;

    tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + DRAIN_GRACE;
        for _ in 0..readers {
            let left = deadline.saturating_duration_since(Instant::now());
            if drained.recv_timeout(left).is_err() {
                break;
            }
        }
    })
    .await
    .context("reading `codex login`'s output")?;
    // Whatever a straggler still writes is codex's, not this login's: nothing more is said.
    session.quiet.store(true, Ordering::SeqCst);
    let said = output.said();
    Ok(Ran { status, said })
}

/// Which of codex's streams a reader reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Out,
    Err,
}

/// What the readers learned from codex's output.
#[derive(Default)]
struct Output {
    url_sent: AtomicBool,
    last_err: Mutex<Option<String>>,
    last_out: Mutex<Option<String>>,
}

impl Output {
    /// The last non-empty line codex wrote to stderr, else to stdout.
    fn said(&self) -> Option<String> {
        let err = self.last_err.lock().expect("output lock poisoned").clone();
        err.or_else(|| self.last_out.lock().expect("output lock poisoned").clone())
    }
}

/// Copy one of codex's streams to tcr's stderr, line by line, on a thread of its own.
fn spawn_reader(
    pipe: impl std::io::Read + Send + 'static,
    stream: Stream,
    ui: LoginUi,
    session: &Arc<Session>,
    output: &Arc<Output>,
    done: std::sync::mpsc::Sender<()>,
) {
    let session = Arc::clone(session);
    let output = Arc::clone(output);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if session.quiet.load(Ordering::SeqCst) {
                continue;
            }
            let _ = std::io::stderr().write_all(&raw);
            let line = String::from_utf8_lossy(&raw);
            let cleaned: String = line.chars().filter(|c| !c.is_control()).collect();
            let cleaned = cleaned.trim();
            if !cleaned.is_empty() {
                let last = match stream {
                    Stream::Out => &output.last_out,
                    Stream::Err => &output.last_err,
                };
                *last.lock().expect("output lock poisoned") = Some(cleaned.to_string());
            }
            if let Some(url) = line
                .split_whitespace()
                .find(|token| token.starts_with(AUTHORIZE_URL_PREFIX))
                && !output.url_sent.swap(true, Ordering::SeqCst)
            {
                if CODEX_OPENS_ITS_BROWSER {
                    ui.emit(LoginEvent::BrowserOpened { url });
                } else {
                    ui.emit(LoginEvent::Browser { url });
                }
                ui.emit(LoginEvent::Waiting);
            }
        }
        let _ = done.send(());
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

impl ProcessIdentity {
    fn of(process: &sysinfo::Process) -> Self {
        Self {
            pid: process.pid().as_u32(),
            start_time: process.start_time(),
        }
    }
}

const fn signal_target_is_current(expected: ProcessIdentity, current: ProcessIdentity) -> bool {
    expected.pid == current.pid
        && expected.start_time > 0
        && expected.start_time == current.start_time
}

/// What the signal handler and the flow share: the folder, codex's identity, whether codex has been
/// reaped, and whether tcr has stopped speaking.
struct Session {
    folder: Mutex<Option<tempfile::TempDir>>,
    path: PathBuf,
    codex_process: Mutex<Option<ProcessIdentity>>,
    reaped: AtomicBool,
    quiet: AtomicBool,
}

impl Session {
    fn new(folder: tempfile::TempDir) -> Self {
        Self {
            path: folder.path().to_path_buf(),
            folder: Mutex::new(Some(folder)),
            codex_process: Mutex::new(None),
            reaped: AtomicBool::new(false),
            quiet: AtomicBool::new(false),
        }
    }

    fn folder_path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Delete the folder now, if it is still there, with [`tempfile::TempDir::close`]: a
    /// `std::process::exit` runs no destructor, and a failed delete must be said rather than
    /// dropped, since the folder may hold a live login.
    fn close_folder(&self) -> anyhow::Result<()> {
        let folder = self.folder.lock().expect("folder lock poisoned").take();
        match folder {
            Some(folder) => folder.close().with_context(|| {
                format!(
                    "removing {}, which may hold the login codex just made: delete it by hand",
                    self.path.display()
                )
            }),
            None => Ok(()),
        }
    }
}

/// The throwaway `CODEX_HOME`: `tcr-codex-login.*` beside the config, mode `0700`, created
/// here, so it exists before codex runs (codex refuses a `CODEX_HOME` that does not).
fn throwaway_folder(config_path: &Path) -> anyhow::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::paths::codex_login_parent(config_path);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    tempfile::Builder::new()
        .prefix(crate::paths::CODEX_LOGIN_PREFIX)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(dir)
        .with_context(|| format!("creating a sign-in folder in {}", dir.display()))
}

/// SIGTERM, SIGINT and SIGHUP, held from before the folder exists until `login` returns.
struct Signals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl Signals {
    fn arm() -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate()).context("arming SIGTERM")?,
            interrupt: signal(SignalKind::interrupt()).context("arming SIGINT")?,
            hangup: signal(SignalKind::hangup()).context("arming SIGHUP")?,
        })
    }

    /// The next of the three to arrive.
    async fn recv(&mut self) -> Caught {
        tokio::select! {
            _ = self.terminate.recv() => Caught::Terminate,
            _ = self.interrupt.recv() => Caught::Interrupt,
            _ = self.hangup.recv() => Caught::Hangup,
        }
    }
}

/// A signal [`Signals`] caught.
#[derive(Clone, Copy)]
enum Caught {
    Terminate,
    Interrupt,
    Hangup,
}

impl Caught {
    /// Its number, the same on macOS and Linux.
    const fn number(self) -> i32 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }

    const fn as_sysinfo(self) -> sysinfo::Signal {
        match self {
            Self::Hangup => sysinfo::Signal::Hangup,
            Self::Interrupt => sysinfo::Signal::Interrupt,
            Self::Terminate => sysinfo::Signal::Term,
        }
    }
}

/// Stop `codex login` and everything it started, delete the folder, and exit with 128 plus the
/// signal's number. A failed folder deletion is reported before exiting, since it can leave
/// a live credential behind. It never returns.
async fn stop_and_exit(session: &Session, caught: Caught) -> std::convert::Infallible {
    session.quiet.store(true, Ordering::SeqCst);
    let root = *session.codex_process.lock().expect("process lock poisoned");
    let reaped = || session.reaped.load(Ordering::SeqCst);
    if let Some(root) = root.filter(|_| !reaped()) {
        let family = family_of(root);
        for &member in &family {
            if member.pid != root.pid || !reaped() {
                signal(member, caught.as_sysinfo());
            }
        }
        wait_for(&reaped, STOP_GRACE).await;
        for &member in &family {
            if member.pid != root.pid || !reaped() {
                signal(member, sysinfo::Signal::Kill);
            }
        }
        wait_for(&reaped, STOP_GRACE).await;
    }
    if let Err(error) = session.close_folder() {
        eprintln!("[tcr] {error:#}");
    }
    std::process::exit(128 + caught.number());
}

/// Poll `done` until it holds or `limit` passes.
async fn wait_for(done: &dyn Fn() -> bool, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !done() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Snapshot the owned root and its descendants, retaining each process's start time.
fn family_of(root: ProcessIdentity) -> Vec<ProcessIdentity> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let Some(process) = system.process(Pid::from_u32(root.pid)) else {
        return Vec::new();
    };
    if !signal_target_is_current(root, ProcessIdentity::of(process)) {
        return Vec::new();
    }
    let mut family = vec![root];
    let mut next = 0;
    while next < family.len() {
        let parent = family[next].pid;
        for process in system.processes().values() {
            let identity = ProcessIdentity::of(process);
            if process.parent().map(sysinfo::Pid::as_u32) == Some(parent)
                && !family.iter().any(|member| member.pid == identity.pid)
            {
                family.push(identity);
            }
        }
        next += 1;
    }
    family
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(Pid::from_u32(pid)).map(ProcessIdentity::of)
}

/// Refresh and verify the captured identity before every signal. A missing process or a zombie
/// needs nothing, and a signal the platform cannot send is logged.
fn signal(expected: ProcessIdentity, signal: sysinfo::Signal) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(expected.pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let Some(process) = system.process(Pid::from_u32(expected.pid)) else {
        return;
    };
    if process.status() == ProcessStatus::Zombie
        || !signal_target_is_current(expected, ProcessIdentity::of(process))
    {
        return;
    }
    if process.kill_with(signal) != Some(true) {
        tracing::warn!(
            pid = expected.pid,
            ?signal,
            "could not signal a process codex login started"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_pid_is_not_a_codex_login_signal_target() {
        let original = ProcessIdentity {
            pid: 111,
            start_time: 100,
        };
        let reused = ProcessIdentity {
            pid: 111,
            start_time: 101,
        };
        assert!(
            !signal_target_is_current(original, reused),
            "a process replacing a collected PID must receive neither the caught signal nor SIGKILL"
        );
        assert!(signal_target_is_current(original, original));
        assert!(!signal_target_is_current(
            original,
            ProcessIdentity {
                pid: 112,
                start_time: 100
            },
        ));
        let unknown = ProcessIdentity {
            pid: 111,
            start_time: 0,
        };
        assert!(!signal_target_is_current(unknown, unknown));
    }

    /// The two lines a terminal sign-in prints, around codex's own output, with the plan left
    /// out when it is unknown.
    #[test]
    fn the_terminal_lines_name_the_account_and_its_plan() {
        assert_eq!(
            codex_login_saved_line("bob@example.com", Some("ChatGPT Pro")),
            "[tcr] 'bob@example.com' is a Codex account (ChatGPT Pro): Codex sessions started \
             with `tcr run --codex` can use it"
        );
        assert_eq!(
            codex_login_saved_line("bob@example.com", None),
            "[tcr] 'bob@example.com' is a Codex account: Codex sessions started with `tcr run \
             --codex` can use it"
        );
        assert_eq!(
            CODEX_LOGIN_START_LINE,
            "[tcr] signing in a Codex account: `codex login` runs in a folder of tcr's own, so \
             the codex CLI keeps the login it has"
        );
    }

    /// The failure names how codex ended and, when there is one, the last thing it said.
    #[test]
    fn a_failed_codex_login_names_its_status_and_what_it_said() {
        assert_eq!(
            codex_login_failed_refusal("3", None).line(),
            r#"{"code":"codex-login-failed","event":"error","reason":"`codex login` exited with 3 before a login was saved. Nothing was written."}"#
        );
        assert_eq!(
            codex_login_failed_refusal("signal 9", Some("boom")).reason,
            "`codex login` exited with signal 9 before a login was saved (codex said: boom). \
             Nothing was written."
        );
    }
}
