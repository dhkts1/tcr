//! A Codex request is never offered to a fallback provider: the borrower's half
//! of "Codex is not lent over the mesh in this version". The lender's half, and
//! the shared predicate both halves ask, are in `tests/peer_lease.rs`
//! (`codex_paths_never_open_a_relay`, `codex_path_in_a_frame_is_refused_by_the_lender`).
//!
//! # Why this is its own binary
//!
//! The seam's refusal can only be seen with a provider that refuses NOTHING
//! installed, and the install is process-wide by design (`fallback::PROVIDER`).
//! `tests/peer_lease.rs` already holds that binary's one installer, and a
//! second in the same binary would race it. One test here, one install.
//!
//! # House rules
//!
//! No real account or token; the fleet is EMPTY, so nothing could be served
//! locally and nothing is refreshed. The proxy is driven in-process through
//! `proxy::app`, so no port is bound at all, least of all 3456.

#![expect(
    clippy::expect_used,
    reason = "test helpers outside #[test] fail the test with a message; the ban is for production code"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tcr::config::Config;
use tcr::fallback::{self, Ask, FallbackProvider};
use tcr::manager::Manager;
use tower::ServiceExt as _;

/// What the provider answers with, so the body the client reads proves WHERE
/// the answer came from rather than merely that one arrived.
const SENTINEL: &[u8] = b"{\"served_by\":\"the-test-provider\"}";

/// A provider that says yes to everything and counts what it was offered.
struct Counting {
    asked: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
}

impl FallbackProvider for Counting {
    fn name(&self) -> &'static str {
        "test-counting"
    }

    fn try_serve<'a>(
        &'a self,
        ask: &'a Ask<'a>,
    ) -> futures::future::BoxFuture<'a, Option<axum::response::Response>> {
        Box::pin(async move {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.paths
                .lock()
                .expect("paths lock")
                .push(ask.path.to_string());
            Some(
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(SENTINEL.to_vec()))
                    .expect("build the provider's answer"),
            )
        })
    }
}

/// A fleet with no account at all, so every request reaches the dry-fleet
/// terminal, where the seam decides whether to offer it.
fn dry_fleet() -> Arc<Manager> {
    let config: Config = serde_json::from_str(r#"{"upstream":"http://127.0.0.1:1","accounts":[]}"#)
        .expect("an empty fleet's config parses");
    Manager::with_live_refresher(config, None)
}

/// One POST through the real router, answered in full.
async fn post(path: &str, body: &'static str) -> (StatusCode, bytes::Bytes) {
    let response = tcr::proxy::app(dry_fleet())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", "Bearer client-own")
                .body(Body::from(body))
                .expect("a request"),
        )
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read the body");
    (status, bytes)
}

/// **The dry-fleet seam builds no `Ask` for a Codex request**, so even a
/// provider that would serve anything is never offered one. The Anthropic
/// request on the same dry fleet is the control: it IS offered, which is what
/// makes the zero below a decision rather than a seam that never runs.
///
/// Watch it fail by deleting the Codex arm of
/// `peer::serve::serve_is_allowed_for_path`: the Codex request is offered, and
/// the client reads the provider's sentinel.
#[tokio::test]
async fn a_codex_request_is_never_offered_to_a_fallback_provider() {
    let asked = Arc::new(AtomicUsize::new(0));
    let paths = Arc::new(Mutex::new(Vec::new()));
    assert!(
        fallback::install_provider(Box::new(Counting {
            asked: Arc::clone(&asked),
            paths: Arc::clone(&paths),
        })),
        "nothing else in this binary may install a provider"
    );

    let (status, body) = post(
        "/v1/messages",
        r#"{"model":"claude-sonnet-4-5","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.as_ref(),
        SENTINEL,
        "control: a dry fleet offers an Anthropic request to the provider"
    );

    let (status, body) = post(tcr::provider::CODEX_RESPONSES_PATH, "{}").await;
    assert_ne!(
        body.as_ref(),
        SENTINEL,
        "a Codex request was served through a fallback provider"
    );
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "it gets the dry fleet's own answer: {}",
        String::from_utf8_lossy(&body)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("a JSON body");
    // The fleet is empty, so it has no Codex account, and the dry fleet's own
    // Codex answer says exactly that as an immediate configuration error, rather than
    // reporting a usage limit that waiting could end.
    assert_eq!(parsed["error"]["type"], "no_codex_account");

    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the provider was offered the Anthropic request alone"
    );
    assert_eq!(
        paths.lock().expect("paths lock").as_slice(),
        ["/v1/messages".to_string()]
    );
}
