//! The predecessor's side of a listening-socket handoff.
//!
//! A restart normally closes the listener and the successor binds a fresh one,
//! so the port is unbound in between and every new connection is refused. The
//! predecessor instead hands its socket to the successor, which keeps the
//! socket's refcount above zero and means a connection arriving mid-swap waits
//! in the kernel's accept queue instead. This module owns one step of that: the
//! ordered sequence a predecessor performs once a verified successor has
//! connected.
//!
//! The successor's side is [`crate::server::ServeOptions::inherited_listener`],
//! and the descriptor transport plus the peer check live in `tcr-fdpass`,
//! which exists so the `unsafe` those need stays out of this crate.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::manager::Manager;

/// The code-signing requirement a peer must satisfy to be handed the socket.
///
/// `SecCodeCheckValidity` verifies the signature is intact and the certificate
/// chain builds to the anchor before this is even consulted. What the string
/// adds is *which* Apple-issued certificate is acceptable, and each clause is
/// load-bearing:
///
/// * `anchor apple generic` — the chain roots to an Apple CA.
/// * `certificate 1[field.1.2.840.113635.100.6.2.6]` — the intermediate is the
///   **Developer ID** CA specifically.
/// * `certificate leaf[field.1.2.840.113635.100.6.1.13]` — the leaf is a
///   **Developer ID Application** certificate.
/// * `certificate leaf[subject.OU]` — issued to our team.
///
/// **The two marker OIDs are not decoration.** Without them,
/// `anchor apple generic and certificate leaf[subject.OU] = "<team>"` is
/// satisfied by *any* Apple-issued certificate carrying that OU, and an
/// **Apple Development** certificate carries the team's OU too. Anyone able to
/// sign with a development certificate for the team could then sign arbitrary
/// code and be handed the proxy's listening socket. These are the same clauses
/// Apple puts in a Developer ID binary's own designated requirement.
///
/// **Team-scoped, not bundle-scoped.** A CLI `tcr` handing over to a TcrBar, or
/// the reverse, is ordinary and supported, and those carry different
/// identifiers; pinning the identifier would refuse the common case.
///
/// # What this deliberately does not check
///
/// * **Revocation.** `kSecCSDefaultFlags` does not enforce OCSP/CRL.
///   `kSecCSEnforceRevocationChecks` would, at the cost of needing the network:
///   a handoff on a laptop offline would then fail. Refusing to restart cleanly
///   while offline is a worse day-to-day outcome than the narrow window this
///   closes, so it is off, and said out loud here rather than left to be
///   assumed either way.
/// * **Notarization.** A Developer ID build that was never notarized still
///   satisfies this. Notarization is a distribution check, and the peer here is
///   already a local process that got onto the machine somehow.
pub const PEER_REQUIREMENT: &str = concat!(
    "anchor apple generic",
    " and certificate 1[field.1.2.840.113635.100.6.2.6]",
    " and certificate leaf[field.1.2.840.113635.100.6.1.13]",
    r#" and certificate leaf[subject.OU] = "UJQ3GQF56Y""#,
);

/// Perform the handoff: final flush, release ownership, then send the socket.
///
/// # The order is the whole thing
///
/// Both neighbouring orderings are broken, in opposite directions:
///
/// * **Release before the flush** and the final pin and token write never
///   happens, because [`Manager::persist_now`] is gated on exactly that flag.
///   The successor then starts from a stale file, which is the cold-cache cost
///   this design exists to avoid.
/// * **Never release** and the predecessor clobbers the successor.
///   `ServerHandle::shutdown_within` ends by calling `persist_now`, so an
///   ungated predecessor writes its now-stale in-memory tokens over the
///   successor's fresh ones as its last act.
///
/// That gating is also what makes the order *testable*: reorder the release
/// above the flush and the file simply does not get written.
///
/// The receiver must wait for the caller's release confirmation before booting.
/// Send failure retains ownership; receipt or release failure after a successful
/// send is a committed handoff and drains through normal shutdown.
pub fn complete_handoff(
    manager: &Manager,
    affinity_path: Option<&Path>,
    listener: &std::net::TcpListener,
    peer: &UnixStream,
) -> io::Result<()> {
    // 1. The final write, made while this process is still the owner.
    if let (true, Some(path)) = (manager.session_affinity_enabled(), affinity_path)
        && let Err(err) = manager.flush_affinity(path)
    {
        // Not fatal. The pin file is a cache, so losing it costs the
        // successor a warm start and nothing else — and refusing the whole
        // handoff over a cache write would trade a one-second outage for a
        // guaranteed one.
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "could not flush session-affinity pins before handing over; the successor will start colder"
        );
    }
    manager.persist_now();

    // The CLI receiver waits for listener-released before starting. A failed
    // send therefore leaves both acceptance and mutation ownership intact.
    tcr_fdpass::send_listener(peer, listener)?;
    manager.release_mutation_ownership();
    tracing::info!("listening socket handed to the successor");
    Ok(())
}

/// Check that the connected peer is a build we published before giving it
/// anything.
///
/// Split from [`complete_handoff`] so the refusal is visible at the call site
/// rather than buried inside the thing that gives the socket away.
#[cfg(target_os = "macos")]
pub fn verify_peer(peer: &UnixStream) -> io::Result<()> {
    tcr_fdpass::peer::verify_peer(peer, PEER_REQUIREMENT)
}

/// Non-macOS builds have no code-signing story, so there is nothing to check
/// and the handoff is refused rather than silently unauthenticated.
#[cfg(not(target_os = "macos"))]
pub fn verify_peer(_peer: &UnixStream) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket handoff requires peer code-signature verification, which this platform does not provide",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_config() -> Config {
        serde_json::from_str(
            r#"{
                "proxy": { "port": 0 },
                "sessionAffinity": false,
                "controlAccount": "test-fixture-account",
                "accounts": [
                    { "name": "test-fixture-account", "accessToken": "not-a-real-token" }
                ]
            }"#,
        )
        .expect("the inline test config parses")
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tcr-handoff-{tag}-{}.json", std::process::id()));
        crate::log_err::LogErr::log_debug(std::fs::remove_file(&p), "removing a test temp file");
        p
    }

    /// THE ordering test, and the reason `persist_now` is gated on the release
    /// flag rather than the release being a comment in a runbook.
    ///
    /// The config path does not exist when this starts. `complete_handoff` must
    /// leave a file there, which can only happen if the flush ran while this
    /// process still owned mutation. Move the release above the flush and
    /// `persist_now` becomes a no-op, no file appears, and this fails.
    #[test]
    fn the_final_write_happens_before_ownership_is_released() {
        let path = scratch("order");
        assert!(
            !path.exists(),
            "precondition: the config file does not exist yet"
        );

        let manager = Manager::with_live_refresher(test_config(), Some(path.clone()));
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding a socket to hand over");
        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        complete_handoff(&manager, None, &listener, &ours).expect("the handoff must succeed");

        assert!(
            path.exists(),
            "no config was written: the release ran before the final flush, so \\
             persist_now was already gated off and the successor starts stale"
        );
        assert!(
            manager.mutation_is_released(),
            "ownership must be released by the end of a handoff"
        );

        // And the peer really did get a usable socket.
        let received =
            tcr_fdpass::recv_listener(&theirs).expect("receiving the handed-over socket");
        assert_eq!(
            received.local_addr().expect("addr"),
            listener.local_addr().expect("addr"),
            "the peer received a different socket than the one handed over"
        );

        crate::log_err::LogErr::log_debug(std::fs::remove_file(&path), "removing a test temp file");
    }

    /// The requirement string must PARSE, or every handoff fails at runtime
    /// with `InvalidInput` and the feature is silently dead.
    ///
    /// Asserting `PermissionDenied` is what proves it parsed: this test binary
    /// is ad-hoc signed, so a well-formed requirement reaches
    /// `SecCodeCheckValidity` and is refused there. A typo would come back as
    /// `InvalidInput` instead, from the parser, before any peer is considered.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_peer_requirement_parses_and_refuses_an_unsigned_peer() {
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");
        let err = verify_peer(&ours).expect_err("an ad-hoc signed peer must be refused");
        assert_eq!(
            err.kind(),
            io::ErrorKind::PermissionDenied,
            "expected a refusal from the signature check; InvalidInput would mean \
             PEER_REQUIREMENT does not parse and no handoff could ever succeed. Got: {err}"
        );
    }

    /// Regression guard on the two Developer ID marker OIDs.
    ///
    /// Dropping them leaves a requirement that still reads plausibly and still
    /// accepts our real builds, so nothing visibly breaks — while quietly also
    /// accepting anything signed with an **Apple Development** certificate for
    /// the same team. That is the failure this pins.
    #[test]
    fn the_peer_requirement_pins_developer_id_and_not_merely_the_team() {
        assert!(
            PEER_REQUIREMENT.contains("1.2.840.113635.100.6.2.6"),
            "lost the Developer ID CA marker: {PEER_REQUIREMENT}"
        );
        assert!(
            PEER_REQUIREMENT.contains("1.2.840.113635.100.6.1.13"),
            "lost the Developer ID Application leaf marker: {PEER_REQUIREMENT}"
        );
    }

    /// The root fan helper's requirements carry their own copy of the team
    /// clause (`tcr_fan::identity::TEAM`). One value, checked here rather than
    /// copied unwatched: a change to either breaks this test.
    #[test]
    fn handoff_and_fan_team_clause_match() {
        assert_eq!(
            PEER_REQUIREMENT,
            format!("anchor apple generic{}", tcr_fan::identity::TEAM)
        );
    }

    /// After a handoff this process is draining, not owning. A request that
    /// still lands on it must not rotate a token the successor is holding.
    #[tokio::test]
    async fn a_handed_over_proxy_will_not_refresh_afterwards() {
        let path = scratch("norefresh");
        let manager = Manager::with_live_refresher(test_config(), Some(path.clone()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding");
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");

        complete_handoff(&manager, None, &listener, &ours).expect("handoff");

        assert!(
            !manager.ensure_fresh_force(0).await,
            "a handed-over proxy applied a token refresh; the successor holds \\
             that same single-use token and its copy is now dead"
        );

        crate::log_err::LogErr::log_debug(std::fs::remove_file(&path), "removing a test temp file");
    }

    /// A non-macOS build refuses rather than proceeding unauthenticated. Stated
    /// as a test so the cfg cannot quietly invert.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_platform_without_code_signing_refuses_the_handoff() {
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");
        let err = verify_peer(&ours).expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
