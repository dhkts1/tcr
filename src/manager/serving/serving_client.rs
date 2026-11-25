//! Each account's upstream HTTP client: when it is retired and rebuilt. How one is built,
//! [`build_serving_client`], is in `manager::upstream_client`.

use crate::manager::*;

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
pub(in crate::manager) const MAX_SERVES_PER_CONNECTION: u32 = 500;

/// Why [`Manager::recycle_client`] is retiring one account's client, so its
/// log line names the cause instead of always claiming a spent serve budget.
/// A sleep retires every account at once and logs its own line
/// (`Manager::retire_every_client_after_sleep`), so it is not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::manager) enum RecycleReason {
    /// The client carried [`MAX_SERVES_PER_CONNECTION`] sends.
    ServeBudget,
    /// An HTTP/2 stream reset left the pooled connection open and broken.
    StreamReset,
}

impl Manager {
    /// Give EVERY account a fresh HTTP client after the machine slept for
    /// `slept`, not just the one about to send: the sleep outlasted every
    /// pooled connection's idle life on the server side, and none of the pools
    /// can tell (see [`crate::clients::wake`]). Each account's next send then opens a
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
    pub(in crate::manager) fn retire_every_client_after_sleep(&self, slept: std::time::Duration) {
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
    pub(in crate::manager) fn recycle_client(&self, idx: usize, reason: RecycleReason) {
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
