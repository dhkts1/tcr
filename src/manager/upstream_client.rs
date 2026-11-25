//! How each account's upstream HTTP client is built, and the test hook that makes the next build
//! panic. A leaf below both the account row that holds the client and the serving code that
//! retires and rebuilds it, so neither names the other for it.

use crate::manager::*;

// Test-only fault injection for `build_serving_client`: set via
// `fail_next_client_build`, consumed (and cleared) by the next call on this
// thread. Exists to prove — with a real panic, not a hypothetical one — two
// properties of `Manager::add_or_update_account`: its Added path builds the
// runtime with NEITHER `self.accounts` nor `config_write` held, so a panic
// here poisons neither lock — every other `.expect(...)` call site on either
// one in this module would otherwise go down with it — and its Updated path
// never reaches this function at all. See
// `add_or_update_account_added_path_does_not_poison_either_lock_on_a_client_build_panic`
// and `add_or_update_account_updated_path_never_builds_a_client`.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_CLIENT_BUILD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm [`FAIL_NEXT_CLIENT_BUILD`] so the next call to `build_serving_client` on
/// this thread panics instead of building. Test-only.
#[cfg(test)]
pub(crate) fn fail_next_client_build() {
    FAIL_NEXT_CLIENT_BUILD.with(|f| f.set(true));
}

/// Build one upstream-forwarding HTTP client. Called once per account (see
/// [`AccountRuntime::http`]) so every account's client is configured
/// identically and differs from every other account's only in the connection
/// pool it privately owns.
///
/// `http1_only` mirrors [`config::Config::http1_only`] — see its doc-comment
/// for why this exists and why it defaults to `false`. When `true`, every
/// stream this client opens is capped at HTTP/1.1, so the
/// `http2_keep_alive_*` settings below become inert (h1 has no PING frame to
/// send); they are left in place because they are still correct for the
/// default h2 path.
pub(crate) fn build_serving_client(http1_only: bool) -> Arc<reqwest::Client> {
    #[cfg(test)]
    assert!(
        !FAIL_NEXT_CLIENT_BUILD.with(|f| f.replace(false)),
        "build reqwest client (test-injected failure)"
    );
    // no_proxy(): reqwest honors HTTPS_PROXY/HTTP_PROXY by default. We ARE the
    // proxy — routing our upstream through an ambient proxy (e.g. one on
    // :3456) loops us back through a proxy and every request dies as
    // "upstream unreachable". Always reach Anthropic directly.
    let mut builder = crate::http_client::builder()
        .no_proxy()
        // Cap only the CONNECT phase. A blackholed route (no RST, no reply)
        // otherwise stalls the attempt until the OS TCP timeout, and with a
        // retry budget of `account_count * 2 + 4` that is many minutes of a
        // hung request. `oauth.rs` and `probe.rs` both already set one.
        //
        // DELIBERATELY NOT a total `.timeout(...)`, and do not add one: these
        // responses are long-lived SSE streams that legitimately run longer
        // than any bound worth setting, and a total timeout would truncate
        // them mid-stream. `connect_timeout` cannot — it applies only before
        // the response headers arrive, so once a stream is flowing it is out
        // of the picture.
        .connect_timeout(std::time::Duration::from_secs(10))
        // Keep the single HTTP/2 connection to Anthropic warm across
        // interactive think-time pauses. reqwest reaps idle connections
        // after 90s by default, but a coding session routinely pauses
        // longer — so the next request would pay a fresh TCP+TLS handshake
        // (~100-300ms). h2 keep-alive PINGs + a 5-min idle timeout hold the
        // connection open so a post-pause request skips the reconnect.
        .http2_keep_alive_interval(std::time::Duration::from_secs(30))
        .http2_keep_alive_while_idle(true)
        .pool_idle_timeout(std::time::Duration::from_secs(300));
    if http1_only {
        // Structural blast-radius cap: h1 does not multiplex, so a
        // connection-level fault (GOAWAY / framing error) kills at most the
        // one request on that connection instead of every stream sharing it.
        builder = builder.http1_only();
    }
    // Test builds only: a test whose upstream is a stub sends over the stubs' Unix socket instead
    // of opening a loopback port per request (`test_support::stub_upstream`, § The Unix socket).
    #[cfg(any(test, feature = "test-support"))]
    if let Some(socket) = crate::test_support::stub_upstream::client_socket() {
        builder = builder.unix_socket(socket);
    }
    // A client that cannot be built (the TLS backend failed to initialise) leaves
    // the process unable to make any upstream request, and this function has no
    // error channel: its callers are the constructor, `from_runtimes` and the
    // post-sleep rebuild.
    #[expect(
        clippy::expect_used,
        reason = "no upstream request is possible without a client, and the signature has no error channel"
    )]
    Arc::new(builder.build().expect("build reqwest client"))
}
