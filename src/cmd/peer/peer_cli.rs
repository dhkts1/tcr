//! The clap surface of every `tcr peer` verb: the argument structs and enums, and nothing that runs.

use std::path::PathBuf;

use clap::Subcommand;

/// `--peers` on every peer verb that takes it with the stock help line,
/// resolved in one place.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct PeersFile {
    /// Path to the peers file (default: ~/.config/tcr-peers.json).
    #[arg(long)]
    pub peers: Option<PathBuf>,
}

impl PeersFile {
    /// The path given, or the default peers file.
    pub(crate) fn path(&self) -> PathBuf {
        self.peers
            .clone()
            .unwrap_or_else(tcr::peer::config::default_path)
    }
}

#[derive(clap::Args)]
pub(crate) struct PeerArgs {
    #[command(subcommand)]
    pub action: PeerAction,
}

#[derive(Subcommand)]
pub(crate) enum PeerAction {
    /// Print this node's own peer id, minting the keypair on first use.
    ///
    /// Never re-mints: a new key evicts every peer that pinned this one.
    Id(PeerIdArgs),
    /// List pinned peers, what each may do, and what is in flight.
    Ls(PeerLsArgs),
    /// Find other Macs on this network, and let them find this one.
    Find(PeerFindArgs),
    /// Set this Mac's display name, what another Mac shows for it.
    Name(PeerNameArgs),
    /// Let trusted Macs serve requests with this Mac's accounts, which
    /// means they read those requests, prompts included.
    /// Codex requests always leave from this Mac: no Mac lends, serves or carries them in this version.
    Share(PeerSwitchArgs),
    /// Trust a Mac interactively: both screens show six digits, you
    /// compare them, you confirm on both.
    Pair(PeerPairArgs),
    /// Mint a one-line join key for a Mac with no screen.
    Invite(PeerInviteArgs),
    /// Join another Mac using a key it printed.
    Join(PeerJoinArgs),
    /// Stop trusting a Mac. One deleted line; the next handshake from it
    /// fails.
    ///
    /// It does NOT revoke egress still reachable through a peer that holds
    /// `forward`, and the output says so whenever one does.
    Forget(PeerForgetArgs),
    /// Say hello to one pinned peer over the address already on its row:
    /// tell it where this node listens now, and record where it says it
    /// listens. The one thing that lets a moved Mac be found again
    /// without re-pairing.
    Hello(PeerHelloArgs),
    /// Grant or revoke one thing for one peer.
    Allow(PeerAllowArgs),
    /// Set what one peer may borrow on one window, or take it away.
    /// Codex requests always leave from this Mac: no Mac lends, serves or carries them in this version.
    Lend(PeerLendArgs),
    /// Choose how this Mac reaches the internet.
    /// Codex requests always leave from this Mac: no Mac lends, serves or carries them in this version.
    Via(PeerViaArgs),
    /// Turn this Mac's dead drop on or off: leaving a friend that has also
    /// moved this Mac's current address on an HTTPS surface you own, and
    /// reading theirs back. Needs a store (`tcr peer drop-store set`) and
    /// `control-drop` granted per peer (`tcr peer allow`).
    Drop(PeerDropArgs),
    /// Point the dead drop at an HTTPS surface, or show or clear the one
    /// already set.
    DropStore(PeerDropStoreArgs),
    /// Show the Macs asking to connect with this one, and nothing else about
    /// them: a request is a row, never a trust decision.
    Pending(PeerPendingArgs),
    /// Approve one pairing request, opening a two-minute window for that
    /// one Mac.
    Accept(PeerDecideArgs),
    /// Turn one pairing request down and stay quiet to that address for an
    /// hour.
    Ignore(PeerDecideArgs),
    /// Never hear from that Mac again: its address, and its key too when
    /// this Mac has learned one.
    Block(PeerDecideArgs),
    /// Lift a block.
    Unblock(PeerUnblockArgs),
    /// The office network key: mint one, paste one in, or clear it.
    NetworkKey(PeerNetworkKeyArgs),
    /// Print one link to share, which brings another Mac onto this mesh.
    Link(PeerLinkArgs),
    /// Mint, or open, the sealed link one Mac sends one friend after it
    /// changed networks. It carries an address and nothing else: it joins
    /// nothing, grants nothing and pairs nothing.
    Moved(PeerMovedArgs),
    /// Print what this Mac can be reached on from off its own LAN.
    Reach(PeerReachArgs),
    /// Show or set how this Mac uses a tailnet: `off`, `app` (read the
    /// Tailscale app on this Mac) or `node` (tcr joins as its own node,
    /// source builds with the `tailnet-node` feature only).
    Tailnet(PeerTailnetArgs),
    /// Let pinned Macs reach this one from off its own LAN. Off by
    /// default: with it on, this Mac asks its router for a port mapping
    /// and answers a return visit against a pinned key from anywhere,
    /// and still answers nothing else from off the LAN.
    Internet(PeerInternetArgs),
    /// Set where one account's requests leave from, and what an exit
    /// this Mac cannot reach costs.
    /// Codex accounts always leave from this Mac.
    Account(PeerAccountArgs),
    /// Ask the RUNNING proxy what it holds for each pinned Mac.
    Status(PeerStatusArgs),
    /// Print, or serve, the whole mesh as this Mac sees it: one node per
    /// Mac, one edge per way to reach one, one edge per live lease.
    Graph(PeerGraphArgs),
}

#[derive(clap::Args)]
pub(crate) struct PeerGraphArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the
    /// operator's own config directory). Also selects the runtime-state
    /// file beside it and the node-key directory, the same one-flag
    /// contract every other peer verb gives a test.
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// Emit the graph as JSON instead of greppable text.
    #[arg(long)]
    pub json: bool,
    /// Serve an inline HTML page drawing the graph, refreshed every 5 s.
    /// Binds LOOPBACK ONLY and refuses any other address; the mesh-served
    /// variant (any trusted Mac reading any other's graph) is deferred.
    #[arg(long)]
    pub serve: bool,
    /// Loopback address to serve on with `--serve`.
    #[arg(long, default_value = "127.0.0.1:7756")]
    pub addr: String,
}

#[derive(clap::Args)]
pub(crate) struct PeerAccountArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the config
    /// directory). Read to turn a Mac's NAME into the peer id the pin
    /// stores, and to say so when no pinned Mac answers to it.
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// Path to the main config. This is the file the pin is written to.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Which account to change, by the same query every other account verb
    /// takes: a name, or enough of one to be unambiguous.
    pub account: String,
    /// Where this account's requests leave from: `local` for this Mac's
    /// own socket, or a pinned Mac, named by its peer id or by the name it
    /// shows under in `tcr peer ls`.
    ///
    /// Omitted leaves the pin alone, which is what lets `--must` change
    /// only the strictness of a pin that is already there.
    #[arg(long = "exits-from", value_name = "local|PEER")]
    pub exits_from: Option<String>,
    /// Refuse a request this exit cannot carry, rather than sending it out
    /// of this Mac instead. For an account pinned BECAUSE the address is
    /// load-bearing, where leaving by another route is worse than not
    /// leaving at all.
    #[arg(long)]
    pub must: bool,
    /// Let a request fall back to this Mac when the exit is unavailable,
    /// which is the default.
    #[arg(long = "no-must", conflicts_with = "must")]
    pub no_must: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerInternetArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the config directory).
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// `on` or `off`.
    #[arg(value_enum)]
    pub state: Switch,
}

#[derive(clap::Args)]
pub(crate) struct PeerDropArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the config directory).
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// `on` or `off`.
    #[arg(value_enum)]
    pub state: Switch,
}

/// `tcr peer drop-store set|show|clear [value]`.
///
/// A flat args struct with the action as a positional value, `PeerNetworkKeyArgs`'s
/// shape and for the same measured reason: a nested subcommand refuses `--peers`
/// placed before the action.
#[derive(clap::Args)]
pub(crate) struct PeerDropStoreArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the config directory).
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// Point the store at a template, print what is set, or clear it.
    #[arg(value_enum)]
    pub action: PeerDropStoreAction,
    /// For `set`: the URL template containing `{name}`, used for both the
    /// publish and the fetch.
    pub value: Option<String>,
    /// For `set`: the bearer token. Give the flag with no value to read it
    /// from standard input instead, so it never enters this process's argv
    /// or the shell history.
    #[arg(long, num_args = 0..=1)]
    pub token: Option<Option<String>>,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PeerDropStoreAction {
    /// Point the store at a template, or a token, or both.
    Set,
    /// Print the kind and the template, token redacted.
    Show,
    /// Forget the store. Does not turn `peer.drop` off by itself.
    Clear,
}

/// `tcr peer tailnet [off|app|node|leave] [--name N] [--auth-key -] [--json]`.
///
/// A flat args struct with the mode as an optional positional value,
/// `PeerDropStoreArgs`'s shape and for the same measured reason: a nested
/// subcommand refuses `--peers` placed before the action.
#[derive(clap::Args)]
pub(crate) struct PeerTailnetArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The mode to set, or `leave`. Omit to print the current one.
    #[arg(value_enum)]
    pub mode: Option<PeerTailnetMode>,
    /// `node` only: the machine name to ask the tailnet for.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// `node` only: `-` reads a Tailscale auth key from standard input
    /// and joins with it. The key is never taken as an argument, never
    /// written anywhere and never logged.
    #[arg(long, value_name = "-")]
    pub auth_key: Option<String>,
    /// Print the setting and what the running proxy recorded as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum PeerTailnetMode {
    /// No tailnet use.
    Off,
    /// Read the Tailscale app on this Mac to find your own other Macs.
    App,
    /// tcr joins a tailnet as its own node.
    Node,
    /// Leave the tailnet as tcr's own node: mode off, then delete the
    /// node's key file once the running tcr has let go of it.
    Leave,
}

impl PeerTailnetMode {
    /// The setting this word writes, or `None` for `leave`, which is a
    /// verb rather than a setting.
    pub(crate) const fn mode(self) -> Option<tcr::peer::config::TailnetMode> {
        match self {
            Self::Off => Some(tcr::peer::config::TailnetMode::Off),
            Self::App => Some(tcr::peer::config::TailnetMode::App),
            Self::Node => Some(tcr::peer::config::TailnetMode::Node),
            Self::Leave => None,
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct PeerStatusArgs {
    /// Path to the main config, read for the port and api-key this asks on.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Emit the peers block as JSON instead of greppable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerReachArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the config directory).
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// Emit a machine-readable JSON object instead of greppable text.
    #[arg(long)]
    pub json: bool,
    /// Also ask the router for a port mapping, held for two minutes.
    ///
    /// Off by default because it WRITES to the router. A repeated NAT-PMP
    /// request replaces the lifetime of an existing mapping rather than
    /// adding one (RFC 6886 s3.3), so a probe run while something holds a
    /// long mapping for the same port would cut that mapping down to this
    /// probe's two minutes. Without the flag this verb only asks questions.
    #[arg(long)]
    pub map: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerPendingArgs {
    /// Path to the peers file (default: ~/.config/tcr-peers.json). Also
    /// selects the runtime-state file beside it, so a test points the
    /// whole peer surface at a temp dir with this one argument.
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// Emit a machine-readable JSON object instead of greppable text.
    #[arg(long)]
    pub json: bool,
}

/// The selector every approval verb takes: an instance id or an address.
///
/// One shape for `accept`, `ignore` and `block` rather than three, because
/// the three are one decision an operator makes about one row and a
/// selector that meant different things per verb is the kind of difference
/// nobody discovers until they block the wrong Mac.
#[derive(clap::Args)]
pub(crate) struct PeerDecideArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The instance id or the address, as `tcr peer pending` prints them.
    pub target: String,
}

#[derive(clap::Args)]
pub(crate) struct PeerUnblockArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The address to unblock, as `tcr peer ls --json` lists it under
    /// `blocked`.
    pub addr: String,
}

/// `tcr peer network-key set|join|clear|show [<key>]`.
///
/// A flat args struct with the action as a positional value, not a nested
/// `#[command(subcommand)]`. Measured, not a preference: with a nested
/// subcommand, clap requires the parent's own flags BEFORE it, so
/// `tcr peer network-key set --peers <path>` is refused with "unexpected
/// argument '--peers' found" while every other peer verb accepts `--peers`
/// anywhere. One surface where a flag goes in a different place is a
/// surface an operator gets wrong once and a test harness gets wrong
/// silently.
#[derive(clap::Args)]
pub(crate) struct PeerNetworkKeyArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// What to do: mint one, paste one in, clear it, or say whether one is
    /// set.
    #[arg(value_enum)]
    pub action: PeerNetworkKeyAction,
    /// For `join`: the key string the other Mac printed. Omit with
    /// `--stdin`.
    pub key: Option<String>,
    /// For `join`: read the key from standard input instead, so it never
    /// enters this process's argv and so never appears in `ps` or in shell
    /// history.
    #[arg(long)]
    pub stdin: bool,
    /// Required by `set` and `join` when a key is ALREADY set, because
    /// replacing one cuts this Mac off from every Mac still holding the old
    /// one. Without it the verb refuses and prints what it would have cut
    /// off.
    #[arg(long)]
    pub replace: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PeerNetworkKeyAction {
    /// Mint a key and print it once. Every Mac in the office pastes it in
    /// with `network-key join`.
    Set,
    /// Paste in the key another Mac printed.
    Join,
    /// Forget the key. This Mac then sees, and is seen by, every `tcr` on
    /// the network again.
    Clear,
    /// Say whether a key is set, without printing it.
    Show,
}

#[derive(clap::Args)]
pub(crate) struct PeerLinkArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// Also mint a one-use join key and carry it in the link, so the other
    /// Mac enrols with this one on opening it. **The link is then a live
    /// bearer secret with ten minutes on it**, without this flag it
    /// carries the network key alone.
    #[arg(long)]
    pub invite: bool,
    /// A name for the joining Mac, when `--invite` is given.
    #[arg(long)]
    pub label: Option<String>,
}

/// Which half of `tcr peer moved` is being run.
///
/// A typed pair rather than two verbs, because the two are one feature and
/// a reader looking for either finds both, and rather than a bare string,
/// so a third spelling cannot appear at one call site and not another.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PeerMovedAction {
    /// Print one link for one already-trusted Mac, saying where this Mac
    /// is now.
    Mint,
    /// Read a link somebody sent: say which of this Mac's peers it is from
    /// and where that Mac now is.
    Open,
}

#[derive(clap::Args)]
pub(crate) struct PeerMovedArgs {
    /// `mint` or `open`.
    #[arg(value_enum)]
    pub action: PeerMovedAction,
    /// For `mint`: the peer id to seal for, in its full wire form, the
    /// `node` field of `tcr peer ls --json`. For `open`: the
    /// `tcr://peer/moved?…` link.
    ///
    /// **A link typed here is visible in `ps` to every process on this Mac
    /// and in the shell history**, and `open` says so on stderr when it
    /// finds one here. `--stdin` is the path that does not leak, and it is
    /// the only one the panel and the `tcr://` handler use.
    pub target: Option<String>,
    /// For `open`: read the link from standard input, one line, so it never
    /// enters this process's argument vector.
    #[arg(long)]
    pub stdin: bool,
    /// For `open`: write the addresses. Without it `open` reads the link,
    /// says what it would add, and changes nothing. No effect on `mint`.
    #[arg(long)]
    pub yes: bool,
    /// Path to the peers file (default: ~/.config/tcr-peers.json). Also
    /// selects the runtime-state file beside it and the node-key directory,
    /// the same one-flag contract every other peer verb gives a test.
    #[arg(long)]
    pub peers: Option<PathBuf>,
}

/// On or off. A typed pair rather than a bare string, so a third spelling
/// cannot appear at one call site and not another.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Switch {
    On,
    Off,
}

/// The argv spelling of [`tcr::peer::config::LendMode`].
///
/// A separate enum because clap's derive needs one it owns, and the
/// conversion is the one place the two spellings meet.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum PeerLendMode {
    /// The borrower's requests go over the owner's Mac.
    Serve,
    /// The owner hands over a short-lived bearer and the borrower sends
    /// on its own IP.
    Hand,
}

impl From<PeerLendMode> for tcr::peer::config::LendMode {
    fn from(mode: PeerLendMode) -> Self {
        match mode {
            PeerLendMode::Serve => Self::Serve,
            PeerLendMode::Hand => Self::Hand,
        }
    }
}

/// Which grant `tcr peer allow` is setting.
///
/// The two inspect grants are one direction each, and the asymmetry is the
/// point: `inspect` is "I will read their requests" and `disclose` is
/// "they may read mine". A lease needs an explicit act on both machines.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PeerGrant {
    /// Carry this peer's bytes out to an allow-listed origin, blind.
    Gateway,
    /// We may ask this peer to carry us out. The inverse of `Gateway`,
    /// separate because one act must not grant both directions.
    Carry,
    /// Forward to peers this Mac has pinned. **Transitive**: the node two
    /// hops out then reaches them with this peer's authority.
    Forward,
    /// Accept requests from this peer and serve them here, reading them in
    /// full.
    Inspect,
    /// Send requests to this peer to serve, letting it read them in full.
    Disclose,
    /// Accept an account moved from this peer. The only grant under which
    /// a credential crosses a host boundary.
    AcceptMove,
    /// Tell this peer about this Mac's other peers, one level out.
    ControlBriefs,
    /// Tell this peer how much this Mac could lend, per window.
    ControlLendable,
    /// Tell this peer this Mac's build and boot id.
    ControlDiag,
    /// Leave this peer this Mac's current address at the dead drop, and
    /// apply the address it leaves back. Read on both sides: a peer this
    /// Mac does not publish for does not get to write endpoints onto its
    /// row either.
    ControlDrop,
}

/// `--peers <path>`, on every verb, for the same reason `--config` is on
/// every account verb: a test points the whole peer surface at a temp file
/// with one argument and touches nothing real.
#[derive(clap::Args)]
pub(crate) struct PeerIdArgs {
    /// Path to the peers file (default: ~/.config/tcr-peers.json). Also
    /// selects the node-key directory, so a test points the whole peer
    /// surface at a temp dir with this one argument.
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// Mint a NEW keypair, evicting every peer that pinned this one, the
    /// next handshake from any of them fails the pin check, correctly.
    /// Requires `--yes`.
    #[arg(long)]
    pub regenerate: bool,
    /// Confirm `--regenerate`. Has no effect alone.
    #[arg(long)]
    pub yes: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerLsArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// Emit a machine-readable JSON object instead of greppable text.
    ///
    /// A SIBLING document, never an addition to `tcr status --json`: that
    /// is a bare array of accounts and clients depend on exactly that
    /// shape.
    ///
    /// Every row under `peers` carries two derived keys beside the file's
    /// own: `until`, the unix SECONDS this Mac's lending to that row stops
    /// (`null` when nothing it holds has an end), and `ended`, true when
    /// every lease it holds is past its `until`, the row stays in the
    /// list either way. Names as written, camelCase like their
    /// neighbours; the panel decodes both.
    #[arg(long)]
    pub json: bool,
    /// Path to the main config. The
    /// `--json` form reads it for the ACCOUNT LABELS the `lentTo` block is
    /// keyed by, each account's exit lock (`exits`) and the expiry a
    /// handed-over key ends at (`handedKeyUntil`); the text form reads it
    /// for the Codex exit pins it lists. A config that is missing or
    /// unreadable reads as one with no accounts, so `lentTo` and `exits`
    /// are empty and no pins are listed, rather than failing the listing:
    /// the peers half of this output does not depend on it.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(clap::Args)]
pub(crate) struct PeerSwitchArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// `on` or `off`.
    #[arg(value_enum)]
    pub state: Switch,
    /// Which rate-limit window is shared. `5h` and `7d` are UNTIERED, a
    /// lease on either is not per-model, because `7d_oi` is the only
    /// window upstream reports per model at all.
    #[arg(long, value_enum, default_value = "7d")]
    pub window: PeerWindow,
    /// The ceiling on any one lease, as a fraction of that window.
    /// Clamped to 0.0..=0.5, the same clamp the main config applies to its
    /// own reserve.
    #[arg(long, default_value_t = 0.10)]
    pub fraction: f64,
    /// How long a granted lease lives, in seconds.
    #[arg(long, default_value_t = 600)]
    pub ttl: u32,
    /// How many borrowed requests may be in flight against one lease.
    /// `0` mints leases every request of which is refused, which is what
    /// writing `0` asks for; it is not silently raised to `1`.
    #[arg(long, default_value_t = 2)]
    pub max_inflight: u8,
    /// What the DEFAULT lease draws from: `all`, `group:<name>`, or
    /// `account:<label>[,<label>]`. Written onto every pinned Mac and
    /// recorded as `defaultLend`, which is what the Sharing defaults sheet
    /// shows.
    #[arg(long, default_value = "all")]
    pub scope: String,
    /// Path to the main config. Read only when `--scope account:` names accounts, to
    /// refuse a label that is a Codex account's; a config that is missing
    /// or unreadable skips that check.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// The wire's three windows, as an operator types them.
///
/// A typed pair with [`tcr_peer_wire::Window`] rather than a string parsed
/// at each call site: `Window::Unknown` is a FORWARD-COMPAT arm for a
/// window a future build names, and an operator must not be able to ask for
/// it, a lease against a window this build cannot measure is a lease
/// against nothing.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PeerWindow {
    /// The rolling five-hour window. Untiered.
    #[value(name = "5h")]
    FiveHour,
    /// The weekly window. Untiered.
    #[value(name = "7d")]
    SevenDay,
    /// The weekly OUTPUT window, which is the only model-scoped one.
    #[value(name = "7d_oi")]
    SevenDayOi,
}

impl From<PeerWindow> for tcr_peer_wire::Window {
    fn from(window: PeerWindow) -> Self {
        match window {
            PeerWindow::FiveHour => Self::FiveHour,
            PeerWindow::SevenDay => Self::SevenDay,
            PeerWindow::SevenDayOi => Self::SevenDayOi,
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct PeerLendArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The peer id to change, in its full wire form, the `node` field of
    /// `tcr peer ls --json`, never the `tcr-…` short form the text output
    /// prints. See [`PeerAllowArgs::peer`].
    pub peer: String,
    /// Which window this grant is against.
    #[arg(long, value_enum, default_value = "7d")]
    pub window: PeerWindow,
    /// The ceiling on any one lease, as a fraction of that window.
    /// Clamped to 0.0..=0.5. `0` removes the grant.
    #[arg(long, default_value_t = 0.10)]
    pub fraction: f64,
    /// How long a granted lease lives, in seconds.
    #[arg(long, default_value_t = 600)]
    pub ttl: u32,
    /// How many borrowed requests may be in flight against one lease.
    #[arg(long, default_value_t = 2)]
    pub max_inflight: u8,
    /// What this lease draws from: `all`, `group:<name>`, or
    /// `account:<label>[,<label>]`. A Mac may hold one lease
    /// per scope, so lending a second scope adds a lease rather than
    /// replacing the first.
    ///
    /// Labels are the sanitized ones `tcr status` prints, never an email
    /// and never a uuid, because this is written to a file and printed by a
    /// CLI in a public repository.
    #[arg(long, default_value = "all")]
    pub scope: String,
    /// How the borrower spends this grant: `serve` (its requests go over
    /// this Mac and out on this Mac's IP, and this Mac reads them) or
    /// `hand` (this Mac hands over the account's short-lived access token
    /// and the borrower sends on its own IP).
    ///
    /// Omitted keeps the mode of the grant being replaced, and `serve` for
    /// a new one. Without that, editing a hand grant's fraction would
    /// quietly turn it back into a serve grant, which is a different
    /// disclosure decision than the one the operator took.
    #[arg(long, value_enum)]
    pub mode: Option<PeerLendMode>,
    /// Path to the main config. Read by `--mode hand`, to refuse a grant every account
    /// in its scope is strictly pinned away from, and when `--scope
    /// account:` names accounts, to refuse a label that is a Codex
    /// account's.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Lend for a duration (`2h`, `90m`, `3d`), after which this Mac stops
    /// renewing the lease. `none` clears an end.
    #[arg(long = "for", value_name = "DURATION", conflicts_with = "until")]
    pub for_: Option<String>,
    /// Lend until a time of day (`18:00`), today or tomorrow, whichever
    /// comes next.
    #[arg(long)]
    pub until: Option<String>,
    /// Only lend between these hours, local time (`22:00-08:00`). A window
    /// that ends before it starts crosses midnight. Unlike `--until`, which
    /// ends the lending once, this closes and re-opens every day.
    #[arg(long, value_name = "HH:MM-HH:MM")]
    pub between: Option<String>,
    /// Only lend on these days (`mon,tue`), meaning the days the
    /// `--between` window may START on: an overnight window granted on
    /// `fri` runs into Saturday morning.
    #[arg(long, value_name = "DAYS")]
    pub days: Option<String>,
    /// List this peer's leases instead of changing them, one greppable
    /// line each, with the lease id `--revoke` and `--relend` take.
    #[arg(long, conflicts_with_all = ["revoke", "relend"])]
    pub list: bool,
    /// Take one lease away, by the id `--list` printed. The other leases
    /// this Mac holds are untouched.
    #[arg(long, value_name = "LEASE-ID", conflicts_with = "relend")]
    pub revoke: Option<String>,
    /// Put an ended lease back to work, with a new `--for`/`--until` or
    /// with no end at all, for "re-lend with one click".
    #[arg(long, value_name = "LEASE-ID")]
    pub relend: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct PeerFindArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// `on` or `off`.
    #[arg(value_enum)]
    pub state: Switch,
    /// Whether the beacon carries this Mac's display name. **Off by
    /// default**: a name is the one field in the
    /// beacon that is not ephemeral, so it is the one field the operator
    /// has to ask for. Turn it on and a row says "studio-mac" instead of an
    /// address, which is the difference between a feature and a manual.
    ///
    /// Off leaves the beacon as presence, a port and this boot's instance
    /// id. It is NOT a privacy control for identity, the beacon never
    /// carries a node id, a public key or any other key material, whichever
    /// way this is set.
    #[arg(long, value_enum)]
    pub announce_name: Option<Switch>,
}

#[derive(clap::Args)]
pub(crate) struct PeerNameArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The name to show. Omit to print the current one, which is the host
    /// name until you set something else. Refused if it carries an `@`, a
    /// uuid shape, or an organization name: a name reaches other machines.
    pub name: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct PeerPairArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// `host:port` of the Mac to pair with.
    pub addr: String,
    /// The six digits the other screen is showing. Omit to start the
    /// pairing and print this side's digits.
    pub code: Option<String>,
    /// Print one JSON object per line on stdout instead of prose, for a
    /// caller that is not a terminal.
    ///
    /// The pairing holds ONE live handshake across both phases and cannot
    /// be split into two invocations, so a panel has to stay attached to
    /// this process, read these lines and answer on its stdin. The lines
    /// are `crate::peer::pair::PairEvent` and `tests/peer/pairing/peer_pairing.rs`
    /// pins them.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerInviteArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// A name for the joining Mac. Refused if it carries an `@`, a uuid
    /// shape, or an organization name: a label reaches every trusted peer.
    #[arg(long)]
    pub label: Option<String>,
    /// How long the key stays usable, in seconds. Short on purpose, while
    /// it exists, anything that can read the peers file can use it.
    #[arg(long, default_value_t = 600)]
    pub ttl: u32,
    /// How many Macs may join with this one key.
    #[arg(long, default_value_t = 1)]
    pub uses: u8,
    /// Revoke an outstanding key by id instead of minting one.
    #[arg(long)]
    pub revoke: Option<u64>,
    /// Mint the old readable key (`tcr-join:v2:…`, addresses in plain
    /// text) instead of the opaque `tcr-join:v3:…` this build mints by
    /// default. For a script that greps a key out of stdout, and for a
    /// friend still on a build old enough to only read this one.
    #[arg(long, conflicts_with_all = ["sealed", "reply"])]
    pub plain: bool,
    /// Mint an ask instead of a key: a one-time public key that names no
    /// address and grants nothing, for the friend who does not want an
    /// address in the chat at all. Good for ten minutes; the friend
    /// answers with `tcr peer join --stdin`, and the reply is opened with
    /// `--reply --stdin`.
    #[arg(long, conflicts_with_all = ["plain", "revoke", "reply"])]
    pub sealed: bool,
    /// Open a reply to an ask this Mac minted, read from standard input,
    /// and print the address it names plus the `tcr peer pair` command
    /// that dials it. Requires `--stdin`.
    #[arg(long, requires = "stdin", conflicts_with_all = ["plain", "sealed", "revoke"])]
    pub reply: bool,
    /// Read the reply from standard input, one line, for `--reply`. The
    /// only source: a reply is a live answer to a live ask and does not
    /// belong in argv any more than a join key does.
    #[arg(long)]
    pub stdin: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerJoinArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The `tcr-join:…` key or the `tcr://peer/join?…` link the other
    /// Mac printed.
    ///
    /// **Typed here, the secret is visible in `ps` output to every process
    /// on this Mac and in the shell history.** Kept because it is the
    /// one-paste headless path this design is built around and a machine
    /// with no screen has no better channel; `--stdin` is the one that does
    /// not leak, and it is what the panel and the `tcr://` URL handler use.
    pub key: Option<String>,
    /// Read the key or link from standard input instead, one line,
    /// consumed by this process and never placed in an argument vector.
    ///
    /// This is the ONLY path the panel uses (its Paste-a-key sheet and its
    /// `tcr://` handler both pipe to it), because a secret in argv is a
    /// secret every other process on the Mac can read out of `ps`.
    #[arg(long, conflicts_with = "key")]
    pub stdin: bool,
    /// This Mac's name, as the other one will show it.
    #[arg(long)]
    pub label: Option<String>,
    /// Replace a network key this Mac already has.
    ///
    /// Without it, a link carrying a network key is refused when one is
    /// already set, the same refusal `tcr peer network-key join` gives and
    /// for the same reason: a second office's key pasted over the first is
    /// the commonest way a Mac disappears from its own mesh.
    #[arg(long)]
    pub replace: bool,
}

#[derive(clap::Args)]
pub(crate) struct PeerForgetArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The peer id to forget, as `tcr peer ls` prints it.
    pub peer: String,
}

#[derive(clap::Args)]
pub(crate) struct PeerHelloArgs {
    /// Path to the peers file (default: `tcr-peers.json` in the config directory).
    #[arg(long)]
    pub peers: Option<PathBuf>,
    /// The peer id to greet, in its full wire form, the `node` field of
    /// `tcr peer ls --json` (not the short `tcr-...` form `tcr peer ls`
    /// prints as text).
    pub peer: String,
}

#[derive(clap::Args)]
pub(crate) struct PeerAllowArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// The peer id to change, in its full wire form, the `node` field of
    /// `tcr peer ls --json`.
    ///
    /// NOT the `tcr-…` short form `tcr peer ls` prints as text:
    /// `PeerId::parse` refuses it deliberately, because a truncated id that
    /// silently resolved to a pinned peer would be a prefix-collision
    /// attack with a friendly face.
    pub peer: String,
    /// Which grant.
    #[arg(value_enum)]
    pub grant: PeerGrant,
    /// `on` or `off`.
    #[arg(value_enum)]
    pub state: Switch,
}

#[derive(clap::Args)]
pub(crate) struct PeerViaArgs {
    #[command(flatten)]
    pub peers: PeersFile,
    /// `auto` to use a trusted Mac whenever the direct path is dead, `off`
    /// to never route out through a peer, or a peer id to pin one.
    pub target: String,
    /// How long a carry may take to set up, in milliseconds. This only
    /// LOWERS the built-in bound (5000 ms): a carry runs after the direct
    /// path has already failed, so waiting longer than the default is worse
    /// for the caller than the answer it already has.
    #[arg(long)]
    pub setup_timeout_ms: Option<u64>,
}
