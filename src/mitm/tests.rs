//! Unit tests for the MITM listener, its TLS assets and the CONNECT handling in [`super`].

use super::*;
use crate::log_err::LogErr as _;

#[tokio::test]
async fn a_handoff_serves_already_queued_http_before_draining() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut client, server) = tokio::io::duplex(4096);
    client
        .write_all(b"GET /_tcr/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (_signal, drain) = tokio::sync::watch::channel(true);
    let serving = tokio::spawn(serve_http(
        server,
        "127.0.0.1:12345".parse().unwrap(),
        dummy_manager(),
        None,
        drain,
    ));
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    serving.await.unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "an already queued request must get HTTP 200 during drain, got {} bytes",
        response.len()
    );
}

/// `write_file`'s `mode` must be enforced when the destination ALREADY exists,
/// not only when it is created. `OpenOptionsExt::mode()` applies solely to the
/// creating open, so without a post-write `set_permissions` a pre-existing
/// `tcr-leaf.key` at `0644` — an older build, an interrupted write, a restore,
/// a umask accident — survives regeneration world-readable while the call site
/// reads as though it asked for `0600`. That file is the MITM leaf's private
/// key: anything that can read it can impersonate every intercepted host.
#[test]
fn write_file_enforces_mode_on_an_existing_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = std::env::temp_dir().join(format!(
        "tcr-mitm-mode-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tcr-leaf.key");

    // Pre-create the destination at a looser mode, as a stale artifact would be.
    std::fs::write(&path, b"stale").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    write_file(&path, b"fresh key material", 0o600).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "an overwritten private key must end up at 0600, got {mode:o}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"fresh key material");

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// A client names the minted CA when a minted chain is on disk, and nothing
/// when there is none, writing no certificate material either way.
#[test]
fn existing_client_ca_is_the_minted_one_or_none() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(existing_ca_path_in(dir.path()).unwrap(), None);
    assert!(
        !dir.path().join("tcr-ca.pem").exists(),
        "the lookup mints nothing"
    );
    let minted = load_tls_in(dir.path()).unwrap().ca_path;
    let minted_bytes = std::fs::read(&minted).unwrap();
    assert_eq!(
        existing_ca_path_in(dir.path()).unwrap(),
        Some(minted.clone())
    );
    assert_eq!(std::fs::read(minted).unwrap(), minted_bytes);
}

/// A minted chain must be reused on the next load. Re-minting silently
/// invalidates the `NODE_EXTRA_CA_CERTS` the user exported after the first
/// run, so the fallback path breaks on its second use.
#[test]
fn load_tls_reuses_a_minted_chain_across_restarts() {
    let dir = std::env::temp_dir().join(format!("tcr-mitm-reuse-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");

    let first = load_tls_in(&dir).expect("first load mints a chain");
    let advertised_ca = first.ca_path;
    let leaf_as_minted = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");

    let second = load_tls_in(&dir).expect("second load reuses the chain");
    let leaf_after_reload = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");

    assert_eq!(
        second.ca_path, advertised_ca,
        "the same CA must stay advertised across restarts"
    );
    assert_eq!(
        leaf_as_minted, leaf_after_reload,
        "the persisted leaf must be reused, not re-minted"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// What each [`load_tls_in`] branch's leaf lets the listener intercept is
/// [`ALLOWED_HOSTS`] ∩ its coverage: the fact a running proxy reports on its
/// status wire ([`crate::status::MitmStatus`]) and `tcr run --codex` pools
/// on, with the account rows. A minted leaf (C, then B, then D) lets it
/// intercept every policy host, `chatgpt.com` included (terminated only
/// while the pool holds a Codex account), and never `www.example.org`, which
/// it carries for the probe but [`handle_connect`] blind-tunnels.
#[test]
fn each_load_branch_reports_its_leaf_and_what_it_intercepts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let minted = crate::status::MitmStatus {
        intercepted_hosts: vec![
            "api.anthropic.com".to_string(),
            "platform.claude.com".to_string(),
            "chatgpt.com".to_string(),
        ],
        leaf: "minted".to_string(),
    };

    let fresh = load_tls_in(dir.path()).expect("C: mint a chain");
    assert_eq!(crate::status::MitmStatus::of(Some(&fresh)), minted, "C");

    let reused = load_tls_in(dir.path()).expect("B: reuse it");
    assert_eq!(crate::status::MitmStatus::of(Some(&reused)), minted, "B");

    write_sans_sidecar(&dir.path().join("tcr-leaf.sans"), &["api.anthropic.com"])
        .expect("drift the sidecar");
    let reminted = load_tls_in(dir.path()).expect("D: re-mint under the same CA");
    assert_eq!(crate::status::MitmStatus::of(Some(&reminted)), minted, "D");

    assert_eq!(
        crate::status::MitmStatus::of(None),
        crate::status::MitmStatus {
            intercepted_hosts: Vec::new(),
            leaf: "none".to_string(),
        },
        "no TLS material: MITM off"
    );
}

#[test]
fn parse_connect_accepts_host_port() {
    assert_eq!(
        parse_connect_target("CONNECT api.anthropic.com:443 HTTP/1.1"),
        Some(("api.anthropic.com".to_string(), 443))
    );
    // Method match is case-insensitive; trailing version optional.
    assert_eq!(
        parse_connect_target("connect example.com:8443"),
        Some(("example.com".to_string(), 8443))
    );
}

#[test]
fn parse_connect_rejects_malformed() {
    assert_eq!(parse_connect_target("GET / HTTP/1.1"), None);
    assert_eq!(parse_connect_target("CONNECT api.anthropic.com"), None); // no port
    assert_eq!(parse_connect_target("CONNECT :443 HTTP/1.1"), None); // no host
    assert_eq!(parse_connect_target("CONNECT host:notaport"), None);
    assert_eq!(parse_connect_target(""), None);
}

#[test]
fn host_allowlist_is_case_insensitive_and_closed() {
    assert!(host_allowed("api.anthropic.com"));
    assert!(host_allowed("API.Anthropic.COM"));
    // platform.claude.com replaced the two dead OAuth hosts below — see
    // ALLOWED_HOSTS's doc comment for the measured evidence.
    assert!(host_allowed("platform.claude.com"));
    assert!(host_allowed("Platform.Claude.COM"));
    // console.anthropic.com / platform.anthropic.com are dead in current
    // Claude Code (measured zero references across three releases) and must
    // no longer be policy-allowed.
    assert!(!host_allowed("console.anthropic.com"));
    assert!(!host_allowed("platform.anthropic.com"));
    // Everything else is NOT MITM-terminated — it is blind-tunneled instead
    // (host_allowed decides "decrypt + inject" vs "pass through", not allow
    // vs deny), so these all read false and take the tunnel path.
    assert!(!host_allowed("example.com"));
    assert!(!host_allowed("evil.api.anthropic.com.attacker.net"));
    assert!(!host_allowed("localhost"));
    assert!(!host_allowed(""));
}

/// `chatgpt.com`, Codex's inference and backend host, is policy-allowed
/// case-insensitively; a lookalike that merely contains the name is not.
/// `auth.openai.com`, where the Codex client refreshes its OWN token, is
/// not allowed either: that exchange has to reach the real host untouched,
/// so it must keep taking the blind tunnel.
#[test]
fn chatgpt_com_is_policy_allowed_and_lookalikes_are_not() {
    assert!(host_allowed("chatgpt.com"));
    assert!(host_allowed("CHATGPT.COM"));
    assert!(!host_allowed("evil.chatgpt.com.attacker.net"));
    assert!(
        !host_allowed("auth.openai.com"),
        "the client's own refresh stays a blind tunnel"
    );
    assert!(!host_allowed("api.openai.com"));
}

/// With no Codex account in the pool, `chatgpt.com` is blind-tunneled, so a codex that
/// reaches tcr runs on its own login instead of being answered `no_codex_account`; with one,
/// it is intercepted.
#[test]
fn an_unmarked_chatgpt_connect_is_tunneled_even_with_a_codex_pool() {
    assert!(!host_intercepted("chatgpt.com", 1, false));
}

#[test]
fn only_the_codex_proxy_authorization_marks_a_connect() {
    for header in [
        "Proxy-Authorization: Basic dGNyLWNvZGV4Og==",
        "proxy-authorization: basic dGNyLWNvZGV4Og==",
        // `tcr-codex:k3y`: a keyed Codex launch is still marked, so still pooled.
        "Proxy-Authorization: Basic dGNyLWNvZGV4OmszeQ==",
    ] {
        let head = format!("CONNECT chatgpt.com:443 HTTP/1.1\r\n{header}\r\n\r\n");
        assert!(ProxyCredential::from_connect_head(&head, None).codex_marked);
        assert!(host_intercepted(
            "chatgpt.com",
            1,
            ProxyCredential::from_connect_head(&head, None).codex_marked
        ));
    }
    for header in [
        "",
        "Authorization: Basic dGNyLWNvZGV4Og==",
        "Proxy-Authorization: Basic dGNyLWNvZGV4",
        "Proxy-Authorization: Bearer dGNyLWNvZGV4Og==",
        // `tcr:k3y`: keyed, but not the Codex username.
        "Proxy-Authorization: Basic dGNyOmszeQ==",
    ] {
        let head = format!("CONNECT chatgpt.com:443 HTTP/1.1\r\n{header}\r\n\r\n");
        assert!(
            !ProxyCredential::from_connect_head(&head, None).codex_marked,
            "{header}"
        );
        assert!(!host_intercepted(
            "chatgpt.com",
            1,
            ProxyCredential::from_connect_head(&head, None).codex_marked
        ));
    }
    assert!(codex_proxy_marked("http://tcr-codex@127.0.0.1:3999"));
    assert!(codex_proxy_marked("http://tcr-codex:@127.0.0.1:3999"));
    assert!(codex_proxy_marked("http://tcr-codex:k3y@127.0.0.1:3999"));
    assert!(!codex_proxy_marked("http://tcr:k3y@127.0.0.1:3999"));
    assert!(!codex_proxy_marked("http://127.0.0.1:3999"));
    assert!(!codex_proxy_marked("http://other@127.0.0.1:3999"));
}

#[test]
fn chatgpt_com_is_intercepted_only_while_the_pool_holds_a_codex_account() {
    for host in ["chatgpt.com", "CHATGPT.COM", "ChatGPT.com"] {
        assert!(
            !host_intercepted(host, 0, true),
            "{host}: no Codex account, so the tunnel must be blind"
        );
        assert!(host_intercepted(host, 1, true), "{host}: one Codex account");
        assert!(
            host_intercepted(host, 3, true),
            "{host}: three Codex accounts"
        );
    }
}

/// The Codex condition is `chatgpt.com`'s alone: the Anthropic hosts are intercepted whatever
/// the pool's Codex count, and a host off the policy list never is, whatever it is.
#[test]
fn only_chatgpt_com_waits_for_a_codex_account() {
    for (codex_accounts, marked) in [(0, false), (0, true), (1, false), (1, true)] {
        for host in ["api.anthropic.com", "platform.claude.com"] {
            assert!(
                host_intercepted(host, codex_accounts, marked),
                "{host} with {codex_accounts} Codex account(s)"
            );
        }
        for host in [
            "auth.openai.com",
            "api.openai.com",
            "evil.chatgpt.com.attacker.net",
            "chatgpt.com.",
            "example.com",
            "",
        ] {
            assert!(
                !host_intercepted(host, codex_accounts, marked),
                "{host:?} with {codex_accounts} Codex account(s)"
            );
        }
    }
}

/// The count [`handle_connect`] decides on is the pool's Codex accounts, read at each
/// CONNECT: a Claude-only pool blind-tunnels `chatgpt.com` and intercepts its own hosts, and
/// a Codex account added to the running pool, disabled even (an account counts in any
/// state), turns `chatgpt.com` on for the next tunnel.
#[test]
fn a_claude_only_pool_tunnels_chatgpt_com_until_a_codex_account_is_added() {
    let m = dummy_manager();
    assert!(
        !intercepts_now(&m, "chatgpt.com", true),
        "a Claude-only pool must blind-tunnel chatgpt.com"
    );
    assert!(intercepts_now(&m, "api.anthropic.com", true));

    m.add_account(crate::config::Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        access_token: "at-fake-codex".to_string(),
        refresh_token: Some("rt-fake-codex".to_string()),
        disabled: Some(true),
        ..crate::config::Account::fixture("bob@example.com/codex")
    });
    assert!(
        intercepts_now(&m, "chatgpt.com", true),
        "an imported account enables interception; a disabled pool gets an immediate configuration error"
    );
}

#[test]
fn unrelated_connects_never_wait_for_a_config_write() {
    for (host, marked, expected) in [
        ("example.com", false, false),
        ("api.anthropic.com", false, true),
        ("chatgpt.com", false, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{}").unwrap();
        let manager = dummy_manager_at(Some(path));
        let held = Arc::clone(&manager);
        let (ready, acquired) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            held.with_config_write_for_test(|| {
                ready.send(()).unwrap();
                released
                    .recv_timeout(Duration::from_secs(2))
                    .log_debug("waiting for the test's release signal");
            });
        });
        acquired.recv().unwrap();
        let start = std::time::Instant::now();
        assert_eq!(intercepts_now(&manager, host, marked), expected);
        let elapsed = start.elapsed();
        release
            .send(())
            .log_debug("releasing the test's config writer");
        holder.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "{host} waited {elapsed:?}"
        );
    }
}

/// [`FALLBACK_LEAF_SANS`] is "the policy allowlist + the probe host", so
/// every [`ALLOWED_HOSTS`] entry must be one of its SANs. A host that is
/// policy-allowed but absent from the leaf tcr mints (a typo such as
/// `chatgtp.com` in one of the two lists) is never terminated:
/// [`handle_connect`]'s coverage check blind-tunnels it, correctly and
/// silently, so nothing else would ever notice. Asked the way production
/// asks it: [`leaf_covers`] over the coverage branches B, C and D report.
#[test]
fn every_policy_host_is_covered_by_the_leaf_tcr_mints() {
    let covered = fallback_hosts_vec();
    for host in ALLOWED_HOSTS {
        assert!(
            leaf_covers(&covered, host),
            "{host} is policy-allowed but the leaf tcr mints would not cover it, so it \
                 would be blind-tunneled forever: {covered:?}"
        );
    }
}

/// The load-bearing property: `platform.claude.com` is policy-allowed AND
/// covered by a freshly-minted leaf (branch C, [`FALLBACK_LEAF_SANS`]), so
/// it is intercepted.
#[test]
fn platform_claude_com_is_intercepted_when_the_loaded_leaf_covers_it() {
    let dir = std::env::temp_dir().join(format!("tcr-mitm-covers-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");

    let assets = load_tls_in(&dir).expect("mints a fresh chain covering FALLBACK_LEAF_SANS");
    assert!(
        host_allowed("platform.claude.com"),
        "platform.claude.com must be policy-allowed"
    );
    assert!(
        leaf_covers(&assets.covered_hosts, "platform.claude.com"),
        "a freshly minted leaf must cover platform.claude.com: {:?}",
        assets.covered_hosts
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// The anti-outage property: a policy-allowed host the loaded leaf does not
/// cover must read as NOT covered, so `handle_connect` blind-tunnels it
/// instead of presenting a leaf that will fail the TLS handshake.
#[test]
fn platform_claude_com_is_not_intercepted_when_the_loaded_leaf_lacks_it() {
    let narrow = vec!["api.anthropic.com".to_string()];
    assert!(
        host_allowed("platform.claude.com"),
        "policy still allows it — the point is the SAN check catches what policy cannot"
    );
    assert!(
        !leaf_covers(&narrow, "platform.claude.com"),
        "a leaf must not claim coverage it does not have: {narrow:?}"
    );
    assert!(leaf_covers(&narrow, "API.anthropic.com"));
}

/// A persisted leaf whose sidecar SAN set does not match what we require
/// today (or has no sidecar at all — a leaf minted before this field
/// existed) must be re-minted, not served stale.
#[test]
fn a_stale_or_missing_sidecar_forces_a_remint() {
    let dir = std::env::temp_dir().join(format!("tcr-mitm-stale-sidecar-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");

    let first = load_tls_in(&dir).expect("first load mints a chain");
    let leaf_before = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");

    // Simulate a leaf minted under the OLD (pre-change) SAN set.
    write_sans_sidecar(
        &dir.join("tcr-leaf.sans"),
        &[
            "api.anthropic.com",
            "console.anthropic.com",
            "platform.anthropic.com",
            "www.example.org",
        ],
    )
    .expect("write stale sidecar");

    let second = load_tls_in(&dir).expect("second load re-mints on SAN drift");
    let leaf_after_drift = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
    assert_ne!(
        leaf_before, leaf_after_drift,
        "a SAN-drifted sidecar must trigger a fresh leaf, not a stale reuse"
    );
    assert!(leaf_covers(&second.covered_hosts, "platform.claude.com"));
    assert_eq!(
        first.ca_path, second.ca_path,
        "a leaf re-mint must not change which CA path is advertised"
    );

    // Now simulate a leaf minted before the sidecar field existed at all.
    std::fs::remove_file(dir.join("tcr-leaf.sans")).expect("remove sidecar");
    let third = load_tls_in(&dir).expect("third load re-mints on a missing sidecar");
    let leaf_after_missing = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
    assert_ne!(
        leaf_after_drift, leaf_after_missing,
        "a missing sidecar (pre-change leaf) must re-mint, not silently reuse unknown coverage"
    );
    assert!(leaf_covers(&third.covered_hosts, "platform.claude.com"));

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// The upgrade path for every existing install: a sidecar written before
/// `chatgpt.com` joined [`FALLBACK_LEAF_SANS`] must read as stale. Branch B
/// of [`load_tls_in`] reports `FALLBACK_LEAF_SANS` itself as the reused
/// leaf's coverage, so a pre-Codex sidecar that still matched would claim
/// `chatgpt.com` for a leaf that cannot present it, and once tcr intercepts
/// `chatgpt.com`, every Codex handshake it terminates would fail. The
/// mismatch is what re-mints the leaf instead.
#[test]
fn a_sidecar_from_before_codex_is_stale_and_forces_a_remint() {
    let before: Vec<String> = [
        "api.anthropic.com",
        "platform.claude.com",
        "www.example.org",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert!(
        !sans_match(&before, FALLBACK_LEAF_SANS),
        "the pre-Codex SAN set must read as stale"
    );
    let now: Vec<String> = FALLBACK_LEAF_SANS.iter().map(ToString::to_string).collect();
    assert!(sans_match(&now, FALLBACK_LEAF_SANS));
}

/// A persisted leaf whose sidecar MATCHES the required coverage is reused
/// as-is — no gratuitous re-mint (already covered by
/// `load_tls_reuses_a_minted_chain_across_restarts` above; this asserts
/// the sidecar specifically is what's being checked, not just presence).
#[test]
fn a_matching_sidecar_is_reused_without_a_remint() {
    let dir = std::env::temp_dir().join(format!("tcr-mitm-fresh-sidecar-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");

    let _first = load_tls_in(&dir).expect("first load mints a chain + matching sidecar");
    let leaf_before = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf persisted");
    let ca_key_before =
        std::fs::read(dir.join("tcr-ca.key")).expect("CA key persisted on first mint");

    let _second = load_tls_in(&dir).expect("second load reuses the chain");
    let leaf_after = std::fs::read(dir.join("tcr-leaf.pem")).expect("leaf still there");
    let ca_key_after = std::fs::read(dir.join("tcr-ca.key")).expect("CA key still there");

    assert_eq!(
        leaf_before, leaf_after,
        "a matching sidecar must not trigger a re-mint"
    );
    assert_eq!(
        ca_key_before, ca_key_after,
        "no CA rotation should happen when nothing drifted"
    );

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// Branch D end-to-end: after a SAN-drift re-mint, the CA cert stays byte-
/// identical (no rotation), AND a client trusting ONLY the persisted CA
/// completes a real TLS handshake against the newly re-minted leaf — the
/// strongest available proof the re-mint actually reused the persisted CA
/// key rather than silently minting a new CA.
#[tokio::test]
async fn remint_after_san_drift_reuses_the_persisted_ca_key() {
    let dir = std::env::temp_dir().join(format!("tcr-mitm-ca-reuse-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");

    let first = load_tls_in(&dir).expect("first load mints CA + leaf");
    let ca_pem_before = std::fs::read(dir.join("tcr-ca.pem")).expect("CA cert persisted");

    // Force a SAN drift so the next load must re-mint the leaf.
    write_sans_sidecar(&dir.join("tcr-leaf.sans"), &["api.anthropic.com"])
        .expect("write drifted sidecar");
    let second = load_tls_in(&dir).expect("second load re-mints under the SAME CA");
    let ca_pem_after = std::fs::read(dir.join("tcr-ca.pem")).expect("CA cert still there");

    assert_eq!(
        ca_pem_before, ca_pem_after,
        "a leaf re-mint must not rotate the CA cert"
    );
    assert_eq!(first.ca_path, second.ca_path);

    // Cryptographic proof: a client trusting ONLY the persisted CA must
    // complete a real TLS handshake against the re-minted leaf.
    ensure_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = io::BufReader::new(&ca_pem_after[..]);
    for cert in rustls_pemfile::certs(&mut reader) {
        roots
            .add(cert.expect("parse persisted CA cert"))
            .expect("add CA to root store");
    }
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    // The handshake is rustls against rustls; no TCP is needed to prove it, so the two ends
    // are an in-memory pipe.
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let acceptor = second.acceptor;
    let server = tokio::spawn(async move {
        acceptor
            .accept(server_stream)
            .await
            .expect("server-side handshake over the re-minted leaf must succeed")
    });

    let server_name = rustls::pki_types::ServerName::try_from("api.anthropic.com")
        .expect("valid DNS name")
        .to_owned();
    let client_tls = connector
        .connect(server_name, client_stream)
        .await
        .expect("a client trusting the persisted CA must accept the re-minted leaf");
    // Unlike a TCP socket, the pipe reports the peer's close at once, so the client end is
    // held open until the server's side of the handshake has finished writing.
    server.await.expect("server task must complete");
    drop(client_tls);

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// An echo upstream on an ephemeral loopback port: each connection echoes its first read
/// (up to 64 bytes) and closes. The one copy for every test that needs a real upstream
/// socket, because [`TunnelConnector`] dials a `TcpStream`.
async fn spawn_echo_upstream() -> SocketAddr {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    stream
                        .write_all(&buf[..n])
                        .await
                        .log_debug("echoing in a test upstream");
                }
            });
        }
    });
    addr
}

/// The blind tunnel pipes bytes to the requested upstream and back, and first
/// answers `200 Connection Established` — proving a non-allowlisted CONNECT
/// (e.g. platform.claude.com) passes straight through instead of being
/// refused, which is what Claude Code needs from the forward proxy.
#[tokio::test]
async fn tunnel_pipes_bytes_through_to_upstream() {
    // Upstream: the shared echo server.
    let echo_port = spawn_echo_upstream().await.port();

    // A front socket standing in for the client<->tcr connection.
    let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front_addr = front.local_addr().unwrap();
    let mut client = TcpStream::connect(front_addr).await.unwrap();
    let (server_side, _) = front.accept().await.unwrap();

    // Tunnel the accepted (tcr) side out to the echo upstream.
    tokio::spawn(async move {
        tunnel(
            server_side,
            "127.0.0.1",
            echo_port,
            &direct_tunnel_connector(),
        )
        .await
        .log_debug("tunnelling in a test");
    });

    // The tunnel first writes the CONNECT-OK line.
    let mut ok = [0u8; RESP_CONNECT_OK.len()];
    client.read_exact(&mut ok).await.unwrap();
    assert_eq!(&ok, RESP_CONNECT_OK);

    // Bytes we send are piped to the upstream and echoed straight back.
    client.write_all(b"ping").await.unwrap();
    let mut back = [0u8; 4];
    client.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"ping");
}

/// Cert load path (no network): mint a chain with the rcgen fallback, persist
/// it, and prove `build_acceptor` loads the leaf into a rustls `ServerConfig`.
#[test]
fn generated_chain_loads_into_acceptor() {
    ensure_crypto_provider();
    let (ca_pem, leaf_pem, leaf_key_pem, ca_key_pem) =
        generate_chain(FALLBACK_LEAF_SANS).expect("generate chain");
    assert!(ca_pem.contains("BEGIN CERTIFICATE"));
    assert!(leaf_pem.contains("BEGIN CERTIFICATE"));
    assert!(leaf_key_pem.contains("PRIVATE KEY"));
    assert!(ca_key_pem.contains("PRIVATE KEY"));

    let dir = std::env::temp_dir().join(format!("tcr-mitm-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert_path = dir.join("leaf.pem");
    let key_path = dir.join("leaf.key");
    write_file(&cert_path, leaf_pem.as_bytes(), 0o644).unwrap();
    write_file(&key_path, leaf_key_pem.as_bytes(), 0o600).unwrap();

    // A single leaf cert parses out, and the acceptor builds without error.
    assert_eq!(load_certs(&cert_path).unwrap().len(), 1);
    assert!(build_acceptor(&cert_path, &key_path).is_ok());

    std::fs::remove_dir_all(&dir).ok();
}

/// A `Manager` with one never-expiring dummy account and no proxy api-key,
/// so a loopback GET on `/_tcr/status` is answered locally with no upstream
/// call and no auth gate — the cheapest real route through `serve_http`'s
/// router to observe which HTTP version `serve_connection` negotiated.
fn dummy_manager() -> Arc<Manager> {
    dummy_manager_at(None)
}

fn dummy_manager_at(config_path: Option<PathBuf>) -> Arc<Manager> {
    dummy_manager_keyed(config_path, None)
}

/// [`dummy_manager_at`], holding `request_key` as `proxy.requestKey`.
fn dummy_manager_keyed(config_path: Option<PathBuf>, request_key: Option<&str>) -> Arc<Manager> {
    let config = crate::config::Config {
        proxy: crate::config::ProxyConfig {
            port: 0,
            api_key: None,
            request_key: request_key.map(str::to_string),
            extra: serde_json::Map::new(),
        },
        upstream: "http://127.0.0.1:1".to_string(),
        ..crate::config::Config::fixture(vec![crate::config::Account::fixture("dummy")])
    };
    Manager::new(
        config,
        Arc::new(crate::oauth::NoRefresh),
        Arc::new(crate::probe::LiveUsageProber::new()),
        Arc::new(crate::warmer::LiveWarmer::new()),
        config_path,
    )
}

/// Start a base-URL-mode listener (`handle_conn`'s non-CONNECT branch, i.e.
/// `serve_http` over a raw TCP stream — the exact path this change touches)
/// and return its address.
async fn spawn_base_url_listener() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let manager = dummy_manager();
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                break;
            };
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                // Same shape as the production accept loop in
                // `serve_with_shutdown`: a connection error is non-fatal but is
                // never swallowed silently, so a test that fails because the
                // connection died has the reason in the captured log rather
                // than presenting as an unexplained client-side error.
                if let Err(err) = handle_conn(
                    stream,
                    peer,
                    manager,
                    None,
                    direct_tunnel_connector(),
                    tokio::sync::watch::channel(false).1,
                )
                .await
                {
                    tracing::debug!(error = %err, "connection ended with error");
                }
            });
        }
    });
    addr
}

/// Log mode through the real listener: an unkeyed CONNECT is still answered 200 and
/// tunneled, and it bumps the unkeyed counter; a keyed one is answered the same way and
/// counts nothing. The connector dials a held loopback socket, never `example.com`.
#[tokio::test]
async fn an_unkeyed_connect_is_served_and_counted_and_a_keyed_one_is_not_counted() {
    const KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    let upstream_addr = spawn_echo_upstream().await;
    let connector: TunnelConnector =
        Arc::new(move |_host, _port| Box::pin(TcpStream::connect(upstream_addr)));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let manager = dummy_manager_keyed(None, Some(KEY));
    tokio::spawn(serve_with_tunnel_connector(
        listener,
        Arc::clone(&manager),
        None,
        connector,
    ));

    let connect = |auth: Option<String>| async move {
        let mut client = TcpStream::connect(addr).await.unwrap();
        let auth = auth.map_or_else(String::new, |a| format!("Proxy-Authorization: {a}\r\n"));
        let head = format!(
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\
                 User-Agent: curl/8.7.1\r\n{auth}\r\n"
        );
        client.write_all(head.as_bytes()).await.unwrap();
        let mut ok = [0u8; RESP_CONNECT_OK.len()];
        client.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, RESP_CONNECT_OK, "log mode answers 200 either way");
        client.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping", "and tunnels either way");
    };

    // `tcr:<KEY>`, standard base64.
    let keyed = "Basic dGNyOjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTE=";
    connect(Some(keyed.to_string())).await;
    assert!(
        manager.unkeyed().snapshot().is_empty(),
        "a keyed CONNECT counts nothing"
    );

    connect(None).await;
    connect(Some("Basic dGNyOndyb25n".to_string())).await; // `tcr:wrong`
    assert_eq!(
        manager.unkeyed().snapshot(),
        vec![crate::request_key::UnkeyedRow {
            what: Unkeyed::Connect(ConnectClass::Other),
            user_agent: crate::request_key::UaFamily::Curl,
            count: 2,
        }],
        "a missing and a wrong key both count, once each"
    );
}

/// Discriminating control: an ordinary h1 client must still work after the
/// swap to the auto-negotiating builder. If this regresses, the h2 test
/// below is measuring a broken server, not a real negotiation.
#[tokio::test]
async fn base_url_mode_still_serves_http1() {
    let addr = spawn_base_url_listener().await;
    let client = crate::http_client::builder().no_proxy().build().unwrap();
    let resp = client
        .get(format!("http://{addr}/_tcr/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
}

/// The behavior this change exists to add: a base-URL client that speaks
/// h2 via cleartext prior knowledge (no ALPN/TLS involved — this is the
/// plain-TCP `serve_http` call site at `handle_conn`'s non-CONNECT branch)
/// negotiates HTTP/2 instead of being forced onto HTTP/1.1.
#[tokio::test]
async fn base_url_mode_negotiates_http2_prior_knowledge() {
    let addr = spawn_base_url_listener().await;
    let client = crate::http_client::builder()
        .no_proxy()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{addr}/_tcr/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.version(), reqwest::Version::HTTP_2);
}

/// The CA notices name `CODEX_CA_CERTIFICATE` beside `NODE_EXTRA_CA_CERTS` once the config
/// holds a Codex account, since codex reads its extra CA from that variable alone. Without one
/// each reads as it always did.
#[test]
fn the_ca_export_hint_names_both_variables_with_a_codex_account() {
    let ca = Path::new("/Users/example/.config/tcr-ca.pem");
    assert_eq!(
        ca_export_hint(ca, false),
        "[tcr] minted a fresh MITM CA — trust it with:\n         \
             export NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem"
    );
    assert_eq!(
        ca_export_hint(ca, true),
        "[tcr] minted a fresh MITM CA — trust it with:\n         \
             export NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem\n\
             [tcr] for Codex, use `tcr run --codex` to scope proxy routing and CA trust to that launch"
    );
    assert_eq!(
        remint_note(false),
        "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS stays valid"
    );
    assert_eq!(
        remint_note(true),
        "MITM: re-minted leaf under the persisted CA — NODE_EXTRA_CA_CERTS and \
             CODEX_CA_CERTIFICATE stay valid"
    );
    assert_eq!(
        advertise_note(false),
        "MITM: advertise this CA via NODE_EXTRA_CA_CERTS"
    );
    assert_eq!(
        advertise_note(true),
        "MITM: advertise this CA via NODE_EXTRA_CA_CERTS and CODEX_CA_CERTIFICATE"
    );
}
