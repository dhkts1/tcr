//! `Manager` glue for the account-bound-token → minting account map — the live half of
//! [`crate::bound_tokens`], mirroring `pins.rs`'s split for session affinity.
//!
//! The rule this map serves: **a request whose history carries an account-bound token is served
//! by the account that minted that token while that account can hold it**, and — when the
//! history is bound but no token is known to this process — by the account the session is
//! PINNED to. An advisor result or server-side thread state is unusable off its minting
//! account, so a sibling answers the first request with a 400 or a 404.
//!
//! A thinking signature from a 5.5 model is not rejected, and what happens to it depends on the
//! model (measured 2026-10-10, replaying one block unchanged on another account; September's
//! logs counted 170 thinking 400s). Opus 5.5 keeps the block on any account. Sonnet 5.5 and Haiku 5.5 keep it on
//! another seat in the minting account's organization and, on an account in another
//! organization, drop it silently with a 200: no error, and the conversation's earlier reasoning
//! is gone. So a release prefers a same-organization seat for those two models
//! ([`Manager::outside_org_of`], `proxy::same_org_release`), and for every model the hold is also
//! what keeps the prompt cache warm.
//!
//! "Minted" widens after a move. A conversation released to another account and answered there
//! with a 2xx is re-bound to that account ([`Manager::rebind_served_history`]), so a hold names
//! the account that minted the history OR last served it after a move: the cache is warm where
//! the conversation last ran, not where it began. The one exception is a thinking block an
//! org-bound model dropped on an account in another organization; it stays where it was minted.
//!
//! "Can hold it" is [`Manager::bound_account_holds`], and it is deliberately narrower than "is
//! bound". Claude Code recovers from the advisor and thread-state rejections on its own, in one
//! extra round trip (read in the 2.1.277..2.1.280 binaries: `retry:advisor-strip`, and
//! `retry:tether-replay` on a 404 `thread_not_found`; `retry:thinking-signature-strip` is there
//! too, for the 400 that no 5.5 model sent in that measurement). So holding a conversation on its account
//! is worth a short wait, and never worth one that outlasts the cache: an account that is gone,
//! or out for longer than [`super::CACHE_WARM_HOLD_SECS`], releases the conversation to the
//! fleet. The cost of the other choice, measured 2026-09-22: a conversation bound to a
//! `rejected` account got a 429 on every turn, 6-11 a minute, until the operator abandoned the
//! session, because `/compact` keeps the thread id.
//!
//! Two responsibilities the store next door does not have, same as `pins.rs`:
//!
//! - **Lock discipline.** The map's mutex is never held while the accounts lock is taken. Every
//!   method takes one lock, copies what it needs, drops it, and only then takes the other.
//! - **Translating index ↔ identity.** The live map is positional, because the response path
//!   learns an index and must not pay the accounts lock per token; the file is not.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use time::OffsetDateTime;

use crate::bound_tokens::{
    self, BOUND_TOKEN_CAP, BOUND_TOKEN_TTL_MS, BoundTokenKind, LoadReport, StoredAccount,
};
use crate::config::ConfigError;
use crate::identity;

use super::{Manager, WarmWindow};

/// How long a session stays told that its held conversation is moving off one account (see
/// [`Manager::first_switch_warning`]). The client's own retry comes within seconds; this only
/// has to outlast a user who cancels the retry and sends the next turn by hand, without
/// silencing a warning that is due again much later.
pub(super) const SWITCH_WARNING_TTL_MS: i64 = 10 * 60 * 1000;

/// One token as the response path hands it over: its kind and the token itself, unhashed. The
/// hashing happens here, once, so no caller can forget it.
pub type MintedToken = (BoundTokenKind, String);

/// What the map holds for one token: the minting account's index, the kind, and the mint time
/// in ms.
type Minted = (usize, BoundTokenKind, i64);

/// The bound-token map, with its entries also kept in mint order so that bounding it costs the
/// entries it drops and not a pass over all of them.
///
/// Before this, every record walked the whole map for the TTL and, once the map was full,
/// copied every key into a list and sorted it to drop the oldest one or two. A busy proxy keeps
/// the map full (65,536 entries, measured 2026-10-15), so each response that minted a token
/// paid 4-6 ms for that sort, under the lock the request path reads through, and a live profile
/// put it at about half of the proxy's busy time. Here bounding pops from the oldest end.
///
/// Only these methods touch the two halves, which is what keeps an entry in `by_age` exactly
/// when it is in `by_hash`, at its current mint time.
#[derive(Debug, Default)]
pub(super) struct BoundTokenMap {
    by_hash: HashMap<Arc<str>, Minted>,
    /// (mint ms, hash), oldest first. Equal mint times fall to the hash's order; the sort this
    /// replaced left them to the map's iteration order, so no caller relied on one.
    by_age: BTreeSet<(i64, Arc<str>)>,
}

impl BoundTokenMap {
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub(super) fn get(&self, hash: &str) -> Option<Minted> {
        self.by_hash.get(hash).copied()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&str, Minted)> + '_ {
        self.by_hash.iter().map(|(hash, &minted)| (&**hash, minted))
    }

    /// Set `hash`'s entry, replacing an earlier mint of the same token.
    pub(super) fn insert(&mut self, hash: &str, value: Minted) {
        let key = match self.by_hash.get_key_value(hash) {
            Some((key, &(_, _, old_ms))) => {
                let key = Arc::clone(key);
                self.by_age.remove(&(old_ms, Arc::clone(&key)));
                key
            }
            None => Arc::from(hash),
        };
        self.by_age.insert((value.2, Arc::clone(&key)));
        self.by_hash.insert(key, value);
    }

    /// Set `hash`'s entry only when it has none, because the entry already here is fresher.
    pub(super) fn insert_if_absent(&mut self, hash: &str, value: Minted) {
        if !self.by_hash.contains_key(hash) {
            self.insert(hash, value);
        }
    }

    /// Drop every entry minted more than `ttl_ms` before `now_ms`, then the oldest until at most
    /// `cap` remain. The expired entries are always the oldest ones, so both are pops from the
    /// same end.
    pub(super) fn prune(&mut self, now_ms: i64, ttl_ms: i64, cap: usize) {
        while let Some(oldest_ms) = self.by_age.first().map(|&(minted, _)| minted) {
            if now_ms.saturating_sub(oldest_ms) <= ttl_ms && self.by_hash.len() <= cap {
                break;
            }
            let Some((_, hash)) = self.by_age.pop_first() else {
                break;
            };
            self.by_hash.remove(&hash);
        }
    }
}

/// How many independently locked parts [`ShardedBoundTokens`] splits the map into.
pub(super) const BOUND_TOKEN_SHARDS: usize = 16;

/// The shard a token hash lives in: the hash's first 32 bits when it is the hex digest
/// [`bound_tokens::hash_token`] makes (uniform, so the shards fill evenly), and std's hasher
/// over the whole string for anything else a restored file might hold.
fn shard_of(hash: &str, shards: usize) -> usize {
    use std::hash::{Hash, Hasher};
    let bits = hash
        .get(..8)
        .and_then(|head| u32::from_str_radix(head, 16).ok())
        .map_or_else(
            || {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                hash.hash(&mut hasher);
                hasher.finish()
            },
            u64::from,
        );
    (bits % shards as u64) as usize
}

/// The bound-token map split into [`BOUND_TOKEN_SHARDS`] parts by token hash, each a
/// [`BoundTokenMap`] behind its own mutex and bounded at its share of the cap.
///
/// The map used to sit behind one mutex, which every request's lookup and every response's
/// record took: under load it was the proxy's largest lock wait once the log file's lock was
/// gone (2026-10-16, 74 of 1,954 profile samples). A lookup or a record now locks only the part
/// its token hashes to, so two requests wait on each other only when their tokens land in the
/// same part.
///
/// Each part keeps #497's mint order, so bounding still costs the entries dropped and not a pass
/// over the rest. What changes is WHICH entry goes at the cap: the oldest in the part the new
/// token landed in, not the oldest in the whole map. The hash is uniform, so a part holds a
/// near-even slice of every age: at the cap the entry dropped is among the oldest few percent
/// of the map, and `eviction_at_the_cap_keeps_the_newest_nine_tenths` pins that every one of
/// the newest 90% survives. Total entries never pass [`BOUND_TOKEN_CAP`], because the parts'
/// caps add up to it. The TTL, too, is pruned per part, so a lookup also refuses an expired
/// entry itself ([`Self::get`]).
///
/// The other design measured was a `DashMap`, which keeps no order, bounded in batches: the
/// record that took it past the cap copied every mint time out and removed the oldest 1,024. It
/// evicted in exact order, but that batch is the whole-map pass #497 removed, only spread out:
/// with one thread recording it cost 7.4 to 22.5 µs a request against 3.0 µs here (three runs of
/// `bound_token_store_designs`, 2026-10-16, before it was removed with its dependency). Under
/// 4 to 16 threads the two were within noise of each other. 64 parts beat 16 under 8 or more
/// threads, but evict further from exact order (the oldest mint kept was 3,876 places early
/// against 16 parts' 1,393, of 65,536), and every part's share of the newest 90% sits closer to
/// its cap.
#[derive(Debug)]
pub(super) struct ShardedBoundTokens {
    shards: Box<[Mutex<BoundTokenMap>]>,
    shard_cap: usize,
}

impl Default for ShardedBoundTokens {
    fn default() -> Self {
        Self::with_shards(BOUND_TOKEN_SHARDS)
    }
}

impl ShardedBoundTokens {
    /// `shards` parts sharing [`BOUND_TOKEN_CAP`]. One part is the single-lock map this
    /// replaced, which is what the comparison bench runs it as.
    pub(super) fn with_shards(shards: usize) -> Self {
        let shards = shards.max(1);
        Self {
            shards: (0..shards).map(|_| Mutex::default()).collect(),
            shard_cap: BOUND_TOKEN_CAP / shards,
        }
    }

    fn shard(&self, hash: &str) -> std::sync::MutexGuard<'_, BoundTokenMap> {
        self.shards[shard_of(hash, self.shards.len())]
            .lock()
            .expect("bound tokens shard lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .expect("bound tokens shard lock poisoned")
                    .len()
            })
            .sum()
    }

    /// `hash`'s entry, unless it is past the TTL. A record prunes only its own part, so an
    /// expired entry can sit in a part no record has reached since; it is never honoured.
    pub(super) fn get(&self, hash: &str) -> Option<Minted> {
        let minted = self.shard(hash).get(hash)?;
        (crate::now_ms().saturating_sub(minted.2) <= BOUND_TOKEN_TTL_MS).then_some(minted)
    }

    /// Set `hash`'s entry and bound its part: anything past the TTL, then the oldest past the
    /// part's cap.
    pub(super) fn record(&self, hash: &str, value: Minted, now_ms: i64) {
        let mut shard = self.shard(hash);
        shard.insert(hash, value);
        shard.prune(now_ms, BOUND_TOKEN_TTL_MS, self.shard_cap);
    }

    /// Move `hash` to `value` only while it still names account `from`, checked and set under
    /// one lock: a mint that landed since the caller read the entry is fresher than this.
    pub(super) fn rebind(&self, hash: &str, from: usize, value: Minted, now_ms: i64) -> bool {
        let mut shard = self.shard(hash);
        if shard.get(hash).is_none_or(|(idx, _, _)| idx != from) {
            return false;
        }
        shard.insert(hash, value);
        shard.prune(now_ms, BOUND_TOKEN_TTL_MS, self.shard_cap);
        true
    }

    /// Set `hash`'s entry only when it has none, without bounding: restore's path, whose
    /// entries were bounded when they were written.
    pub(super) fn insert_if_absent(&self, hash: &str, value: Minted) {
        self.shard(hash).insert_if_absent(hash, value);
    }

    /// Set `hash`'s entry without bounding, for a test that needs an entry the record path
    /// would drop.
    #[cfg(test)]
    pub(super) fn insert_unbounded(&self, hash: &str, value: Minted) {
        self.shard(hash).insert(hash, value);
    }

    /// Every entry, one part locked at a time: a save's copy, not a single instant's.
    pub(super) fn entries(&self) -> Vec<(String, Minted)> {
        let mut out = Vec::new();
        for shard in &*self.shards {
            let shard = shard.lock().expect("bound tokens shard lock poisoned");
            out.extend(
                shard
                    .iter()
                    .map(|(hash, minted)| (hash.to_string(), minted)),
            );
        }
        out
    }
}

impl Manager {
    /// Flag the bound-token map as changed since the last flush — same relaxed-atomic debounce
    /// contract as [`Self::mark_affinity_dirty`].
    pub fn mark_bound_tokens_dirty(&self) {
        self.bound_tokens_dirty.store(true, Ordering::Relaxed);
        self.bound_tokens_wake.notify_one();
    }

    /// Resolves once the bound-token map has changed since the last time this resolved: the
    /// bound-token flusher's wake-up, as [`Self::affinity_changed`] is the pins'.
    pub async fn bound_tokens_changed(&self) {
        self.bound_tokens_wake.notified().await;
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take_bound_tokens_dirty(&self) -> bool {
        self.bound_tokens_dirty.swap(false, Ordering::Relaxed)
    }

    /// Remember that the account at `account_idx` minted these tokens.
    ///
    /// Called from both response paths in `proxy.rs`: the response is the only place a token
    /// can be learned, and the serving account is only known there.
    ///
    /// Bounding is done HERE rather than at save time, because the map has to stay bounded in
    /// memory too: anything past the TTL goes first (it can never be honoured again), and if
    /// that is not enough the OLDEST mints are evicted until the map is back under
    /// [`BOUND_TOKEN_CAP`]. Oldest, not newest: an evicted token stops pinning its conversation,
    /// and the conversations most likely to still be live are the ones minted most recently.
    /// [`BoundTokenMap::prune`] does both by popping from the oldest end, so a full map costs
    /// this call the entries it drops, not a sort of all of them.
    pub fn record_bound_tokens(&self, tokens: &[MintedToken], account_idx: usize, now_ms: i64) {
        if tokens.is_empty() {
            return;
        }
        for (kind, token) in tokens {
            self.bound_tokens.record(
                &bound_tokens::hash_token(token),
                (account_idx, *kind, now_ms),
                now_ms,
            );
        }
        self.mark_bound_tokens_dirty();
    }

    /// Re-bind the known tokens of a history that account `served_idx` just answered with a 2xx
    /// to that account, stamped `now_ms`: how many entries moved.
    ///
    /// A released conversation keeps running on the account it moved to, often without minting
    /// a token its history echoes back, so its newest known tokens still name the account it
    /// left, and [`Self::bound_token_account`] pulled the next full-history request back there
    /// once that account recovered, onto a cold cache. Measured 2026-10-15: 7 such holds wrote
    /// 2.6M cache tokens, 5 of them within the hour after the same conversation ran warm on the
    /// account it moved to.
    ///
    /// A 2xx on a request that carried a token proves the serving account reads it, with one
    /// exception this keeps in place: Sonnet 5.5 and Haiku 5.5 drop a thinking block minted in
    /// another organization and still answer 200 (this module's doc). So a thinking token stays
    /// on its minting account when `model` is org-bound and `served_idx` is not a sibling in its
    /// organization ([`Self::outside_org_of`], where an unknown organization counts as outside).
    pub fn rebind_served_history(
        &self,
        tokens: &[String],
        served_idx: usize,
        model: Option<&str>,
        now_ms: i64,
    ) -> usize {
        if tokens.is_empty() {
            return 0;
        }
        let hashes: Vec<String> = tokens.iter().map(|t| bound_tokens::hash_token(t)).collect();
        // Map lock first, copied out and dropped: the organization check below takes the
        // accounts lock, and the two are never held together (this module's doc).
        let elsewhere: Vec<(usize, Minted)> = hashes
            .iter()
            .enumerate()
            .filter_map(|(i, hash)| self.bound_tokens.get(hash).map(|minted| (i, minted)))
            .filter(|&(_, (idx, _, _))| idx != served_idx)
            .collect();
        if elsewhere.is_empty() {
            return 0;
        }
        let org_bound = model.is_some_and(crate::model::thinking_is_org_bound);
        let mut outside_org: HashMap<usize, bool> = HashMap::new();
        let mut moves: Vec<(usize, Minted)> = Vec::with_capacity(elsewhere.len());
        for (i, minted) in elsewhere {
            let (from, kind, _) = minted;
            let thinking = matches!(
                kind,
                BoundTokenKind::ThinkingSignature | BoundTokenKind::RedactedThinking
            );
            if org_bound && thinking {
                let outside = *outside_org
                    .entry(from)
                    .or_insert_with(|| self.outside_org_of(from).contains(&served_idx));
                if outside {
                    continue;
                }
            }
            moves.push((i, minted));
        }
        if moves.is_empty() {
            return 0;
        }
        let mut moved = 0;
        for (i, (from, kind, _)) in moves {
            // Only the entry read above: a mint that landed in between is fresher than this,
            // and the store checks and sets under one lock.
            if self
                .bound_tokens
                .rebind(&hashes[i], from, (served_idx, kind, now_ms), now_ms)
            {
                moved += 1;
            }
        }
        if moved > 0 {
            self.mark_bound_tokens_dirty();
            tracing::debug!(
                account_index = served_idx,
                moved,
                "re-bound a served history's tokens to the account that served it"
            );
        }
        moved
    }

    /// The account that minted any of `tokens`, if this process knows one: the account index,
    /// the kind that named it, and the newest mint's timestamp.
    ///
    /// A token this map has never seen (minted before this shipped, expired, or evicted)
    /// contributes nothing — which is what the caller's bound-history fallback is for.
    ///
    /// Tokens from one conversation name the same account in practice; when they disagree (one
    /// minted before this shipped and one after), the NEWEST mint wins and the caller says so
    /// once.
    pub fn bound_token_account(&self, tokens: &[String]) -> Option<(usize, BoundTokenKind)> {
        if tokens.is_empty() {
            return None;
        }
        tokens
            .iter()
            .filter_map(|token| self.bound_tokens.get(&bound_tokens::hash_token(token)))
            .max_by_key(|&(_, _, minted)| minted)
            .map(|(index, kind, _)| (index, kind))
    }

    /// Whether the account at `idx` can still hold a conversation whose history is bound to it:
    /// the one question that decides between holding the request there and releasing it to the
    /// fleet.
    ///
    /// Yes while the account is alive for every model class ([`Self::account_hard_ok`], which
    /// passes a hold that clears while the cache is warm) and for THIS request's class
    /// ([`Self::model_blocked`]). A short hold is waited out, or answered with that hold's own
    /// `retry-after`. The soft quota threshold is not consulted: an account over it still serves
    /// until the upstream says otherwise.
    ///
    /// No once the account is terminally gated (`disabled`, a dead login, a `rejected` unified
    /// status), parked, reserved away from this request, held for longer than the cache lives,
    /// or out of this model class's weekly bucket. None of those clears soon, and the client
    /// recovers from the one rejection a sibling gives (see this module's doc). An index that
    /// names no account is a no too: there is nothing to hold the request to.
    pub fn bound_account_holds(
        &self,
        idx: usize,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
        warm: WarmWindow,
    ) -> bool {
        let now_ms = super::odt_to_ms(now);
        let reserved = self.reserved_groups();
        let parked = self.parked_groups();
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        accounts.get(idx).is_some_and(|account| {
            Self::account_hard_ok(account, now_ms, group, &reserved, &parked, warm)
                // "Holds" means this account answers the next request itself and every
                // other account is benched for it, so a rejection that merely clears
                // while the cache is warm (which `account_hard_ok` lets through to keep a
                // PIN) must still answer no here: holding a conversation on a rejected
                // account is the 429-every-turn incident this module's doc describes.
                && Self::account_terminal_gate(account).is_none()
                && !Self::model_blocked(
                    account,
                    self.global_threshold,
                    self.fable_weekly_threshold,
                    now,
                    is_fable,
                )
        })
    }

    /// Serve a held request on the account its history is bound to, over the soft threshold if
    /// need be: `Some(idx)` when that account can answer this request now.
    ///
    /// [`Self::bound_account_holds`] keeps a conversation on its account without consulting the
    /// soft threshold, so the serve side has to agree with it. It did not: a held request on an
    /// account over the threshold fell through to [`Self::select_revalidation`], whose fallback
    /// lets one request through per 2 s across the whole fleet, and the turn that lost that race
    /// got an exhausted 429 naming the account's 5-hour window. Claude Code renders that as
    /// "You've hit your session limit" (2026-09-24: 3 of 3 such 429s came 0.05-1.4 s after
    /// another turn on the same account, while four other accounts sat at 0%).
    ///
    /// Served the way `select_revalidation` serves a session's pin: the hard gates for THIS
    /// request ([`Self::hard_ok`] with the request's own group, so a live hold of any length
    /// still refuses), no revalidation window, and the session's pin left alone. `None` when the
    /// account already failed this request (`tried`), cannot serve it now, or sits outside the
    /// group this request asked for strictly: a strict group never serves from outside itself,
    /// held history included.
    #[allow(clippy::too_many_arguments)]
    pub fn select_bound(
        &self,
        idx: usize,
        tried: &HashSet<usize>,
        now: OffsetDateTime,
        is_fable: bool,
        group: Option<&str>,
        strict_group: Option<&str>,
        warm: WarmWindow,
    ) -> Option<usize> {
        if tried.contains(&idx) {
            return None;
        }
        let provider = self
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)?
            .provider;
        let unsupported_exit = match provider {
            crate::provider::Provider::Anthropic => false,
            crate::provider::Provider::Codex => {
                self.codex_accounts_with_unsupported_exit().contains(&idx)
            }
        };
        if unsupported_exit {
            return None;
        }
        let now_ms = super::odt_to_ms(now);
        let reserved = self.reserved_groups();
        let parked = self.parked_groups();
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        let account = accounts.get_mut(idx)?;
        let outside_strict_group =
            strict_group.is_some_and(|g| !account.groups.iter().any(|carried| carried == g));
        if outside_strict_group
            || !Self::hard_ok(
                account,
                self.global_threshold,
                self.fable_weekly_threshold,
                now,
                now_ms,
                is_fable,
                group,
                &reserved,
                &parked,
                warm,
            )
        {
            return None;
        }
        let utilization = account.quota.max_utilization(now, is_fable);
        account.last_selected_seq = self.select_seq.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            account = %account.name,
            utilization,
            is_fable,
            "revalidation-serve (bound-honor): serving the account this conversation's history is bound to"
        );
        Some(idx)
    }

    /// Every account index that is NOT a same-organization sibling of `idx`: `idx` itself, and
    /// every account whose organization ([`identity::org_key_of`]) differs from `idx`'s or is
    /// unknown. Adding this set to a request's `tried` leaves only the siblings selectable, which
    /// is how a released conversation is steered to a seat where its thinking survives (see this
    /// module's doc).
    ///
    /// When `idx` names no account, or its organization is unknown, no account can be shown to
    /// share it, so the set is every index and nothing is left to select.
    pub fn outside_org_of(&self, idx: usize) -> HashSet<usize> {
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let org = accounts
            .get(idx)
            .and_then(|a| identity::org_key_of(a.org_uuid.as_deref(), a.org_name.as_deref()));
        accounts
            .iter()
            .enumerate()
            .filter(|&(i, a)| {
                i == idx
                    || org.is_none()
                    || identity::org_key_of(a.org_uuid.as_deref(), a.org_name.as_deref()) != org
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether `tokens` name more than one distinct account — the disagreement
    /// [`Self::bound_token_account`] resolves by taking the newest.
    pub fn bound_tokens_disagree(&self, tokens: &[String]) -> bool {
        let mut seen: Option<usize> = None;
        for token in tokens {
            let Some((index, _, _)) = self.bound_tokens.get(&bound_tokens::hash_token(token))
            else {
                continue;
            };
            match seen {
                None => seen = Some(index),
                Some(first) if first != index => return true,
                Some(_) => {}
            }
        }
        false
    }

    /// First time this session key is held to its pin by bound history, `true`; every later
    /// time, `false`.
    ///
    /// The hold itself is unconditional — this only decides whether it is worth a log line, so
    /// a long conversation does not write one per turn. Bounded by the same cap the token map
    /// uses, cleared wholesale when it fills: the worst case of forgetting is one extra line.
    pub fn first_bound_history_hold(&self, session_key: u64) -> bool {
        let mut seen = self
            .bound_history_logged
            .lock()
            .expect("bound history log set poisoned");
        if seen.len() >= BOUND_TOKEN_CAP {
            seen.clear();
        }
        seen.insert(session_key)
    }

    /// Whether to warn this session before its held conversation moves off account `idx`:
    /// `true` the first time within [`SWITCH_WARNING_TTL_MS`], `false` after that.
    ///
    /// The move itself is [`Self::bound_account_holds`] answering no. Before this existed the
    /// move was silent. Now the first request that would move gets one plain 429 that says so,
    /// and Claude Code retries a plain 429 on its own (measured 2026-09-24 against 2.1.281: two
    /// retries allowed, three requests seen), so the retry is the request that moves. Keyed by
    /// account as well as session, so a conversation that moves again later, off the account it
    /// moved to, is told again.
    ///
    /// Bounded like [`Self::first_bound_history_hold`]: stale entries go on every call, and the
    /// map is cleared wholesale if it still fills. The worst case of forgetting is one extra
    /// warning.
    pub fn first_switch_warning(&self, session_key: u64, idx: usize, now_ms: i64) -> bool {
        let mut warned = self
            .bound_switch_warned
            .lock()
            .expect("bound switch warned map poisoned");
        warned.retain(|_, &mut at| now_ms.saturating_sub(at) < SWITCH_WARNING_TTL_MS);
        if warned.len() >= BOUND_TOKEN_CAP {
            warned.clear();
        }
        match warned.entry((session_key, idx)) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(now_ms);
                true
            }
        }
    }

    /// Forget every switch warning, as [`SWITCH_WARNING_TTL_MS`] passing would: lets a test send
    /// a request that comes after the warning's life without waiting ten minutes for it.
    #[cfg(test)]
    pub(crate) fn expire_switch_warnings(&self) {
        self.bound_switch_warned
            .lock()
            .expect("bound switch warned map poisoned")
            .clear();
    }

    /// Whether this session was told, within [`SWITCH_WARNING_TTL_MS`], that its held
    /// conversation is moving off account `idx`: the read-only half of
    /// [`Self::first_switch_warning`].
    ///
    /// The request path reads it before [`Self::bound_account_holds`], so a warning is a
    /// commitment. The warning says the retry moves the conversation; an account whose hold
    /// crept back under the cache's life in the 3 s before the retry would otherwise hold it
    /// again and answer the retry with a 429 of its own (found in review, 2026-09-24).
    pub fn switch_warned(&self, session_key: u64, idx: usize, now_ms: i64) -> bool {
        self.bound_switch_warned
            .lock()
            .expect("bound switch warned map poisoned")
            .get(&(session_key, idx))
            .is_some_and(|&at| now_ms.saturating_sub(at) < SWITCH_WARNING_TTL_MS)
    }

    /// The map as a persistable [`bound_tokens::Snapshot`]: each live entry's index replaced by
    /// the identity of the account at that index, copied once per account rather than once per
    /// token.
    ///
    /// An entry whose index names no account is skipped rather than written: writing a
    /// placeholder identity is precisely the mis-resolution this design exists to prevent.
    pub fn bound_token_snapshot(&self) -> bound_tokens::Snapshot {
        let entries: Vec<(String, usize, BoundTokenKind, i64)> = self
            .bound_tokens
            .entries()
            .into_iter()
            .map(|(hash, (index, kind, minted))| (hash, index, kind, minted))
            .collect();
        let mut snapshot = bound_tokens::Snapshot::default();
        if entries.is_empty() {
            return snapshot;
        }
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let mut slots: Vec<Option<bound_tokens::AccountSlot>> = vec![None; accounts.len()];
        for (hash, index, kind, minted_at_ms) in entries {
            let (Some(account), Some(cell)) = (accounts.get(index), slots.get_mut(index)) else {
                continue;
            };
            let slot = *cell.get_or_insert_with(|| {
                snapshot.account_slot(StoredAccount {
                    name: account.name.clone(),
                    account_uuid: account.account_uuid.clone(),
                    org_uuid: account.org_uuid.clone(),
                    org_name: account.org_name.clone(),
                    provider: account.provider,
                })
            });
            snapshot.push(hash, kind, slot, minted_at_ms);
        }
        snapshot
    }

    /// Write the map to `path`, atomically. Returns how many tokens landed.
    ///
    /// The caller logs the failure and carries on: a proxy that cannot write this cache serves
    /// traffic exactly as it did before the file existed, and pays the rejected turns it was
    /// buying back.
    pub fn flush_bound_tokens(&self, path: &Path) -> Result<usize, ConfigError> {
        let prepared = bound_tokens::prepare(self.bound_token_snapshot())?;
        let content = prepared.content_hash()?;
        let count = prepared.write(path, crate::now_ms())?;
        self.bound_tokens_save
            .wrote(content, content, std::time::Instant::now());
        Ok(count)
    }

    /// The flusher's save: [`Self::flush_bound_tokens`] unless the map is what the last write
    /// put on disk. Every change here is one a restart needs, so nothing is deferred.
    pub fn flush_bound_tokens_if_changed(
        &self,
        path: &Path,
    ) -> Result<crate::save_gate::SaveOutcome, ConfigError> {
        let prepared = bound_tokens::prepare(self.bound_token_snapshot())?;
        let content = prepared.content_hash()?;
        let now = std::time::Instant::now();
        if let Some(skipped) = self.bound_tokens_save.skip(content, content, now) {
            return Ok(skipped);
        }
        let count = prepared.write(path, crate::now_ms())?;
        self.bound_tokens_save.wrote(content, content, now);
        Ok(crate::save_gate::SaveOutcome::Wrote(count))
    }

    /// Restore the map from `path`, resolving each stored identity against the accounts loaded
    /// THIS boot and dropping everything stale or not resolvable to exactly one account.
    ///
    /// Existing in-memory entries win, same as [`Self::restore_affinity`]: a token learned from
    /// a response served between boot and this call is fresher than anything on disk.
    pub fn restore_bound_tokens(&self, path: &Path, ttl_ms: i64) -> LoadReport {
        let candidates: Vec<crate::config::Account> = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            accounts
                .iter()
                .map(|a| {
                    identity::probe(
                        a.provider,
                        &a.name,
                        a.account_uuid.clone(),
                        a.org_uuid.clone(),
                        a.org_name.clone(),
                    )
                })
                .collect()
        };
        let report = bound_tokens::load(path, &candidates, crate::now_ms(), ttl_ms);
        for (hash, &value) in &report.tokens {
            self.bound_tokens.insert_if_absent(hash, value);
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::config::Account;
    use crate::manager::AccountRuntime;

    fn account(name: &str, uuid: &str) -> Account {
        let mut account = identity::probe(
            crate::provider::Provider::Anthropic,
            name,
            Some(uuid.to_string()),
            Some("org-1".into()),
            None,
        );
        account.access_token = format!("at-{name}");
        account.refresh_token = Some(format!("rt-{name}"));
        account.expires_at = Some(crate::now_ms() + 3_600_000);
        account.priority = Some(0);
        account
    }

    fn manager_over(accounts: &[Account]) -> Arc<Manager> {
        Manager::from_runtimes(
            accounts
                .iter()
                .map(|a| AccountRuntime::from_config(a, false))
                .collect(),
        )
    }

    fn tmp(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tcr-bound-tokens-manager-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(bound_tokens::FILE_NAME)
    }

    fn sig(token: &str) -> Vec<MintedToken> {
        vec![(BoundTokenKind::ThinkingSignature, token.to_string())]
    }

    fn tokens(list: &[&str]) -> Vec<String> {
        list.iter().map(std::string::ToString::to_string).collect()
    }

    /// The flusher's save writes nothing when the map is what it last wrote. The file is
    /// removed between the saves, so a write of any kind would bring it back. A new token is
    /// written.
    #[test]
    fn a_second_bound_token_save_with_no_change_writes_nothing() {
        use crate::save_gate::SaveOutcome;
        let path = tmp("unchanged");
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        manager.record_bound_tokens(&sig("sig_fake_first"), 0, now);
        assert_eq!(
            manager.flush_bound_tokens_if_changed(&path).expect("save"),
            SaveOutcome::Wrote(1)
        );
        std::fs::remove_file(&path).expect("remove the bound-token file");

        assert_eq!(
            manager.flush_bound_tokens_if_changed(&path).expect("save"),
            SaveOutcome::Unchanged
        );
        assert!(!path.exists(), "an unchanged save wrote the file");

        manager.record_bound_tokens(&sig("sig_fake_second"), 0, now);
        assert_eq!(
            manager.flush_bound_tokens_if_changed(&path).expect("save"),
            SaveOutcome::Wrote(2)
        );
        assert!(path.exists());
    }

    /// The whole claim of the map, at its own layer, for every kind.
    #[test]
    fn a_recorded_token_names_its_minting_account_whatever_its_kind() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        let now = crate::now_ms();
        manager.record_bound_tokens(
            &[
                (BoundTokenKind::ServerToolUse, "srvtoolu_fake_1".into()),
                (BoundTokenKind::ThinkingSignature, "sig_fake_1".into()),
                (BoundTokenKind::RedactedThinking, "data_fake_1".into()),
                (BoundTokenKind::MessageId, "msg_fake_1".into()),
            ],
            1,
            now,
        );

        for (token, kind) in [
            ("srvtoolu_fake_1", BoundTokenKind::ServerToolUse),
            ("sig_fake_1", BoundTokenKind::ThinkingSignature),
            ("data_fake_1", BoundTokenKind::RedactedThinking),
            ("msg_fake_1", BoundTokenKind::MessageId),
        ] {
            assert_eq!(
                manager.bound_token_account(&tokens(&[token])),
                Some((1, kind)),
                "{token} must name the account that minted it"
            );
        }
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_never_minted"])),
            None,
            "a token this process never minted must not pin anything"
        );
    }

    #[test]
    fn disagreeing_tokens_resolve_to_the_newest_mint() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        let now = crate::now_ms();
        manager.record_bound_tokens(&sig("sig_fake_old"), 0, now - 60_000);
        manager.record_bound_tokens(&sig("sig_fake_new"), 1, now);

        let both = tokens(&["sig_fake_old", "sig_fake_new"]);
        assert!(manager.bound_tokens_disagree(&both));
        assert_eq!(
            manager.bound_token_account(&both),
            Some((1, BoundTokenKind::ThinkingSignature))
        );
        assert!(!manager.bound_tokens_disagree(&tokens(&["sig_fake_new"])));
    }

    /// The map is bounded in memory at the raised cap, and it is the OLDEST mints that go.
    ///
    /// Twice the cap is recorded, oldest first, so every part of the map has had to evict. The
    /// map then holds exactly the cap, the oldest mint is gone, and every one of the newest 90%
    /// is still there: a part evicts its own oldest, not the map's, and this is the bound on
    /// what that costs (see [`ShardedBoundTokens`]).
    #[test]
    fn eviction_at_the_cap_keeps_the_newest_nine_tenths() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        let total = 2 * BOUND_TOKEN_CAP;
        for n in 0..total {
            manager.record_bound_tokens(
                &sig(&format!("sig_fake_{n}")),
                0,
                now - (total - n) as i64,
            );
        }
        assert_eq!(manager.bound_tokens.len(), BOUND_TOKEN_CAP);
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_0"])),
            None,
            "the oldest mint is evicted"
        );
        let keep_from = total - BOUND_TOKEN_CAP * 9 / 10;
        let lost: Vec<usize> = (keep_from..total)
            .filter(|n| {
                manager
                    .bound_token_account(&tokens(&[&format!("sig_fake_{n}")]))
                    .is_none()
            })
            .collect();
        assert!(
            lost.is_empty(),
            "{} of the newest 90% were evicted, first {:?}",
            lost.len(),
            lost.first()
        );
    }

    /// Eight threads record and look up at once, each its own tokens on its own account index
    /// plus one token set every thread records: each thread reads its own token back the moment
    /// it is recorded, and at the end the map holds exactly one entry per distinct token, each
    /// naming the thread that wrote it. A lost update (a record whose write is overwritten by a
    /// concurrent one) or a double count shows up as a missing token or a wrong total.
    ///
    /// The `tsan` CI job runs every test whose name starts `concurrent_lock_stress`.
    #[test]
    fn concurrent_lock_stress_bound_tokens() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 2_000;
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let own = format!("sig_fake_{t}_{i}");
                        manager.record_bound_tokens(&sig(&own), t, now);
                        manager.record_bound_tokens(&sig(&format!("sig_fake_shared_{i}")), t, now);
                        assert_eq!(
                            manager.bound_token_account(&[own]),
                            Some((t, BoundTokenKind::ThinkingSignature)),
                            "thread {t} lost its own record {i}"
                        );
                        let other = format!("sig_fake_{}_{i}", (t + 1) % THREADS);
                        if let Some((idx, _)) = manager.bound_token_account(&[other]) {
                            assert_eq!(idx, (t + 1) % THREADS);
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("a stress thread panicked");
        }
        assert_eq!(
            manager.bound_tokens.len(),
            THREADS * PER_THREAD + PER_THREAD
        );
        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                assert_eq!(
                    manager.bound_token_account(&tokens(&[&format!("sig_fake_{t}_{i}")])),
                    Some((t, BoundTokenKind::ThinkingSignature)),
                    "token {t}_{i} is missing or names the wrong account"
                );
            }
        }
        for i in 0..PER_THREAD {
            let shared = manager.bound_token_account(&tokens(&[&format!("sig_fake_shared_{i}")]));
            assert!(shared.is_some_and(|(idx, _)| idx < THREADS));
        }
    }

    /// The same eight threads on a map already at its cap with older mints: every record
    /// evicts, and at the end the map holds exactly the cap and every token the threads wrote,
    /// because each is newer than everything it could have pushed out.
    #[test]
    fn concurrent_lock_stress_bound_tokens_at_the_cap() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 1_000;
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        let fill: Vec<MintedToken> = (0..BOUND_TOKEN_CAP)
            .map(|n| (BoundTokenKind::MessageId, format!("msg_fake_old_{n}")))
            .collect();
        manager.record_bound_tokens(&fill, 0, now - 60_000);
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        manager.record_bound_tokens(&sig(&format!("sig_fake_{t}_{i}")), 1, now);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("a stress thread panicked");
        }
        assert_eq!(manager.bound_tokens.len(), BOUND_TOKEN_CAP);
        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                assert_eq!(
                    manager.bound_token_account(&tokens(&[&format!("sig_fake_{t}_{i}")])),
                    Some((1, BoundTokenKind::ThinkingSignature)),
                    "a fresh mint {t}_{i} was evicted ahead of an older one"
                );
            }
        }
    }

    #[test]
    fn an_entry_past_the_ttl_is_pruned_in_memory() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        manager.record_bound_tokens(
            &sig("sig_fake_ancient"),
            0,
            now - BOUND_TOKEN_TTL_MS - 1_000,
        );
        manager.record_bound_tokens(&sig("sig_fake_fresh"), 0, now);

        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_ancient"])),
            None
        );
        assert!(
            manager
                .bound_token_account(&tokens(&["sig_fake_fresh"]))
                .is_some()
        );
    }

    /// A Codex account's bound token comes back to the Codex row after a restart, beside an
    /// Anthropic row of the same name. Neither row carries a uuid, so the name fallback alone
    /// calls them one account and only the provider the record stored tells them apart. The
    /// Anthropic row's own token, written beside it with no provider key at all, comes back to
    /// the Anthropic row: a record written before the field existed reads as Anthropic.
    ///
    /// Watched red, twice: drop the provider guard from `identity::same_identity` and both
    /// tokens tie and are dropped as ambiguous; read the stored provider back as Anthropic and
    /// the Codex token lands on the Anthropic row.
    #[test]
    fn a_codex_bound_token_restores_to_the_codex_row_beside_a_same_named_anthropic_row() {
        let path = tmp("codex-beside-anthropic");
        let named = |provider| Account {
            provider,
            account_uuid: None,
            org_uuid: None,
            ..account("alice@example.com", "unused")
        };
        let anthropic = named(crate::provider::Provider::Anthropic);
        let codex = named(crate::provider::Provider::Codex);
        let now = crate::now_ms();

        let first = manager_over(&[anthropic.clone(), codex.clone()]);
        first.record_bound_tokens(&sig("sig_fake_anthropic"), 0, now);
        first.record_bound_tokens(&sig("sig_fake_codex"), 1, now);
        assert_eq!(first.flush_bound_tokens(&path).expect("flush"), 2);
        let raw = std::fs::read_to_string(&path).expect("read the token file");
        assert_eq!(
            raw.matches(r#""provider":"codex""#).count(),
            1,
            "the Codex record is written with its provider: {raw}"
        );
        assert_eq!(
            raw.matches("provider").count(),
            1,
            "and the Anthropic record with none, as before this field existed: {raw}"
        );

        // Next boot, the two rows in the other order.
        let second = manager_over(&[codex, anthropic]);
        let report = second.restore_bound_tokens(&path, BOUND_TOKEN_TTL_MS);
        assert_eq!(
            (report.degraded, report.unresolved, report.ambiguous),
            (None, 0, 0),
            "both tokens resolve to exactly one row"
        );
        assert_eq!(
            second.bound_token_account(&tokens(&["sig_fake_codex"])),
            Some((0, BoundTokenKind::ThinkingSignature)),
            "the Codex token follows the Codex row to position 0"
        );
        assert_eq!(
            second.bound_token_account(&tokens(&["sig_fake_anthropic"])),
            Some((1, BoundTokenKind::ThinkingSignature)),
            "the Anthropic token follows the Anthropic row to position 1"
        );
    }

    /// The persistence bridge's own half: indices out to identities on save, identities back to
    /// (possibly different) indices on load, kinds intact.
    #[test]
    fn tokens_round_trip_across_a_reordered_account_list() {
        let path = tmp("round-trip");
        let a = account("a@example.com", "uuid-a");
        let b = account("b@example.com", "uuid-b");

        let first = manager_over(&[a.clone(), b.clone()]);
        first.record_bound_tokens(&sig("sig_fake_1"), 1, crate::now_ms());
        assert_eq!(first.flush_bound_tokens(&path).expect("flush"), 1);

        let raw = std::fs::read_to_string(&path).expect("read the token file");
        assert!(raw.contains("uuid-b"), "the entry must name b: {raw}");
        assert!(!raw.contains("uuid-a"), "and nobody else: {raw}");
        assert!(
            !raw.contains("sig_fake_1"),
            "and never the signature itself: {raw}"
        );

        // Next boot: b first, a second.
        let second = manager_over(&[b, a]);
        let report = second.restore_bound_tokens(&path, BOUND_TOKEN_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(
            second.bound_token_account(&tokens(&["sig_fake_1"])),
            Some((0, BoundTokenKind::ThinkingSignature)),
            "the token must follow b to position 0"
        );
    }

    #[test]
    fn a_stale_entry_is_dropped_at_restore() {
        let path = tmp("expiry");
        let accounts = [account("a@example.com", "uuid-a")];
        let now = crate::now_ms();

        let first = manager_over(&accounts);
        first.record_bound_tokens(&sig("sig_fake_fresh"), 0, now - 60_000);
        // Seeded straight into the map: `record_bound_tokens` prunes anything already past the
        // TTL, and this test is about what LOAD does with a file that holds one.
        first.bound_tokens.insert_unbounded(
            &bound_tokens::hash_token("sig_fake_stale"),
            (
                0,
                BoundTokenKind::ThinkingSignature,
                now - BOUND_TOKEN_TTL_MS - 60_000,
            ),
        );
        assert_eq!(first.flush_bound_tokens(&path).expect("flush"), 2);

        let second = manager_over(&accounts);
        let report = second.restore_bound_tokens(&path, BOUND_TOKEN_TTL_MS);
        assert_eq!(report.expired, 1);
        assert!(
            second
                .bound_token_account(&tokens(&["sig_fake_fresh"]))
                .is_some()
        );
        assert_eq!(
            second.bound_token_account(&tokens(&["sig_fake_stale"])),
            None
        );
    }

    #[test]
    fn the_dirty_flag_is_raised_by_a_record_and_consumed_by_one_take() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        assert!(!manager.take_bound_tokens_dirty());
        manager.record_bound_tokens(&sig("sig_fake_1"), 0, crate::now_ms());
        assert!(manager.take_bound_tokens_dirty());
        assert!(!manager.take_bound_tokens_dirty());
    }

    /// A 5h rejection that rolls while the cache is warm keeps a PIN
    /// (`account_hard_ok`), but it never HOLDS a conversation: holding benches every
    /// other account and sends the next request to the rejected one, which is the
    /// 429-every-turn incident this module's doc describes. Watched red with the
    /// terminal check removed from `bound_account_holds`.
    #[test]
    fn a_rejection_rolling_while_warm_still_does_not_hold_the_conversation() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = OffsetDateTime::now_utc();
        assert!(
            manager.bound_account_holds(0, now, false, None, WarmWindow::DEFAULT),
            "a healthy account holds its conversation"
        );
        {
            let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
            accounts[0].quota.status = Some("rejected".to_string());
            // Utilization UNDER every threshold on purpose: headers go stale by
            // minutes, so a rejection can arrive while the stored window still reads
            // half full. At 1.0 the model-class gate would refuse the hold on its own
            // and this test would pass without the terminal check it exists to pin.
            accounts[0].quota.five_hour = Some(crate::quota::QuotaWindow {
                utilization: 0.5,
                reset: Some(now + time::Duration::minutes(4)),
            });
        }
        {
            // Positive control for the premise: the PIN predicate does let this
            // account through, so the refusal below is the terminal check's alone.
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            assert!(
                Manager::account_hard_ok(
                    &accounts[0],
                    super::super::odt_to_ms(now),
                    None,
                    &HashSet::new(),
                    &HashSet::new(),
                    WarmWindow::DEFAULT,
                ),
                "a rejection rolling in 4 minutes keeps a pin"
            );
        }
        assert!(
            !manager.bound_account_holds(0, now, false, None, WarmWindow::DEFAULT),
            "rejected with a reset 4 minutes out: the pin may survive, the hold may not"
        );
    }

    /// Three accounts: 0 and 1 share an organization, 2 sits in another.
    fn two_orgs() -> Arc<Manager> {
        let mut outsider = account("c@example.com", "uuid-c");
        outsider.org_uuid = Some("org-2".into());
        manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
            outsider,
        ])
    }

    /// A 2xx on another account proves that account reads the history, so the history now
    /// names it, and the next request carrying it is held there rather than back where it began.
    #[test]
    fn a_served_history_moves_to_the_account_that_served_it() {
        let manager = two_orgs();
        let now = crate::now_ms();
        manager.record_bound_tokens(
            &[
                (BoundTokenKind::ThinkingSignature, "sig_fake_1".into()),
                (BoundTokenKind::MessageId, "msg_fake_1".into()),
            ],
            0,
            now - 60_000,
        );
        let history = tokens(&["sig_fake_1", "msg_fake_1"]);
        assert_eq!(
            manager.rebind_served_history(&history, 2, Some("claude-opus-5-5"), now),
            2,
            "Opus keeps thinking across organizations, so both tokens move"
        );
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_1"])),
            Some((2, BoundTokenKind::ThinkingSignature)),
            "the signature names the serving account, kind intact"
        );
        assert!(!manager.bound_tokens_disagree(&history));
        assert!(manager.take_bound_tokens_dirty(), "a move is saved");
    }

    /// Sonnet 5.5 dropped the block on an account in another organization and still answered
    /// 200, so that 200 proves nothing about the signature: it stays on its minting account.
    #[test]
    fn an_org_bound_thinking_signature_does_not_move_across_organizations() {
        let manager = two_orgs();
        let now = crate::now_ms();
        manager.record_bound_tokens(&sig("sig_fake_1"), 0, now - 60_000);
        assert_eq!(
            manager.rebind_served_history(
                &tokens(&["sig_fake_1"]),
                2,
                Some("claude-sonnet-5-5"),
                now
            ),
            0
        );
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_1"])),
            Some((0, BoundTokenKind::ThinkingSignature)),
            "the signature still names the account that minted it"
        );
    }

    /// The same model inside the minting account's organization keeps the block, so it moves.
    #[test]
    fn an_org_bound_thinking_signature_moves_within_its_organization() {
        let manager = two_orgs();
        let now = crate::now_ms();
        manager.record_bound_tokens(&sig("sig_fake_1"), 0, now - 60_000);
        assert_eq!(
            manager.rebind_served_history(
                &tokens(&["sig_fake_1"]),
                1,
                Some("claude-sonnet-5-5"),
                now
            ),
            1
        );
        assert_eq!(
            manager.bound_token_account(&tokens(&["sig_fake_1"])),
            Some((1, BoundTokenKind::ThinkingSignature))
        );
    }

    /// A history already on the serving account is left alone: nothing moved, nothing to save,
    /// and its mint time is not refreshed.
    #[test]
    fn a_history_already_on_the_serving_account_is_untouched() {
        let manager = two_orgs();
        let now = crate::now_ms();
        manager.record_bound_tokens(&sig("sig_fake_1"), 1, now - 60_000);
        assert!(manager.take_bound_tokens_dirty());
        assert_eq!(
            manager.rebind_served_history(
                &tokens(&["sig_fake_1"]),
                1,
                Some("claude-opus-5-5"),
                now
            ),
            0
        );
        assert!(
            !manager.take_bound_tokens_dirty(),
            "nothing moved, nothing to save"
        );
        let minted_at = manager
            .bound_tokens
            .get(&bound_tokens::hash_token("sig_fake_1"))
            .map(|(_, _, ms)| ms);
        assert_eq!(minted_at, Some(now - 60_000));
    }

    /// The log gate says yes once per session and no afterwards — a conversation held for a
    /// hundred turns is one line, not a hundred.
    #[test]
    fn a_bound_history_hold_is_logged_once_per_session() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        assert!(manager.first_bound_history_hold(11));
        assert!(!manager.first_bound_history_hold(11));
        assert!(manager.first_bound_history_hold(22));
    }

    /// `select_bound` serves the held account only where this request may go. Its strict-group
    /// check is the only thing keeping a group-scoped request inside its group on this path,
    /// because `bound_account_holds` passes an account outside a strict group (found in review,
    /// 2026-09-24).
    #[test]
    fn select_bound_serves_the_held_account_only_where_this_request_may_go() {
        let mut member = account("a@example.com", "uuid-a");
        member.groups = Some(vec!["g".to_string()]);
        let manager = manager_over(&[member]);
        let now = OffsetDateTime::now_utc();
        let untried = HashSet::new();
        assert_eq!(
            manager.select_bound(
                0,
                &untried,
                now,
                false,
                Some("g"),
                Some("g"),
                WarmWindow::DEFAULT
            ),
            Some(0),
            "a member of the strict group it asked for"
        );
        assert_eq!(
            manager.select_bound(
                0,
                &untried,
                now,
                false,
                Some("other"),
                Some("other"),
                WarmWindow::DEFAULT
            ),
            None,
            "outside the strict group it asked for"
        );
        assert_eq!(
            manager.select_bound(
                0,
                &HashSet::from([0]),
                now,
                false,
                None,
                None,
                WarmWindow::DEFAULT
            ),
            None,
            "already failed this request"
        );
        assert_eq!(
            manager.select_bound(1, &untried, now, false, None, None, WarmWindow::DEFAULT),
            None,
            "no such account"
        );
        manager.mark_rate_limited(0, 60);
        assert_eq!(
            manager.select_bound(0, &untried, now, false, None, None, WarmWindow::DEFAULT),
            None,
            "on a live hold"
        );
    }

    /// One warning per session per account, so the client's retry of the warning moves; told
    /// again when the conversation later moves off a different account, or once the warning's
    /// life has run out.
    #[test]
    fn a_switch_is_warned_once_per_session_and_account() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let t0 = 1_000_000;
        assert!(
            manager.first_switch_warning(11, 0, t0),
            "first request: warn"
        );
        assert!(
            !manager.first_switch_warning(11, 0, t0 + 3_000),
            "its retry: move"
        );
        assert!(
            manager.first_switch_warning(11, 1, t0 + 3_000),
            "another account: warn"
        );
        assert!(
            manager.first_switch_warning(22, 0, t0 + 3_000),
            "another session: warn"
        );
        assert!(
            manager.first_switch_warning(11, 0, t0 + SWITCH_WARNING_TTL_MS),
            "past the warning's life: warn again"
        );
    }

    /// The mint order changes what bounding costs, never what it keeps. Replays a mixed sequence
    /// (re-mints of one token, mint times out of order, some past the TTL) through the map and
    /// through the prune it replaced, a full TTL retain and then a sort of everything, and
    /// requires the same entries after every step. Equal mint times are broken by hash in the
    /// old prune here so that it has one answer.
    #[test]
    fn pruning_by_mint_order_keeps_what_the_full_sort_kept() {
        const CAP: usize = 40;
        const TTL: i64 = 1_000;
        let mut map = BoundTokenMap::default();
        let mut old: HashMap<String, Minted> = HashMap::new();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut cap_evictions, mut ttl_drops) = (0usize, 0usize);
        let mut now: i64 = 10_000;
        for step in 0..5_000usize {
            now += (next() % 7) as i64;
            for _ in 0..=(next() % 3) {
                let hash = bound_tokens::hash_token(&format!("tok_fake_{}", next() % 120));
                let value = (
                    step % 3,
                    BoundTokenKind::MessageId,
                    now - (next() % 1_500) as i64,
                );
                map.insert(&hash, value);
                old.insert(hash, value);
            }
            map.prune(now, TTL, CAP);

            let before = old.len();
            old.retain(|_, &mut (_, _, minted)| now.saturating_sub(minted) <= TTL);
            ttl_drops += before - old.len();
            if old.len() > CAP {
                let mut by_age: Vec<(i64, String)> = old
                    .iter()
                    .map(|(hash, &(_, _, minted))| (minted, hash.clone()))
                    .collect();
                by_age.sort();
                let excess = old.len() - CAP;
                cap_evictions += excess;
                for (_, hash) in by_age.into_iter().take(excess) {
                    old.remove(&hash);
                }
            }

            let mut got: Vec<(String, Minted)> =
                map.iter().map(|(hash, v)| (hash.to_string(), v)).collect();
            got.sort_by(|a, b| a.0.cmp(&b.0));
            let mut want: Vec<(String, Minted)> =
                old.iter().map(|(hash, &v)| (hash.clone(), v)).collect();
            want.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                got, want,
                "step {step}: the two prunes kept different entries"
            );
            assert_eq!(
                map.by_age.len(),
                map.by_hash.len(),
                "step {step}: the mint order lists exactly the map's entries"
            );
        }
        // Both ways out of the map must have happened, or the comparison proved nothing.
        assert!(cap_evictions > 0, "the cap never bound");
        assert!(ttl_drops > 0, "the TTL never dropped anything");
    }

    /// What one record costs with the map full, which is where a busy proxy lives: the live map
    /// sat at exactly [`BOUND_TOKEN_CAP`] on 2026-10-15, so every response that minted a token
    /// also evicted one. A timing, not a gate, so it is ignored; run it by hand with
    /// `cargo test --release --lib record_at_the_cap_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing benchmark, run by hand"]
    fn record_at_the_cap_costs() {
        let manager = manager_over(&[account("a@example.com", "uuid-a")]);
        let now = crate::now_ms();
        let fill: Vec<MintedToken> = (0..BOUND_TOKEN_CAP)
            .map(|n| (BoundTokenKind::MessageId, format!("msg_fake_{n}")))
            .collect();
        manager.record_bound_tokens(&fill, 0, now - 1_000);
        let rounds: u32 = 500;
        let start = std::time::Instant::now();
        for n in 0..rounds {
            manager.record_bound_tokens(
                &sig(&format!("sig_fake_bench_{n}")),
                0,
                now + i64::from(n),
            );
        }
        let per_call = start.elapsed() / rounds;
        println!(
            "record_bound_tokens at the cap ({BOUND_TOKEN_CAP} entries): {per_call:?} per call"
        );
        assert_eq!(manager.bound_tokens.len(), BOUND_TOKEN_CAP);
        let newest = format!("sig_fake_bench_{}", rounds - 1);
        assert!(
            manager
                .bound_token_account(std::slice::from_ref(&newest))
                .is_some()
        );
    }

    /// The comparison behind the store's design: one lock (the map as it was), 16 locked parts
    /// and 64, each filled to the cap and then driven by `threads` threads doing what a proxy
    /// request does: look up two tokens, record two. Prints the wall time per request and, per
    /// design, the share of the newest half-cap mints still held at the end and the oldest mint
    /// left, so a faster design that evicts the wrong entries shows as such. The `DashMap`
    /// design [`ShardedBoundTokens`]'s doc names was measured by this bench too, in the commit
    /// before it was removed. A timing, so it is ignored; run it with
    /// `cargo test --release --lib bound_token_store_designs -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing benchmark, run by hand"]
    fn bound_token_store_designs() {
        const ROUNDS: usize = 200_000;
        let hashes: Vec<String> = (0..BOUND_TOKEN_CAP * 2)
            .map(|n| bound_tokens::hash_token(&format!("sig_fake_{n}")))
            .collect();
        let hashes = Arc::new(hashes);
        let designs = [("one lock", 1), ("16 parts", 16), ("64 parts", 64)];
        for threads in [1usize, 4, 8, 16] {
            for (name, parts) in designs {
                let store = Arc::new(ShardedBoundTokens::with_shards(parts));
                let base = crate::now_ms() - 1_000_000;
                for (n, hash) in hashes.iter().take(BOUND_TOKEN_CAP).enumerate() {
                    store.record(hash, (0, BoundTokenKind::MessageId, base + n as i64), base);
                }
                let next = Arc::new(std::sync::atomic::AtomicUsize::new(BOUND_TOKEN_CAP));
                let start = std::time::Instant::now();
                let handles: Vec<_> = (0..threads)
                    .map(|t| {
                        let (store, hashes, next) =
                            (Arc::clone(&store), Arc::clone(&hashes), Arc::clone(&next));
                        std::thread::spawn(move || {
                            let mut seed = 0x9e37_79b9_u64 + t as u64;
                            for _ in 0..ROUNDS / threads {
                                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                                let probe = (seed >> 33) as usize % hashes.len();
                                std::hint::black_box(store.get(&hashes[probe]));
                                std::hint::black_box(
                                    store.get(&hashes[(probe + 7) % hashes.len()]),
                                );
                                for _ in 0..2 {
                                    let n = next.fetch_add(1, Ordering::Relaxed);
                                    let at = base + n as i64;
                                    store.record(
                                        &hashes[n % hashes.len()],
                                        (1, BoundTokenKind::MessageId, at),
                                        at,
                                    );
                                }
                            }
                        })
                    })
                    .collect();
                for handle in handles {
                    handle.join().expect("bench thread panicked");
                }
                let elapsed = start.elapsed();
                let written = next.load(Ordering::Relaxed);
                let newest_half = (written - BOUND_TOKEN_CAP / 2..written)
                    .filter(|&n| store.get(&hashes[n % hashes.len()]).is_some())
                    .count();
                let oldest_left = (0..written)
                    .find(|&n| {
                        store
                            .get(&hashes[n % hashes.len()])
                            .is_some_and(|(_, _, at)| at == base + n as i64)
                    })
                    .unwrap_or(written);
                println!(
                    "{threads:>2} threads  {name:<9} {:>7.0} ns/request  held {:>6}  newest half kept {:>5.1}%  oldest left #{oldest_left} of {written}",
                    elapsed.as_nanos() as f64 / ROUNDS as f64,
                    store.len(),
                    100.0 * newest_half as f64 / (BOUND_TOKEN_CAP / 2) as f64,
                );
            }
        }
    }

    /// What one flush of a full map costs: the snapshot, the file build, the serialization and
    /// the write. The live proxy does this every 60 s. A timing, not a gate, so it is ignored;
    /// run it with `cargo test --release --lib flush_at_the_cap_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing benchmark, run by hand"]
    fn flush_at_the_cap_costs() {
        let manager = manager_over(&[
            account("a@example.com", "uuid-a"),
            account("b@example.com", "uuid-b"),
        ]);
        let now = crate::now_ms();
        let fill: Vec<MintedToken> = (0..BOUND_TOKEN_CAP)
            .map(|n| (BoundTokenKind::MessageId, format!("msg_fake_{n}")))
            .collect();
        let (first, second) = fill.split_at(BOUND_TOKEN_CAP / 2);
        manager.record_bound_tokens(first, 0, now);
        manager.record_bound_tokens(second, 1, now);
        let path = tmp("flush-bench");
        let rounds: u32 = 10;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            assert_eq!(
                manager.flush_bound_tokens(&path).expect("flush"),
                BOUND_TOKEN_CAP
            );
        }
        println!(
            "flush_bound_tokens at the cap ({BOUND_TOKEN_CAP} entries): {:?} per flush",
            start.elapsed() / rounds
        );
    }
}
