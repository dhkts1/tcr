//! Each account's upstream HTTP client: how it is built, and when it is retired and rebuilt.

use super::*;

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

/// Upstream sends after which an account's HTTP client is retired and rebuilt,
/// dropping its pooled h2 connection so the next send opens a fresh one.
///
/// Anthropic's edge eventually drains a long-lived h2 connection, and the
/// settings in [`build_serving_client`] — `http2_keep_alive_while_idle` plus a
/// 300s pool idle timeout — are deliberately chosen to keep ONE connection warm
/// forever, so nothing retired it. Measured over one day's live log (2026-08-27,
/// n=2136 `Reset(_, PROTOCOL_ERROR, Remote)` transport failures), the resets are
/// concentrated by connection age, not by concurrency:
///
/// | stream id at reset | share |
/// |---|---|
/// | 1-99 (fresh connection) | 6.8% |
/// | 100-999 | 21.5% |
/// | 1000-4999 | 5.9% |
/// | 5000-9999 | 21.9% |
/// | 10000+ | 43.9% |
///
/// Median 9699, max 17449 — roughly 8700 requests multiplexed onto one socket.
/// h2 client-initiated stream ids advance by 2, so N serves reaches stream id
/// ~2N: 500 keeps a connection under ~1000, below the p25 of the observed reset
/// distribution, while still amortising the handshake over 500 requests.
///
/// Retiring a connection is CHEAP and must not be confused with rotating an
/// account. It costs one TCP+TLS handshake (~100-300ms, the cost
/// [`build_serving_client`]'s keep-alive settings exist to avoid); it does NOT
/// cost a prompt-cache miss, because Anthropic's prompt cache is keyed on the
/// account and prefix server-side and has nothing to do with which socket the
/// bytes arrived on. Account rotation is the expensive one (measured 5-10x the
/// quota of a warm serve), and it is exactly what this constant exists to make
/// rarer.
pub(super) const MAX_SERVES_PER_CONNECTION: u32 = 500;

/// Why [`Manager::recycle_client`] is retiring one account's client, so its
/// log line names the cause instead of always claiming a spent serve budget.
/// A sleep retires every account at once and logs its own line
/// (`Manager::retire_every_client_after_sleep`), so it is not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecycleReason {
    /// The client carried [`MAX_SERVES_PER_CONNECTION`] sends.
    ServeBudget,
    /// An HTTP/2 stream reset left the pooled connection open and broken.
    StreamReset,
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

impl Manager {
    /// Give EVERY account a fresh HTTP client after the machine slept for
    /// `slept`, not just the one about to send: the sleep outlasted every
    /// pooled connection's idle life on the server side, and none of the pools
    /// can tell (see [`crate::wake`]). Each account's next send then opens a
    /// new connection instead of failing once on a dead one and retrying.
    ///
    /// Same rules as [`Self::recycle_client`]: in-flight requests keep their
    /// own `Arc` of the old client, and the replacements are built with no lock
    /// held. An account appended between the count and the swap already came
    /// with a fresh client of its own, so it is left alone. Each swapped
    /// account's serve-budget counter restarts, since it counts serves on the
    /// CURRENT client.
    ///
    /// The one INFO line per wake is here; nothing logs per request.
    pub(super) fn retire_every_client_after_sleep(&self, slept: std::time::Duration) {
        let count = self
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        let http1_only = self.http1_only();
        let fresh: Vec<_> = (0..count)
            .map(|_| build_serving_client(http1_only))
            .collect();
        let retired = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let mut retired = 0usize;
            for (account, http) in accounts.iter_mut().zip(fresh) {
                account.http = http;
                account.serves_since_client_build = 0;
                retired += 1;
            }
            retired
        };
        tracing::info!(
            slept_secs = slept.as_secs(),
            clients_retired = retired,
            "the machine slept; retiring every account's pooled upstream connection, \
             which the sleep most likely closed while the pool's idle timers stood still"
        );
    }

    /// Recycle account `idx`'s client after an HTTP/2 stream reset, before the
    /// proxy's same-account retry sends on it again.
    ///
    /// An h2 stream reset (`RST_STREAM`) fails one stream; the connection
    /// itself stays open and pooled, unlike a connection-level death, which
    /// reqwest evicts from the pool as part of raising the error. Left alone,
    /// every retry and every rotation would keep checking out that same
    /// broken connection, the live incident this exists to fix: 477
    /// stream-reset failures across every account over 11 minutes, cured only
    /// by a full process restart, because nothing in the ladder ever swapped
    /// the connection out. This is [`Self::recycle_client`] with
    /// [`RecycleReason::StreamReset`], so its one log line names the reset
    /// rather than a serve budget that was never spent.
    pub fn recycle_client_after_reset(&self, idx: usize) {
        self.recycle_client(idx, RecycleReason::StreamReset);
    }

    /// Retire account `idx`'s HTTP client and give it a fresh one, dropping the
    /// pooled h2 connection that has now carried [`MAX_SERVES_PER_CONNECTION`]
    /// sends before Anthropic's edge drains it out from under us.
    ///
    /// Requests already in flight are untouched. Each holds its own
    /// `Arc<reqwest::Client>` clone, taken by [`Self::http_client`] before the
    /// send, so the OLD client — and the connection carrying their streams —
    /// stays alive until the last of them finishes and drops the final `Arc`.
    /// Only sends selected after this point get the new pool. That is the whole
    /// reason this swaps an `Arc` instead of reaching into the existing client.
    ///
    /// The replacement is built with NEITHER lock held, matching
    /// [`Self::add_or_update_account`]'s Added path and for the same reason: a
    /// panic in [`build_serving_client`] (which tests inject) must not poison
    /// `self.accounts`.
    ///
    /// `reason` is the caller's, and picks the one log line this writes: the
    /// line used to claim a spent serve budget for every recycle, including
    /// the stream-reset one.
    pub(super) fn recycle_client(&self, idx: usize, reason: RecycleReason) {
        let fresh = build_serving_client(self.http1_only());
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(account) = accounts.get_mut(idx) else {
            // Stale index: accounts are appended, never removed, so this is not
            // reachable on the live path — same rationale as `http_client`.
            return;
        };
        account.http = fresh;
        match reason {
            RecycleReason::ServeBudget => tracing::info!(
                account_index = idx,
                account = %account.name,
                serves = MAX_SERVES_PER_CONNECTION,
                "retiring the upstream connection after its serve budget — \
                 a fresh h2 connection costs a handshake, not a prompt-cache miss"
            ),
            RecycleReason::StreamReset => tracing::info!(
                account_index = idx,
                account = %account.name,
                "retiring the upstream connection after an HTTP/2 stream reset, \
                 the connection survives the reset, so the retry needs a fresh one"
            ),
        }
    }
}
