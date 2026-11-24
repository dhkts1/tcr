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

mod peer_cmd;
mod run_cmd;
mod server_cmd;
mod tool_env;

use tcr::cli::{self, PriorityArg};
use tcr::config;
use tcr::{demo, mint, oauth, server, update};

use peer_cmd::{peer_cli, run_peer};
use run_cmd::run_harness;
use server_cmd::run_server;

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

/// `--config` on every verb that takes it with the stock help line, resolved
/// in one place.
#[derive(clap::Args, Clone, Debug)]
struct ConfigFile {
    /// Path to the config file.
    #[arg(long)]
    config: Option<PathBuf>,
}

impl ConfigFile {
    /// The path given, or the default config path.
    fn path(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(config::default_path)
    }
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

#[derive(clap::Args)]
struct RunArgs {
    #[command(flatten)]
    config: ConfigFile,
    /// Serve this session from accounts labelled with this group. Strict by
    /// default: when no member can serve, the request waits for one or gets a
    /// 429, and is never sent to an account outside the group, even one the
    /// session is already pinned to. Set `groupSettings.<group>.spillToPool:
    /// true` in the config to let it fall back to the whole pool instead (a
    /// reserved group stays strict either way).
    /// Claude sessions only; a Codex session cannot ask for a group.
    #[arg(long)]
    group: Option<String>,
    /// Launch `codex` instead of `claude`, routed through the proxy's Codex accounts.
    #[arg(long)]
    codex: bool,
    /// Args passed verbatim to `claude`, or to `codex` with `--codex` (e.g. `tcr run -- -p "hi"`,
    /// or `tcr run --codex -- exec "hi"`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(clap::Args)]
struct ServerArgs {
    /// Port to bind (overrides `proxy.port` from the config).
    #[arg(long)]
    port: Option<u16>,
    #[command(flatten)]
    config: ConfigFile,
    /// Run without the TUI, logging to stdout.
    #[arg(long)]
    headless: bool,
    /// Take over the listening socket without unbinding the port. The old server
    /// drains its accepted connections. An older server without handoff support
    /// is left running and must be stopped explicitly once to upgrade.
    #[arg(long)]
    replace: bool,
    /// DEPRECATED and now a no-op: this is the default. Kept accepted so existing
    /// scripts and launch agents that pass it keep working. Pass `--replace` for
    /// the old default (take the port over).
    ///
    /// `conflicts_with` rather than a silent precedence rule: the two flags are a
    /// contradiction, and the previous wiring resolved it by quietly discarding
    /// `--replace`. An operator whose launchd plist or shell alias already carries
    /// `--no-replace`, adding `--replace` to force a rebuilt binary onto the port,
    /// got a stand-down and exit 0 — while `--help` told them the flag they left
    /// in place does nothing. clap now rejects the pair by name, which is the only
    /// outcome that cannot be misread.
    #[arg(long, conflicts_with = "replace")]
    no_replace: bool,
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
mod tests {
    use super::*;
    // What these tests reach in the binary modules split out of this file. They
    // move beside those modules next, and these lines go with them.
    use crate::peer_cmd::ls::peer_ls_inert_exit_lines;
    use crate::run_cmd::launch_env::{
        LaunchKey, LaunchRequestKey, apply_base_url_env, apply_codex_see_through_env,
        apply_nested_claude_ca, apply_see_through_env, compose_group_header, see_through_ca_notice,
    };
    use crate::run_cmd::{
        ClaudeVersionCheck, CodexLaunch, NoProxyRewrite, RUN_ACTIVE_ENV, apply_codex_launch,
        classify_claude_version_output, claude_command, codex_launch, codex_pool_preflight,
        mark_run_active, no_claude_account_warning, parse_claude_version, refuse_group_with_codex,
        resolve_harness_bin_from, see_through_ca_in, validate_group, withheld_api_key_notice,
    };
    use crate::server_cmd::logging::{headless_subscriber, log_location, open_log_appender};
    use crate::server_cmd::{
        EXIT_STOOD_DOWN_NOT_ANSWERING, EXIT_STOOD_DOWN_OK, EXIT_STOOD_DOWN_STALE, ShutdownTrigger,
        default_config, load_config, migration_persist_target, stand_down_exit_code,
        wedged_incumbent_recovery,
    };
    use build_info::StandDownBuild;
    use std::os::unix::fs::PermissionsExt as _;
    use tcr::cli::validate_group_label_chars;
    use tcr::codex_route::no_codex_account_reason;
    use tcr::doctor::{no_proxy_entry_exempts, points_at_tcr};
    use tcr::log_err::LogErr as _;
    use tcr::{affinity, build_info, mitm, singleton, status};

    fn silent() -> cli::Liveness {
        cli::Liveness::Silent {
            why: "the server did not answer within 5s".to_string(),
        }
    }

    /// `TCR_CLAUDE_BIN` outranks `CLAUDE_BIN` when both are set.
    #[test]
    fn tcr_claude_bin_wins_over_the_legacy_key() {
        assert_eq!(
            resolve_harness_bin_from(
                Some("/opt/beta/claude".to_string()),
                Some("/opt/legacy/claude".to_string()),
                "claude",
            ),
            "/opt/beta/claude",
        );
    }

    /// `CLAUDE_BIN` is used when the tcr-specific key is unset.
    #[test]
    fn claude_bin_is_the_fallback_when_tcr_claude_bin_is_unset() {
        assert_eq!(
            resolve_harness_bin_from(None, Some("/opt/legacy/claude".to_string()), "claude"),
            "/opt/legacy/claude",
        );
    }

    /// An empty value counts as unset, for either key — a shell that leaves
    /// `TCR_CLAUDE_BIN=` behind must not silently win over a real `CLAUDE_BIN`.
    #[test]
    fn an_empty_value_is_treated_as_missing() {
        assert_eq!(
            resolve_harness_bin_from(
                Some(String::new()),
                Some("/opt/legacy/claude".to_string()),
                "claude",
            ),
            "/opt/legacy/claude",
            "an empty TCR_CLAUDE_BIN must fall through to CLAUDE_BIN"
        );
        assert_eq!(
            resolve_harness_bin_from(Some(String::new()), Some(String::new()), "claude"),
            "claude",
            "two empty values must fall through to the default"
        );
    }

    /// Neither key set falls back to the default (`claude` on `PATH`).
    #[test]
    fn neither_key_set_falls_back_to_the_default() {
        assert_eq!(resolve_harness_bin_from(None, None, "claude"), "claude");
    }

    /// Pins that a configured proxy key is withheld from `claude`, and says why.
    #[test]
    fn the_proxy_key_is_withheld_from_claude_with_the_reason() {
        let notice = withheld_api_key_notice(true, false).expect("a configured key is withheld");
        assert!(
            notice.contains("connector"),
            "the notice must name what exporting it costs: {notice}"
        );
        assert!(
            notice.contains("Export it yourself"),
            "the notice must name the escape hatch: {notice}"
        );

        assert_eq!(
            withheld_api_key_notice(false, false),
            None,
            "nothing is withheld when no proxy key is configured"
        );
        assert_eq!(
            withheld_api_key_notice(true, true),
            None,
            "a key the caller exported is their choice — we neither replace it nor comment"
        );
    }

    /// A nested Codex keeps its own login through the unmarked proxy, and keeps
    /// whichever CA bundle it inherited rather than replacing it with tcr's.
    #[test]
    fn claude_see_through_env_preserves_a_nested_codex_ca_choice() {
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_see_through_env(
            &mut cmd,
            3999,
            Path::new("/tmp/ca.pem"),
            &LaunchKey::NoKeyInConfig,
        );
        let set = |name: &str| {
            cmd.get_envs()
                .find_map(|(k, v)| (k == name).then(|| v.map(|v| v.to_string_lossy().into_owned())))
        };
        assert_eq!(
            set("CODEX_CA_CERTIFICATE"),
            None,
            "an unmarked nested codex never meets our leaf and keeps its own CA choice"
        );
        assert_eq!(
            set("NODE_EXTRA_CA_CERTS"),
            Some(Some("/tmp/ca.pem".to_string())),
            "the same CA claude trusts"
        );
        assert_eq!(
            set("HTTPS_PROXY"),
            Some(Some("http://127.0.0.1:3999".to_string())),
            "the proxy a nested codex inherits"
        );
    }

    /// Neither routing mode may hand `claude` an `ANTHROPIC_API_KEY`.
    #[test]
    fn no_routing_mode_gives_claude_an_api_key() {
        for (label, apply) in [
            (
                "see-through",
                &(|cmd: &mut std::process::Command| {
                    let _lines =
                        apply_see_through_env(cmd, 3456, Path::new("/tmp/ca.pem"), &sent(KEY));
                }) as &dyn Fn(&mut std::process::Command),
            ),
            (
                "base-URL",
                &(|cmd: &mut std::process::Command| {
                    let _lines = apply_base_url_env(cmd, 3456, &sent(KEY));
                }),
            ),
        ] {
            let mut cmd = claude_command();
            apply(&mut cmd);
            assert!(
                !cmd.get_envs()
                    .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_some()),
                "{label} mode must not set ANTHROPIC_API_KEY — it outranks the claude.ai \
                 login and disables every claude.ai connector"
            );
        }
    }

    /// `codex`'s see-through env: both proxy spellings, because a client that reads only the
    /// one we skipped would go direct and bypass the pool in silence, and the CA in the one
    /// variable Codex reads it from. Nothing of Claude's: `NODE_EXTRA_CA_CERTS` means nothing to
    /// `codex`, and `ANTHROPIC_BASE_URL` has no business in its environment.
    #[test]
    fn codex_see_through_env_sets_proxy_and_ca_and_nothing_anthropic() {
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_see_through_env(
            &mut cmd,
            3999,
            Path::new("/tmp/ca.pem"),
            false,
            &LaunchKey::ServerPredatesKeys,
        );
        let env: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://tcr-codex@127.0.0.1:3999")
        );
        assert_eq!(
            env.get("https_proxy").map(String::as_str),
            Some("http://tcr-codex@127.0.0.1:3999")
        );
        assert_eq!(
            env.get("CODEX_CA_CERTIFICATE").map(String::as_str),
            Some("/tmp/ca.pem")
        );
        assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!env.contains_key("NODE_EXTRA_CA_CERTS"));
    }

    #[test]
    fn codex_see_through_env_respects_the_callers_ca() {
        let mut cmd = std::process::Command::new("true");
        cmd.env("CODEX_CA_CERTIFICATE", "/tmp/corporate-ca.pem");
        let _lines = apply_codex_see_through_env(
            &mut cmd,
            3999,
            Path::new("/tmp/tcr-ca.pem"),
            true,
            &LaunchKey::NoKeyInConfig,
        );
        assert_eq!(
            cmd.get_envs().find_map(|(key, value)| {
                (key == "CODEX_CA_CERTIFICATE").then_some(value.unwrap())
            }),
            Some(std::ffi::OsStr::new("/tmp/corporate-ca.pem")),
            "an explicit caller CA must not be replaced"
        );
    }

    /// A fake request key, hex like the ones the server writes.
    const KEY: &str = "0000111122223333444455556666777788889999aaaabbbbccccddddeeeeffff";

    /// The launch decision that sends `key`.
    fn sent(key: &str) -> LaunchKey {
        LaunchKey::Send(LaunchRequestKey::parse(key).expect("a URL-safe key"))
    }

    /// A running server's status read, parsed from the wire, with `requestKeyMode`
    /// set to `mode` or left out, as a server older than request keys leaves it.
    fn status_reporting(mode: Option<&str>) -> Result<status::StatusPayload, cli::LiveStatusError> {
        let mut wire = serde_json::json!({
            "kind": status::STATUS_KIND,
            "accounts": [],
            "build": {"sha": "abc1234"},
        });
        if let Some(mode) = mode {
            wire["requestKeyMode"] = mode.into();
        }
        Ok(serde_json::from_value(wire).expect("a status payload"))
    }

    /// Every decision that launches unkeyed, one of each.
    fn unkeyed_decisions() -> [LaunchKey; 4] {
        [
            LaunchKey::NoKeyInConfig,
            LaunchKey::KeyNotUrlSafe,
            LaunchKey::StatusUnread("nothing is listening".to_string()),
            LaunchKey::ServerPredatesKeys,
        ]
    }

    /// The gate: the key goes only to a server whose status carries
    /// `requestKeyMode`, whatever its value, and only when the config holds a key a
    /// URL can carry. Every other case is unkeyed with one notice that says why and
    /// never carries the key.
    #[test]
    fn the_request_key_is_sent_only_to_a_server_that_reports_request_key_mode() {
        let log = status_reporting(Some("log"));
        assert_eq!(LaunchKey::decide(Some(KEY), &log), sent(KEY));
        assert_eq!(
            LaunchKey::decide(Some(KEY), &status_reporting(Some("enforce"))),
            sent(KEY),
            "presence is the contract, not the value"
        );
        assert_eq!(
            LaunchKey::decide(Some(KEY), &status_reporting(None)),
            LaunchKey::ServerPredatesKeys,
            "an older server compares the whole Codex header: a keyed URL would run unpooled"
        );
        assert_eq!(
            LaunchKey::decide(Some(KEY), &Err(cli::LiveStatusError::NoServer)),
            LaunchKey::StatusUnread("nothing is listening".to_string())
        );
        assert_eq!(
            LaunchKey::decide(
                Some(KEY),
                &Err(cli::LiveStatusError::Unusable("HTTP 401".to_string()))
            ),
            LaunchKey::StatusUnread("HTTP 401".to_string())
        );
        assert_eq!(LaunchKey::decide(None, &log), LaunchKey::NoKeyInConfig);
        assert_eq!(LaunchKey::decide(Some(""), &log), LaunchKey::NoKeyInConfig);
        assert_eq!(
            LaunchKey::decide(Some("not a:url@key"), &log),
            LaunchKey::KeyNotUrlSafe
        );

        assert_eq!(sent(KEY).key().map(LaunchRequestKey::as_str), Some(KEY));
        assert_eq!(
            sent(KEY).notice(3999),
            None,
            "a keyed launch says nothing more"
        );
        for decision in unkeyed_decisions() {
            assert_eq!(decision.key(), None, "{decision:?}");
            let notice = decision
                .notice(3999)
                .unwrap_or_else(|| panic!("{decision:?} launches unkeyed and must say why"));
            assert!(
                notice.starts_with("[tcr] launching without the request key: "),
                "{notice}"
            );
        }
        assert!(
            !format!("{:?}", sent(KEY)).contains(KEY),
            "Debug must not print the key"
        );
    }

    /// (a): keyed, both proxy spellings carry the key as the plain `tcr` user's
    /// password; unkeyed, exactly the URL `tcr run` set before keys.
    #[test]
    fn a_keyed_see_through_launch_puts_the_key_in_both_proxy_urls() {
        let keyed = format!("http://tcr:{KEY}@127.0.0.1:3999");
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_see_through_env(&mut cmd, 3999, Path::new("/tmp/ca.pem"), &sent(KEY));
        let env = env_changes(&cmd);
        assert_eq!(env.get("HTTPS_PROXY"), Some(&Some(keyed.clone())));
        assert_eq!(env.get("https_proxy"), Some(&Some(keyed)));

        for decision in unkeyed_decisions() {
            let mut cmd = std::process::Command::new("true");
            let _lines = apply_see_through_env(&mut cmd, 3999, Path::new("/tmp/ca.pem"), &decision);
            let env = env_changes(&cmd);
            for var in ["HTTPS_PROXY", "https_proxy"] {
                assert_eq!(
                    env.get(var),
                    Some(&Some("http://127.0.0.1:3999".to_string())),
                    "{decision:?}: {var}"
                );
            }
        }
    }

    /// The `Proxy-Authorization` a client builds from `url` for an https request,
    /// by the matcher codex's HTTP stack runs, read back by the server's own parse.
    fn server_reads(url: &str) -> tcr::request_key::ProxyCredential {
        use hyper_util::client::proxy::matcher::Matcher;
        let target: hyper::Uri = "https://chatgpt.com/backend-api/codex/responses"
            .parse()
            .expect("a uri");
        let intercept = Matcher::builder()
            .https(url)
            .build()
            .intercept(&target)
            .expect("proxied");
        let auth = intercept
            .basic_auth()
            .map_or_default(|value| value.to_str().expect("ascii").to_string());
        tcr::request_key::ProxyCredential::from_connect_head(
            &format!("CONNECT chatgpt.com:443 HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n"),
            Some(KEY),
        )
    }

    /// Every variable the Codex shell policy sets, in order.
    const SHELL_POLICY_VARS: [&str; 8] = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NODE_EXTRA_CA_CERTS",
        "CODEX_CA_CERTIFICATE",
    ];

    /// The `shell_environment_policy.set.*` arguments on `cmd`, as (name, value).
    fn shell_policy(cmd: &std::process::Command) -> Vec<(String, String)> {
        cmd.get_args()
            .filter_map(|arg| {
                let arg = arg.to_string_lossy();
                let (name, value) = arg
                    .strip_prefix("shell_environment_policy.set.")?
                    .split_once('=')?;
                let value: String = serde_json::from_str(value).expect("a JSON string");
                Some((name.to_string(), value))
            })
            .collect()
    }

    /// Fail unless `cmd`'s shell policy sets exactly `want`, in [`SHELL_POLICY_VARS`]
    /// order. The message names the variables that differ and never their values,
    /// because a value may be a proxy URL that carries a request key.
    fn assert_shell_policy(cmd: &std::process::Command, want: &[(&str, &str)], context: &str) {
        let got = shell_policy(cmd);
        let names: Vec<&str> = got.iter().map(|(name, _)| name.as_str()).collect();
        assert!(
            names == SHELL_POLICY_VARS,
            "{context}: shell policy names {names:?}"
        );
        let differ: Vec<&str> = got
            .iter()
            .filter(|(name, value)| {
                let want = want.iter().find(|(n, _)| n == name).map_or("", |(_, v)| *v);
                value != want
            })
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(
            differ.is_empty(),
            "{context}: shell policy values differ for {differ:?}"
        );
    }

    /// Keyed model traffic retains the marker; shell defaults contain no key.
    /// The caller's environment and Codex config are injected and empty, so the
    /// result does not depend on the machine, or on a `tcr run` session around it.
    #[test]
    fn a_keyed_codex_launch_keeps_the_marker_and_restores_shell_settings() {
        let pooled = CodexLaunch::SeeThrough {
            ca: PathBuf::from("/tmp/ca.pem"),
            no_proxy: Vec::new(),
        };
        let caller = inherited(&[]);
        let host = test_host(&caller);

        let mut cmd = std::process::Command::new("true");
        let lines = apply_codex_launch(&mut cmd, 3999, &pooled, &sent(KEY), &host);
        let codex_url = format!("http://tcr-codex:{KEY}@127.0.0.1:3999");

        let env = env_changes(&cmd);
        for var in ["HTTPS_PROXY", "https_proxy"] {
            assert!(
                env.get(var) == Some(&Some(codex_url.clone())),
                "{var} is not the keyed codex URL"
            );
        }
        assert_shell_policy(&cmd, &[], "keyed");
        let codex = server_reads(&codex_url);
        assert!(codex.codex_marked && codex.key_ok, "{codex:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("caller's proxy and CA"))
        );

        for decision in unkeyed_decisions() {
            let mut cmd = std::process::Command::new("true");
            let lines = apply_codex_launch(&mut cmd, 3999, &pooled, &decision, &host);
            let env = env_changes(&cmd);
            assert!(
                env.get("HTTPS_PROXY")
                    == Some(&Some("http://tcr-codex@127.0.0.1:3999".to_string())),
                "{decision:?}: HTTPS_PROXY is not the unkeyed codex URL"
            );
            assert_shell_policy(&cmd, &[], &format!("{decision:?}"));
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.contains("without the request key"))
                    .count(),
                1,
                "{decision:?}: one notice says why: {lines:?}"
            );
        }

        let mut cmd = std::process::Command::new("true");
        let lines = apply_codex_launch(
            &mut cmd,
            3999,
            &CodexLaunch::Direct {
                reason: "no proxy answered on 127.0.0.1:3999".to_string(),
                unset: Vec::new(),
            },
            &sent(KEY),
            &host,
        );
        assert!(env_changes(&cmd).is_empty(), "a direct launch sets nothing");
        assert!(shell_policy(&cmd).is_empty());
        assert!(
            !lines.iter().any(|line| line.contains("request key")),
            "{lines:?}"
        );
    }

    /// The shell policy is built from the caller's environment and Codex config as
    /// injected: a caller proxy aimed at this tcr (carrying its key) and tcr's own
    /// CA are dropped, a caller's other proxy is kept, and a proxy the Codex config
    /// sets wins.
    #[test]
    fn the_codex_shell_policy_reads_the_injected_environment_and_codex_config() {
        let home = tempfile::tempdir().expect("a temporary HOME");
        std::fs::create_dir(home.path().join(".codex")).expect("a .codex dir");
        std::fs::write(
            home.path().join(".codex/config.toml"),
            "[shell_environment_policy.set]\nALL_PROXY = \"http://config.example:8080\"\n",
        )
        .expect("a codex config");
        let home_text = home.path().to_str().expect("a UTF-8 path");
        let pooled = CodexLaunch::SeeThrough {
            ca: PathBuf::from("/tmp/ca.pem"),
            no_proxy: Vec::new(),
        };
        let caller = inherited(&[
            ("HOME", home_text),
            ("HTTPS_PROXY", "http://k:secret@127.0.0.1:3999"),
            ("http_proxy", "http://corp.example:3128"),
            ("NODE_EXTRA_CA_CERTS", "/tmp/ca.pem"),
        ]);
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_launch(&mut cmd, 3999, &pooled, &sent(KEY), &test_host(&caller));
        assert_shell_policy(
            &cmd,
            &[
                ("http_proxy", "http://corp.example:3128"),
                ("ALL_PROXY", "http://config.example:8080"),
            ],
            "HOME's codex config",
        );

        // CODEX_HOME names the config directory and wins over HOME.
        let codex_home = tempfile::tempdir().expect("a CODEX_HOME");
        std::fs::write(
            codex_home.path().join("config.toml"),
            "[shell_environment_policy.set]\nALL_PROXY = \"http://codex-home.example:8080\"\n",
        )
        .expect("a codex config");
        let caller = inherited(&[
            ("HOME", home_text),
            (
                "CODEX_HOME",
                codex_home.path().to_str().expect("a UTF-8 path"),
            ),
        ]);
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_launch(&mut cmd, 3999, &pooled, &sent(KEY), &test_host(&caller));
        assert_shell_policy(
            &cmd,
            &[("ALL_PROXY", "http://codex-home.example:8080")],
            "CODEX_HOME's codex config",
        );
    }

    /// (d): keyed, `x-tcr-key` is appended after the caller's and `--group`'s
    /// headers, replacing an outer `tcr run`'s key line; unkeyed, the caller's
    /// value is untouched.
    #[test]
    fn a_keyed_base_url_launch_appends_the_key_after_the_callers_headers() {
        let headers = |inherited: Option<&str>, key: &LaunchKey| {
            let mut cmd = std::process::Command::new("true");
            match inherited {
                Some(value) => cmd.env("ANTHROPIC_CUSTOM_HEADERS", value),
                None => cmd.env_remove("ANTHROPIC_CUSTOM_HEADERS"),
            };
            let _lines = apply_base_url_env(&mut cmd, 3999, key);
            env_changes(&cmd)
                .get("ANTHROPIC_CUSTOM_HEADERS")
                .cloned()
                .flatten()
        };
        let line = format!("x-tcr-key: {KEY}");
        assert_eq!(
            headers(Some("x-user: 1\nx-tcr-group: g"), &sent(KEY)),
            Some(format!("x-user: 1\nx-tcr-group: g\n{line}"))
        );
        assert_eq!(
            headers(Some("X-TCR-KEY: outer\nx-user: 1"), &sent(KEY)),
            Some(format!("x-user: 1\n{line}")),
            "a nested launch replaces the outer key line"
        );
        assert_eq!(headers(None, &sent(KEY)), Some(line));
        for decision in unkeyed_decisions() {
            assert_eq!(
                headers(Some("x-user: 1"), &decision),
                Some("x-user: 1".to_string()),
                "{decision:?}"
            );
            assert_eq!(headers(None, &decision), None, "{decision:?}");
        }
    }

    /// No stderr line a launch prints carries the key: the banners show the URL
    /// with the key as `***`, and the notices name `tcr request-key` instead.
    #[test]
    fn no_launch_line_carries_the_request_key() {
        let key = sent(KEY);
        let ca = Path::new("/tmp/ca.pem");
        let mut lines = Vec::new();
        let mut cmd = std::process::Command::new("true");
        lines.extend(apply_see_through_env(&mut cmd, 3999, ca, &key));
        lines.extend(apply_base_url_env(&mut cmd, 3999, &key));
        for caller_ca in [false, true] {
            lines.extend(apply_codex_see_through_env(
                &mut cmd, 3999, ca, caller_ca, &key,
            ));
        }
        lines.extend(apply_codex_launch(
            &mut cmd,
            3999,
            &CodexLaunch::SeeThrough {
                ca: ca.to_path_buf(),
                no_proxy: Vec::new(),
            },
            &key,
            &test_host(&inherited(&[])),
        ));
        lines.extend(unkeyed_decisions().iter().filter_map(|d| d.notice(3999)));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("via http://tcr:***@127.0.0.1:3999")),
            "control: the see-through banner names the redacted URL: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("via http://tcr-codex:***@127.0.0.1:3999")),
            "control: the codex banner names the redacted URL: {lines:?}"
        );
        for line in &lines {
            assert!(!line.contains(KEY), "a stderr line carries the key: {line}");
        }
    }

    /// The re-entry marker must reach the child, and must not be named such that
    /// the wrapper it informs deletes it on the way.
    ///
    /// cmux's claude wrapper runs `for k in ${!CMUX_@}; do unset "$k"; done` before
    /// exec'ing the real binary. A marker under that prefix would be swept exactly
    /// where it is needed, and the double-wrap would come back looking like a bug
    /// in the launcher rather than in the name.
    #[test]
    fn marker_survives_the_cmux_prefix_sweep() {
        assert!(
            !RUN_ACTIVE_ENV.starts_with("CMUX_"),
            "{RUN_ACTIVE_ENV} would be erased by cmux's own CMUX_* sweep before the \
             launcher that reads it ever runs"
        );

        let mut cmd = claude_command();
        mark_run_active(&mut cmd);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == RUN_ACTIVE_ENV && v == Some("1".as_ref())),
            "every `tcr run` child must carry {RUN_ACTIVE_ENV}"
        );
    }

    /// A WEDGED INCUMBENT MUST NOT BE OFFERED A RECOVERY THAT CANNOT WORK.
    ///
    /// `--replace` is refused for an embedded incumbent on every path in
    /// `singleton::takeover_decision`, deliberately: the pid is the host
    /// application's. Printing "Run `tcr --replace`" for that kind sends the
    /// operator around a loop that ends in the same stand-down, while the
    /// instruction that does work — quit the host application — never appears.
    #[test]
    fn a_wedged_embedded_incumbent_is_not_told_to_run_replace() {
        let embedded = wedged_incumbent_recovery(singleton::ProxyKind::TcrEmbedded);
        assert!(
            !embedded.contains("Run `tcr --replace`"),
            "must not prescribe an override that this kind refuses: {embedded}"
        );
        assert!(
            embedded.contains("Quit the host application"),
            "must name the recovery that works: {embedded}"
        );
        // The control: for the kind `--replace` CAN take over, the prescription is
        // unchanged — so the assertions above are about the kind, not about the
        // advice having been dropped for everyone.
        let advice = wedged_incumbent_recovery(singleton::ProxyKind::Tcr);
        assert!(
            advice.contains("Stop the proxy explicitly")
                && advice.contains(&format!("tcr {} or newer", tcr::restart::HANDOFF_SINCE)),
            "standalone recovery must explain when handoff is available: {advice}"
        );
    }

    /// The ordinary stand-down: a peer is serving this binary's commit. Exit 0,
    /// or every `tcr` in a script becomes a failure.
    #[test]
    fn a_clean_stand_down_exits_zero() {
        for verdict in [
            StandDownBuild::InSync,
            StandDownBuild::DirtyBuild,
            StandDownBuild::Unknown,
        ] {
            assert_eq!(
                stand_down_exit_code(&cli::Liveness::Answering, verdict),
                0,
                "{verdict:?} is a working incumbent"
            );
        }
    }

    /// DETECTED BUILD SKEW MUST NOT EXIT 0. The whole point of the stale-server
    /// warning is that `cargo build && tcr` no longer guarantees the new binary
    /// is serving; returning success anyway leaves the guarantee broken for every
    /// script, CI step and launchd job, which read the code and not the stderr.
    #[test]
    fn a_stale_incumbent_exits_non_zero() {
        let code = stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::Stale);
        assert_ne!(code, 0, "a detected skew must be visible to `tcr && next`");
        assert_ne!(code, 1, "1 is a genuine startup failure");
        assert_ne!(code, 2, "2 is clap's usage error");
        assert_eq!(code, EXIT_STOOD_DOWN_STALE);
    }

    /// THE WEDGED PROXY. Nothing is serving on the port, so exiting 0 tells every
    /// caller — and TcrBar — that the server is up. The code has to say
    /// otherwise, and it outranks the build verdict: a proxy that will not answer
    /// cannot report a build either, so `Unknown` is what it always comes with.
    #[test]
    fn a_silent_incumbent_exits_its_own_non_zero_code() {
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Unknown),
            EXIT_STOOD_DOWN_NOT_ANSWERING
        );
        assert_ne!(EXIT_STOOD_DOWN_NOT_ANSWERING, 0);
        assert_ne!(
            EXIT_STOOD_DOWN_NOT_ANSWERING, EXIT_STOOD_DOWN_STALE,
            "the two need different recoveries, so they need different codes"
        );
        // Liveness outranks every build verdict, including a comparable one.
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::InSync),
            EXIT_STOOD_DOWN_NOT_ANSWERING,
            "a matching sha from a process that answers nothing is not a healthy port"
        );
    }

    /// THE EXIT CODES ARE A CROSS-LANGUAGE CONTRACT, exactly like
    /// `singleton::INCUMBENT_MARKER`, and nothing but this test couples them.
    ///
    /// TcrBar switches on the numbers: `ServerController.StandDownExit` in
    /// `apps/macos/Sources/TcrBarCore/ServerController.swift` declares
    /// `stale = 3` and `notAnswering = 4`, and `classifyExit` turns them into
    /// `.incumbentIsStale` and `.incumbentNotAnswering`. Renumbering a constant
    /// here is a one-character edit that every other Rust test survives, while
    /// the menu-bar app silently falls through to a bare `.exited(5, …)` and
    /// reports a wedged proxy — one serving NOTHING — as a clean exit. That is
    /// the misreport this whole round exists to eliminate.
    ///
    /// The numbers are SPELLED OUT rather than referenced through the constants,
    /// deliberately: `assert_eq!(EXIT_STOOD_DOWN_STALE, EXIT_STOOD_DOWN_STALE)`
    /// compares a value with itself and passes for every value of it. The
    /// constant is the thing that must not drift, so the test has to hold the
    /// other copy — the one Swift carries.
    #[test]
    fn the_stand_down_exit_codes_are_the_numbers_tcrbar_switches_on() {
        // Transcribed from ServerController.StandDownExit.
        let tcrbar_stale: i32 = 3;
        let tcrbar_not_answering: i32 = 4;

        assert_eq!(
            EXIT_STOOD_DOWN_OK, 0,
            "a clean stand-down is success; anything else fails every `tcr && next`"
        );
        assert_eq!(
            EXIT_STOOD_DOWN_STALE, tcrbar_stale,
            "ServerController.StandDownExit.stale is 3 — change one, change both"
        );
        assert_eq!(
            EXIT_STOOD_DOWN_NOT_ANSWERING, tcrbar_not_answering,
            "ServerController.StandDownExit.notAnswering is 4 — change one, change both"
        );

        // The constants being right is worthless if the mapping does not emit
        // them, so the contract is asserted through the function TcrBar's input
        // actually comes from, against the same literals.
        assert_eq!(
            stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::InSync),
            0
        );
        assert_eq!(
            stand_down_exit_code(&cli::Liveness::Answering, StandDownBuild::Stale),
            3,
            "a stale incumbent must reach Swift as .incumbentIsStale"
        );
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Unknown),
            4,
            "a wedged incumbent must reach Swift as .incumbentNotAnswering"
        );

        // Liveness outranks build skew across the boundary too: a proxy that
        // answers nothing is not merely stale, and 4 must win over 3 — the
        // operator's next command differs (recover, not upgrade).
        assert_eq!(
            stand_down_exit_code(&silent(), StandDownBuild::Stale),
            4,
            "NOT SERVING outranks a stale build; reporting 3 here understates it"
        );

        // Three outcomes, three codes. Two of them collapsing would make the
        // Swift switch pick one arm for both.
        let codes = [
            EXIT_STOOD_DOWN_OK,
            EXIT_STOOD_DOWN_STALE,
            EXIT_STOOD_DOWN_NOT_ANSWERING,
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "the stand-down codes must stay distinct: {codes:?}");
            }
        }
        // And neither may take a code that already means something else.
        assert!(
            !codes[1..].contains(&1),
            "1 is a genuine startup failure (anyhow::Error out of main)"
        );
        assert!(
            !codes[1..].contains(&2),
            "2 is clap's usage error, which TcrBar maps to the unknown-argument hint"
        );
    }

    /// Every `--config` flag, on every verb, says the default once and the same
    /// way.
    #[test]
    fn every_config_flag_states_the_default_file() {
        fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String, String)>) {
            for arg in cmd.get_arguments() {
                if arg.get_long() == Some("config") {
                    out.push((
                        path.to_string(),
                        arg.get_help().map_or_default(ToString::to_string),
                        arg.get_long_help().map_or_default(ToString::to_string),
                    ));
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), out);
            }
        }
        let mut flags = Vec::new();
        walk(&cli_command(), "tcr", &mut flags);
        // 31 doc comments carried the default before; a few verbs share one
        // args struct, so the walk sees at least that many flags.
        assert!(
            flags.len() >= 31,
            "found only {} --config flags",
            flags.len()
        );
        let sentence = tcr::paths::config_flag_default_help();
        for (verb, help, long_help) in &flags {
            assert!(
                help.ends_with(&format!("{sentence}.")),
                "`{verb} --config` help must end with the default: {help:?}"
            );
            assert_eq!(help.matches("Default:").count(), 1, "`{verb}`: {help:?}");
            // A hand-written "(default: ...)" beside the generated sentence
            // states the default twice (the doctor flag did, once).
            assert_eq!(
                help.to_ascii_lowercase().matches("default:").count(),
                1,
                "`{verb} --config` states its default more than once: {help:?}"
            );
            assert!(
                long_help.is_empty() || long_help.ends_with(&format!("{sentence}.")),
                "`{verb} --config` long help must end with the default: {long_help:?}"
            );
        }
        // And what a user sees on screen.
        let rendered = cli_command()
            .find_subcommand_mut("accounts")
            .expect("an accounts verb")
            .render_help()
            .to_string();
        assert!(
            rendered.contains(&format!("~/.config/{}", tcr::paths::CONFIG_FILE)),
            "{rendered}"
        );
    }

    /// `--no-replace` is documented as a deprecated no-op, and used to be wired as
    /// a SILENT VETO over `--replace`: an operator whose launchd plist or alias
    /// already carried it, adding `--replace` to force a rebuilt binary onto the
    /// port, took over nothing and got exit 0. clap must reject the contradiction
    /// by name instead.
    #[test]
    fn replace_and_no_replace_together_are_a_usage_error() {
        // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which does
        // not implement Debug (nor should it — it would print the config path).
        let Err(err) = Cli::try_parse_from(["tcr", "server", "--replace", "--no-replace"]) else {
            panic!("the pair is a contradiction, not a precedence puzzle — clap accepted it");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "it must fail as a conflict, not as some other parse error: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("--no-replace") && rendered.contains("--replace"),
            "the message must name BOTH flags so the operator knows what to remove: {rendered}"
        );
    }

    /// `--token` and `--from-claude-code` are two different credentials from
    /// two different places; asking for both is a contradiction, not a
    /// precedence puzzle. clap must reject it by name rather than let one
    /// silently win.
    #[test]
    fn token_and_from_claude_code_together_are_a_usage_error() {
        // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which
        // does not implement Debug (nor should it — it would print the config
        // path).
        let Err(err) = Cli::try_parse_from(["tcr", "login", "--token", "--from-claude-code"])
        else {
            panic!("--token with --from-claude-code must not parse");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "it must fail as a conflict, not as some other parse error: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("--token") && rendered.contains("--from-claude-code"),
            "the message must name BOTH flags so the operator knows what to remove: {rendered}"
        );
    }

    /// Each alone still parses, and `--from-claude-code` combines with the
    /// flags a browser login combines with — `--account` especially, which
    /// `--token` has to refuse (an inference-only credential has no identity
    /// to confirm; this one does).
    #[test]
    fn from_claude_code_parses_alone_and_with_account_and_name() {
        for args in [
            vec!["tcr", "login", "--from-claude-code"],
            vec!["tcr", "login", "--from-claude-code", "--account", "work"],
            vec!["tcr", "login", "--from-claude-code", "--name", "work"],
            vec!["tcr", "login", "--from-claude-code", "--force"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
        }
    }

    /// `--from-codex` is a third credential from a third place; asking for it
    /// with either of the other two is a contradiction clap must reject by name,
    /// exactly as it rejects `--token` with `--from-claude-code`.
    #[test]
    fn from_codex_with_token_or_from_claude_code_is_a_usage_error() {
        for other in ["--token", "--from-claude-code"] {
            let Err(err) = Cli::try_parse_from(["tcr", "login", "--from-codex", other]) else {
                panic!("--from-codex with {other} must not parse");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "it must fail as a conflict, not as some other parse error: {err}"
            );
            let rendered = err.to_string();
            assert!(
                rendered.contains("--from-codex") && rendered.contains(other),
                "the message must name BOTH flags so the operator knows what to remove: {rendered}"
            );
        }
    }

    /// `--codex-home` means something only to the Codex import. Accepted alone,
    /// every other login would ignore it in silence, so clap refuses it.
    #[test]
    fn codex_home_without_from_codex_is_a_usage_error() {
        let Err(err) = Cli::try_parse_from(["tcr", "login", "--codex-home", "/tmp/x"]) else {
            panic!("--codex-home without --from-codex must not parse");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "it must fail as the missing --from-codex: {err}"
        );
    }

    #[test]
    fn from_codex_parses_alone_and_with_codex_home_name_and_account() {
        for args in [
            vec!["tcr", "login", "--from-codex"],
            vec![
                "tcr",
                "login",
                "--from-codex",
                "--codex-home",
                "/tmp/x",
                "--name",
                "alice",
            ],
            vec![
                "tcr",
                "login",
                "--from-codex",
                "--account",
                "alice@example.com",
            ],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
        }
    }

    /// The [`RunArgs`] of a parsed `tcr run …`. `let … else` rather than a match arm that
    /// formats the other variant: neither `Cli` nor `Command` implements Debug.
    fn run_args_of(cli: Cli) -> RunArgs {
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected the parsed command to be `tcr run`");
        };
        args
    }

    /// `--group` works by setting `ANTHROPIC_CUSTOM_HEADERS`, which Claude Code turns into a
    /// request header and `codex` ignores, so with `--codex` the session would quietly route to
    /// the whole Codex pool. clap accepts the pair; the launcher has to refuse it before
    /// anything is spawned.
    #[test]
    fn run_codex_with_group_is_a_usage_error() {
        let err =
            Cli::try_parse_from(["tcr", "run", "--codex", "--group", "g", "--", "exec", "hi"]);
        // Parsing succeeds (clap cannot know), but run_harness must refuse before launching:
        let args = match err {
            Ok(cli) => cli,
            Err(e) => panic!("{e}"),
        };
        assert!(refuse_group_with_codex(&run_args_of(args)).is_err());
    }

    /// `tcr run --codex -- exec hi`, as the launch-decision tests run it.
    const CODEX_RUN: &[&str] = &["tcr", "run", "--codex", "--", "exec", "hi"];

    /// A parsed `tcr run …`'s [`RunArgs`].
    fn run_args(argv: &[&str]) -> RunArgs {
        run_args_of(
            Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} must parse: {e}")),
        )
    }

    /// The running proxy's status as `cli::fetch_live_status` hands it over:
    /// parsed from the wire, so the tri-state `mitm` key is read the way a real
    /// answer is. `None` leaves the key out, as a server older than it does.
    /// Its pool holds one Codex account, without which the proxy intercepts no
    /// `chatgpt.com` tunnel at all.
    fn live_status(
        mitm: Option<serde_json::Value>,
    ) -> Result<status::StatusPayload, cli::LiveStatusError> {
        let mut wire = serde_json::json!({
            "kind": status::STATUS_KIND,
            "accounts": [{
                "name": "bob@example.com/codex", "provider": "codex", "priority": 0,
                "status": "active", "disabled": false, "requests": 0, "inputTokens": 0,
                "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
                "probeStatus": "ok", "quotaState": "normal", "gate": "ok", "threshold": 0.9,
            }],
            "build": {"sha": "abc1234"},
        });
        if let Some(mitm) = mitm {
            wire["mitm"] = mitm;
        }
        Ok(serde_json::from_value(wire).expect("a status payload"))
    }

    /// A launch host that reads only `env` (no process environment, no real
    /// `HOME`) and names `/tmp/ca.pem` as tcr's CA.
    fn test_host(env: &dyn Fn(&str) -> Option<String>) -> tool_env::Host<'_> {
        tool_env::Host {
            env,
            own_ca: PathBuf::from("/tmp/ca.pem"),
        }
    }

    /// An inherited environment holding exactly `vars`.
    fn inherited(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    /// What `cmd` will set (`Some`) or remove (`None`) in the child's environment.
    fn env_changes(
        cmd: &std::process::Command,
    ) -> std::collections::BTreeMap<String, Option<String>> {
        cmd.get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// One row of [`status_with_rows`]: a provider, a name, a gate, and optionally a `freeAtMs`
    /// and a `limitReachedType`.
    type WireRow<'a> = (&'a str, &'a str, &'a str, Option<i64>, Option<&'a str>);

    /// A running proxy's status, parsed from the wire, holding `rows`.
    fn status_with_rows(rows: &[WireRow]) -> status::StatusPayload {
        let accounts: Vec<serde_json::Value> = rows
            .iter()
            .enumerate()
            .map(|(i, (provider, name, gate, free_at_ms, limit_reached))| {
                let mut row = serde_json::json!({
                    "name": name, "priority": i, "status": "active",
                    "disabled": *gate == "disabled", "requests": 0, "inputTokens": 0,
                    "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
                    "probeStatus": "ok", "quotaState": "normal", "gate": gate, "threshold": 0.9,
                });
                if *provider == "codex" {
                    row["provider"] = serde_json::json!("codex");
                }
                if let Some(at) = free_at_ms {
                    row["freeAtMs"] = serde_json::json!(at);
                }
                if let Some(kind) = limit_reached {
                    row["limitReachedType"] = serde_json::json!(kind);
                }
                row
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "kind": status::STATUS_KIND,
            "accounts": accounts,
            "build": {"sha": "abc1234"},
        }))
        .expect("a status payload")
    }

    /// 2026-10-14T00:00:00Z.
    fn preflight_now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_791_936_000).expect("a valid instant")
    }

    /// **`tcr run --codex` says, before its banner, when no Codex account can serve**: why each
    /// is out, as tcr's own 429 lists its gates but in Codex words, and when the first frees.
    /// One Codex account in rotation, or none in the pool at all, says nothing.
    #[test]
    fn the_run_codex_preflight_names_the_gates_and_the_soonest_return() {
        let payload = status_with_rows(&[
            ("anthropic", "alice@example.com", "ok", None, None),
            (
                "codex",
                "carol@example.com/codex",
                "seven-day",
                Some(1_792_072_800_000),
                None,
            ),
            ("codex", "erin@example.com/codex", "login", None, None),
        ]);
        assert_eq!(
            codex_pool_preflight(&payload, preflight_now()).as_deref(),
            Some(
                "[tcr] no Codex account can serve right now: 1 in the weekly window, 1 needing \
                 re-login — the soonest frees at 2026-10-15 14:00:00 UTC"
            )
        );

        // An account held by an upstream 403 is out for a short hold, which is then the
        // soonest return, under a day away, so the clock alone.
        let payload = status_with_rows(&[
            (
                "codex",
                "carol@example.com/codex",
                "seven-day",
                Some(1_792_072_800_000),
                None,
            ),
            ("codex", "erin@example.com/codex", "login", None, None),
            (
                "codex",
                "frank@example.com/codex",
                "refused",
                Some(1_791_936_090_000),
                None,
            ),
        ]);
        assert_eq!(
            codex_pool_preflight(&payload, preflight_now()).as_deref(),
            Some(
                "[tcr] no Codex account can serve right now: 1 refused upstream (403), 1 in the \
                 weekly window, 1 needing re-login — the soonest frees at 00:01:30 UTC"
            )
        );

        let serving = status_with_rows(&[
            (
                "codex",
                "carol@example.com/codex",
                "seven-day",
                Some(1_792_072_800_000),
                None,
            ),
            ("codex", "dana@example.com/codex", "ok", None, None),
        ]);
        assert_eq!(codex_pool_preflight(&serving, preflight_now()), None);
        let claude_only =
            status_with_rows(&[("anthropic", "alice@example.com", "login", None, None)]);
        assert_eq!(codex_pool_preflight(&claude_only, preflight_now()), None);
    }

    /// When no Codex account reports when it frees, the line says so rather than naming a time.
    /// An account whose hold sits over ChatGPT's own refusal is counted as refused by ChatGPT:
    /// the refusal outlasts the hold, and is why it stays out.
    ///
    /// Watched red: with a disabled row counted as able to serve, this pool says nothing.
    #[test]
    fn the_run_codex_preflight_says_none_reports_when_it_frees() {
        let mut rows = vec![
            ("codex", "erin@example.com/codex", "login", None, None),
            ("codex", "gil@example.com/codex", "disabled", None, None),
        ];
        assert_eq!(
            codex_pool_preflight(&status_with_rows(&rows), preflight_now()).as_deref(),
            Some(
                "[tcr] no Codex account can serve right now: 1 needing re-login, 1 disabled — \
                 none reports when it frees"
            )
        );
        rows.push((
            "codex",
            "hal@example.com/codex",
            "hold",
            None,
            Some("rate_limit_reached"),
        ));
        assert_eq!(
            codex_pool_preflight(&status_with_rows(&rows), preflight_now()).as_deref(),
            Some(
                "[tcr] no Codex account can serve right now: 1 needing re-login, 1 refused by \
                 ChatGPT, 1 disabled — none reports when it frees"
            )
        );
    }

    /// **`tcr run` warns a Claude session that the running proxy holds no Claude account**, and
    /// so will refuse every request it sends. Any Claude account, or an empty pool, says
    /// nothing.
    #[test]
    fn run_warns_when_the_proxy_holds_no_claude_account() {
        let codex_only = status_with_rows(&[
            ("codex", "bob@example.com/codex", "ok", None, None),
            ("codex", "dana@example.com/codex", "disabled", None, None),
        ]);
        assert_eq!(
            no_claude_account_warning(3999, &codex_only).as_deref(),
            Some(
                "[tcr] warning: the proxy on :3999 holds no Claude account, so it will refuse \
                 every request from this session — add one with `tcr login`"
            )
        );
        let mixed = status_with_rows(&[
            ("anthropic", "alice@example.com", "disabled", None, None),
            ("codex", "bob@example.com/codex", "ok", None, None),
        ]);
        assert_eq!(no_claude_account_warning(3999, &mixed), None);
        assert_eq!(
            no_claude_account_warning(3999, &status_with_rows(&[])),
            None
        );
    }

    /// Every row of `tcr run --codex`'s launch decision ([`codex_launch`]): the
    /// launch it picks, and the exact notice it prints. Each row inherits an
    /// `HTTPS_PROXY` aimed at this tcr, so every direct row must drop it, the
    /// "not listening" one included: a dead tcr behind that variable refuses the
    /// connection rather than launching anything.
    ///
    /// Only the "key absent" row may tell the user to restart. MITM off survives
    /// any restart, and saying otherwise sends the user to pay a
    /// cold prompt cache for nothing.
    ///
    /// Mismatches are collected rather than asserted one at a time, so a
    /// regression names every row it breaks.
    #[test]
    fn codex_launch_follows_what_the_running_proxy_reports() {
        const PORT: u16 = 3999;
        const AT_TCR: &[(&str, &str)] = &[("HTTPS_PROXY", "http://127.0.0.1:3999")];
        let ca = Path::new("/tmp/tcr-run-test/tcr-ca.pem");
        let pooling = || {
            live_status(Some(serde_json::json!({
                "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
                "leaf": "minted",
            })))
        };
        let direct = |reason: &str| {
            Ok((
                CodexLaunch::Direct {
                    reason: reason.to_string(),
                    unset: vec!["HTTPS_PROXY"],
                },
                vec![
                    format!("[tcr] launching codex directly (not pooled): {reason}"),
                    "[tcr] removed HTTPS_PROXY from codex's environment: it points at tcr on \
                     :3999, which this launch bypasses"
                        .to_string(),
                ],
            ))
        };
        // (label, argv, the status read, whether the CA is on disk, the launch and
        // notices wanted — or the refusal's text).
        type Live = Result<status::StatusPayload, cli::LiveStatusError>;
        type Want = Result<(CodexLaunch, Vec<String>), &'static str>;
        let rows: Vec<(&str, &[&str], Live, bool, Want)> = vec![
            (
                "--group is refused before anything launches",
                &["tcr", "run", "--codex", "--group", "g", "--", "exec", "hi"],
                pooling(),
                true,
                Err("--group is not supported with --codex"),
            ),
            (
                "not listening",
                CODEX_RUN,
                Err(cli::LiveStatusError::NoServer),
                true,
                direct("no proxy answered on 127.0.0.1:3999"),
            ),
            (
                "no answer",
                CODEX_RUN,
                Err(cli::LiveStatusError::NoAnswer(
                    "the server did not answer within 5s".to_string(),
                )),
                true,
                direct(
                    "could not read the running proxy's status (the server did not answer \
                     within 5s)",
                ),
            ),
            (
                "an answer that is not a usable status",
                CODEX_RUN,
                Err(cli::LiveStatusError::Unusable(
                    "HTTP 401 Unauthorized — the proxy api-key in the config was rejected"
                        .to_string(),
                )),
                true,
                direct(
                    "could not read the running proxy's status (HTTP 401 Unauthorized — the \
                     proxy api-key in the config was rejected)",
                ),
            ),
            (
                "key absent: a proxy older than the key",
                CODEX_RUN,
                live_status(None),
                true,
                direct(
                    "the proxy on :3999 (build abc1234) predates Codex interception — restart \
                     it on this build at a quiet moment",
                ),
            ),
            (
                "MITM off",
                CODEX_RUN,
                live_status(Some(
                    serde_json::json!({"interceptedHosts": [], "leaf": "none"}),
                )),
                true,
                direct(
                    "the proxy on :3999 is running without MITM: its TLS material failed to \
                     load at boot — see its log",
                ),
            ),
            (
                "pooled",
                CODEX_RUN,
                pooling(),
                true,
                Ok((
                    CodexLaunch::SeeThrough {
                        ca: ca.to_path_buf(),
                        no_proxy: Vec::new(),
                    },
                    Vec::new(),
                )),
            ),
            (
                "chatgpt.com intercepted, but no CA on disk",
                CODEX_RUN,
                pooling(),
                false,
                direct("no readable CA at /tmp/tcr-run-test/tcr-ca.pem"),
            ),
            (
                "a leaf that covers chatgpt.com, but no Codex account in the pool",
                CODEX_RUN,
                pooling().map(|mut payload| {
                    payload
                        .accounts
                        .retain(|row| row.provider != tcr::provider::Provider::Codex);
                    payload
                }),
                true,
                direct(
                    "the proxy on :3999 holds no Codex account, so it blind-tunnels chatgpt.com \
                     and codex keeps its own login — add one with `tcr login --codex`",
                ),
            ),
            (
                "a minted leaf that does not cover chatgpt.com",
                CODEX_RUN,
                live_status(Some(serde_json::json!({
                    "interceptedHosts": ["api.anthropic.com", "platform.claude.com"],
                    "leaf": "minted",
                }))),
                true,
                direct(
                    "the proxy on :3999 serves a leaf it minted but does not intercept \
                     chatgpt.com (it intercepts [api.anthropic.com, platform.claude.com])",
                ),
            ),
            (
                "a leaf this build does not know, whose CA it cannot name",
                CODEX_RUN,
                live_status(Some(serde_json::json!({
                    "interceptedHosts": ["chatgpt.com"],
                    "leaf": "future",
                }))),
                true,
                direct(
                    "the proxy on :3999 (build abc1234) reports a leaf this build does not know \
                     (\"future\") — run tcr run --codex from the same build as the proxy",
                ),
            ),
        ];

        let mut wrong = Vec::new();
        for (label, argv, live, ca_is_file, want) in rows {
            let got = codex_launch(
                &run_args(argv),
                PORT,
                &live,
                ca,
                ca_is_file,
                &inherited(AT_TCR),
            )
            .map(|launch| {
                let notices = launch.notices(PORT);
                (launch, notices)
            })
            .map_err(|err| err.to_string());
            let right = match (&got, &want) {
                (Ok(got), Ok(want)) => got == want,
                (Err(got), Err(want)) => got.contains(want),
                _ => false,
            };
            if !right {
                wrong.push(format!("{label}:\n  got  {got:?}\n  want {want:?}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} row(s) wrong:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn a_codex_launch_keeps_a_corporate_ca_and_runs_direct_when_tcr_is_absent() {
        let scratch = tempfile::tempdir().unwrap();
        let ca = scratch.path().join("tcr.pem");
        let corporate = scratch.path().join("corporate.pem");
        std::fs::write(&ca, "tcr certificate fixture").unwrap();
        std::fs::write(&corporate, "different corporate certificate fixture").unwrap();
        let live = live_status(Some(
            serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf": "minted"}),
        ));
        for variable in ["CODEX_CA_CERTIFICATE", "SSL_CERT_FILE"] {
            let corporate = corporate.to_str().unwrap();
            let launch = codex_launch(
                &run_args(CODEX_RUN),
                3999,
                &live,
                &ca,
                true,
                &inherited(&[
                    ("HTTPS_PROXY", "http://127.0.0.1:3999"),
                    (variable, corporate),
                ]),
            )
            .unwrap();
            let CodexLaunch::Direct { reason, unset } = launch else {
                panic!(
                    "pooling without trust in both CAs must not break a working corporate setup"
                );
            };
            assert!(reason.contains(variable));
            assert_eq!(unset, ["HTTPS_PROXY"]);
        }
    }

    #[test]
    fn a_pooled_codex_launch_uses_the_callers_complete_ssl_bundle() {
        let scratch = tempfile::tempdir().unwrap();
        let ca = scratch.path().join("tcr.pem");
        let bundle = scratch.path().join("bundle.pem");
        let generated = rcgen::generate_simple_self_signed(vec!["example.test".into()]).unwrap();
        let pem = generated.cert.pem();
        std::fs::write(&ca, &pem).unwrap();
        std::fs::write(&bundle, format!("# caller bundle\n{pem}")).unwrap();
        let live = live_status(Some(
            serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf": "minted"}),
        ));
        let launch = codex_launch(
            &run_args(CODEX_RUN),
            3999,
            &live,
            &ca,
            true,
            &inherited(&[("SSL_CERT_FILE", bundle.to_str().unwrap())]),
        )
        .unwrap();
        assert!(
            matches!(launch, CodexLaunch::SeeThrough {ca, ..} if ca == bundle),
            "setting CODEX_CA_CERTIFICATE must retain the caller's complete bundle"
        );
    }

    /// A direct launch drops only the proxy variables that point at THIS tcr
    /// (loopback host, the configured port, scheme optional) and keeps every
    /// other proxy: one on another port (a local px or cntlm) or on another host
    /// (a corporate egress) may be how this machine reaches the internet at all,
    /// and removing it would break a launch that only had to go un-pooled.
    #[test]
    fn a_direct_codex_launch_drops_only_the_proxy_variables_aimed_at_this_tcr() {
        let env = inherited(&[
            ("HTTPS_PROXY", "http://127.0.0.1:3999"),
            ("https_proxy", "localhost:3999"),
            ("HTTP_PROXY", "http://[::1]:3999/"),
            ("http_proxy", "http://127.0.0.1:8080"),
            ("ALL_PROXY", "http://proxy.corp.example:3999"),
            ("all_proxy", "socks5://127.0.0.1:1080"),
        ]);
        let launch = codex_launch(
            &run_args(CODEX_RUN),
            3999,
            &Err(cli::LiveStatusError::NoServer),
            Path::new("/tmp/ca.pem"),
            false,
            &env,
        )
        .expect("a direct launch");
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_launch(
            &mut cmd,
            3999,
            &launch,
            &LaunchKey::NoKeyInConfig,
            &test_host(&env),
        );
        assert_eq!(
            env_changes(&cmd),
            [
                ("HTTPS_PROXY".to_string(), None),
                ("HTTP_PROXY".to_string(), None),
                ("https_proxy".to_string(), None),
            ]
            .into_iter()
            .collect(),
            "exactly the three aimed at tcr are removed, and nothing is set"
        );
        assert_eq!(
            launch.notices(3999).get(1).map(String::as_str),
            Some(
                "[tcr] removed HTTPS_PROXY, https_proxy, HTTP_PROXY from codex's environment: \
                 they point at tcr on :3999, which this launch bypasses"
            )
        );

        for (value, aimed_at_tcr) in [
            ("http://127.0.0.1:3999", true),
            ("HTTP://LOCALHOST:3999/", true),
            ("https://[::1]:3999", true),
            ("127.0.0.1:3999", true),
            (" http://127.0.0.1:3999 ", true),
            ("socks5h://localhost:3999", true),
            ("http://127.0.0.1", false),
            ("http://127.0.0.1:39990", false),
            ("http://127.0.0.2:3999", false),
            ("http://proxy.corp.example:3999", false),
            ("", false),
            ("not a url", false),
        ] {
            assert_eq!(points_at_tcr(value, 3999), aimed_at_tcr, "{value:?}");
        }
    }

    /// A pooled launch drops only the `NO_PROXY` entries that would exempt chatgpt.com,
    /// keeps the rest in order, keeps an emptied variable SET (codex reads the first
    /// spelling that is set), and says what it dropped, so the banner it prints is true.
    #[test]
    fn a_pooled_codex_launch_drops_only_the_no_proxy_entries_that_exempt_chatgpt_com() {
        let env = inherited(&[
            ("NO_PROXY", "localhost,.chatgpt.com, 10.0.0.0/8"),
            ("no_proxy", "*"),
        ]);
        let live = live_status(Some(serde_json::json!({
            "interceptedHosts": ["chatgpt.com"],
            "leaf": "minted",
        })));
        let launch = codex_launch(
            &run_args(CODEX_RUN),
            3999,
            &live,
            Path::new("/tmp/ca.pem"),
            true,
            &env,
        )
        .expect("a pooled launch");
        assert_eq!(
            launch,
            CodexLaunch::SeeThrough {
                ca: PathBuf::from("/tmp/ca.pem"),
                no_proxy: vec![
                    NoProxyRewrite {
                        var: "NO_PROXY",
                        kept: "localhost, 10.0.0.0/8".to_string(),
                        dropped: vec![".chatgpt.com".to_string()],
                    },
                    NoProxyRewrite {
                        var: "no_proxy",
                        kept: String::new(),
                        dropped: vec!["*".to_string()],
                    },
                ],
            }
        );
        assert_eq!(
            launch.notices(3999),
            [
                "[tcr] dropped .chatgpt.com from NO_PROXY for codex: it exempts chatgpt.com from \
                 the proxy, and the session would bypass tcr",
                "[tcr] dropped * from no_proxy for codex: it exempts chatgpt.com from the proxy, \
                 and the session would bypass tcr",
            ]
        );

        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_launch(
            &mut cmd,
            3999,
            &launch,
            &LaunchKey::NoKeyInConfig,
            &test_host(&env),
        );
        let env = env_changes(&cmd);
        assert_eq!(
            env.get("NO_PROXY"),
            Some(&Some("localhost, 10.0.0.0/8".to_string()))
        );
        assert_eq!(
            env.get("no_proxy"),
            Some(&Some(String::new())),
            "emptied, and still set"
        );
        assert_eq!(
            env.get("HTTPS_PROXY"),
            Some(&Some("http://tcr-codex@127.0.0.1:3999".to_string()))
        );
        assert_eq!(
            env.get("CODEX_CA_CERTIFICATE"),
            Some(&Some("/tmp/ca.pem".to_string()))
        );
    }

    /// [`no_proxy_entry_exempts`] against the matcher codex's HTTP stack runs:
    /// hyper-util's, which reqwest builds its environment proxy from (reqwest
    /// enables hyper-util's `client-proxy` feature for it; this tree and
    /// `openai/codex` both lock hyper-util 0.1.20). An entry exempts chatgpt.com
    /// exactly when that matcher sends `https://chatgpt.com/…` direct. The
    /// expected column documents the rules; the matcher is the authority.
    #[test]
    fn no_proxy_matching_agrees_with_the_matcher_codex_uses() {
        use hyper_util::client::proxy::matcher::Matcher;
        let target: hyper::Uri = "https://chatgpt.com/backend-api/codex/responses"
            .parse()
            .expect("a uri");
        let proxied = |no_proxy: &str| {
            Matcher::builder()
                .https("http://127.0.0.1:3999")
                .no(no_proxy)
                .build()
                .intercept(&target)
                .is_some()
        };
        assert!(
            proxied(""),
            "control: with no NO_PROXY the request is proxied"
        );
        for (entry, exempts) in [
            ("chatgpt.com", true),
            ("CHATGPT.COM", true),
            (".chatgpt.com", true),
            (" chatgpt.com ", true),
            ("com", true),
            (".com", true),
            ("*", true),
            ("api.chatgpt.com", false),
            ("gpt.com", false),
            ("chatgpt.com.example", false),
            ("openai.com", false),
            ("*.chatgpt.com", false),
            (".", false),
            ("", false),
            ("10.0.0.0/8", false),
            ("127.0.0.1", false),
            ("::1", false),
        ] {
            assert_eq!(
                no_proxy_entry_exempts(entry, "chatgpt.com"),
                exempts,
                "{entry:?}"
            );
            assert_eq!(!proxied(entry), exempts, "the matcher on {entry:?}");
        }
    }

    /// Every `tcr` command the "proxy too old" warnings tell the user to run
    /// must be one this binary accepts. All four named a `restart` subcommand,
    /// which clap rejected with exit 2: the remedy in the warning was itself an
    /// error.
    #[test]
    fn restart_hint_names_only_commands_that_parse() {
        let commands: Vec<&str> = cli::RESTART_HINT
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| span.starts_with("tcr "))
            .collect();
        assert!(
            !commands.is_empty(),
            "the hint must name a tcr command to run: {}",
            cli::RESTART_HINT
        );
        for command in commands {
            let argv: Vec<&str> = command.split_whitespace().collect();
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!("the warning tells the user to run `{command}`, which does not parse: {e}");
            }
        }
    }

    /// Every `tcr` command a Codex login or launch tells someone to run parses: each production
    /// string that names one in backticks is split on its backticks, and each span starting
    /// `tcr ` goes through the real CLI parser, as does each command a refusal over a running
    /// proxy echoes. A remedy naming a flag clap refuses sends its reader into a usage error.
    ///
    /// Watched red: one flag misspelled in one production string panics naming that span.
    #[test]
    fn codex_remedies_name_only_commands_that_parse() {
        let workspace = oauth::codex_relogin_workspace_refusal(
            Some("erin@example.com"),
            "99999999-9999-4999-8999-999999999999",
            Some("business"),
            "work",
            "88888888-8888-4888-8888-888888888888",
            Some("pro"),
        );
        let strings = [
            oauth::codex_account_login_refusal("bob@example.com/codex"),
            oauth::claude_account_codex_login_refusal("alice@example.com"),
            oauth::claude_account_codex_import_refusal("alice@example.com"),
            oauth::codex_identity_refusal("carol@example.com", "erin@example.com/codex").reason,
            workspace.reason,
            no_codex_account_reason(3999, "chatgpt.com"),
            oauth::NO_REFRESH_CODEX_MESSAGE.to_string(),
            oauth::no_codex_login_message(Path::new("/Users/example/.codex/auth.json")),
            tcr::codex_cli_login::codex_login_saved_line("bob@example.com", Some("ChatGPT Pro")),
            oauth::CODEX_IMPORT_RUN_LINE.to_string(),
            oauth::CODEX_FIRST_ACCOUNT_NOTICE.to_string(),
        ];
        let mut commands: Vec<String> = Vec::new();
        for text in &strings {
            let spans: Vec<&str> = text
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|span| span.starts_with("tcr "))
                .collect();
            assert!(!spans.is_empty(), "names no tcr command: {text}");
            commands.extend(spans.iter().map(ToString::to_string));
        }
        commands.extend(
            [
                oauth::LOGIN_COMMAND,
                oauth::LOGIN_FROM_CODEX_COMMAND,
                oauth::LOGIN_FROM_CLAUDE_CODE_COMMAND,
                oauth::LOGIN_CODEX_COMMAND,
                oauth::IMPORT_COMMAND,
            ]
            .map(str::to_string),
        );
        for command in commands {
            let argv: Vec<&str> = command.split_whitespace().collect();
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!(
                    "a Codex remedy tells the user to run `{command}`, which does not parse: {e}"
                );
            }
        }
    }

    /// The help says what `--non-interactive` does under `--codex`, where `codex login` opens
    /// the browser itself and nothing times the sign-in out: the `browser` line's
    /// `"opened":true` tells the caller to open nothing, and SIGTERM is how it ends an abandoned
    /// sign-in. And `tcr peer ls --help` names the Codex exit pins its text form reads from the
    /// config.
    ///
    /// Watched red: the old help says the caller opens the URL and the same 2-minute timeout
    /// applies, and `peer ls` takes nothing from the config but the account labels.
    #[test]
    fn the_help_tells_the_truth_about_codex_logins_and_pins() {
        use clap::CommandFactory as _;
        let cli = Cli::command();
        let help = |path: &[&str], id: &str| -> String {
            let mut command = &cli;
            for name in path {
                command = command
                    .find_subcommand(name)
                    .unwrap_or_else(|| panic!("no {name} subcommand"));
            }
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("no {id} argument"));
            arg.get_long_help()
                .or_else(|| arg.get_help())
                .map_or_default(ToString::to_string)
        };
        let login = help(&["login"], "non_interactive");
        for fact in [r#""opened":true"#, "`--codex` has no timeout", "SIGTERM"] {
            assert!(
                login.contains(fact),
                "`tcr login --help` must say {fact}:\n{login}"
            );
        }
        let import = help(&["login"], "from_codex");
        for fact in ["auth.json", "only while", "same ChatGPT account"] {
            assert!(import.contains(fact), "{import}");
        }
        // `tcr import --yes` states the take before anyone runs it: the other tool's copy dies,
        // and tcr's own rows are never overwritten. Watched red with the help naming neither.
        let take = help(&["import"], "yes");
        for fact in ["only lists", "single-use", "never written", "non-zero"] {
            assert!(
                take.contains(fact),
                "`tcr import --help` must say {fact}:\n{take}"
            );
        }
        assert!(!import.contains("asks you to log in again"), "{import}");
        let ls = help(&["peer", "ls"], "config");
        assert!(
            ls.contains("Codex exit pins"),
            "`tcr peer ls --help` must name the Codex exit pins it reads:\n{ls}"
        );
        assert!(!ls.contains("Nothing else is taken from it"), "{ls}");
    }

    /// Every command the `login` row of docs/troubleshooting.md's gate table tells a user to run
    /// parses, and its `--from-codex` re-import names the folder to read: without
    /// `--codex-home`, `--from-codex` reads `$CODEX_HOME` or `~/.codex`, never the folder the
    /// row records, so an account imported from another folder would be refused.
    ///
    /// Watched red: the row said `tcr login --from-codex --account <name>` imports the login
    /// again "from the folder it came from".
    #[test]
    fn the_troubleshooting_login_remedies_parse_and_name_the_folder() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/troubleshooting.md");
        let doc =
            std::fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {path}: {err}"));
        let row = doc
            .lines()
            .find(|line| line.starts_with("| `login` |"))
            .expect("docs/troubleshooting.md has a `login` row");
        let commands: Vec<String> = row
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| span.starts_with("tcr "))
            .map(|span| {
                span.replace("<name>", "bob@example.com/codex")
                    .replace("<folder>", "/Users/example/.codex-second")
            })
            .collect();
        assert!(
            commands.iter().any(|command| command.contains("--codex ")),
            "positive control, the row's `--codex` remedy: {commands:?}"
        );
        for command in &commands {
            let argv: Vec<&str> = command.split_whitespace().collect();
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!(
                    "the login row tells the user to run `{command}`, which does not parse: {e}"
                );
            }
            if command.contains("--from-codex") {
                assert!(
                    command.contains("--codex-home "),
                    "`{command}` reads `$CODEX_HOME` or ~/.codex, not the folder the row records"
                );
            }
        }
        assert!(
            commands
                .iter()
                .any(|command| command.contains("--from-codex")),
            "the row names no `--from-codex` re-import: {commands:?}"
        );
    }

    /// The command `tcr doctor`'s pooled Codex verdict names parses, in the sentence for one
    /// request and in the one for several.
    #[test]
    fn the_pooled_codex_verdict_names_a_command_that_parses() {
        for requests in [1, 2] {
            let verdict = tcr::doctor::CodexReport {
                port: 3999,
                rows: Vec::new(),
                route: tcr_status_wire::CodexRoute {
                    pooled: true,
                    code: None,
                    reason: None,
                    upstream: None,
                },
                ca: PathBuf::from("/Users/example/.config/tcr-ca.pem"),
                ca_readable: true,
                upstream: "https://chatgpt.com".to_string(),
                upstream_is_default: true,
                bin: Ok((PathBuf::from("/opt/homebrew/bin/codex"), "PATH".to_string())),
                env: tcr::doctor::CodexEnv::default(),
                login: PathBuf::from("/Users/example/.codex/auth.json"),
                login_exists: true,
                requests_10m: Some(requests),
                traffic: None,
            }
            .verdict();
            let commands: Vec<&str> = verdict
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|span| span.starts_with("tcr "))
                .collect();
            assert!(!commands.is_empty(), "names no tcr command: {verdict}");
            for command in commands {
                let argv: Vec<&str> = command.split_whitespace().collect();
                if let Err(e) = Cli::try_parse_from(&argv) {
                    panic!("the verdict names `{command}`, which does not parse: {e}");
                }
            }
        }
    }

    /// `tcr login --help` carries `--codex` as a whole token, beside the tokens TcrBar already
    /// probes for: `LoginCapability` decides between the in-app Codex sign-in and Terminal by
    /// finding it, so a help that renamed it would silently send every Codex sign-in to
    /// Terminal.
    #[test]
    fn login_help_lists_codex_as_a_whole_token() {
        use clap::CommandFactory as _;
        let mut cli = Cli::command();
        let login = cli
            .find_subcommand_mut("login")
            .expect("tcr has a login subcommand");
        let help = login.render_long_help().to_string();
        for token in [
            "--codex",
            "--non-interactive",
            "--from-codex",
            "--codex-home",
        ] {
            assert!(
                tcr::codex_local::help_has_whole_token(&help, token),
                "`tcr login --help` must carry {token} as a whole token:\n{help}"
            );
        }
    }

    /// The `tcr group` argv TcrBar sends still parses, unchanged: the panel shells
    /// out to these shapes and never passes `--force`, which is additive. A
    /// reserve without it is not forced; with it, it is.
    #[test]
    fn the_group_argv_tcrbar_sends_is_unchanged() {
        for argv in [
            &["tcr", "group", "add", "orchard", "alice@example.com"][..],
            &["tcr", "group", "rm", "orchard", "alice@example.com"],
            &["tcr", "group", "rm", "orchard", "--all"],
            &["tcr", "group", "park", "orchard"],
            &["tcr", "group", "unpark", "orchard"],
        ] {
            if let Err(e) = Cli::try_parse_from(argv) {
                panic!("TcrBar sends {argv:?}, which no longer parses: {e}");
            }
        }

        let reserve = |argv: &[&str]| -> GroupReserveArgs {
            match Cli::try_parse_from(argv) {
                Ok(Cli {
                    command:
                        Some(Command::Group(GroupArgs {
                            action: GroupAction::Reserve(args),
                        })),
                    ..
                }) => args,
                Ok(_) => panic!("{argv:?} parsed to something other than a reserve"),
                Err(e) => panic!("{argv:?} does not parse: {e}"),
            }
        };
        let plain = reserve(&["tcr", "group", "reserve", "orchard"]);
        assert_eq!(plain.group, "orchard");
        assert!(!plain.force, "TcrBar's reserve is never forced");
        let forced = reserve(&["tcr", "group", "reserve", "orchard", "--force"]);
        assert_eq!(forced.group, "orchard");
        assert!(forced.force);
    }

    /// Every `tcr` command the Codex refusals and notes tell an operator to run
    /// is one this binary accepts, read out of the sentences themselves as they
    /// are printed: a remedy that clap rejects is itself an error, which is how
    /// the restart hint above once went wrong.
    #[test]
    fn every_command_a_codex_refusal_names_parses() {
        use tcr::provider_guard;
        let sentences = [
            provider_guard::reserve_strands_codex(
                "orchard",
                &["bob@example.com/codex", "dana@example.com/codex"],
            ),
            provider_guard::reserve_force_warning("orchard", &["bob@example.com/codex"]),
            provider_guard::refused_exit_suffix("bob@example.com/codex"),
        ];
        let mut named: Vec<Vec<String>> = Vec::new();
        for sentence in &sentences {
            let commands: Vec<&str> = sentence
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|span| span.starts_with("tcr "))
                .collect();
            assert_eq!(
                commands.len(),
                1,
                "each sentence names one command to run: {sentence}"
            );
            for command in commands {
                let argv: Vec<&str> = command.split_whitespace().collect();
                if let Err(e) = Cli::try_parse_from(&argv) {
                    panic!(
                        "`{sentence}` tells the user to run `{command}`, which does not parse: {e}"
                    );
                }
                named.push(argv.iter().map(ToString::to_string).collect());
            }
        }

        // Both reserve sentences name the remove that takes the first Codex
        // member out of the group, and it parses as exactly that.
        for argv in &named[..2] {
            assert_eq!(
                argv,
                &["tcr", "group", "rm", "orchard", "bob@example.com/codex"]
            );
            match Cli::try_parse_from(argv) {
                Ok(Cli {
                    command:
                        Some(Command::Group(GroupArgs {
                            action: GroupAction::Rm(rm),
                        })),
                    ..
                }) => {
                    assert_eq!(rm.group, "orchard");
                    assert_eq!(rm.account.as_deref(), Some("bob@example.com/codex"));
                    assert!(!rm.all);
                }
                _ => panic!("{argv:?} is not a group rm"),
            }
        }
        // The strict exit pin's remedy clears it: back to local, and not strict.
        assert_eq!(
            named[2],
            [
                "tcr",
                "peer",
                "account",
                "bob@example.com/codex",
                "--exits-from",
                "local",
                "--no-must"
            ]
        );
        match Cli::try_parse_from(&named[2]) {
            Ok(Cli {
                command:
                    Some(Command::Peer(peer_cli::PeerArgs {
                        action: peer_cli::PeerAction::Account(account),
                    })),
                ..
            }) => {
                assert_eq!(account.account, "bob@example.com/codex");
                assert_eq!(account.exits_from.as_deref(), Some("local"));
                assert!(account.no_must && !account.must);
            }
            _ => panic!("{:?} is not a peer account", named[2]),
        }
    }

    /// Each flag alone still parses — the deprecated one is accepted, as promised
    /// to the scripts and launch agents that already pass it.
    #[test]
    fn each_replace_flag_alone_still_parses() {
        for args in [
            vec!["tcr", "server", "--replace"],
            vec!["tcr", "server", "--no-replace"],
            vec!["tcr", "--no-replace"],
            vec!["tcr", "server"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
        }
    }

    /// A unique, test-only log directory keyed by pid + nanosecond timestamp.
    /// Never the real cache directory's `logs/` — tests must not touch the
    /// live proxy's cache directory (`session-affinity.json` lives one level up
    /// and is being written by a running process).
    fn unique_log_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tcr-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or_default(|d| d.as_nanos())
        ))
    }

    /// The rotating log directory (account emails + request paths inside it)
    /// must be created owner-only. `tracing_appender` has no per-file
    /// `.mode()` hook (verified against its 0.2.5 source — `create_writer` in
    /// `rolling.rs` opens with `OpenOptions::append(true).create(true)` only),
    /// so confidentiality is enforced on the directory, not the file: `0700`
    /// blocks traversal into the directory for everyone but the owner, even
    /// though the files landing inside it carry whatever mode the process
    /// umask gives a freshly `OpenOptions::create`d file (commonly `0644` —
    /// world-readable in their *own* bits, but unreachable because nothing
    /// outside the owner can resolve a path through a `0700` parent).
    #[test]
    fn log_directory_is_created_owner_only() {
        let dir = unique_log_dir("dirmode");
        // Goes through the production opener: re-implementing the mode here
        // would assert 0o700 == 0o700 and pass however `open_log_appender`
        // drifts.
        let _appender = open_log_appender(&dir, tcr::paths::LOG_PREFIX).expect("open temp log dir");
        let dir_mode = std::fs::metadata(&dir)
            .expect("stat temp log dir")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(dir_mode, 0o700, "log directory must be owner-only (0700)");
    }

    /// `DirBuilder::mode` only sets the mode of directories it *creates* — a
    /// directory that already exists (operator `mkdir -p`, a `tar` restore
    /// without `-p`, a baked container path) is left exactly as it was found.
    /// `open_log_appender` must re-assert `0700` on an already-existing
    /// directory, not merely request it at creation time, or a pre-existing
    /// `0755` `logs/` silently ships every rotated file world-readable.
    #[test]
    fn log_directory_is_re_asserted_owner_only_when_it_already_exists() {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = unique_log_dir("dirmode-preexisting");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .recursive(true)
            .create(&dir)
            .expect("pre-create the dir at 0755, simulating an external mkdir/restore");
        let preexisting_mode = std::fs::metadata(&dir)
            .expect("stat pre-created dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            preexisting_mode, 0o755,
            "test setup sanity: the pre-created dir must actually be 0755"
        );

        let _appender =
            open_log_appender(&dir, tcr::paths::LOG_PREFIX).expect("open pre-existing log dir");
        let dir_mode = std::fs::metadata(&dir)
            .expect("stat temp log dir")
            .permissions()
            .mode()
            & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            dir_mode, 0o700,
            "a pre-existing, wrongly-permissioned log directory must be re-asserted to 0700"
        );
    }

    // A `MakeWriter` that keeps what was written, so a test can inspect the
    // stdout sink without capturing the process's real stdout.
    use tcr::test_support::log_capture::SharedBuf;

    /// Reads back whatever `open_log_appender` actually wrote inside `dir` —
    /// there is exactly one matching file after a single, unrotated write.
    /// This is the "it writes", not merely "it initialised", proof: a dropped
    /// `WorkerGuard` (see `headless_subscriber`'s doc-comment) would make
    /// `init_tracing`-style construction succeed while nothing ever lands here.
    ///
    /// `prefix` is the one the appender was opened with: the resolved cache
    /// directory's own, never a fixed name.
    fn read_the_one_log_file(dir: &Path, prefix: &str) -> String {
        let mut matches: Vec<_> = std::fs::read_dir(dir)
            .expect("read temp log dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one log file in {dir:?}, found {matches:?}"
        );
        std::fs::read_to_string(matches.pop().unwrap().path()).expect("read the log file")
    }

    /// HEADLESS MUST REACH DISK. TcrBar spawns `tcr server --headless` and throws
    /// the child's stdout away, so a stdout-only headless subscriber destroys
    /// 100% of the proxy's logs — which is exactly why a restart loop could not
    /// be diagnosed. Both sinks are asserted: dropping either one is a
    /// regression, and asserting only the file would let stdout silently die for
    /// everyone running it in a terminal.
    #[test]
    fn headless_logging_reaches_both_the_file_and_stdout() {
        let dir = unique_log_dir("headless-test");
        let prefix = tcr::paths::LOG_PREFIX;
        let appender = open_log_appender(&dir, prefix).expect("open temp log dir");
        let (queue, file_log) = tcr::file_log::start(appender);
        let stdout = SharedBuf::default();
        let marker = format!("headless-sink-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            Some(queue),
            Some(stdout.clone()),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{marker}");
        });
        // The writer is still running: sync, not close, so this proves the line
        // reached disk while logging continues.
        assert!(
            file_log.sync(std::time::Duration::from_secs(10)),
            "the log writer never reached the barrier"
        );

        let on_disk = read_the_one_log_file(&dir, prefix);
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            on_disk.contains(&marker),
            "headless event never reached the log file; file held: {on_disk:?}"
        );
        assert!(
            !on_disk.contains('\u{1b}'),
            "the log file must be non-ANSI; escape codes read back as garbage"
        );
        assert!(
            stdout.contents().contains(&marker),
            "headless event never reached stdout; stdout held: {:?}",
            stdout.contents()
        );
    }

    /// Redirected stdout carries no escape codes.
    ///
    /// The library default would: `Layer::default` enables colour whenever
    /// `NO_COLOR` is unset, whatever the sink is, so a `tcr --headless >
    /// run.log` on a machine without `NO_COLOR` wrote a log that no `rg` of a
    /// field name could match. Asserted on both sinks, and against the same
    /// sink with colour ON, so a `stdout_ansi` argument that is quietly
    /// ignored fails here rather than on somebody's CI runner.
    #[test]
    fn headless_stdout_is_plain_when_it_is_not_a_terminal() {
        let plain = SharedBuf::default();
        let marker = format!("headless-ansi-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            Some(plain.clone()),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(peer_listen = %"127.0.0.1:34865", "{marker}");
        });

        let written = plain.contents();
        assert!(
            !written.contains('\u{1b}'),
            "non-terminal stdout must hold no escape byte; held: {written:?}"
        );
        assert!(
            written.contains("peer_listen=127.0.0.1:34865"),
            "a reader must find the plain `key=value`; held: {written:?}"
        );

        // The control: the same call with colour on must differ, which is what
        // proves the assertion above measures the argument.
        let coloured = SharedBuf::default();
        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            Some(coloured.clone()),
            true,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(peer_listen = %"127.0.0.1:34865", "{marker}");
        });
        assert!(
            coloured.contents().contains('\u{1b}'),
            "with colour on the same event carries escapes, so the plain case above is a real result"
        );
    }

    /// A log file that will not open must degrade to stdout-only, never take the
    /// proxy down: logging is not worth the traffic it is observing.
    #[test]
    fn headless_logging_survives_an_unopenable_log_file() {
        let stdout = SharedBuf::default();
        let marker = format!("headless-degraded-probe-{}", std::process::id());

        let subscriber = headless_subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            None,
            Some(stdout.clone()),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{marker}");
        });

        assert!(
            stdout.contents().contains(&marker),
            "with no log file, stdout must still receive every event"
        );
    }

    /// `init_tracing` must target the one shared, well-known cache directory,
    /// not a private/generated path — so a human running the documented
    /// `rg 'server started' <cache dir>/logs/*` recipe can always find
    /// it. This is **not** because any code reads it back (a census —
    /// `rg -n 'log_file_path|log_dir_path' src/ scripts/
    /// apps/`, repo-relevant scope, not `src/` alone: a `src/`-only census once
    /// missed `scripts/validate-cache.sh`'s own copy of the old fixed path —
    /// found no programmatic reader anywhere outside this module; the previous
    /// version of this test justified itself with "this is what `tcr status`
    /// and every diagnostic already read", which was false and is not
    /// repeated here).
    ///
    /// Reads this process's real `HOME` to resolve the path, and writes nothing.
    #[test]
    fn the_log_directory_is_the_shared_cache_location() {
        let cache = tcr::paths::cache_dir();
        let (dir, prefix) = log_location();
        assert_eq!(dir, cache.root().join("logs"));
        assert_eq!(
            Some(cache.root()),
            affinity::default_path().parent(),
            "logs and the pin file share one cache directory"
        );
        assert_eq!(prefix, tcr::paths::LOG_PREFIX);
    }

    /// `max_log_files` pruning is real, not merely configured, AND it is the
    /// production `open_log_appender`'s own hardcoded `max_log_files(5)`
    /// being exercised — not a hand-rolled duplicate of the config, so a
    /// regression that drops or weakens the production call is what this test
    /// is sensitive to. Pre-creates 5 dated files (the `prefix.date` shape
    /// `join_date()` produces for any non-`NEVER` rotation), each with a
    /// distinct creation time, then calls the real production opener —
    /// `prune_old_logs()` runs at *construction*, before the first new file is
    /// created (verified against 0.2.5 source, `rolling.rs:615-617`), which is
    /// what makes this deterministic: no wall-clock rotation boundary needs to
    /// be crossed.
    ///
    /// This is portable across the two CI targets, not merely convenient on
    /// one. `prune_old_logs()` sorts by `metadata.created()` where the
    /// platform supports it, but explicitly falls back to parsing the date out
    /// of the filename itself when it does not (`rolling.rs:689-696`,
    /// `parse_date_from_filename`) — and this test's embedded dates
    /// (`2020-02-01` .. `2020-02-05`) sort in the same order as their real
    /// creation timestamps, so the assertions hold identically whichever path
    /// the pruner takes. Verified directly against this repo's own targets:
    /// macOS (APFS) supports `created()`; the crate's own filename-parsing
    /// fallback exists specifically because ext4/most Linux filesystems (this
    /// repo's `ci` job runs `ubuntu-latest`) commonly return
    /// `ErrorKind::Unsupported` for it — an attempt to break this test via
    /// that path does not succeed, because the fallback is exercised, not
    /// skipped.
    #[test]
    fn old_log_files_are_pruned_at_max_log_files() {
        assert_old_log_files_are_pruned(tcr::paths::LOG_PREFIX, "pruning");
    }

    fn assert_old_log_files_are_pruned(prefix: &str, tag: &str) {
        let dir = unique_log_dir(tag);
        std::fs::create_dir_all(&dir).expect("create temp log dir");

        // Five pre-existing dated files, oldest first, each with a distinct
        // creation time (the pruner sorts by `metadata.created()`).
        let oldest = "2020-02-01";
        let survivors = ["2020-02-02", "2020-02-03", "2020-02-04", "2020-02-05"];
        for date in std::iter::once(&oldest).chain(survivors.iter()) {
            std::fs::write(dir.join(format!("{prefix}.{date}")), b"old\n")
                .expect("write stale log file");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Production max_log_files(5): prune keeps (5 - 1) = 4 of the 5
        // existing files, then the appender creates one new file for today —
        // 5 total, with only the single oldest pre-existing file removed.
        let _appender = open_log_appender(&dir, prefix).expect("open temp log dir");

        let remaining: std::collections::BTreeSet<String> = std::fs::read_dir(&dir)
            .expect("read temp log dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            remaining.len() >= 2,
            "at least 2 files must survive pruning: {remaining:?}"
        );
        assert!(
            !remaining.contains(&format!("{prefix}.{oldest}")),
            "the single oldest file must have been pruned: {remaining:?}"
        );
        for date in survivors {
            assert!(
                remaining.contains(&format!("{prefix}.{date}")),
                "pre-existing file {date} must survive pruning: {remaining:?}"
            );
        }
    }

    // ---- `--group` header composition (`compose_group_header`) ----------------

    #[test]
    fn group_header_with_no_inherited_value_is_just_ours() {
        assert_eq!(
            compose_group_header(None, "codereview"),
            "x-tcr-group: codereview"
        );
        // Empty is treated the same as absent.
        assert_eq!(
            compose_group_header(Some(""), "codereview"),
            "x-tcr-group: codereview"
        );
    }

    #[test]
    fn group_header_appends_to_unrelated_inherited_headers() {
        let composed = compose_group_header(Some("X-Custom: yes"), "codereview");
        assert_eq!(composed, "X-Custom: yes\nx-tcr-group: codereview");
    }

    #[test]
    fn group_header_replaces_rather_than_duplicates_an_existing_group_line() {
        let composed = compose_group_header(
            Some("X-Custom: yes\nx-tcr-group: stale\nX-Other: also-kept"),
            "codereview",
        );
        assert_eq!(
            composed, "X-Custom: yes\nX-Other: also-kept\nx-tcr-group: codereview",
            "the stale line is replaced in place at the END, never duplicated, \
             and every unrelated line's text and order survive"
        );
        assert_eq!(
            composed.matches("x-tcr-group").count(),
            1,
            "must never carry two group headers"
        );
    }

    #[test]
    fn group_header_replacement_matches_the_name_case_insensitively() {
        let composed = compose_group_header(Some("X-TCR-Group: stale"), "codereview");
        assert_eq!(composed, "x-tcr-group: codereview");
    }

    // ---- `--group` claude version gate (`classify_claude_version_output`) -----

    #[test]
    fn claude_version_parses_the_real_output_shape() {
        assert_eq!(
            parse_claude_version("2.1.237 (Claude Code)"),
            Some((2, 1, 237))
        );
        assert_eq!(parse_claude_version("2.1.237"), Some((2, 1, 237)));
    }

    #[test]
    fn claude_version_parse_fails_closed_on_garbage() {
        for garbage in ["", "not a version", "v2.1.237", "2.1", "2.1.abc"] {
            assert_eq!(
                parse_claude_version(garbage),
                None,
                "{garbage:?} must not parse as a version"
            );
        }
    }

    #[test]
    fn claude_version_too_old_refuses() {
        assert_eq!(
            classify_claude_version_output("2.1.226 (Claude Code)"),
            ClaudeVersionCheck::TooOld("2.1.226".to_string())
        );
        assert_eq!(
            classify_claude_version_output("1.9.999 (Claude Code)"),
            ClaudeVersionCheck::TooOld("1.9.999".to_string())
        );
    }

    /// The comparison must be NUMERIC per-component, not lexicographic on the
    /// version string. `"2.1.9"` sorts AFTER `"2.1.227"` as a plain string
    /// (`'9' > '2'`), which would wrongly classify it `Ok` against the
    /// 2.1.227 minimum; numerically `9 < 227`, so it must be `TooOld`. Every
    /// other version-gate test here (`2.1.226`/`1.9.999` vs `2.1.227`) would
    /// pass under EITHER comparison and so does not guard this property —
    /// this is the one case where the two implementations disagree.
    #[test]
    fn claude_version_compares_patch_numerically_not_lexicographically() {
        assert_eq!(
            classify_claude_version_output("2.1.9 (Claude Code)"),
            ClaudeVersionCheck::TooOld("2.1.9".to_string()),
            "2.1.9 must be TooOld against a 2.1.227 minimum — a lexicographic \
             compare would wrongly say Ok because '9' > '2' as characters"
        );
    }

    #[test]
    fn claude_version_at_or_above_minimum_is_ok() {
        assert_eq!(
            classify_claude_version_output("2.1.227 (Claude Code)"),
            ClaudeVersionCheck::Ok
        );
        assert_eq!(
            classify_claude_version_output("2.1.237 (Claude Code)"),
            ClaudeVersionCheck::Ok
        );
        assert_eq!(
            classify_claude_version_output("3.0.0 (Claude Code)"),
            ClaudeVersionCheck::Ok
        );
    }

    #[test]
    fn claude_version_unparseable_output_warns_and_proceeds() {
        assert_eq!(
            classify_claude_version_output("garbage"),
            ClaudeVersionCheck::Unknown
        );
        assert_eq!(
            classify_claude_version_output(""),
            ClaudeVersionCheck::Unknown
        );
    }

    // ---- `--group` name validation (`validate_group`) --------------------------

    fn account_with_groups(name: &str, groups: Option<&[&str]>) -> config::Account {
        config::Account {
            name: name.to_string(),
            account_type: config::AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: None,
            expires_at: None,
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: groups.map(|gs| gs.iter().map(ToString::to_string).collect()),
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            plan_display_name: None,
            egress: config::Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn validate_group_accepts_a_configured_group() {
        let mut config = default_config();
        config.accounts = vec![
            account_with_groups("alice", Some(&["codereview"])),
            account_with_groups("bob", None),
        ];
        assert!(validate_group(&config, "codereview").is_ok());
    }

    #[test]
    fn validate_group_rejects_an_unknown_name_and_lists_the_configured_groups() {
        let mut config = default_config();
        config.accounts = vec![
            account_with_groups("alice", Some(&["codereview", "burst"])),
            account_with_groups("bob", Some(&["burst"])),
        ];
        let err = validate_group(&config, "typo-group")
            .expect_err("an unconfigured group name must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("burst") && msg.contains("codereview"),
            "the error must name every configured group so the operator can fix the typo: {msg}"
        );
    }

    #[test]
    fn validate_group_rejects_when_no_account_has_any_group() {
        let mut config = default_config();
        config.accounts = vec![account_with_groups("alice", None)];
        assert!(
            validate_group(&config, "codereview").is_err(),
            "a typo must never silently resolve to the empty set — that routes everywhere"
        );
    }

    // ---- `--group` label character validation (`validate_group_label_chars`) --

    #[test]
    fn group_label_chars_accepts_an_ordinary_ascii_label() {
        assert_eq!(validate_group_label_chars("codereview"), Ok(()));
        assert_eq!(validate_group_label_chars("code-review_2"), Ok(()));
    }

    #[test]
    fn group_label_chars_rejects_empty_and_whitespace_only() {
        assert_eq!(
            validate_group_label_chars(""),
            Err("empty or whitespace-only")
        );
        assert_eq!(
            validate_group_label_chars("   "),
            Err("empty or whitespace-only")
        );
    }

    #[test]
    fn group_label_chars_rejects_a_newline() {
        assert_eq!(
            validate_group_label_chars("code\nreview"),
            Err("contains a newline")
        );
        assert_eq!(
            validate_group_label_chars("code\rreview"),
            Err("contains a newline")
        );
    }

    #[test]
    fn group_label_chars_rejects_other_control_characters() {
        assert_eq!(
            validate_group_label_chars("code\0review"),
            Err("contains a control character")
        );
        assert_eq!(
            validate_group_label_chars("code\treview"),
            Err("contains a control character")
        );
    }

    #[test]
    fn group_label_chars_rejects_codepoints_above_u00ff() {
        assert_eq!(
            validate_group_label_chars("codereview\u{1F600}"), // an emoji
            Err("contains a codepoint above U+00FF")
        );
    }

    #[test]
    fn validate_group_rejects_a_group_argument_with_an_embedded_newline() {
        let mut config = default_config();
        config.accounts = vec![account_with_groups("alice", Some(&["codereview"]))];
        let err = validate_group(&config, "code\nreview")
            .expect_err("a newline in the --group argument must be refused");
        assert!(
            err.to_string().contains("newline"),
            "the error must name the character class at fault: {err}"
        );
    }

    #[test]
    fn validate_group_rejects_a_config_declared_label_with_a_control_character() {
        let mut config = default_config();
        config.accounts = vec![account_with_groups("alice", Some(&["code\0review"]))];
        let err = validate_group(&config, "codereview")
            .expect_err("a control character in a CONFIG-declared label must also be refused");
        assert!(
            err.to_string().contains("control character"),
            "the error must name the character class at fault: {err}"
        );
    }

    /// `tcr run --group` launches a CLAUDE session, and no Codex account can
    /// serve one: a group whose every member is a Codex account is refused before
    /// anything launches, while a group that also holds a Claude account launches
    /// as before.
    #[test]
    fn run_group_with_no_claude_member_is_refused() {
        let mut config = default_config();
        config.accounts = vec![
            account_with_groups("alice", Some(&["review"])),
            config::Account {
                provider: tcr::provider::Provider::Codex,
                ..account_with_groups("bob-codex", Some(&["lab", "review"]))
            },
        ];
        let err = validate_group(&config, "lab").expect_err("lab holds no Claude account");
        assert_eq!(
            err.to_string(),
            "group 'lab' has no Claude account (its members are Codex accounts, which a Claude session cannot use) — nothing launched."
        );
        assert!(validate_group(&config, "review").is_ok());
    }

    /// `spillToPool` does not lift that refusal. With it, every request of such a
    /// session would fall back to the whole pool, so the session would run and
    /// never once be served by the group it asked for. The launch-time check
    /// already treats a label no account carries the same way, spill or not, and
    /// a group with no Claude account is that, to a Claude session.
    #[test]
    fn run_group_refuses_a_codex_only_group_even_when_it_spills_to_the_pool() {
        let mut config = default_config();
        config.accounts = vec![
            account_with_groups("alice", Some(&["review"])),
            config::Account {
                provider: tcr::provider::Provider::Codex,
                ..account_with_groups("bob-codex", Some(&["lab"]))
            },
        ];
        config.group_settings.insert(
            "lab".to_string(),
            config::GroupSettings {
                spill_to_pool: true,
                ..Default::default()
            },
        );
        assert!(config.group_allows_spill("lab") && !config.is_group_reserved("lab"));
        let err = validate_group(&config, "lab")
            .expect_err("spilling does not give lab a Claude account");
        assert_eq!(
            err.to_string(),
            tcr::provider_guard::group_has_no_claude("lab")
        );
    }

    /// `tcr peer ls` lists a Codex account's exit pin with what it really does:
    /// a loose pin is inert, because a Codex request always leaves from this Mac,
    /// and a strict one skips the account during selection, so its line
    /// names the command that clears it. A Claude pin, and a Codex account left
    /// local, get no line.
    #[test]
    fn peer_ls_marks_a_codex_exit_pin_inert() {
        use tcr::provider::Provider;
        let peer = tcr_peer_wire::PeerId([7_u8; 32]);
        let pinned = |name: &str, provider: Provider, strict: bool| config::Account {
            provider,
            egress: config::Egress::Via(peer),
            egress_strict: strict,
            ..account_with_groups(name, None)
        };
        let accounts = vec![
            pinned("bob@example.com/codex", Provider::Codex, false),
            pinned("carol@example.com/codex", Provider::Codex, true),
            pinned("alice@example.com", Provider::Anthropic, false),
            config::Account {
                provider: Provider::Codex,
                ..account_with_groups("dana@example.com/codex", None)
            },
        ];
        assert_eq!(
            peer_ls_inert_exit_lines(&accounts),
            vec![
                format!(
                    "bob@example.com/codex  exits={} (inert: Codex accounts always leave from this Mac)",
                    peer.display()
                ),
                format!(
                    "carol@example.com/codex  exits={} (refused: a Codex account cannot exit from another Mac, so tcr skips this account; clear it with `tcr peer account carol@example.com/codex --exits-from local --no-must`)",
                    peer.display()
                ),
            ]
        );
    }

    // --- legacy `throttle` migration: boot-time behaviour ---------------------

    fn unique_config_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("tcr-main-{tag}-{}-{seq}.json", std::process::id()))
    }

    /// `migration_persist_target` gates the write `run_server` performs after a
    /// migration: an active migration with nowhere quarantined and a real path
    /// on disk gets rewritten.
    #[test]
    fn migration_persist_target_writes_when_clear() {
        let path = unique_config_path("persist-target-clear");
        let mut config = default_config();
        config.migrated_legacy_throttle = true;
        assert_eq!(
            migration_persist_target(&config, &Some(path.clone())),
            Some(path.as_path())
        );
    }

    /// The quarantine gate is not optional (mirrors `cli::load_for_edit`):
    /// writing back a `Config` while an account is quarantined would serialize
    /// over that account's raw JSON (its `importFrom` pointer included) and
    /// drop it permanently. A pending migration must stay in-memory-only until
    /// a human clears the quarantine.
    #[test]
    fn migration_persist_target_is_none_when_an_account_is_quarantined() {
        let path = unique_config_path("persist-target-quarantined");
        let mut config = default_config();
        config.migrated_legacy_throttle = true;
        config.quarantined_accounts = vec!["acct-import".to_string()];
        assert_eq!(migration_persist_target(&config, &Some(path)), None);
    }

    /// Nothing to persist when `load` never migrated anything.
    #[test]
    fn migration_persist_target_is_none_when_nothing_migrated() {
        let path = unique_config_path("persist-target-unmigrated");
        let config = default_config();
        assert_eq!(migration_persist_target(&config, &Some(path)), None);
    }

    /// Nothing to persist without a file path (e.g. the corrupt-config fallback
    /// used to drop the persist path — see `load_config`).
    #[test]
    fn migration_persist_target_is_none_without_a_persist_path() {
        let mut config = default_config();
        config.migrated_legacy_throttle = true;
        assert_eq!(migration_persist_target(&config, &None), None);
    }

    /// A missing config file is a legitimate first run: `load_config` must
    /// still boot on in-memory defaults, keeping the persist path so the first
    /// refresh creates the file.
    #[test]
    fn load_config_creates_the_file_when_it_is_missing() {
        let path = unique_config_path("missing");
        let (config, persist_path) =
            load_config(&path).expect("a missing config file must not refuse to boot");
        assert!(config.accounts.is_empty());
        assert_eq!(persist_path, Some(path.clone()));
        // The server's first boot now LEAVES the file behind, so the `tcr
        // status` a user runs next finds the same config the server booted from
        // rather than nothing at all (`config::load_or_init`).
        assert!(
            path.exists(),
            "booting with no config must create {}",
            path.display()
        );
        std::fs::remove_file(&path).log_debug("removing a test config");
    }

    /// The behaviour this task changed: a config that exists and fails to
    /// parse (malformed JSON, NOT the legacy `throttle` key — that key is
    /// migrated, never an error, per `config::load`) must make the server
    /// REFUSE to boot rather than silently serve a zero-account fleet that
    /// answers every request with 429 while looking alive.
    #[test]
    fn load_config_refuses_to_boot_on_a_corrupt_config() {
        let path = unique_config_path("corrupt");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let err = load_config(&path).expect_err("a corrupt config must refuse to boot");
        assert!(
            err.to_string().contains("unreadable/corrupt"),
            "the refusal must say why: {err}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Every shutdown trigger must log a distinct, non-empty line.
    ///
    /// `ServingStopped` previously logged NOTHING: its arm was `=> {}` and the
    /// only statement after the match warns solely when tasks were aborted. A
    /// process that stopped down that path left no trace of having stopped.
    ///
    /// Distinctness matters as much as presence. `tests/headless_sigterm.rs`
    /// asserts on the SIGTERM text specifically, so two triggers sharing a line
    /// would make that assertion pass for the wrong trigger, which is a green
    /// test for a broken reason.
    #[test]
    fn every_shutdown_trigger_logs_a_line() {
        let triggers = [
            ShutdownTrigger::CtrlC,
            ShutdownTrigger::Sigterm,
            ShutdownTrigger::ServingStopped,
            ShutdownTrigger::Replaced,
        ];
        let mut seen: Vec<&str> = Vec::new();
        for trigger in triggers {
            let line = trigger.shutdown_line();
            assert!(
                !line.trim().is_empty(),
                "{trigger:?} logs nothing on the way out"
            );
            assert!(
                !seen.contains(&line),
                "{trigger:?} shares its line with another trigger ({line:?}); \
                 the SIGTERM assertion in tests/headless_sigterm.rs would then \
                 pass for the wrong reason"
            );
            seen.push(line);
        }
        assert_eq!(
            ShutdownTrigger::Sigterm.shutdown_line(),
            "SIGTERM received; shutting down",
            "tests/headless_sigterm.rs matches this exact text; changing it here \
             without changing it there turns that test red for no real reason"
        );
    }

    /// The help renders no fleet, so every fleet reads the same help: it names both providers,
    /// and says which verbs, flags and peer features are Claude's alone. clap drops a
    /// doc-comment's closing period from the text it shows. The tokens TcrBar probes the help
    /// for stay whole tokens.
    #[test]
    fn help_names_both_providers() {
        use clap::CommandFactory as _;
        let cli = Cli::command();
        let text =
            |styled: Option<&clap::builder::StyledStr>| styled.map_or_default(ToString::to_string);
        let sub = |path: &[&str]| -> clap::Command {
            let mut command = cli.clone();
            for name in path {
                command = command
                    .find_subcommand(name)
                    .unwrap_or_else(|| panic!("tcr has no {path:?} subcommand"))
                    .clone();
            }
            command
        };
        let arg_help = |command: &clap::Command, id: &str| {
            text(
                command
                    .get_arguments()
                    .find(|arg| arg.get_id() == id)
                    .unwrap_or_else(|| panic!("no {id} argument"))
                    .get_help(),
            )
        };

        assert_eq!(
            text(cli.get_about()),
            "Lean single-user rotating proxy for Claude and Codex (ChatGPT) accounts, with a \
             live TUI"
        );
        assert_eq!(
            text(sub(&["login"]).get_about()),
            "Add an account: Claude through the browser, or Codex (ChatGPT) with --codex or \
             --from-codex"
        );
        assert_eq!(
            text(sub(&["mint"]).get_about()),
            "Mint a long-lived (365-day) token for an existing Claude account, or every Claude \
             account in a group, and put the result on the clipboard. Nothing is stored — \
             export only (Claude accounts only)"
        );
        assert_eq!(
            text(sub(&["control"]).get_about()),
            "Set, clear, or show the identity-bound control account (Claude accounts only)"
        );
        assert_eq!(
            text(sub(&["doctor"]).get_about()),
            "Say whether Claude Code is actually reaching this proxy, and which file decides \
             that; and whether codex is pooled, when the fleet holds a Codex account or with \
             --codex. Exits 0 when it is, 2 when another base URL owns the route, 3 when the \
             route points here and no proxy answers"
        );
        assert_eq!(
            arg_help(&sub(&["doctor"]), "codex"),
            "Report only whether codex is pooled, and exit with that verdict's code"
        );
        assert!(
            text(sub(&["list"]).get_about()).starts_with(
                "Print every Claude account as claude-swap's `list --json` document, for \
                 CodexBar."
            ),
            "{}",
            text(sub(&["list"]).get_about())
        );
        let account = text(sub(&["peer", "account"]).get_about());
        assert!(
            account.ends_with(". Codex accounts always leave from this Mac"),
            "{account}"
        );
        for verb in ["share", "via", "lend"] {
            let about = text(sub(&["peer", verb]).get_about());
            assert!(
                about.ends_with(
                    ". Codex requests always leave from this Mac: no Mac lends, serves or \
                     carries them in this version"
                ),
                "peer {verb}: {about}"
            );
        }
        let group = arg_help(&sub(&["run"]), "group");
        assert!(
            group.ends_with(". Claude sessions only; a Codex session cannot ask for a group"),
            "{group}"
        );

        for (path, tokens) in [
            (&["login"][..], &["--non-interactive", "--codex"][..]),
            (&["server"], &["--replace", "--no-replace"]),
            (&["doctor"], &["--codex"]),
        ] {
            let help = sub(path).render_long_help().to_string();
            for token in tokens {
                assert!(
                    tcr::codex_local::help_has_whole_token(&help, token),
                    "`tcr {} --help` must carry {token} as a whole token:\n{help}",
                    path.join(" ")
                );
            }
        }
    }

    fn persisted_tls_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    #[test]
    fn see_through_ca_keeps_a_running_legacy_proxys_chain_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let ca = mitm::load_tls_in(dir.path()).unwrap().ca_path;
        std::fs::remove_file(dir.path().join("tcr-ca.key")).unwrap();
        std::fs::write(
            dir.path().join("tcr-leaf.sans"),
            "api.anthropic.com\nplatform.claude.com\nwww.example.org\n",
        )
        .unwrap();
        let before = persisted_tls_files(dir.path());

        assert_eq!(see_through_ca_in(dir.path()), Some(ca));

        assert!(
            persisted_tls_files(dir.path()) == before,
            "launching a client must not rotate the running proxy's CA or leaf"
        );
        assert!(!dir.path().join("tcr-ca.key").exists());
    }

    #[test]
    fn see_through_ca_does_not_create_certificate_material() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        assert_eq!(see_through_ca_in(&absent), None);
        assert!(!absent.exists(), "only server startup may create a chain");
    }

    #[test]
    fn see_through_ca_reuses_a_fresh_servers_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = mitm::load_tls_in(dir.path()).unwrap().ca_path;
        let before = persisted_tls_files(dir.path());
        assert_eq!(see_through_ca_in(dir.path()), Some(ca));
        assert_eq!(persisted_tls_files(dir.path()), before);
    }

    /// Claude's trust notice names only the setting its own connection needs.
    #[test]
    fn the_claude_see_through_notice_names_only_its_ca() {
        let ca = Path::new("/Users/example/.config/tcr-ca.pem");
        assert_eq!(
            see_through_ca_notice(ca),
            "[tcr] trusting our MITM leaf via NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem"
        );
    }

    /// A pooled codex session carries `HTTPS_PROXY` aimed at tcr, so a `claude` that codex
    /// starts meets tcr's leaf for `api.anthropic.com`, and node trusts it only through
    /// `NODE_EXTRA_CA_CERTS`. A pooled launch sets it to tcr's CA, unless the caller exported one,
    /// which the child then inherits untouched. A direct launch meets no leaf and sets nothing.
    #[test]
    fn a_claude_inside_a_pooled_codex_gets_node_extra_ca_certs() {
        let ca = Path::new("/tmp/ca.pem");
        let mut cmd = std::process::Command::new("true");
        apply_nested_claude_ca(&mut cmd, ca, false);
        assert_eq!(
            env_changes(&cmd).get("NODE_EXTRA_CA_CERTS"),
            Some(&Some("/tmp/ca.pem".to_string()))
        );
        let mut cmd = std::process::Command::new("true");
        apply_nested_claude_ca(&mut cmd, ca, true);
        assert_eq!(
            env_changes(&cmd).get("NODE_EXTRA_CA_CERTS"),
            None,
            "the caller's own value is inherited, not replaced"
        );

        for (caller, expected) in [
            (inherited(&[]), Some(Some("/tmp/ca.pem".to_string()))),
            (
                inherited(&[("NODE_EXTRA_CA_CERTS", "/tmp/corporate-ca.pem")]),
                None,
            ),
        ] {
            let mut cmd = std::process::Command::new("true");
            let _lines = apply_codex_launch(
                &mut cmd,
                3999,
                &CodexLaunch::SeeThrough {
                    ca: ca.to_path_buf(),
                    no_proxy: Vec::new(),
                },
                &LaunchKey::NoKeyInConfig,
                &test_host(&caller),
            );
            assert_eq!(
                env_changes(&cmd).get("NODE_EXTRA_CA_CERTS").cloned(),
                expected,
                "a pooled launch sets the CA for a nested claude unless the caller exported one"
            );
        }

        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_launch(
            &mut cmd,
            3999,
            &CodexLaunch::Direct {
                reason: "no proxy answered on 127.0.0.1:3999".to_string(),
                unset: Vec::new(),
            },
            &LaunchKey::NoKeyInConfig,
            &test_host(&inherited(&[])),
        );
        assert_eq!(env_changes(&cmd).get("NODE_EXTRA_CA_CERTS"), None);
    }
}

#[cfg(test)]
mod routing_cli_tests {
    use super::*;

    /// **`tcr routing` speaks JSON only**: without `--json` it is a usage error rather than
    /// a text form a launcher could mistake for the document.
    #[test]
    fn routing_takes_only_json() {
        let Err(err) = Cli::try_parse_from(["tcr", "routing"]) else {
            panic!("`tcr routing` without --json must be refused");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "{err}"
        );
        assert!(matches!(
            Cli::try_parse_from(["tcr", "routing", "--json"]).map(|cli| cli.command),
            Ok(Some(Command::Routing(_)))
        ));
    }
}
