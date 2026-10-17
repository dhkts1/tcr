//! `Manager` snapshot methods, split verbatim from `mod.rs`.

use super::*;

/// Bound on [`Manager::sessions`], so a long-lived proxy can't grow it without
/// limit. Personal use has a handful of sessions; this is a backstop.
pub(super) const SESSION_CAP: usize = 128;

impl Manager {
    /// Count exactly one served client request against account `idx` (the true
    /// serving account) and stamp its last-used time. Called once per client
    /// request at the terminal outcome — never per upstream response — so retries
    /// that rotate across accounts do not inflate the counter (bug #4).
    pub fn record_served(
        &self,
        idx: usize,
        now: OffsetDateTime,
        session_key: Option<u64>,
        kind: SessionKind,
    ) {
        // Short display id for the log line, computed before the accounts lock so
        // the borrow ordering in `tracing::info!` stays simple.
        let sid = session_key.map(short_session_id);
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(account) = accounts.get_mut(idx) {
                account.requests += 1;
                account.last_used_ms = Some(odt_to_ms(now));
                // One line per forwarded request naming the account that served it. At
                // DEBUG: the INFO record of a served request is its `request usage` line
                // (`src/proxy.rs`), which names the same account, index and session, so
                // the load spread stays observable in the default log without a second
                // INFO line per request.
                tracing::debug!(account = %account.name, index = idx, session = sid, "serving request");
            }
        }
        // Upsert per-session stats (only when a session key exists) so a session
        // pinned by affinity is observable live. Independent of the `affinity`
        // pin map, so routing is unaffected.
        if let Some(key) = session_key {
            let now_ms = odt_to_ms(now);
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let stat = sessions.entry(key).or_insert(SessionStat {
                account_idx: idx,
                requests: 0,
                last_seen_ms: now_ms,
                kind,
            });
            stat.account_idx = idx;
            stat.requests += 1;
            stat.last_seen_ms = now_ms;
            stat.kind = kind;
            sessions.evict_sparing(key);
        }
        self.set_current(idx);
    }

    /// Append a served-request entry to the ring buffer (most-recent-last).
    pub fn push_log(&self, entry: RequestLogEntry) {
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        if log.len() >= REQUEST_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// Each account's effective gating threshold — its own `switchThreshold`, else
    /// the global one — in account order, resolved exactly as [`Self::snapshot`]
    /// resolves it internally.
    ///
    /// Exposed for the status endpoint (`crate::proxy`), which ships the SERVER's
    /// thresholds alongside its snapshot. A client must not re-derive them from
    /// `~/.config/tcr.json`: that file may have been edited since the server
    /// booted, and a client-ordered threshold list zipped against a server-ordered
    /// account list would mislabel which windows are holding which account.
    pub fn thresholds(&self) -> Vec<f64> {
        self.accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            // The same rows `Self::snapshot` lists, in the same order: the
            // status endpoint zips the two by position.
            .filter(|a| !a.removed)
            .map(|a| a.switch_threshold.unwrap_or(self.global_threshold))
            .collect()
    }

    /// Whether this server's accounts are forced onto HTTP/1.1 — see
    /// [`config::Config::http1_only`]. Server-wide, not per-account, and
    /// exposed for the same reason `thresholds` is: a client must not
    /// re-derive this from `~/.config/tcr.json`, which may have been
    /// edited (or the process not yet restarted to pick it up) since the
    /// server actually booted its clients.
    pub fn http1_only(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .http1_only
    }

    /// The port this server listens on, as its config holds it: `server::serve` writes a
    /// `--port` override into the config before it builds the manager. The dashboard passes it
    /// to the in-process Codex route, whose reasons name the port.
    pub fn proxy_port(&self) -> u16 {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .proxy
            .port
    }

    /// What this process's MITM listener can intercept by its policy and leaf — see
    /// [`crate::status::MitmStatus`]. Server-wide boot state, exposed for the
    /// reason [`Self::http1_only`] is: a client must not re-derive it, here
    /// from the leaf on disk, which a client-side load can re-mint while this
    /// process keeps serving the leaf it loaded at boot. A manager that never
    /// had TLS material recorded reports MITM off (`leaf: "none"`), which is
    /// the truth for it.
    pub fn mitm_status(&self) -> crate::status::MitmStatus {
        self.mitm
            .get()
            .cloned()
            .unwrap_or_else(|| crate::status::MitmStatus::of(None))
    }

    /// Record [`Self::mitm_status`]. Called once, by `server::serve`, right
    /// after its TLS load; a later call is ignored, because the listener's
    /// leaf is never reloaded and so what it can intercept never changes.
    pub fn record_mitm_status(&self, status: crate::status::MitmStatus) {
        if self.mitm.set(status).is_err() {
            tracing::debug!("MITM status already recorded; later call ignored");
        }
    }

    /// Every group on the fleet mapped to its resolved color — hot-reloaded
    /// from `groupSettings` on the same cadence [`Self::snapshot`] and
    /// selection reload (see [`Self::reload_groups_if_changed`]'s doc); a
    /// `tcr group color` write is picked up on this process's next natural
    /// check, no restart required.
    pub fn group_colors(&self) -> BTreeMap<String, String> {
        self.groups().colors.clone()
    }

    /// Compute the live snapshot the TUI renders. Every quota figure is evaluated
    /// at `now` so the display can never show a past-reset window as still full.
    pub fn snapshot(&self, now: OffsetDateTime) -> StatsSnapshot {
        // Live-reload group membership/settings before this snapshot reads them
        // — see `Self::reload_groups_if_changed`'s doc. Cheap no-op when the
        // config file's mtime has not moved.
        self.reload_groups_if_changed();
        let groups = self.groups();
        let now_ms = odt_to_ms(now);
        // (1) UNDER THE AFFINITY LOCK ONLY: copy every session's PIN out into a
        // local, then DROP the lock before the accounts lock below is taken — the
        // two are NEVER held simultaneously (the documented deadlock), the same
        // three-section shape [`Manager::select`] uses. The affinity map is the sole
        // authority on where a session is pinned; `SessionStat.account_idx` records
        // only who SERVED last, which a divert deliberately moves while the pin
        // stays put.
        let pins: HashMap<u64, usize> = {
            let affinity = self.affinity.lock().unwrap_or_else(PoisonError::into_inner);
            affinity
                .iter()
                .map(|(&key, &(idx, _))| (key, idx))
                .collect()
        };
        // (2) UNDER THE ACCOUNTS LOCK: everything below resolves indices to names.
        let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
        // A row deleted from the config while this process ran keeps its slot
        // (see `AccountRuntime::removed`) and is left out of everything a
        // reader sees. `current` is re-counted over the rows that remain, so
        // it still indexes `accounts` below, and so is the account serving each
        // provider: the same rule, so the two spaces can never be mixed.
        let listed_position = |idx: usize| {
            (!accounts.get(idx)?.removed)
                .then(|| accounts[..idx].iter().filter(|a| !a.removed).count())
        };
        let current = (*self.current.lock().unwrap_or_else(PoisonError::into_inner))
            .and_then(listed_position);
        let current_by_provider = {
            let serving = *self
                .current_by_provider
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            crate::stats::CurrentByProvider {
                anthropic: serving.anthropic.and_then(listed_position),
                codex: serving.codex.and_then(listed_position),
            }
        };
        let account_snaps = accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.removed)
            .map(|(idx, a)| {
                let five_hour = a.quota.five_hour.map(|w| w.effective(now));
                let seven_day = a.quota.seven_day.map(|w| w.effective(now));
                // Honest quota state vs this account's OWN threshold (same gating
                // dims `eligible` uses): the most-spent of the 5-hour and weekly
                // windows decides near-limit vs exhausted. Status stays "active" —
                // being quota-parked is not a dead credential.
                let threshold = a.switch_threshold.unwrap_or(self.global_threshold);
                let gating = [five_hour, seven_day]
                    .into_iter()
                    .flatten()
                    .reduce(f64::max);
                let quota_state = crate::stats::QuotaState::from_utilization(gating, threshold);
                // Why this account is out and when it clears — the GENERAL
                // (non-Fable) view: `is_fable = false`, so the model-scoped weekly
                // never gates a general fleet row (an account spent only on its
                // Fable bucket still serves every other model, so it reads `Ok`).
                // `group: None` — the fleet view answers "what would UNREQUESTED
                // traffic see right now", the same question `retry_after_hint`
                // asks (see `Self::account_gate`'s doc-comment).
                let (gate, free_at) = Self::account_gate(
                    a,
                    threshold,
                    self.fable_weekly_threshold,
                    now,
                    now_ms,
                    false,
                    None,
                    &groups.reserved,
                    &groups.parked,
                );
                // A Codex account at its usage limit reports the window that refused it, and
                // when that window rolls, instead of a `Rejected` with no time. Display only:
                // selection and the exhaustion hint never see this remap, and the probe
                // cadence, which reads this row, takes the refusal from its
                // `limit_reached_type` instead (`schedule::ProbeUrgency::of`). See
                // `Manager::codex_refusal_gate`.
                let (gate, free_at) = Self::codex_refusal_gate(a, threshold, now, gate, free_at);
                let reserved_groups = groups::carried_in(&a.groups, &groups.reserved);
                let parked_groups = groups::carried_in(&a.groups, &groups.parked);
                let control_allowed_groups = groups::carried_in(&a.groups, &groups.control_allowed);
                let codex = codex_snapshot_facts(a);
                // A Codex row's weekly window: its live weekly reset, and the length its plan
                // reported, or a week when it reported none. `None` on an Anthropic row, which
                // therefore never carries the key.
                let seven_day_span = match a.provider {
                    crate::provider::Provider::Anthropic => None,
                    crate::provider::Provider::Codex => a
                        .quota
                        .seven_day
                        .and_then(|w| w.live_reset(now))
                        .map(|reset| {
                            let length_ms = codex
                                .seven_day_window_minutes
                                .map_or(crate::usage::WEEK_MS, |minutes| {
                                    i64::from(minutes) * 60_000
                                });
                            (odt_to_ms(reset), length_ms)
                        }),
                };
                AccountSnapshot {
                    // The rotation index, which keys act on: this row's position in the list
                    // equals it only until a row before it is deleted.
                    slot: idx,
                    name: a.name.clone(),
                    provider: a.provider,
                    organization_type: a.organization_type.clone(),
                    rate_limit_tier: a.rate_limit_tier.clone(),
                    seat_tier: a.seat_tier.clone(),
                    org_uuid: a.org_uuid.clone(),
                    org_name: a.org_name.clone(),
                    priority: a.priority,
                    // `Throttled` is cleared from the enum only when the account
                    // next serves a non-429 (proxy.rs), so a naturally-expired hold
                    // would linger as a stale "throttled" label. Derive the DISPLAYED
                    // status from the live deadline — exactly as `eligible`,
                    // `rate_limited_until` and the quota bars already do — so the
                    // snapshot never shows a status the routing no longer honours.
                    status: {
                        let displayed = match a.status {
                            AccountStatus::Throttled
                                if a.rate_limited_until_ms
                                    .is_none_or(|until| until <= odt_to_ms(now)) =>
                            {
                                AccountStatus::Active
                            }
                            other => other,
                        };
                        displayed.as_str().to_string()
                    },
                    disabled: a.disabled,
                    five_hour,
                    five_hour_reset: a.quota.five_hour.and_then(|w| w.live_reset(now)),
                    seven_day,
                    seven_day_reset: a.quota.seven_day.and_then(|w| w.live_reset(now)),
                    seven_day_oi: a.quota.seven_day_oi.map(|w| w.effective(now)),
                    seven_day_oi_reset: a.quota.seven_day_oi.and_then(|w| w.live_reset(now)),
                    requests: a.requests,
                    input_tokens: a.input_tokens,
                    output_tokens: a.output_tokens,
                    cache_read_tokens: a.cache_read_tokens,
                    cache_creation_tokens: a.cache_creation_tokens,
                    last_used: a.last_used_ms.and_then(ms_to_odt),
                    rate_limited_until: a
                        .rate_limited_until_ms
                        .filter(|&until| until > odt_to_ms(now))
                        .and_then(ms_to_odt),
                    probe_status: a.probe_status,
                    last_probe: a.last_probe_ms.and_then(ms_to_odt),
                    quota_updated_at: a.quota.updated_at,
                    probe_error: a.probe_error.clone(),
                    quota_state,
                    gate,
                    free_at,
                    stream_error_count: usage::stream_error_count(&a.stream_error_times_ms, now_ms),
                    last_stream_error: a.last_stream_error.clone(),
                    groups: a.groups.clone(),
                    reserved_groups,
                    parked_groups,
                    control_allowed_groups,
                    // Always populated here: this snapshot is built by a
                    // process that has a usage tracker, so the numbers are
                    // measured even when they are all zero (an account that has
                    // genuinely served nothing today). The "not measured" null
                    // is applied one layer out, by `render_accounts_json`,
                    // which is the only place that knows the reading came from
                    // a fresh OFFLINE manager rather than the serving one.
                    //
                    // The window's start is derived from the LIVE reset
                    // (`live_reset`), not the raw stored one, so a window whose
                    // reset has already elapsed does not name a start five
                    // hours before an instant that has passed. A Codex row also
                    // gets its weekly window, from the same live reset.
                    usage: Some(
                        self.usage_row(
                            idx,
                            now_ms,
                            a.quota
                                .five_hour
                                .and_then(|w| w.live_reset(now))
                                .map(odt_to_ms),
                            seven_day_span,
                        ),
                    ),
                    codex,
                }
            })
            .collect();

        let recent = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .cloned()
            .collect();

        // Resolve each session's PIN (from `pins`, read above) and its LAST SERVER
        // (`SessionStat.account_idx`) to names from the accounts guard we already
        // hold. A session with no pin has no home, so it shows where it last served.
        // Its provider is read the same way, from the same unfiltered slots, so a
        // session pinned to a row deleted from the config keeps that row's provider
        // as it keeps that row's name.
        let sessions = {
            let map = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let name_of = |idx: usize| {
                accounts
                    .get(idx)
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            };
            let provider_of =
                |idx: usize| accounts.get(idx).map(|a| a.provider).unwrap_or_default();
            let mut v: Vec<SessionSnapshot> = map
                .iter()
                .map(|(k, s)| {
                    let last_served_account = name_of(s.account_idx);
                    let home = pins.get(k).copied().unwrap_or(s.account_idx);
                    let account = pins
                        .get(k)
                        .map_or_else(|| last_served_account.clone(), |&idx| name_of(idx));
                    SessionSnapshot {
                        id: short_session_id(*k),
                        account,
                        last_served_account,
                        requests: s.requests,
                        last_seen: ms_to_odt(s.last_seen_ms),
                        provider: provider_of(home),
                        kind: s.kind,
                    }
                })
                .collect();
            // STABLE order — (pinned account, session id) — not recency. Sorting
            // most-recent-first re-ordered the pane on every single request, so rows
            // churned under the operator's eyes and a session appeared to move even
            // when its pin never did. Both keys change only when the pin actually
            // moves or a session appears/disappears; the age stays visible as data in
            // the `Last` column rather than being encoded in the row order.
            v.sort_by(|a, b| a.account.cmp(&b.account).then_with(|| a.id.cmp(&b.id)));
            v
        };

        let wire_sessions = self.wire_sessions_snapshot(now);
        let wire_sessions_summary = Self::wire_sessions_summary(&wire_sessions);

        StatsSnapshot {
            accounts: account_snaps,
            current,
            current_by_provider,
            recent,
            sessions,
            wire_sessions,
            wire_sessions_summary,
        }
    }
}

/// The Codex facts a Codex row reports on the status wire: from its quota, which windows the
/// plan has (the probe's once any probe answered, else the headers'), each one's length, the
/// refusal's kind while that refusal stands, the spend control and the credits; and from the
/// row itself, why its login is dead and the folder it was imported from.
///
/// An Anthropic row keeps the defaults, whatever its quota holds, so its wire row gains no key.
fn codex_snapshot_facts(a: &AccountRuntime) -> crate::stats::CodexFacts {
    match a.provider {
        crate::provider::Provider::Anthropic => return crate::stats::CodexFacts::default(),
        crate::provider::Provider::Codex => {}
    }
    let windows = a.quota.codex.windows();
    crate::stats::CodexFacts {
        quota_windows: windows.map(|w| w.wire_names()),
        five_hour_window_minutes: windows.and_then(|w| w.five_hour).and_then(|w| w.minutes),
        seven_day_window_minutes: windows.and_then(|w| w.seven_day).and_then(|w| w.minutes),
        limit_reached_type: a.quota.codex_limit_reached_type().map(str::to_string),
        spend_control_reached: a.quota.codex.spend_control_reached,
        credits: a.quota.codex.credits.clone(),
        login_error: a.login_error.clone(),
        codex_home: a.codex_home.clone(),
        token_sync_pending: a.pending_codex_writeback.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(account: &str) -> AccountRuntime {
        AccountRuntime::from_config(&serde_json::from_str(account).expect("an account"), false)
    }

    /// One turn on account `idx` for a Codex model id. The account, not the model id, decides
    /// the record's provider.
    fn spend(manager: &Manager, idx: usize, ts_ms: i64, reasoning: u64) {
        let mut record = crate::usage::UsageRecord::from_quota_input(
            ts_ms,
            Some("gpt-5.1-codex".to_string()),
            None,
            1_000,
            0,
            0,
            200,
            50,
        );
        record.reasoning = reasoning;
        manager.record_usage(idx, record);
    }

    /// A Codex row's `usage.sevenDayWindow` is what the account spent since its weekly window
    /// opened: the weekly reset minus the window's length. The length is the one the plan
    /// reported, or seven days when it reported none. A Claude row never carries the key,
    /// whatever its weekly window says, and a Codex turn's reasoning tokens reach the row's
    /// buckets.
    #[test]
    fn a_codex_row_carries_its_weekly_window_usage() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let day = 86_400_000;
        let mut alice = runtime(r#"{"name":"alice@example.com","accessToken":"at-alice"}"#);
        alice.quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: 0.2,
            reset: Some(now + time::Duration::days(2)),
        });
        let bob = runtime(
            r#"{"name":"bob@example.com/codex","provider":"codex","accountUuid":"22222222-2222-4222-8222-222222222222","accessToken":"at-bob"}"#,
        );
        let mut carol = runtime(
            r#"{"name":"carol@example.com/codex","provider":"codex","accountUuid":"33333333-3333-4333-8333-333333333333","accessToken":"at-carol"}"#,
        );
        let carol_reset = now + time::Duration::days(2);
        carol.quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: 0.4,
            reset: Some(carol_reset),
        });
        let manager = Manager::from_runtimes(vec![alice, bob, carol]);
        // bob's plan reports a three-day weekly window, on a response's own headers.
        let bob_reset_s = (now + time::Duration::days(1)).unix_timestamp();
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-used-percent", "34".to_string()),
            ("x-codex-primary-window-minutes", "4320".to_string()),
            ("x-codex-primary-reset-at", bob_reset_s.to_string()),
        ] {
            headers.insert(name, value.parse().expect("a header value"));
        }
        manager.update_quota(1, &headers, 200);
        spend(&manager, 1, now_ms - day, 20);
        spend(&manager, 1, now_ms - 600_000, 20);
        spend(&manager, 1, now_ms - 3 * day, 99); // before bob's window opened
        spend(&manager, 2, now_ms - 5 * day + 60_000, 7);
        // alice is an Anthropic account whatever the model id says.
        spend(&manager, 0, now_ms - 600_000, 0);

        let snapshot = manager.snapshot(now);
        let usage = |idx: usize| {
            snapshot.accounts[idx]
                .usage
                .clone()
                .expect("a serving process measures usage")
        };
        let bob_week = usage(1)
            .seven_day_window
            .expect("a Codex row with a weekly reset");
        assert_eq!(
            bob_week.since,
            bob_reset_s * 1000 - 4_320 * 60_000,
            "the length the plan reported"
        );
        assert_eq!(
            (
                bob_week.totals.requests,
                bob_week.totals.reasoning_tokens,
                bob_week.totals.unpriced_requests
            ),
            (2, 40, 2)
        );
        let carol_week = usage(2)
            .seven_day_window
            .expect("a Codex row with a weekly reset");
        assert_eq!(
            carol_week.since,
            odt_to_ms(carol_reset) - crate::usage::WEEK_MS,
            "seven days by default"
        );
        assert_eq!(carol_week.totals.requests, 1);
        assert!(
            usage(0).seven_day_window.is_none(),
            "a Claude row never carries it"
        );
        assert_eq!(usage(1).last_hour.reasoning_tokens, 20);
        assert_eq!(usage(0).last_hour.reasoning_tokens, 0);
    }

    /// A row deleted from the config keeps its rotation slot but leaves the snapshot, so a
    /// Codex row after it is listed one position before its slot. Its usage, the weekly
    /// window included, is still read from its own slot: the snapshot reads by slot, never
    /// by the position it lists a row at.
    #[test]
    fn a_deleted_row_before_a_codex_row_leaves_its_weekly_window_its_own() {
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        // dana was deleted while the proxy ran: parked and marked removed, as the reload marks her.
        let mut dana = runtime(
            r#"{"name":"dana@example.com/codex","provider":"codex","accountUuid":"44444444-4444-4444-8444-444444444444","accessToken":"at-dana"}"#,
        );
        dana.removed = true;
        dana.disabled = true;
        let mut bob = runtime(
            r#"{"name":"bob@example.com/codex","provider":"codex","accountUuid":"22222222-2222-4222-8222-222222222222","accessToken":"at-bob"}"#,
        );
        bob.quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: 0.4,
            reset: Some(now + time::Duration::days(2)),
        });
        let manager = Manager::from_runtimes(vec![dana, bob]);
        // A turn dana was serving when she was deleted still lands on her slot.
        spend(&manager, 0, now_ms - 600_000, 5);
        spend(&manager, 0, now_ms - 600_000, 5);
        spend(&manager, 1, now_ms - 600_000, 3);

        let snapshot = manager.snapshot(now);
        assert_eq!(snapshot.accounts.len(), 1, "the deleted row is not listed");
        let row = &snapshot.accounts[0];
        assert_eq!(row.name, "bob@example.com/codex");
        let usage = row.usage.clone().expect("a serving process measures usage");
        let week = usage.seven_day_window.expect("bob's weekly window");
        assert_eq!(
            (
                week.totals.requests,
                week.totals.reasoning_tokens,
                usage.last_hour.requests
            ),
            (1, 3, 1),
            "bob's slot, not the deleted row's"
        );
    }
}
