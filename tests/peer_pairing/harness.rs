// ---------------------------------------------------------------------------
// Scratch files, one set per test
// ---------------------------------------------------------------------------

use super::*;

/// A scratch directory named after this process, thread and a caller tag, so
/// two tests in this binary (and five lanes running at once), never collide on
/// one path.
pub(super) fn scratch(tag: &str) -> std::path::PathBuf {
    crate::peer_support::scratch_dir("pairing", tag)
}

/// A command's default files belong to its fixture, including the node key a
/// join loads separately from `--peers`. Reusing this home within one fixture
/// preserves its identity without sharing files with another test.
pub(super) fn tcr_command(home: &std::path::Path) -> std::process::Command {
    let config_dir = home.join(".config");
    let codex_home = home.join("codex");
    std::fs::create_dir_all(&config_dir).expect("create the fixture's config directory");
    std::fs::create_dir_all(&codex_home).expect("create the fixture's Codex directory");
    let default_config = config_dir.join("tcr.json");
    if !default_config.exists() {
        std::fs::write(&default_config, r#"{"proxy":{"port":9},"accounts":[]}"#)
            .expect("write the fixture's offline default config");
    }
    let mut command = crate::tcr_env::tcr(home);
    command
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("CODEX_HOME", &codex_home)
        .env("TCR_CODEX_AUTH", codex_home.join("auth.json"))
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("claude-credentials.json"),
        );
    command
}

/// One Mac's files: a peers file at 0600, a state file beside it, and a node
/// key in the same directory.
pub(super) struct Node {
    pub(super) dir: std::path::PathBuf,
    pub(super) peers: std::path::PathBuf,
    pub(super) state: std::path::PathBuf,
    pub(super) key: NodeKey,
}

impl Node {
    pub(super) fn new(tag: &str) -> Self {
        let dir = scratch(tag);
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        config::save(&peers, &PeerFile::default()).expect("write the peers file");
        let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
        Self {
            dir,
            peers,
            state,
            key,
        }
    }

    /// Read the peers file the way every production caller does.
    pub(super) fn file(&self) -> PeerFile {
        config::read_or_default(&self.peers).expect("the peers file reads")
    }

    pub(super) fn write_file(&self, file: &PeerFile) {
        config::save(&self.peers, file).expect("the peers file writes");
    }

    pub(super) fn state(&self) -> PeerState {
        state::load(&self.state, 0).expect("the state file reads")
    }

    pub(super) fn write_state(&self, value: &PeerState) {
        state::save(&self.state, value).expect("the state file writes");
    }

    pub(super) fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }
}

/// Bind `127.0.0.1:0`, start the SHIPPED accept loop on it, and report the
/// address the kernel chose.
///
/// `serve_on_with` and not a hand-rolled accept loop: the caps, the ban check,
/// the pattern dispatch and the knock queue all live in it, so a test harness
/// that re-implemented the loop would be measuring the harness.
pub(super) async fn serve(context: SessionContext) -> SocketAddr {
    crate::peer_support::serve_shipped(context).await
}

pub(super) const fn instance(byte: u8) -> InstanceId {
    InstanceId([byte; INSTANCE_ID_BYTES])
}

/// Send one knock over a fresh connection and report whether the far side
/// answered with the ack byte.
pub(super) async fn knock_at(
    addr: SocketAddr,
    id: InstanceId,
    name: Option<&str>,
) -> anyhow::Result<()> {
    knock_at_with_key(addr, id, name, None).await
}

pub(super) async fn knock_at_with_key(
    addr: SocketAddr,
    id: InstanceId,
    name: Option<&str>,
    network_key: Option<&[u8; 32]>,
) -> anyhow::Result<()> {
    knock_at_with(addr, id, name, network_key, None).await
}

/// The same knock, saying which port this side listens on: the number the far
/// side has to answer at, since the source port of this connection is
/// ephemeral. `None` is what a build from before the field sends.
pub(super) async fn knock_at_with_port(
    addr: SocketAddr,
    id: InstanceId,
    name: Option<&str>,
    listen_port: Option<u16>,
) -> anyhow::Result<()> {
    knock_at_with(addr, id, name, None, listen_port).await
}

pub(super) async fn knock_at_with(
    addr: SocketAddr,
    id: InstanceId,
    name: Option<&str>,
    network_key: Option<&[u8; 32]>,
    listen_port: Option<u16>,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    noise::send_knock(
        &mut stream,
        &Knock {
            instance_id: id,
            proposed_name: name.map(str::to_string),
            wire_version: tcr_peer_wire::PROTO_VERSION,
            listen_port,
        },
        network_key,
    )
    .await
}

/// Approve one pending knock the way `tcr peer accept` does, and report the
/// window.
pub(super) fn accept_pending(node: &Node, selector: &str) -> state::AcceptedInstance {
    let mut value = node.state();
    let window = value
        .accept_knock(selector, pair::now_ms(), pair::PAIRING_WINDOW_SECS)
        .expect("a pending row to accept");
    node.write_state(&value);
    window
}

/// A socket that counts what was WRITTEN to it, so "zero bytes in answer" is
/// asserted on the wire and not on an error type.
///
/// A listener that answered first and refused afterwards satisfies every
/// assertion about its `Err` and fails this one.
pub(super) async fn count_written(addr: SocketAddr, message_1: &[u8]) -> usize {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to the test listener");
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");

    let mut back = Vec::new();
    // Read until the far side closes. A refusal closes with nothing written, so
    // this returns 0; an answer is at least message 2.
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut back)).await;
    match read {
        Ok(Ok(_)) => back.len(),
        // A reset or a timeout is not an answer either, and the number that
        // matters is how many bytes came back.
        _ => back.len(),
    }
}

/// The bytes of a knock's `NN` message 1 (no payload: a knock's payload
/// rides the transport session message 2 establishes, not message 1 itself),
/// built with a throwaway ephemeral secret exactly as `noise::send_knock`
/// builds its own.
pub(super) fn knock_message_1() -> Vec<u8> {
    let throwaway = noise::random_secret().expect("a throwaway secret");
    let mut state = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("a knock initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("knock message 1");
    scratch[..len].to_vec()
}

/// The bytes of an `XX` message 1 carrying `id` in the versioned pairing
/// payload, built with a throwaway static key.
pub(super) fn xx_message_1(id: InstanceId) -> Vec<u8> {
    let (secret, _) = noise::generate_static().expect("a keypair");
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&noise::pair_message_1_payload(&id), &mut scratch)
        .expect("XX message 1");
    scratch[..len].to_vec()
}

/// Run `tcr peer …` against a temp peers file and return `(stdout, stderr,
/// success)`.
pub(super) fn run_tcr(peers: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let mut full = vec!["peer"];
    full.extend_from_slice(args);
    let output = tcr_command(
        &peers
            .parent()
            .expect("the peers fixture has a directory")
            .join("cli-home"),
    )
    .arg("peer")
    .args(args)
    .args(["--peers", peers.to_str().expect("a utf-8 path")])
    .output()
    .unwrap_or_else(|err| panic!("spawn tcr {full:?}: {err}"));
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}
