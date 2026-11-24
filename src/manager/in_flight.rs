//! The RAII guards that count a request in flight on one account, and taking one.

use super::*;

/// Resets the keep-warm in-flight flag on drop, so a sweep that unwinds early
/// still clears the guard.
pub(super) struct WarmInFlightGuard<'a>(pub(super) &'a AtomicBool);

impl Drop for WarmInFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// RAII counter for a request currently being served on one account. Taken via
/// [`Manager::enter_in_flight`] when the proxy picks an account to forward to, it
/// decrements that account's `in_flight` on Drop — so EVERY exit path of a serve
/// attempt (success, 429/401 rotate, transport error, panic-unwind) releases the
/// slot and a counter can never leak and strand an account out of rotation.
/// Modeled on [`WarmInFlightGuard`]; the count lives under the accounts write-lock
/// (no second lock — preserves the no-lock-nesting discipline).
///
/// Holds an owned `Arc` of the manager's account table (not a borrow) so the guard is
/// `'static` and can be MOVED into a streamed response body — its Drop then fires at
/// stream completion rather than at handler return. The `Arc` shares the manager's own
/// table and its one lock; no second lock and no detached counter are introduced.
pub struct InFlightGuard {
    accounts: Arc<RwLock<Vec<AccountRuntime>>>,
    pub(super) idx: usize,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(self.idx) {
            // saturating_sub so a double-drop / underflow can never wrap to u32::MAX
            // and pin the account out forever.
            account.in_flight = account.in_flight.saturating_sub(1);
        }
    }
}

impl Manager {
    /// Mark account `idx` as serving one more in-flight request: bump its
    /// `in_flight` count and stamp `last_served_ms`, returning an [`InFlightGuard`]
    /// that decrements the count on Drop. The proxy takes this the moment it picks
    /// an account to forward to, so the guard's lifetime spans the whole serve
    /// attempt and releases the slot on every exit path (success, rotate, error,
    /// panic). For a terminal SSE (`text/event-stream`) serve the proxy MOVES the
    /// owned guard into the streamed body, so the decrement fires at stream
    /// completion — not at handler return, when axum has yet to poll the body.
    ///
    /// Also ages the account's HTTP client: this is the one call taken exactly
    /// once per upstream send, so it is where [`MAX_SERVES_PER_CONNECTION`] is
    /// counted and where a due recycle is claimed. The accounts write-lock is
    /// taken here and then AGAIN by [`Self::recycle_client`] — never nested, and
    /// never held across the client build. It used to be a single uncontested
    /// section; that is why the claim resets the counter rather than letting
    /// `recycle_client` re-read it, which would race between the two sections.
    ///
    /// It is also where a sleep is noticed, for the same reason: it runs once
    /// per send and BEFORE the proxy checks the send's client out with
    /// [`Self::http_client`], so the send that first observes the wake already
    /// goes out on a fresh pool. [`Self::http_client`] is a read accessor that
    /// tests call to compare `Arc`s; making it rebuild clients would turn a
    /// getter into a writer.
    pub fn enter_in_flight(self: &Arc<Self>, idx: usize) -> InFlightGuard {
        if let Some(slept) = self.wake.observe() {
            self.retire_every_client_after_sleep(slept);
        }
        self.enter_in_flight_at(idx, OffsetDateTime::now_utc())
    }

    /// Claim a send slot and stamp it on the caller's clock. Sleep observation
    /// belongs to the production wrapper; RAII and client recycling are shared.
    /// Replay-only entry; production sends use [`Self::enter_in_flight`] to observe sleep.
    #[doc(hidden)]
    pub fn enter_in_flight_at(self: &Arc<Self>, idx: usize, now: OffsetDateTime) -> InFlightGuard {
        let claimed_recycle = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match accounts.get_mut(idx) {
                Some(account) => {
                    account.in_flight = account.in_flight.saturating_add(1);
                    account.last_served_ms = odt_to_ms(now);
                    account.serves_since_client_build =
                        account.serves_since_client_build.saturating_add(1);
                    // Claim the recycle under the SAME write lock that observes the
                    // threshold, and reset the counter as the act of claiming it:
                    // that is what makes the claim exclusive, so two sends crossing
                    // the threshold concurrently cannot both rebuild — the loser
                    // reads 0 and returns false.
                    if account.serves_since_client_build >= MAX_SERVES_PER_CONNECTION {
                        account.serves_since_client_build = 0;
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };
        if claimed_recycle {
            self.recycle_client(idx, RecycleReason::ServeBudget);
        }
        InFlightGuard {
            accounts: Arc::clone(&self.accounts),
            idx,
        }
    }
}
