//! Who is on the other end of a login, and what it is told: the terminal or
//! machine mode, the JSON events TcrBar parses, the coded refusals and the
//! notices and command names the messages quote.

use super::*;

/// Who is on the other end of `tcr login`.
///
/// Every interactive thing the login does — reading stdin, prompting for a
/// name, opening a browser, narrating on stdout — is decided by this one
/// value rather than by four scattered `is_terminal()` probes, so a caller
/// cannot end up half-interactive: [`LoginUi::Machine`] is exactly the set of
/// behaviours a GUI parent (TcrBar's `LoginSession`) can drive, and there is
/// no way to switch one of them on without the others.
///
/// The browser is deliberately NOT opened in [`LoginUi::Machine`]: the parent
/// opens it, because only the parent can bring its own window forward
/// afterwards. A CLI `open` from a background child leaves the browser behind
/// whatever had focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginUi {
    /// A human at a terminal. Prose progress on stdout, `read_stdin_code` and
    /// `prompt_account_name` live, browser opened here.
    Terminal,
    /// A parent process. One JSON object per line on stdout
    /// ([`LoginEvent::line`]), stdin never read, browser left to the caller.
    Machine,
}

/// Whether the human-prose lines this module prints on the way to a finished
/// login are wanted at all.
///
/// Ambient rather than threaded, deliberately and narrowly: the three prose
/// lines that survive deep inside the persist path
/// ([`finish_login_account`], [`persist_via_file`], [`account_write_result`]) would
/// otherwise need a display mode threaded through six function signatures and
/// twenty test call sites to silence one `println!` each. Nothing about
/// BEHAVIOUR reads this — stdin, the browser and the name prompt are all
/// decided by an explicit [`LoginUi`] argument — so the worst a wrong value
/// can do is print or omit a sentence. One process runs one login, which is
/// what makes a process-wide answer the true one here.
static PROSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Print one human line, unless [`PROSE`] is off (machine mode), in which case
/// stdout stays one JSON object per line and nothing else.
pub(super) fn prose(message: std::fmt::Arguments<'_>) {
    if PROSE.load(std::sync::atomic::Ordering::Relaxed) {
        println!("{message}");
    }
}

impl LoginUi {
    /// Point [`PROSE`] at this mode for the rest of the process. Called once,
    /// at the top of [`login`] and of every other login entry point.
    pub(crate) fn install(self) {
        PROSE.store(self == Self::Terminal, std::sync::atomic::Ordering::Relaxed);
    }

    /// True when stdin is a completion path and a prompt has somewhere to go.
    pub(super) fn reads_stdin(self) -> bool {
        self == Self::Terminal
    }

    /// Emit one progress event, or nothing at all in [`LoginUi::Terminal`]
    /// (which narrates in prose instead — see [`LoginUi::say`]).
    pub(crate) fn emit(self, event: LoginEvent<'_>) {
        if self == Self::Machine {
            println!("{}", event.line());
        }
    }

    /// Print one line of human prose, suppressed in [`LoginUi::Machine`] so
    /// the machine stdout stays one-JSON-object-per-line with nothing else in
    /// it. Goes through [`prose`] rather than `println!` so this mode and the
    /// deep persist-path lines can never disagree about what silence means.
    pub(super) fn say(self, message: &str) {
        if self == Self::Terminal {
            prose(format_args!("{message}"));
        }
    }
}

/// A `tcr login --non-interactive` progress line.
///
/// The wire format is the contract `apps/macos/Sources/TcrBarCore/LoginSession.swift`
/// parses, so it is a type with one rendering function rather than four
/// `println!("{{\"event\":…")` call sites that can drift from the parser one
/// at a time.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginEvent<'a> {
    /// Open this URL. Nothing has been read or written yet.
    Browser { url: &'a str },
    /// The authorize URL is out; waiting for the loopback callback.
    Waiting,
    /// The account is written. `account` is the name it was written under.
    Saved { account: &'a str },
    /// The login failed, and this is the whole reason, on one line.
    Error { reason: &'a str },
    /// The authorize URL, with the browser already open on it: codex opened it itself, so the
    /// caller opens nothing. Never written with `opened: false`; a URL the caller must open is
    /// [`LoginEvent::Browser`].
    BrowserOpened { url: &'a str },
    /// A sentence for the person signing in, shown verbatim, one paragraph each; never a
    /// failure. Every notice comes before `saved` or `error`.
    Notice { text: &'a str },
    /// A Codex account is written: the name it was written under, and its plan as `tcr status`
    /// names it (`ChatGPT Pro`). No `plan` key when the plan is unknown. A Claude save stays
    /// [`LoginEvent::Saved`], whose line carries neither key.
    SavedCodex {
        account: &'a str,
        plan: Option<&'a str>,
    },
    /// A refusal the caller titles by its code ([`LoginRefusal`]); the reason is carried
    /// verbatim. A failure with no code stays [`LoginEvent::Error`].
    Refused { code: &'a str, reason: &'a str },
}

impl LoginEvent<'_> {
    /// One JSON object, no newline. `serde_json` does the escaping — a URL
    /// carries `&` and `=` and a failure reason carries quotes and newlines,
    /// and hand-built JSON is how a reason containing a `"` becomes an
    /// unparseable line at the worst possible moment.
    pub fn line(&self) -> String {
        let value = match self {
            LoginEvent::Browser { url } => {
                serde_json::json!({ "event": "browser", "url": url })
            }
            LoginEvent::Waiting => serde_json::json!({ "event": "waiting" }),
            LoginEvent::Saved { account } => {
                serde_json::json!({ "event": "saved", "account": account })
            }
            LoginEvent::Error { reason } => {
                serde_json::json!({ "event": "error", "reason": reason })
            }
            LoginEvent::BrowserOpened { url } => {
                serde_json::json!({ "event": "browser", "opened": true, "url": url })
            }
            LoginEvent::Notice { text } => serde_json::json!({ "event": "notice", "text": text }),
            LoginEvent::SavedCodex { account, plan } => {
                let mut value = serde_json::json!({
                    "account": account,
                    "event": "saved",
                    "provider": crate::provider::Provider::Codex.as_str(),
                });
                if let Some(plan) = plan {
                    value["plan"] = serde_json::Value::from(*plan);
                }
                value
            }
            LoginEvent::Refused { code, reason } => {
                serde_json::json!({ "code": code, "event": "error", "reason": reason })
            }
        };
        value.to_string()
    }
}

/// A login refusal a caller can title by its code (`codex-identity`, `account-is-codex`, …):
/// the `--non-interactive` error event carries `code` and this reason, verbatim and without the
/// context chain, so a GUI shows tcr's words and decides nothing from them
/// ([`machine_error_line`]). Its `Display` is the reason alone, so a refusal reads the same in
/// the terminal as it did before it carried a code.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct LoginRefusal {
    pub code: &'static str,
    pub reason: String,
}

impl LoginRefusal {
    /// The login belongs to a FedRAMP ChatGPT workspace, which tcr does not pool.
    pub const CODEX_FEDRAMP: &'static str = "codex-fedramp";
    /// The workspace check placed the login's workspace somewhere other than chatgpt.com.
    pub const CODEX_ROUTED: &'static str = "codex-routed";
    /// The workspace check could not complete, so the workspace is unconfirmed.
    pub const CODEX_UNCONFIRMED: &'static str = "codex-unconfirmed";
    /// No `codex` executable was found, or a variable naming one names none.
    pub const CODEX_MISSING: &'static str = "codex-missing";
    /// The ChatGPT login that came back is not the row `--account` named.
    pub const CODEX_IDENTITY: &'static str = "codex-identity";
    /// `codex login` ended without leaving a login behind.
    pub const CODEX_LOGIN_FAILED: &'static str = "codex-login-failed";
    /// `tcr login --account` named a Codex row, which only a Codex login can sign in.
    pub const ACCOUNT_IS_CODEX: &'static str = "account-is-codex";
    /// `--codex` or `--from-codex` with `--account` named a Claude row.
    pub const ACCOUNT_IS_CLAUDE: &'static str = "account-is-claude";

    /// The `--non-interactive` error line this refusal is.
    pub fn line(&self) -> String {
        LoginEvent::Refused {
            code: self.code,
            reason: &self.reason,
        }
        .line()
    }
}

/// The `--non-interactive` error line for `error`, and the one-line reason the same failure
/// prints on stderr.
///
/// A [`LoginRefusal`] anywhere in the chain gives its coded line, with its own reason and none of
/// the context a caller wrapped it in: the reason is tcr's whole sentence, written to be shown
/// as it is. Anything else gives the uncoded line it always gave, its reason flattened from the
/// whole chain by [`one_line_reason`].
pub fn machine_error_line(error: &anyhow::Error) -> (String, String) {
    if let Some(refusal) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LoginRefusal>())
    {
        let reason = refusal
            .reason
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let line = LoginEvent::Refused {
            code: refusal.code,
            reason: &reason,
        }
        .line();
        (line, reason)
    } else {
        let reason = one_line_reason(error);
        let line = LoginEvent::Error { reason: &reason }.line();
        (line, reason)
    }
}

/// Printed once a Codex account is saved into a config that held no Codex row before, in any
/// state, enabled, disabled or needing a re-login. The account enables marked Codex connections
/// to join the pool (`mitm::handle_connect`); unmarked clients keep their own TLS and login.
pub const CODEX_FIRST_ACCOUNT_NOTICE: &str = "tcr pools marked Codex connections to chatgpt.com: use `tcr run --codex`; keep \
    the marker scoped to that launch, since unmarked connections keep their own login";

/// The warning for a `tcr` at `path`, reporting `version`, that is 0.2.45 or older: it predates
/// the `credentialVersion` check, so it would read a Codex row as a Claude account and send its
/// OpenAI token to Anthropic the next time it starts on this config.
pub fn older_tcr_reads_codex_as_claude_notice(path: &Path, version: &str) -> String {
    format!(
        "the tcr at {} ({version}) predates Codex accounts: update it before it next starts — it \
         would read this login as a Claude account and send its token to Anthropic",
        path.display()
    )
}

/// The warning for a `tcr` at `path`, reporting `version`, from 0.2.46 on that still lacks
/// `--from-codex`: it checks `credentialVersion`, so it refuses a config holding a Codex row
/// outright, and every command it runs on this config fails until it is updated.
pub fn older_tcr_refuses_config_notice(path: &Path, version: &str) -> String {
    format!(
        "the tcr at {} ({version}) predates Codex accounts: update it — until then it refuses \
         this config, so every command it runs fails",
        path.display()
    )
}

/// The notice for a codex with no login of its own at `path` (an existence check, never a
/// read): tcr re-credentials a pooled codex's requests, but codex itself still has to start
/// signed in. A login kept in the system keyring (codex's `keyring` and `auto` stores) is
/// invisible to tcr, which is why the sentence leaves that case open.
pub fn codex_has_no_login_notice(path: &Path) -> String {
    format!(
        "codex needs a login of its own to start, even when pooled, and none is at {}: unless \
         codex keeps its login in the keyring, run `codex login` once",
        path.display()
    )
}

/// The line `tcr login --from-codex` prints after its import lines in the terminal: how to use
/// the account it just saved.
pub const CODEX_IMPORT_RUN_LINE: &str = "[tcr] start a pooled session with `tcr run --codex`";

/// The command each login entry point names when it tells someone to run it again.
pub const LOGIN_COMMAND: &str = "tcr login";
/// See [`LOGIN_COMMAND`].
pub const LOGIN_FROM_CODEX_COMMAND: &str = "tcr login --from-codex";
/// See [`LOGIN_COMMAND`].
pub const LOGIN_FROM_CLAUDE_CODE_COMMAND: &str = "tcr login --from-claude-code";
/// See [`LOGIN_COMMAND`].
pub const LOGIN_CODEX_COMMAND: &str = "tcr login --codex";
/// See [`LOGIN_COMMAND`]: `tcr import --yes`, which writes every login `tcr import` lists as new.
pub const IMPORT_COMMAND: &str = "tcr import --yes";

/// Flatten an error and its whole `anyhow` context chain onto ONE line.
///
/// `{:#}` already does this, but a message can still carry an embedded
/// newline (a `bail!` with a wrapped multi-line explanation — this module has
/// several), and one physical line is what both the JSON event stream and a
/// GUI's single-line failure label need. Whitespace runs collapse to a single
/// space rather than being dropped, so words never fuse together.
pub fn one_line_reason(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
