//! The admission every login starts with: is a live proxy on the port, can it
//! take the account over its add route, and if not, refuse or write the file.

use super::*;

/// What `login()` should do with the finished credential, decided purely from
/// the incumbent detected on the guarded port, whether a probe already
/// confirmed THAT incumbent carries the live account-add route, and
/// `--force`. Returned by [`login_guard_refusal_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LoginRoute {
    /// No live proxy on the port at all — the historical file-only login,
    /// unchanged. Also reached under `--force` when the probe could not
    /// confirm a safe live route (no route, or an unusable answer):
    /// `--force` is the escape hatch PAST A REFUSAL, never a way to skip a
    /// live route that IS there — `Live` below takes priority over it. An
    /// api-key rejection (`AddCapability::Unauthorized`) never resolves to
    /// this, under `--force` or otherwise: that is positive evidence the
    /// proxy is alive, so there is no unsafe file write for `--force` to
    /// rescue — see `admit_login_against`'s `Unauthorized` arm.
    File,
    /// A live proxy is there and its add route is confirmed: route the
    /// finished credential through it instead of the file. Wins even against
    /// `--force` — forcing the unsafe file write only makes sense when the
    /// safe live route is not available.
    Live,
    /// A live proxy is there without the route, and `--force` was not given:
    /// refuse outright with this message, unchanged from before this route
    /// existed.
    Refuse(String),
}

/// The pure login-guard DECISION: given whether the HTTP capability probe
/// already confirmed a live account-add route ([`probe_add_route`], run
/// impure and BEFORE this), any live proxy separately detected on the port
/// (the impure lsof/owner-file detection,
/// [`crate::singleton::live_proxy_server`]), the port, and the `--force`
/// flag, return which [`LoginRoute`] `login()` should take. Split from both
/// impure detectors so the decision itself stays unit-testable, mirroring
/// singleton's pure-decision / impure-executor split.
///
/// `has_add_route` is decided FIRST and is decisive on its own, independent
/// of `incumbent` — including when `incumbent` is `None`. The HTTP probe is
/// first-hand evidence that a live proxy with the route is answering on this
/// exact port; `incumbent` is an INFERENCE from a pid, an argv string, or an
/// owner-file claim, any of which can be stale, absent, or blind to a host
/// application the pid/argv matcher was never taught to recognize (see
/// `ProxyHost::Unknown`'s doc-comment). Gating the live path on `incumbent`
/// agreeing would mean any future regression in that inference silently
/// reintroduces the whole-file clobber this unit exists to remove — as a
/// silent fallback rather than a loud one. So a confirmed route wins even
/// against a `None` incumbent, and `incumbent` is consulted only to build the
/// REFUSAL message, for the one case where the probe did NOT confirm the
/// route.
///
/// The incumbent's KIND decides the refusal instruction, and it is not
/// cosmetic. Since the owner file, detection reaches a proxy served INSIDE a
/// host application, and the pid reported is then the HOST's. "kill {pid}"
/// would SIGTERM a GUI process that installs no handler for it: no
/// `applicationWillTerminate`, no final session→account pin write, and every
/// live session cold-starts its prompt cache at the next boot.
/// `takeover_decision` and `incumbents_to_signal` are both hardened against
/// exactly that signal; a message that ADVISES it would walk around both. So
/// an embedded incumbent is told to be quit, not killed.
///
/// The refusal names `command`, the login that was run (`tcr login`,
/// `tcr login --from-codex`, `tcr login --codex`, …), as the one to run again
/// once the server is stopped: telling someone who ran an import to run
/// `tcr login` sends them into the browser flow they did not ask for.
pub(super) fn login_guard_refusal_for(
    incumbent: Option<singleton::Incumbent>,
    has_add_route: bool,
    port: u16,
    force: bool,
    command: &str,
) -> LoginRoute {
    // `has_add_route` is checked BEFORE `force`, deliberately: a confirmed
    // route wins even against the escape hatch. Checking `force` first (the
    // pre-fix order) meant `--force` chose the file write even when the live
    // route was confirmed safe — the exact whole-file clobber this unit
    // exists to remove, reintroduced by the escape hatch meant to route
    // around a REFUSAL, not around a safe route that was never refused.
    if has_add_route {
        return LoginRoute::Live;
    }
    if force {
        return LoginRoute::File;
    }
    match incumbent {
        Some(incumbent) => {
            let pid = incumbent.pid;
            LoginRoute::Refuse(match incumbent.kind {
                singleton::ProxyKind::TcrEmbedded => format!(
                    "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — that pid is the HOST APPLICATION serving the proxy in-process, so do not signal it: quit the host application (killing it skips its shutdown and loses the session pin map), run '{command}', then start it again. Re-run with --force to log in anyway."
                ),
                singleton::ProxyKind::Tcr => format!(
                    "a tcr server is already running on port {port} (pid {pid}); logging in now would be overwritten by the server's next token refresh — stop it (kill {pid}, or Ctrl-C in its terminal), run '{command}', then restart 'tcr server'. Re-run with --force to log in anyway."
                ),
            })
        }
        None => LoginRoute::File,
    }
}

/// What a live proxy said, structurally, about whether it can accept a live
/// account add. Read from [`crate::cli::post_add_account`]'s own
/// [`crate::cli::LiveControlError`] classification, which is itself driven by
/// [`crate::proxy::ENDPOINT_HEADER`] — never by matching error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AddCapability {
    /// The route answered (stamped, whatever the status) — a live add is
    /// possible.
    Present,
    /// A connection could not even be made — genuinely nothing is listening
    /// on the port. The ONLY outcome that may resolve to `LoginRoute::File`
    /// without `--force` and without `incumbent` separately confirming a
    /// server: there is truly no live process to disagree with the file.
    Absent,
    /// The proxy answered and rejected our api-key. Its own condition — never
    /// folded into `Absent`: that would either wrongly read as an older proxy
    /// with no route, or let `login()` silently fall back to writing the file
    /// out from under a server that is right there, listening.
    Unauthorized,
    /// Something holds the port and is either an older proxy answering
    /// without the route (identified structurally, never by error text) or
    /// accepted the connection and then produced nothing usable before the
    /// deadline — the wedged-port shape `cli::probe_incumbent`'s own tests
    /// document as measured and real (`a_listener_that_never_answers_is_silent_not_answering`).
    /// Both are first-hand evidence a live process holds the port — at least
    /// as strong as the `incumbent` pid heuristic, and stronger exactly where
    /// that heuristic is documented to miss (an embedded host). Split out of
    /// `Absent` because the two are NOT the same conservative answer: folding
    /// them together let a `None` incumbent silently choose `File` beside a
    /// real, wedged server, without `--force` ever being asked for. Carries
    /// the probe's own account of what happened, for the refusal message.
    Unusable(String),
}

/// [`probe_add_route`]'s capability alone. Test-only: every production caller
/// goes through `probe_add_route`, which this delegates to, so the tests that
/// pin the capability pin the very probe production sends.
#[cfg(test)]
pub(super) async fn probe_add_capability(config: &Config) -> AddCapability {
    probe_add_route(config).await.0
}

/// Probe whether the proxy `config` points at can accept a live account add,
/// via a deliberately-invalid POST to [`crate::proxy::ADD_ACCOUNT_PATH`] — a
/// blank name and access token, always rejected by the route's own
/// validation — so this can run BEFORE any OAuth tokens exist and before the
/// browser opens. Never probes by adding a real account.
///
/// Also returns the providers the server named in
/// [`crate::proxy::PROVIDERS_HEADER`] on that SAME answer, so a login of a
/// provider that must be proven storable (anything but Anthropic: see that
/// header) learns it with no second request. Empty when the server named none,
/// which is how a server that predates the header, and every non-answer, reads.
async fn probe_add_route(config: &Config) -> (AddCapability, Vec<crate::provider::Provider>) {
    let probe = Account::new("", crate::provider::Provider::default(), "");
    let (answer, providers) = crate::cli::post_add_account_with_providers(config, &probe).await;
    let capability = match answer {
        // A blank name/token can never be added, so `Ok` cannot happen in
        // practice — but a stamped success is just as much "the route
        // exists" as a stamped rejection, so it counts as `Present` too.
        Ok(_) => AddCapability::Present,
        // The route validated the (deliberately bad) body and said so,
        // stamped — exactly the positive capability signal.
        Err(crate::cli::LiveControlError::Rejected(_)) => AddCapability::Present,
        Err(crate::cli::LiveControlError::Unauthorized) => AddCapability::Unauthorized,
        // A connection could not be made at all — the ONLY genuinely-absent
        // case.
        Err(crate::cli::LiveControlError::NoServer) => AddCapability::Absent,
        // Everything else (`NoRoute`, `NoAnswer`, `Unusable`) is first-hand
        // evidence something holds the port, just not usably.
        Err(other) => AddCapability::Unusable(other.why()),
    };
    (capability, providers)
}

/// The login a refusal tells the user to run again ([`login_guard_refusal_for`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginCommand {
    /// `tcr login`
    Browser,
    /// `tcr login --from-claude-code`
    FromClaudeCode,
    /// `tcr login --from-codex`
    FromCodex,
    /// `tcr login --codex`
    Codex,
    /// `tcr import --yes`
    Import,
}

impl LoginCommand {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => LOGIN_COMMAND,
            Self::FromClaudeCode => LOGIN_FROM_CLAUDE_CODE_COMMAND,
            Self::FromCodex => LOGIN_FROM_CODEX_COMMAND,
            Self::Codex => LOGIN_CODEX_COMMAND,
            Self::Import => IMPORT_COMMAND,
        }
    }
}

/// A login the guard let through: the route to write by, the port and incumbent it was decided against, and
/// the providers the probe's answer advertised ([`probe_add_route`]).
#[derive(Debug)]
pub(super) struct Admitted {
    pub(super) route: WriteRoute,
    port: u16,
    pub(super) pid: Option<u32>,
    providers: Vec<crate::provider::Provider>,
}

impl Admitted {
    /// Refuse a live server that cannot store a Codex login, naming `command`
    /// as the login to run again ([`codex_live_route_refusal`]).
    pub(super) fn require_codex(&self, command: LoginCommand) -> anyhow::Result<()> {
        if self.route == WriteRoute::Live
            && !self.providers.contains(&crate::provider::Provider::Codex)
        {
            bail!(
                "{}",
                codex_live_route_refusal(self.port, self.pid, command.as_str())
            );
        }
        Ok(())
    }
}

/// The admission every login starts with: find the port the server binds, look
/// for an incumbent on it, and resolve which route the finished credential
/// takes ([`admit_login_against`]). A refusal comes back as the error, so an
/// [`Admitted`] carries a [`WriteRoute`], which has no refusal.
///
/// Reads the config once: its `proxy.port` is the port the running server
/// binds, so the guard checks the SAME port `tcr server` takes over, and the
/// same value feeds the probe.
pub(super) async fn admit_login(
    config_path: &Path,
    force: bool,
    command: LoginCommand,
) -> anyhow::Result<Admitted> {
    let config = load_or_default(config_path)?;
    let port = config.proxy.port;
    let incumbent = singleton::live_proxy_server(port);
    admit_login_against(config_path, &config, incumbent, port, force, command).await
}

/// [`login_guard_refusal_for`] for plain `tcr login`, whose refusal says `run 'tcr login'`. The
/// guard's own tests pin the decision through it.
#[cfg(test)]
pub(super) fn login_guard_refusal(
    incumbent: Option<singleton::Incumbent>,
    has_add_route: bool,
    port: u16,
    force: bool,
) -> LoginRoute {
    login_guard_refusal_for(incumbent, has_add_route, port, force, LOGIN_COMMAND)
}

/// [`admit_login`] against a given `incumbent` and `port`, which is the seam a
/// test uses to say what process detection saw. `command` is the login that
/// was run, which a refusal names ([`login_guard_refusal_for`]).
///
/// Runs the impure HTTP
/// capability probe UNCONDITIONALLY — including under `--force` — deliberately
/// never gated on whether [`crate::singleton::live_proxy_server`]
/// (`incumbent`) noticed anything on the port first, NOR on `force`: skipping
/// the probe under `--force` is exactly the bug bridge item C removed, since
/// it is what let `--force` take the file path even when the live route was
/// confirmed and safe. `force` only changes what happens when the probe
/// itself produced an UNUSABLE answer (something holds the port but is not
/// confirmed safe) — that case falls back to `LoginRoute::File` under
/// `--force` instead of erroring out, mirroring the historical "force always
/// succeeds via the file" behaviour. An api-key rejection (`Unauthorized`) is
/// a DIFFERENT case, and `--force` never rescues it: it is positive evidence
/// of a live, healthy proxy — it answered, parsed our request, and rejected
/// the key — which makes it the worst-informed moment to whole-file-write
/// beside it, since a live server rotating tokens is exactly what makes that
/// write destructive. Fully resolves before a login starts the OAuth
/// dance, so a user is never told to stop a server AFTER completing a full
/// browser round-trip, and never silently falls back to the file when a live
/// proxy answered but rejected our api-key.
pub(super) async fn admit_login_against(
    config_path: &Path,
    config: &Config,
    incumbent: Option<singleton::Incumbent>,
    port: u16,
    force: bool,
    command: LoginCommand,
) -> anyhow::Result<Admitted> {
    let pid = incumbent.map(|incumbent| incumbent.pid);
    let command = command.as_str();
    let (capability, providers) = probe_add_route(config).await;
    let route = match capability {
        // Positive evidence the proxy is alive: it answered, parsed our
        // request, and rejected the key. Refuses regardless of `--force` —
        // unlike `Unusable` below, there is no unsafe-but-forceable file
        // write to fall back to here, only a safe path one config edit away.
        AddCapability::Unauthorized => {
            bail!(
                "the proxy on :{port} rejected the api-key in {} while checking whether it \
                 could take a live login — no browser was opened and nothing was changed. This \
                 refuses even under --force: a rejected api-key is proof the proxy is alive, \
                 which makes this the worst-informed moment to write the config file beside it. \
                 Fix `proxy.apiKey` in the config to match the running server, or stop the \
                 server and log in offline.",
                config_path.display()
            );
        }
        AddCapability::Present => login_guard_refusal_for(incumbent, true, port, force, command),
        AddCapability::Absent => login_guard_refusal_for(incumbent, false, port, force, command),
        // Bridge item D: something holds the port but answered unusably —
        // first-hand evidence at least as strong as `incumbent`, so this
        // refuses REGARDLESS of what `incumbent` separately concluded, never
        // falling through to `LoginRoute::File` the way folding this into
        // `Absent` used to when `incumbent` was `None`.
        AddCapability::Unusable(why) => {
            if !force {
                bail!(
                    "the proxy on :{port} answered but not usably ({why}) while checking whether \
                     it could take a live login — no browser was opened and nothing was changed. \
                     This is first-hand evidence something holds the port even though process \
                     detection may have missed it, so the historical whole-file-write hazard still \
                     applies. Re-run with --force to log in via the file instead (this still risks \
                     the running server overwriting it on its next token refresh)."
                );
            }
            LoginRoute::File
        }
    };
    let route = match route {
        LoginRoute::Refuse(msg) => bail!("{msg}"),
        LoginRoute::File => WriteRoute::File,
        LoginRoute::Live => WriteRoute::Live,
    };
    Ok(Admitted {
        route,
        port,
        pid,
        providers,
    })
}

/// [`import_codex_login`]'s refusal of a live proxy that did not name `codex`
/// among the providers it can store. `pid` is the incumbent process detection
/// found on the port, when it found one; `command` is the login that was run,
/// named as the one to run again.
pub(super) fn codex_live_route_refusal(port: u16, pid: Option<u32>, command: &str) -> String {
    let pid = pid.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
    format!(
        "the proxy on :{port} ({pid}) predates Codex accounts and would store this login as a \
         Claude account and send its token to Anthropic — nothing was written. Restart the proxy \
         on this build (update TcrBar first if it supervises the proxy), then re-run `{command}`. \
         --force does not get past this: it never trades a confirmed live route for a file \
         write, and that proxy would refuse the file at its next start, or read it the same way \
         if it is older still."
    )
}
