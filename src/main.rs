//! `tcr` — the command-line binary.
//!
//! Boot sequence (DESIGN §main): load the config → hand it to
//! [`tcr::server::serve`], which builds the [`Manager`], spawns the
//! axum proxy task and the background loops, and binds → run the TUI (or block
//! in `--headless`) → shut the handle down, which flushes on exit.
//!
//! What is left in THIS file is what only a binary may do: parse clap, install a
//! logging subscriber, print operator diagnostics, and turn a stand-down into a
//! process exit code. Everything reusable lives in the library.
//!
//! [`Manager`]: tcr::manager::Manager

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};

mod cmd;
mod tool_env;

use tcr::cli::{self, PriorityArg};
use tcr::config;
use tcr::{demo, mint, oauth, server, update};

use cmd::ConfigFile;
use cmd::peer::{peer_cli, run_peer};
use cmd::run::{RunArgs, run_harness};
use cmd::server::{ServerArgs, run_server};

#[derive(Parser)]
#[command(
    name = "tcr",
    version,
    about = "Lean single-user rotating proxy for Claude and Codex (ChatGPT) accounts, with a live TUI",
    // Let `tcr [flags]` (no subcommand) behave as the default server run, while
    // `tcr server [flags]` is the explicit form. The two cannot be mixed.
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    server: ServerArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server (the default when no subcommand is given).
    Server(ServerArgs),
    /// Run Claude Code, or `codex` with `--codex`, through the proxy (launches it
    /// directly if we're not up).
    Run(RunArgs),
    /// Add an account: Claude through the browser, or Codex (ChatGPT) with
    /// --codex or --from-codex.
    Login(LoginArgs),
    /// List the Claude and Codex subscription logins other tools on this machine hold
    /// (claude-swap, `CLIProxyAPI`'s auth folders, Claude Code, the Codex CLI), each marked new or
    /// already in tcr. Reads only; `--yes` imports the new ones.
    Import(ImportArgs),
    /// Mint a long-lived (365-day) token for an existing Claude account, or
    /// every Claude account in a group, and put the result on the clipboard.
    /// Nothing is stored — export only (Claude accounts only).
    Mint(MintArgs),
    /// List the configured accounts (offline; `--probe` refreshes live quota).
    Accounts(AccountsArgs),
    /// Redeem banked resets through the running proxy.
    Codex(CodexArgs),
    /// Use one banked limit reset on a Claude or Codex account, after confirmation, through
    /// the running proxy. `tcr codex reset` is the same command.
    Reset(ResetArgs),
    /// Remove an account from the config.
    Remove(RemoveArgs),
    /// Set an account's rotation priority (lower value = preferred).
    Priority(PriorityArgs),
    /// Print an account's current access token to stdout (a secret — pipe it, do
    /// not paste it into a chat or a shell history).
    Token(TokenArgs),
    /// Print the request key from the config to stdout, for a launcher outside tcr
    /// (a secret, like `token`). Never writes one: the server writes it at boot,
    /// so this and the server cannot disagree. Exits non-zero when there is none.
    RequestKey(RequestKeyArgs),
    /// Enable an account (clears the `disabled` flag).
    Enable(EnableArgs),
    /// Disable an account (holds it out of rotation).
    Disable(DisableArgs),
    /// Set, clear, or show the identity-bound control account (Claude accounts
    /// only).
    Control(ControlArgs),
    /// Manage account group membership (`ls` / `add` / `rm`).
    Group(GroupArgs),
    /// Read the config file's rotation and limit settings (`show`), defaults
    /// applied, without asking a running proxy.
    Config(ConfigArgs),
    /// The LAN peer mesh: find your other Macs, trust one, share accounts with
    /// it, or reach the internet through it.
    Peer(peer_cli::PeerArgs),
    /// Probe every account's live quota and print the fleet status.
    Status(StatusArgs),
    /// Print the live sessions the running proxy has seen in the last hour.
    ///
    /// Separate from `status` on purpose: `tcr status --json` emits a bare
    /// array of accounts and that contract has clients (the panel, `jq`
    /// one-liners in the docs). Sessions get their own object-shaped verb
    /// rather than a flag that changes `status`'s top-level type.
    Sessions(SessionsArgs),
    /// Print the running proxy's routing document: which models a new request can be
    /// routed to now, per provider. Exits 0 printed, 3 no proxy listening, 4 unreadable.
    ///
    /// Its own verb for the same reason as `sessions`: `tcr status --json` keeps its shape.
    Routing(RoutingArgs),
    /// Print every Claude account as claude-swap's `list --json` document, for
    /// CodexBar. JSON only: `tcr status` is the view for a person.
    ///
    /// Its own verb for the same reason as `sessions`: the document is an
    /// object, and `status --json`'s bare array has clients.
    List(ListArgs),
    /// Print a usage report for the last N days (default 7), read from the
    /// usage ledger — cost, tokens, cache-hit ratio, by model/account/day, and
    /// the busiest sessions.
    Wrap(WrapArgs),
    /// Self-update: `git pull --ff-only` + `cargo build --release` in the checkout.
    Update(UpdateArgs),
    /// Render the TUI against fake accounts (for a sanitized README screenshot).
    Demo,
    /// Open TcrBar, the macOS menu-bar app (macOS only).
    Ui,
    /// Say whether Claude Code is actually reaching this proxy, and which file
    /// decides that; and whether codex is pooled, when the fleet holds a Codex
    /// account or with --codex. Exits 0 when it is, 2 when another base URL
    /// owns the route, 3 when the route points here and no proxy answers.
    Doctor(DoctorArgs),
    /// Send the `claude` command through tcr: from your shell (`setup shell`),
    /// or on every launch (`setup claude-settings`). Each refuses rather than
    /// overwrite a `claude` alias, function or base URL already in place, and
    /// each takes `--remove`.
    Setup(SetupArgs),
    /// Fan control through the running server: with no action, print the mode
    /// asked for, what the controller is doing, and each fan. `auto`, `fixed`
    /// and `follow` set the mode, wait until it holds, and exit 1 with the reason
    /// when it does not. Needs the tcr bundled in TcrBar.app and its helper.
    Fan(FanArgs),
}

/// `tcr fan …`.
#[derive(clap::Args)]
struct FanArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Print the server's `fanControl` object as JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    action: Option<FanAction>,
}

#[derive(Subcommand)]
enum FanAction {
    /// Give the fans back to macOS.
    Auto {
        /// Hand back without checking that the fans are the ones this tcr
        /// set: the recovery for fans left manual by a run no process
        /// remembers. Still waits while Macs Fan Control runs.
        #[arg(long)]
        force: bool,
    },
    /// Hold every fan at one speed, clamped to each fan's range.
    Fixed {
        /// Revolutions per minute, 1 to 20000.
        rpm: u32,
    },
    /// Follow the CPU: each fan's minimum at or below the quiet temperature, full
    /// speed at the full one (at most 100 °C), a straight line between.
    Follow {
        /// °C, at least 20.
        quiet_c: u8,
        /// °C, at least 5 above the quiet temperature and at most 100.
        full_c: u8,
    },
    /// Send the root fan helper its version op and print the reply. Writes no
    /// fan. Dials the helper this binary's signature belongs to, or `--label`'s.
    #[command(hide = true)]
    Probe(FanProbeArgs),
}

#[derive(clap::Args)]
struct FanProbeArgs {
    /// Dial this helper label instead of the one this binary's signature
    /// names, to watch the helper refuse a caller it should refuse.
    #[arg(long)]
    label: Option<String>,
}

/// `tcr setup …`: install, remove or report the two routes that make typing
/// `claude` go through tcr. See `src/setup.rs` for what each writes.
#[derive(clap::Args)]
struct SetupArgs {
    #[command(subcommand)]
    action: SetupAction,
}

#[derive(Subcommand)]
enum SetupAction {
    /// Add a `claude` function to your shell's startup file that runs
    /// `tcr run`. Covers terminals; an app that starts claude itself is not
    /// covered.
    Shell(SetupShellArgs),
    /// Point every claude launch at this proxy: set
    /// `env.ANTHROPIC_BASE_URL` in ~/.claude/settings.json. With tcr not
    /// running, claude then cannot reach Anthropic until tcr starts or this
    /// is removed.
    ClaudeSettings(SetupSettingsArgs),
    /// Say which route is installed, and anything in the way of either.
    Status(SetupStatusArgs),
    /// Link `tcr-run` beside the `tcr` on PATH: `tcr run` under another name,
    /// for a tool that takes a Claude Code executable path.
    TcrRun(SetupTcrRunArgs),
}

#[derive(clap::Args)]
struct SetupTcrRunArgs {
    /// Take the link back out (only the symlink tcr made).
    #[arg(long)]
    remove: bool,
}

/// Parse `--shell` through the one recogniser `src/setup.rs` uses for
/// `$SHELL`, so the flag and the variable can never disagree on a name.
fn parse_shell(value: &str) -> Result<tcr::setup::Shell, String> {
    tcr::setup::Shell::from_name(value)
        .ok_or_else(|| format!("{value} is not one of zsh, bash, fish"))
}

#[derive(clap::Args)]
struct SetupShellArgs {
    /// Take tcr's block back out of every shell startup file that has one.
    #[arg(long)]
    remove: bool,
    /// The shell to set up: zsh, bash or fish (default: $SHELL).
    #[arg(long, value_parser = parse_shell)]
    shell: Option<tcr::setup::Shell>,
}

#[derive(clap::Args)]
struct SetupSettingsArgs {
    /// Take out the key tcr wrote, and nothing else.
    #[arg(long)]
    remove: bool,
    /// Replace an `ANTHROPIC_BASE_URL` that points somewhere else.
    #[arg(long, conflicts_with = "remove")]
    force: bool,
    #[command(flatten)]
    config: ConfigFile,
}

#[derive(clap::Args)]
struct SetupStatusArgs {
    /// Emit one JSON object (TcrBar reads this).
    #[arg(long)]
    json: bool,
    /// The shell to check: zsh, bash or fish (default: $SHELL).
    #[arg(long, value_parser = parse_shell)]
    shell: Option<tcr::setup::Shell>,
    #[command(flatten)]
    config: ConfigFile,
}

#[derive(clap::Args)]
struct AccountsArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Refresh each account's live quota before listing (network probe).
    #[arg(long)]
    probe: bool,
}

#[derive(clap::Args)]
struct RemoveArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct RequestKeyArgs {
    #[command(flatten)]
    config: ConfigFile,
}

#[derive(clap::Args)]
struct TokenArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct PriorityArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
    /// The explicit priority value (lower = preferred). Omit with --first/--last.
    #[arg(conflicts_with_all = ["first", "last"])]
    value: Option<i64>,
    /// Move the account to the front of rotation (min priority - 1).
    #[arg(long, conflicts_with = "last")]
    first: bool,
    /// Move the account to the back of rotation (max priority + 1).
    #[arg(long)]
    last: bool,
}

#[derive(clap::Args)]
struct EnableArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct DisableArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Account name — exact and case-sensitive, not a substring. Names are
    /// unique, so this always names one row; `tcr accounts` prints them.
    query: String,
}

#[derive(clap::Args)]
struct ControlArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Account name — exact and case-sensitive, not a substring. Omit with
    /// `--clear` or `--show`.
    #[arg(conflicts_with_all = ["clear", "show"])]
    query: Option<String>,
    /// Clear the control account (identity traffic resolves to none).
    #[arg(long, conflicts_with = "show")]
    clear: bool,
    /// Print the current control account and change nothing.
    #[arg(long)]
    show: bool,
}

/// `tcr config show|set`. TcrBar's Settings window runs `config show --json`
/// and `config set <key> <value>...`, so both argvs are a contract.
#[derive(clap::Args)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the effective rotation and limit settings: what a server started
    /// on this file uses, every default applied. Offline; a running proxy
    /// keeps the values it booted with until it restarts.
    Show(ConfigShowArgs),
    /// Save one rotation or limit setting. With a proxy running it saves it
    /// itself; either way it keeps its current value until it restarts.
    ///
    /// Keys and values: switchThreshold, controlReserve and
    /// fableWeeklyThreshold take a share (90% or 0.9; fableWeeklyThreshold
    /// also `default`); resetUrgencyTierHours and usageRetentionDays a whole
    /// number; sessionAffinity, controlPooled and http1Only on or off;
    /// accountThrottle and fleetThrottle `off` or two numbers, the
    /// milliseconds between sends and the burst; pacing `off` or a count.
    Set(ConfigSetArgs),
}

#[derive(clap::Args)]
struct ConfigSetArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The key as the config file spells it, e.g. switchThreshold.
    key: String,
    /// The value; a throttle takes two words, `350 24`.
    /// Negative numbers allowed so `-1` reaches the parser and its refusal,
    /// rather than clap's "unexpected argument". Not every hyphen value: that
    /// would take a trailing `--config` as the second value of `pacing 4`.
    #[arg(required = true, num_args = 1..=2, allow_negative_numbers = true)]
    value: Vec<String>,
}

#[derive(clap::Args)]
struct ConfigShowArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Emit one JSON object instead of the table.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct GroupArgs {
    #[command(subcommand)]
    action: GroupAction,
}

/// `tcr group ls|add|rm|reserve|unreserve|park|unpark|allow-control|disallow-control|color` — the argument shape here is a
/// CONTRACT with the TcrBar panel, which shells out to it (`TcrTool.run`); do
/// not change it.
#[derive(Subcommand)]
enum GroupAction {
    /// List groups and their members.
    Ls(GroupLsArgs),
    /// Add one account to one group.
    Add(GroupAddArgs),
    /// Remove one account from one group, or `--all` to delete the group.
    Rm(GroupRmArgs),
    /// Reserve a group: an account carrying it becomes off-limits to traffic
    /// that did not ask for one of its groups. A running proxy picks this up
    /// live (no restart) on its next natural cadence check.
    Reserve(GroupReserveArgs),
    /// Clear a group's reserved flag.
    Unreserve(GroupUnreserveArgs),
    /// Park a group: every account carrying it is held out of rotation, the
    /// way `tcr disable` holds one account out — no request reaches it, not
    /// even an explicit `--group` ask. A running proxy picks this up live (no
    /// restart) on its next natural cadence check.
    Park(GroupParkArgs),
    /// Clear a group's parked flag and put its members back in rotation. An
    /// account disabled on its own stays disabled.
    Unpark(GroupUnparkArgs),
    /// Opt a group in to selecting the control account on an explicit
    /// `--group` ask — otherwise inference never selects it. A running proxy
    /// picks this up live (no restart) on its next natural cadence check.
    AllowControl(GroupAllowControlArgs),
    /// Clear a group's `allowControlAccount` flag.
    DisallowControl(GroupDisallowControlArgs),
    /// Set (or `--clear`) a group's color — the tag the panel draws for it.
    Color(GroupColorArgs),
}

#[derive(clap::Args)]
struct GroupLsArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Emit a machine-readable JSON equivalent instead of greppable text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct GroupAddArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to add.
    group: String,
    /// Account name — exact and case-sensitive.
    account: String,
}

#[derive(clap::Args)]
struct GroupRmArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to remove.
    group: String,
    /// Account name to drop from the group. Required unless `--all` is given;
    /// conflicts with `--all` so the parser — not a runtime panic — refuses
    /// "both" and "neither".
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    account: Option<String>,
    /// Remove the group from every member instead of one account — deletes
    /// the group, since groups exist only while some account carries the
    /// label.
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
struct GroupReserveArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to reserve.
    group: String,
    /// Reserve the group even though it holds Codex accounts, which then serve nothing while it stays reserved.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct GroupUnreserveArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to unreserve.
    group: String,
}

#[derive(clap::Args)]
struct GroupParkArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to park.
    group: String,
}

#[derive(clap::Args)]
struct GroupUnparkArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to unpark.
    group: String,
}

#[derive(clap::Args)]
struct GroupAllowControlArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to opt in.
    group: String,
}

#[derive(clap::Args)]
struct GroupDisallowControlArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to opt out.
    group: String,
}

#[derive(clap::Args)]
struct GroupColorArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// The group label to color.
    group: String,
    /// The color, as `#RGB` or `#RRGGBB` (case-insensitive). Required unless
    /// `--clear` is given; conflicts with `--clear` so the parser refuses
    /// "both" and "neither" the same way `GroupRmArgs`'s `account`/`--all`
    /// does.
    #[arg(required_unless_present = "clear", conflicts_with = "clear")]
    hex: Option<String>,
    /// Revert to the color derived from the group name instead of setting one.
    #[arg(long)]
    clear: bool,
}

#[derive(clap::Args)]
struct StatusArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Emit the fleet status as a JSON array instead of greppable text.
    #[arg(long)]
    json: bool,
    /// Exit 4 when an enabled account's last probe failed (error, timeout or
    /// upstream-down). Never-probed, rate-limited and disabled accounts do not
    /// count. Output is unchanged; only the exit code differs.
    ///
    /// Opt-in because the DEFAULT exit is a contract: TcrBar runs
    /// `tcr status --json` and reads any non-zero exit as a failed poll
    /// (`StatusPoller.classify`), which would blank the panel. The rule and why
    /// 4 rather than 1: `cli::StatusCheck`.
    #[arg(long)]
    check: bool,
}

/// `tcr doctor [--json] [--codex]`: the route check. The same two options every
/// read-only verb here takes, plus `--codex`, which narrows the question to
/// codex's route: the question it answers is about THIS machine's
/// configuration, so there is nothing to select.
#[derive(clap::Args)]
struct DoctorArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Emit one JSON object instead of greppable `key: value` lines.
    #[arg(long)]
    json: bool,
    /// Report only whether codex is pooled, and exit with that verdict's code.
    #[arg(long)]
    codex: bool,
}

/// `tcr list --json`: the same two options as `status`, and `--json` is the
/// only output there is.
#[derive(clap::Args)]
struct ListArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Emit claude-swap's list schema v1 (required: there is no text form).
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct SessionsArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Emit `{"supported": bool, "sessions": [...]}` instead of greppable text.
    #[arg(long)]
    json: bool,
}

/// Commands for banked resets on the running proxy.
#[derive(clap::Args)]
struct CodexArgs {
    #[command(subcommand)]
    command: CodexCommand,
}

#[derive(Subcommand)]
enum CodexCommand {
    /// Redeem one reset for the named account, after confirmation.
    Reset {
        account: String,
        #[command(flatten)]
        config: ConfigFile,
        /// Reuse one attempt across retries; never printed.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Skip the questions, for a caller that already asked: redeem without
        /// reading input, and end on an unknown outcome instead of offering a retry.
        #[arg(long)]
        yes: bool,
    },
}

/// `tcr reset <account> [--kind weekly|full|fable|session] [--yes]`.
#[derive(clap::Args)]
struct ResetArgs {
    account: String,
    #[command(flatten)]
    config: ConfigFile,
    /// Which kind of reset to use on a Claude account: `weekly`, `full`, `fable` or
    /// `session`. Needed only when the account holds more than one kind.
    #[arg(long, value_parser = parse_reset_kind)]
    kind: Option<tcr_status_wire::ResetKind>,
    /// Reuse one attempt across retries; never printed.
    #[arg(long)]
    idempotency_key: Option<String>,
    /// Skip the questions, for a caller that already asked: use the reset without
    /// reading input, and end on an unknown outcome instead of offering a retry.
    #[arg(long)]
    yes: bool,
}

fn parse_reset_kind(token: &str) -> Result<tcr_status_wire::ResetKind, String> {
    tcr_status_wire::ResetKind::parse(token)
        .ok_or_else(|| format!("expected weekly, full, fable or session, not {token:?}"))
}

/// `tcr routing --json`. `--json` is required: there is no text form a launcher could
/// mistake for the document.
#[derive(clap::Args)]
struct RoutingArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Print the schema-1 routing document as JSON (required).
    #[arg(long, required = true)]
    json: bool,
}

#[derive(clap::Args)]
struct WrapArgs {
    /// Path to the config file, consulted only for pricing overrides, never
    /// for accounts.
    #[arg(long)]
    config: Option<PathBuf>,
    /// How many days back to report, ending today (UTC).
    #[arg(long, default_value_t = 7)]
    days: u32,
    /// Emit the report as one JSON object instead of plain text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct UpdateArgs {
    /// Rebuild even when `git pull` reports the checkout is already up to date.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct LoginArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Skip the refusal a running proxy would otherwise trigger, and write the
    /// login to the config file directly. Still probes the proxy first and still
    /// prefers its live account-add route when one is confirmed and safe — this
    /// only overrides the cases login would otherwise refuse on: an older proxy
    /// with no account-add route, or one that answered but not usably (wedged or
    /// timed out). Never overrides a confirmed live route (already safe, nothing
    /// to force) or a rejected api-key (proof the proxy is alive, so writing the
    /// file beside it is the worst-informed moment to do it) — both still refuse
    /// under --force. Unsafe when it takes the file path: the running server's
    /// next token refresh can overwrite what was just written.
    #[arg(long)]
    force: bool,
    /// Re-login a specific existing account. The identity that comes back must
    /// match, or nothing is written.
    #[arg(long)]
    account: Option<String>,
    /// Add an account from a `claude setup-token` credential instead of the
    /// browser flow — no value here. The token is read from stdin (prompted
    /// when stdin is a TTY), never from argv: an argv value is visible in
    /// `ps` and lands in shell history, both worse leaks than a stdin prompt.
    /// A setup-token credential carries only the `user:inference` scope, so
    /// there is no refresh token (the account serves until the token expires,
    /// about a year, then goes dead — see the warning `tcr login --token`
    /// prints) and usually no email (name it with `--name`, or answer the
    /// prompt). Refuses to combine with `--account`: an inference-only token
    /// carries no identity for that flag to confirm, and an assertion that
    /// cannot be evaluated must fail closed.
    #[arg(long)]
    token: bool,
    /// Add an account from the login the `claude` CLI on this machine already
    /// holds, instead of the browser flow — no value here. The credential is
    /// read from the login Keychain (item `Claude Code-credentials`) on macOS,
    /// else from `~/.claude/.credentials.json`; `TCR_CLAUDE_CODE_CREDENTIALS`
    /// overrides both with a file path. It carries a refresh token and a real
    /// expiry, so the account keeps working the same way a browser login's
    /// does. The cost, printed on import: refresh tokens are single-use, so the
    /// first time tcr refreshes this one, the `claude` CLI's own copy dies and
    /// `claude` asks for a browser login once. Refuses to combine with
    /// `--token`, which is a different credential from a different place.
    /// A first run does this by itself when it finds no accounts — this flag is
    /// the explicit redo (after `tcr remove`, or onto a second config).
    #[arg(long, conflicts_with = "token")]
    from_claude_code: bool,
    /// Import the login the `codex` CLI already holds (`$CODEX_HOME/auth.json`,
    /// default `~/.codex/auth.json`; `TCR_CODEX_AUTH` names a file to read
    /// instead) as a Codex account — no value here. The account is named by the
    /// login's email, or `<email>/codex` when another account holds that name.
    /// With `--account`, the login must be that account or nothing is written;
    /// it takes the row name `tcr accounts` prints, or the login's email.
    /// Refreshed tokens are written back to auth.json only while its refresh
    /// token still matches the one tcr loaded. Newer tokens for the same ChatGPT account
    /// are adopted; a changed login is left untouched. A second account: `mkdir -p <dir>` and
    /// `CODEX_HOME=<dir> codex login`, then this with `--codex-home <dir>`.
    #[arg(long, conflicts_with_all = ["token", "from_claude_code"])]
    from_codex: bool,
    /// Sign in a Codex (ChatGPT) account with a login of tcr's own: tcr runs `codex login` in a
    /// throwaway folder, imports it, and deletes the folder, so the codex CLI's own login is not
    /// touched. With --account, the login must be that row's user and workspace, or nothing is
    /// written.
    #[arg(long, conflicts_with_all = ["token", "from_claude_code", "from_codex"])]
    codex: bool,
    /// Read `<DIR>/auth.json` instead of `$CODEX_HOME` / `~/.codex` (for a second login made
    /// with `mkdir -p <DIR>` and `CODEX_HOME=<DIR> codex login`).
    #[arg(long, requires = "from_codex", value_name = "DIR")]
    codex_home: Option<PathBuf>,
    /// Drive the login from another program instead of a terminal: stdin is
    /// never read, and progress goes to stdout as one JSON object per line —
    /// `{"event":"browser","url":…}`, `{"event":"waiting"}`,
    /// `{"event":"notice","text":…}`, `{"event":"saved","account":…}`,
    /// `{"event":"error","reason":…}`; a Codex account's `saved` adds
    /// `provider` and, when known, `plan`, and a refusal a caller can name adds
    /// `code`. tcr's own browser flow never opens the browser here: the caller
    /// opens the URL, the loopback callback is then the only way the login can
    /// complete, and the same 2-minute timeout exits non-zero with the reason
    /// on one stderr line. Under `--codex`, `codex login` opens the browser
    /// itself, so its `browser` line carries `"opened":true` and the caller
    /// opens nothing; `--codex` has no timeout, and waits until the sign-in
    /// ends or the caller sends SIGTERM, which also stops `codex login`.
    /// Refuses to combine with `--token`, which reads the credential from
    /// stdin: that is the one input this mode has no way to supply.
    #[arg(long, conflicts_with = "token")]
    non_interactive: bool,
    /// Name this account explicitly, overriding the name login would mint for
    /// it. Refused if some other account already has that name — names are
    /// unique, and taking one from an existing row is how a login overwrites
    /// the wrong credential. `--token` needs it most (an inference-only
    /// credential's profile fetch usually comes back with no email to name it
    /// from), but the browser flow honours it too.
    #[arg(long)]
    name: Option<String>,
}

/// `tcr import [--yes]`.
#[derive(clap::Args)]
struct ImportArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Import every login the listing marks new, through the same path `tcr login` writes by.
    /// Without it, `tcr import` only lists: no refresh, no profile fetch, no write, no call to a
    /// running proxy. Each refresh token is single-use, so a login taken from claude-swap or
    /// `CLIProxyAPI` stops working there at tcr's first refresh: remove it from that tool. A login
    /// already in tcr is never written, so tcr's own token is never replaced by another tool's
    /// copy. Exits non-zero when any new login could not be imported.
    #[arg(long)]
    yes: bool,
}

#[derive(clap::Args)]
struct MintArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Mint a long-lived token for one existing account — exact name, not a
    /// substring (see `resolve_account`). Exactly one of `--account` /
    /// `--group` is required.
    #[arg(long, required_unless_present = "group", conflicts_with = "group")]
    account: Option<String>,
    /// Mint a long-lived token for every account carrying this group label.
    #[arg(long)]
    group: Option<String>,
}

/// The name under which this binary is `tcr run`: a symlink called `tcr-run`
/// (`tcr setup tcr-run`) gives a tool that takes a Claude Code executable path
/// one path to point at.
const TCR_RUN_ALIAS: &str = "tcr-run";

/// The argv clap parses: this process's own, or, when it was started as
/// [`TCR_RUN_ALIAS`], `tcr run` followed by every argument exactly as given.
/// Rewriting the argv rather than building a `RunArgs` by hand means
/// `tcr-run X` and `tcr run X` go through one parser and cannot drift.
fn effective_argv(argv: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let invoked_as_alias = argv.first().is_some_and(|argv0| {
        Path::new(argv0)
            .file_name()
            .is_some_and(|name| name == TCR_RUN_ALIAS)
    });
    if !invoked_as_alias {
        return argv;
    }
    let mut out = vec![std::ffi::OsString::from("tcr"), "run".into()];
    out.extend(argv.into_iter().skip(1));
    out
}

/// The CLI with every `--config` flag's help ending in the one sentence that
/// says what the default is ([`tcr::paths::config_flag_default_help`]).
///
/// Thirty-odd verbs take `--config`, each with its own description of what it
/// reads the file for. The default used to be hand-copied into every one of
/// them; appending it here keeps it in one place, so the default cannot be
/// right on one verb and stale on the next.
fn cli_command() -> clap::Command {
    fn with_default(cmd: clap::Command, sentence: &str) -> clap::Command {
        cmd.mut_args(|arg| {
            if arg.get_long() != Some("config") {
                return arg;
            }
            let join = |help: &clap::builder::StyledStr| {
                let help = help.to_string();
                let help = help.trim_end().trim_end_matches('.');
                format!("{help}. {sentence}.")
            };
            let help = arg.get_help().map_or_else(|| format!("{sentence}."), join);
            match arg.get_long_help().map(join) {
                Some(long_help) => arg.help(help).long_help(long_help),
                None => arg.help(help),
            }
        })
        .mut_subcommands(|sub| with_default(sub, sentence))
    }
    with_default(
        <Cli as clap::CommandFactory>::command(),
        &tcr::paths::config_flag_default_help(),
    )
}

/// `Cli::parse_from`, over [`cli_command`] rather than the bare derived command.
fn parse_cli(argv: Vec<std::ffi::OsString>) -> Cli {
    let mut cmd = cli_command();
    let mut matches = cmd
        .try_get_matches_from_mut(argv)
        .unwrap_or_else(|err| err.exit());
    <Cli as clap::FromArgMatches>::from_arg_matches_mut(&mut matches)
        .unwrap_or_else(|err| err.format(&mut cmd).exit())
}

/// The runtime is built here rather than by `#[tokio::main]`, whose default is one worker
/// thread per core: see `tcr::server::worker_threads` for the count and why.
fn main() -> anyhow::Result<()> {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let workers =
        server::worker_threads(std::env::var("TOKIO_WORKER_THREADS").ok().as_deref(), cores);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?
        .block_on(run_main())
}

async fn run_main() -> anyhow::Result<()> {
    let cli = parse_cli(effective_argv(std::env::args_os().collect()));
    match cli.command {
        Some(Command::Server(args)) => run_server(args).await,
        Some(Command::Run(args)) => run_harness(args).await,
        Some(Command::Login(args)) => run_login(args).await,
        Some(Command::Import(args)) => tcr::import::run(&args.config.path(), args.yes).await,
        Some(Command::Mint(args)) => run_mint(args).await,
        Some(Command::Accounts(args)) => run_accounts(args).await,
        Some(Command::Remove(args)) => run_remove(args).await,
        Some(Command::Token(args)) => run_token(args),
        Some(Command::RequestKey(args)) => run_request_key(args),
        Some(Command::Priority(args)) => run_priority(args).await,
        Some(Command::Enable(args)) => run_enable(args).await,
        Some(Command::Disable(args)) => run_disable(args).await,
        Some(Command::Control(args)) => run_control(args).await,
        Some(Command::Group(args)) => run_group(args),
        Some(Command::Config(args)) => run_config(args).await,
        Some(Command::Peer(args)) => run_peer(args).await,
        Some(Command::Status(args)) => run_status(args).await,
        Some(Command::Sessions(args)) => run_sessions(args).await,
        Some(Command::Routing(args)) => run_routing(args).await,
        Some(Command::Codex(args)) => {
            let CodexCommand::Reset {
                account,
                config,
                idempotency_key,
                yes,
            } = args.command;
            let code = cli::reset(
                &config.path(),
                &account,
                None,
                idempotency_key.as_deref(),
                yes,
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Some(Command::Reset(args)) => {
            let code = cli::reset(
                &args.config.path(),
                &args.account,
                args.kind,
                args.idempotency_key.as_deref(),
                args.yes,
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Some(Command::List(args)) => run_list(args).await,
        Some(Command::Wrap(args)) => run_wrap(args),
        Some(Command::Update(args)) => update::run_update(args.force),
        Some(Command::Demo) => demo::run_demo().await.map_err(anyhow::Error::from),
        Some(Command::Ui) => run_ui(),
        Some(Command::Doctor(args)) => run_doctor(args).await,
        Some(Command::Setup(args)) => run_setup(args),
        Some(Command::Fan(args)) => run_fan(args).await,
        None => run_server(cli.server).await,
    }
}

/// `tcr accounts [--probe]` — list the configured accounts (offline).
async fn run_accounts(args: AccountsArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    cli::list_accounts(&config_path, args.probe).await
}

/// `tcr remove <query>` — delete an account from the config, applying
/// a live disable through the RUNNING proxy first where there is one.
async fn run_remove(args: RemoveArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    cli::remove_account(&config_path, &args.query).await
}

/// `tcr token <query>` — print the account's access token.
fn run_token(args: TokenArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    cli::print_access_token(&config_path, &args.query)
}

/// `tcr request-key`: print the request key the server wrote to the config.
fn run_request_key(args: RequestKeyArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    println!("{}", cli::request_key(&config_path)?);
    Ok(())
}

/// `tcr priority <query> [N|--first|--last]` — set rotation priority, in the
/// RUNNING proxy where there is one.
async fn run_priority(args: PriorityArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let priority = if args.first {
        PriorityArg::First
    } else if args.last {
        PriorityArg::Last
    } else if let Some(n) = args.value {
        PriorityArg::N(n)
    } else {
        anyhow::bail!("provide a priority value, or one of --first / --last");
    };
    cli::set_priority(&config_path, &args.query, priority).await
}

/// `tcr enable <query>` — clear an account's `disabled` flag, in the
/// RUNNING proxy where there is one (async for that reason alone).
async fn run_enable(args: EnableArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    cli::set_enabled(&config_path, &args.query, false).await
}

/// `tcr disable <query>` — hold an account out of rotation, in the RUNNING
/// proxy where there is one.
async fn run_disable(args: DisableArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    cli::set_enabled(&config_path, &args.query, true).await
}

/// `tcr control <query> | --clear | --show` — set, clear, or show the
/// identity-bound control account, in the RUNNING proxy where there is one.
async fn run_control(args: ControlArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    if args.show {
        return cli::show_control(&config_path).await;
    }
    if args.clear {
        return cli::set_control(&config_path, None).await;
    }
    let Some(query) = args.query else {
        anyhow::bail!("provide an account query, or --clear / --show");
    };
    cli::set_control(&config_path, Some(&query)).await
}

/// `tcr config show [--json]` and `tcr config set <key> <value>`.
async fn run_config(args: ConfigArgs) -> anyhow::Result<()> {
    match args.action {
        ConfigAction::Show(a) => {
            let config_path = a.config.path();
            cli::show_config(&config_path, a.json).await
        }
        ConfigAction::Set(a) => {
            let config_path = a.config.path();
            cli::set_config(&config_path, &a.key, &a.value.join(" ")).await
        }
    }
}

/// `tcr group ls|add|rm|reserve|unreserve|allow-control|disallow-control|color` — manage account group membership. Argument shape is
/// the TcrBar panel contract — see [`GroupAction`]'s doc-comment.
fn run_group(args: GroupArgs) -> anyhow::Result<()> {
    match args.action {
        GroupAction::Ls(a) => {
            let config_path = a.config.path();
            cli::list_groups(&config_path, a.json)
        }
        GroupAction::Add(a) => {
            let config_path = a.config.path();
            cli::add_to_group(&config_path, &a.group, &a.account)
        }
        GroupAction::Rm(a) => {
            let config_path = a.config.path();
            cli::remove_from_group(&config_path, &a.group, a.account.as_deref(), a.all)
        }
        GroupAction::Reserve(a) => {
            let config_path = a.config.path();
            cli::reserve_group_with(&config_path, &a.group, a.force)
        }
        GroupAction::Unreserve(a) => {
            let config_path = a.config.path();
            cli::unreserve_group(&config_path, &a.group)
        }
        GroupAction::Park(a) => {
            let config_path = a.config.path();
            cli::park_group(&config_path, &a.group)
        }
        GroupAction::Unpark(a) => {
            let config_path = a.config.path();
            cli::unpark_group(&config_path, &a.group)
        }
        GroupAction::AllowControl(a) => {
            let config_path = a.config.path();
            cli::allow_control_group(&config_path, &a.group)
        }
        GroupAction::DisallowControl(a) => {
            let config_path = a.config.path();
            cli::disallow_control_group(&config_path, &a.group)
        }
        GroupAction::Color(a) => {
            let config_path = a.config.path();
            // clap's `conflicts_with`/`required_unless_present` on
            // `GroupColorArgs` guarantee exactly one of `hex`/`--clear`.
            cli::set_group_color(&config_path, &a.group, a.hex.as_deref())
        }
    }
}

/// `tcr status [--json] [--check]` — probe every account's live quota and print it.
///
/// The exit code is carried out of [`cli::status`] and spent here, the same
/// shape as [`run_doctor`]: it is non-zero only under `--check`, after the
/// output is printed.
async fn run_status(args: StatusArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let code = cli::status(&config_path, args.json, args.check).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `tcr doctor [--json] [--codex]`: whether Claude Code is routed to this
/// proxy, and whether codex is pooled when the fleet holds a Codex account or
/// `--codex` asks.
///
/// The exit code is the point of the verb (a script asks `tcr doctor >/dev/null
/// || …`), so it is carried out of [`cli::doctor`] and spent here rather than
/// flattened into the `anyhow::Result` every other verb returns: `Ok(())` is
/// exit 0 and there is no way to say 2 or 3 through it. `std::process::exit`
/// after the printing is done, the same shape `run_server`'s stand-down path
/// uses.
/// `tcr setup shell|claude-settings|status`. A refusal is an error, so the
/// exit code is non-zero and the conflict lines are on stderr.
fn run_setup(args: SetupArgs) -> anyhow::Result<()> {
    use tcr::setup;
    let homes = setup::Homes::from_env()?;
    match args.action {
        SetupAction::Shell(args) => {
            let changes = if args.remove {
                setup::remove_shell(&homes)?
            } else {
                let Some(shell) = setup::chosen_shell(args.shell) else {
                    anyhow::bail!(
                        "$SHELL does not name zsh, bash or fish; pass --shell to choose one"
                    );
                };
                setup::install_shell(shell, &homes, &setup::tcr_word()?)?
            };
            println!("{}", setup::describe_changes(&changes));
            if !args.remove {
                println!("open a new terminal (or source the file) for `claude` to go through tcr");
            }
        }
        SetupAction::ClaudeSettings(args) => {
            let change = if args.remove {
                setup::remove_settings(&homes)?
            } else {
                let config_path = args.config.path();
                let port = setup::configured_port(&config_path)?;
                setup::install_settings(&homes, port, args.force)?
            };
            let path = change.path.display();
            let var = tcr::doctor::BASE_URL_VAR;
            println!(
                "{}",
                match change.change {
                    setup::Change::Added | setup::Change::Updated =>
                        format!("set env.{var} in {path}"),
                    setup::Change::Removed => format!("removed env.{var} from {path}"),
                    setup::Change::Unchanged | setup::Change::Deleted =>
                        format!("nothing to change in {path}"),
                }
            );
            if !args.remove && change.change != setup::Change::Unchanged {
                println!(
                    "every claude launch now goes through tcr; while tcr is not running, claude cannot reach Anthropic (undo: tcr setup claude-settings --remove)"
                );
            }
        }
        SetupAction::TcrRun(args) => {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let Some(dir) = setup::tcr_dir_on_path(&path) else {
                anyhow::bail!(
                    "no tcr on PATH; tcr-run is linked beside it, so put tcr on PATH first"
                );
            };
            let change = if args.remove {
                setup::remove_tcr_run(&dir)?
            } else {
                setup::install_tcr_run(&dir)?
            };
            let link = change.path.display();
            println!(
                "{}",
                match change.change {
                    setup::Change::Added => format!("linked {link} -> tcr"),
                    setup::Change::Deleted => format!("removed {link}"),
                    _ if args.remove => format!("no {link} to remove"),
                    _ => format!("already linked: {link} -> tcr"),
                }
            );
        }
        SetupAction::Status(args) => {
            let config_path = args.config.path();
            let port = setup::configured_port(&config_path)?;
            let status = setup::status(setup::chosen_shell(args.shell), &homes, port)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("{}", status.render_text());
            }
        }
    }
    Ok(())
}

async fn run_doctor(args: DoctorArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let code = cli::doctor(&config_path, args.json, args.codex).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `tcr fan [auto [--force] | fixed <RPM> | follow <QUIET_C> <FULL_C>]`.
async fn run_fan(args: FanArgs) -> anyhow::Result<()> {
    use tcr::fan::FanRequest;
    let request = match args.action {
        Some(FanAction::Probe(probe)) => return tcr::fan_probe::run(probe.label).await,
        None => None,
        Some(FanAction::Auto { force }) => Some(FanRequest::Auto { force }),
        Some(FanAction::Fixed { rpm }) => Some(FanRequest::Fixed { rpm }),
        Some(FanAction::Follow { quiet_c, full_c }) => Some(FanRequest::FollowCpu {
            quiet_below_c: quiet_c,
            full_at_c: full_c,
        }),
    };
    let code = cli::fan(&args.config.path(), request, args.json).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `tcr sessions [--json]` — the running proxy's live sessions.
async fn run_sessions(args: SessionsArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    cli::sessions(&config_path, args.json).await
}

/// `tcr routing --json` — the running proxy's routing document.
///
/// The exit code is carried out of [`cli::routing`] and spent here, the same shape as
/// [`run_status`]: 3 when no proxy is listening, 4 when it could not be read.
async fn run_routing(args: RoutingArgs) -> anyhow::Result<()> {
    debug_assert!(args.json, "clap refuses `tcr routing` without --json");
    let config_path = args.config.path();
    let code = cli::routing(&config_path).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `tcr list --json`: claude-swap's list schema v1, for CodexBar.
///
/// Bare `tcr list` is a usage error (exit 2, clap's code for one) that names
/// both ways forward, rather than a second, text rendering: the people who
/// want a table already have `tcr status`, and a text form here would be one
/// more thing to keep in step with it.
///
/// Exit 1 carries the schema-v1 error envelope on stdout ([`cli::list_v1`]);
/// `std::process::exit` after the printing, the same shape `run_doctor` uses.
async fn run_list(args: ListArgs) -> anyhow::Result<()> {
    if !args.json {
        eprintln!(
            "error: `tcr list` prints JSON only, for CodexBar: run `tcr list --json`, or `tcr status` for the readable view"
        );
        std::process::exit(2);
    }
    let config_path = args.config.path();
    let code = cli::list_v1(&config_path).await;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `tcr wrap [--days N] [--json]` — a usage report read from the ledger.
fn run_wrap(args: WrapArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    cli::wrap(&config_path, args.days, args.json)
}

/// `tcr ui` — open TcrBar, the macOS menu-bar app.
///
/// This exists for discoverability, not capability: `open -a TcrBar` already
/// works. Without it nothing in `tcr --help` reveals that a UI exists at all, so
/// the app is only findable by knowing it is there.
///
/// It deliberately does NOT build the app or know where the checkout is. It asks
/// `LaunchServices` to open a bundle id, which resolves wherever the app was
/// installed. A `tcr` that shells into a source tree would break the moment the
/// checkout moved.
/// The shared mount/swap logic, embedded once so `install.sh` and this binary
/// never carry two copies that drift. See `scripts/install-tcrbar-from-dmg.sh`
/// for what it does and why.
#[cfg(target_os = "macos")]
const INSTALL_TCRBAR_FROM_DMG_SH: &str = include_str!("../scripts/install-tcrbar-from-dmg.sh");

/// Is a process literally named `TcrBar` running right now?
///
/// Matched by exact name (`pgrep -x`), not a path pattern like
/// `apps/macos/scripts/install.sh` uses — this runs on a machine that may
/// have TcrBar installed from a dmg, never built from source, so there is no
/// destination path to derive a pattern from. `pgrep -x` still cannot
/// distinguish it from an unrelated program that happens to share the name;
/// that gap already exists in `apps/macos/scripts/uninstall.sh`.
#[cfg(target_os = "macos")]
fn tcrbar_is_running() -> anyhow::Result<bool> {
    use anyhow::Context;
    let status = std::process::Command::new("pgrep")
        .args(["-x", "TcrBar"])
        .status()
        .context("failed to run `pgrep`")?;
    Ok(status.success())
}

/// `tcr ui` — open TcrBar, installing it first if it is missing (macOS only).
#[cfg(target_os = "macos")]
fn run_ui() -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write as _};

    use anyhow::Context;

    let status = std::process::Command::new("open")
        .args(["-b", "io.github.dhkts1.tcrbar"])
        .status()
        .context("failed to run `open`")?;

    if status.success() {
        return Ok(());
    }

    // `open -b` fails when the bundle id is not registered, which almost
    // always means "not installed" rather than "broken".
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "TcrBar is not installed. Run `tcr ui` in a terminal to install it, or \
             download the dmg from https://github.com/dhkts1/tcr/releases/latest"
        );
    }

    if tcrbar_is_running()? {
        anyhow::bail!(
            "a process named TcrBar is already running but is not registered with \
             LaunchServices under io.github.dhkts1.tcrbar — quit it before `tcr ui` \
             installs a fresh copy, then run `tcr ui` again. Installing over a running \
             copy is refused: its own bundled `tcr` may be an executing image inside \
             the very bundle being replaced."
        );
    }

    eprint!("TcrBar is not installed. Download and install it to /Applications? [y/N] ");
    std::io::stderr()
        .flush()
        .context("failed to flush the prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed to read the answer")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        anyhow::bail!("not installing — re-run `tcr ui` when you're ready.");
    }

    let tag = update::fetch_latest_release_tag()
        .context("could not resolve the latest TcrBar release")?;
    println!("tcr: downloading TcrBar {tag}…");

    let tmp_dir = tempfile::Builder::new()
        .prefix("tcr-ui-install-")
        .tempdir()
        .context("could not create a temp directory for the download")?;
    let dmg_path = tmp_dir.path().join("TcrBar.dmg");
    update::download_tcrbar_dmg(&tag, &dmg_path)
        .with_context(|| format!("could not download the TcrBar {tag} dmg"))?;

    let script_path = tmp_dir.path().join("install-tcrbar-from-dmg.sh");
    std::fs::write(&script_path, INSTALL_TCRBAR_FROM_DMG_SH)
        .context("could not write the install script to a temp file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .context("could not make the install script executable")?;
    }

    let install_status = std::process::Command::new("bash")
        .arg(&script_path)
        .arg(&dmg_path)
        .status()
        .context("failed to run the TcrBar install script")?;
    if !install_status.success() {
        anyhow::bail!("installing TcrBar {tag} failed with {install_status}");
    }

    let status = std::process::Command::new("open")
        .args(["-b", "io.github.dhkts1.tcrbar"])
        .status()
        .context("failed to run `open`")?;
    if !status.success() {
        anyhow::bail!("TcrBar {tag} was installed but `open -b` still failed with {status}");
    }
    Ok(())
}

/// Non-macOS builds keep the subcommand so `--help` is identical everywhere, and
/// fail with the reason rather than a missing-subcommand error.
#[cfg(not(target_os = "macos"))]
fn run_ui() -> anyhow::Result<()> {
    anyhow::bail!("`tcr ui` opens the macOS menu-bar app, and this is not macOS.")
}

/// `tcr login` — browser OAuth PKCE flow that authenticates a Claude account
/// and appends (or updates) it in the config, OR (`--token`) a
/// `claude setup-token` credential read from stdin. The heavy lifting lives
/// in [`oauth::login`] / [`oauth::login_with_token`]; this just resolves the
/// config path and reports.
async fn run_login(args: LoginArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let ui = if args.non_interactive {
        oauth::LoginUi::Machine
    } else {
        oauth::LoginUi::Terminal
    };
    if args.from_claude_code {
        let result = oauth::login_from_claude_code(
            &config_path,
            args.force,
            args.name.as_deref(),
            args.account.as_deref(),
            ui,
        )
        .await
        .context("importing the Claude Code login failed");
        return finish_login(result, args.non_interactive);
    }
    if args.from_codex {
        let result = oauth::login_from_codex(
            &config_path,
            args.force,
            args.name.as_deref(),
            args.account.as_deref(),
            args.codex_home.as_deref(),
            ui,
        )
        .await
        .context("importing the Codex login failed");
        return finish_login(result, args.non_interactive);
    }
    if args.codex {
        let result = tcr::codex_cli_login::login(
            &config_path,
            args.force,
            args.account.as_deref(),
            args.name.as_deref(),
            ui,
        )
        .await
        .context("the Codex sign-in failed");
        return finish_login(result, args.non_interactive);
    }
    if args.token {
        let name = oauth::login_with_token(
            &config_path,
            args.force,
            args.name.as_deref(),
            args.account.as_deref(),
        )
        .await
        .context("setup-token login failed")?;
        println!("Logged in as '{name}'.");
        return Ok(());
    }
    let result = oauth::login(
        &config_path,
        args.force,
        args.account.as_deref(),
        args.name.as_deref(),
        ui,
    )
    .await
    .context("OAuth login failed");
    finish_login(result, args.non_interactive)
}

/// How every login ends: `Logged in as` in the terminal, nothing more in the machine form
/// (whose stream already said it as `{"event":"saved",…}`).
///
/// A machine caller gets a failure the same way it got every other step — one JSON line on
/// stdout, coded when it is a refusal a GUI titles by its code (`oauth::machine_error_line`) —
/// and the human-readable half on one stderr line rather than as anyhow's indented multi-line
/// chain, which a GUI would have to render as a wall of text. It exits here rather than
/// returning the error, because `main`'s reporter would print that chain on top of what was
/// just said.
fn finish_login(result: anyhow::Result<String>, non_interactive: bool) -> anyhow::Result<()> {
    match result {
        Ok(name) => {
            if !non_interactive {
                println!("Logged in as '{name}'.");
            }
            Ok(())
        }
        Err(error) if non_interactive => {
            let (line, reason) = oauth::machine_error_line(&error);
            println!("{line}");
            eprintln!("{reason}");
            std::process::exit(1);
        }
        Err(error) => Err(error),
    }
}

/// `tcr mint --account <name> | --group <name>` — mint a long-lived token for
/// one account or every account in a group and put the result on the
/// clipboard. Exits non-zero when any targeted account failed or mismatched,
/// so a UI driving this (the TcrBar menu item) can surface the failure.
async fn run_mint(args: MintArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let all_ok = mint::run_mint(&config_path, args.account.as_deref(), args.group.as_deref())
        .await
        .context("mint failed")?;
    if !all_ok {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
#[path = "cmd/main_tests.rs"]
mod tests;
