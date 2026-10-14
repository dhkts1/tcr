//! `Manager` glue for [`crate::session_wire::WireSessionTracker`] — split verbatim from
//! `mod.rs`, mirroring `usage.rs`/`snapshot.rs`'s split.

use std::path::Path;
use std::sync::atomic::Ordering;

use super::*;
use crate::session_wire::{CodexToolCall, ToolResultEvent, ToolUseEvent};
use crate::session_wire_persist::{self, LoadReport};

impl Manager {
    /// Fold one request's session identity, model and parsed tool events into the wire-session
    /// table. A no-op when `session_id` is `None` — a request with no recognizable Claude Code
    /// identity has nothing for this table to key on.
    ///
    /// Called once per client request at the terminal outcome, alongside [`Self::record_served`]
    /// (same call site in `proxy.rs`) — not per upstream retry, for the same reason
    /// `record_served` isn't: a request rotated across accounts must count once, not once per
    /// account tried.
    pub fn record_wire_session(
        &self,
        session_id: Option<&str>,
        account: Option<String>,
        model: Option<String>,
        now: OffsetDateTime,
        tool_uses: &[ToolUseEvent],
        tool_results: &[ToolResultEvent],
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let now_ms = odt_to_ms(now);
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_request(session_id, account, model, now_ms, tool_uses, tool_results);
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Fold the `tool_use` blocks parsed out of a RESPONSE into the wire-session table, as
    /// running from `now` (the instant the response finished — stream end, or the JSON body
    /// being read). A no-op when `session_id` is `None` or the tool list is empty.
    ///
    /// Called from both response paths in `proxy.rs`, beside
    /// [`Self::record_wire_session_usage`], and NOT from the request call site: this is the
    /// only source that can ever put a tool in `running`, because a Claude Code request body
    /// carries each `tool_use` together with its `tool_result` — see
    /// [`crate::session_wire::tool_use_event_from_block`].
    pub fn record_wire_session_tool_uses(
        &self,
        session_id: Option<&str>,
        now: OffsetDateTime,
        tool_uses: &[ToolUseEvent],
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        if tool_uses.is_empty() {
            return;
        }
        let now_ms = odt_to_ms(now);
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_response_tool_uses(session_id, now_ms, tool_uses);
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Add token counts learned from a response's usage to the wire-session table. Called
    /// beside [`Self::record_usage`] (the per-account ledger) at both of its call sites in
    /// `proxy.rs` — streamed and non-streamed — with the SAME [`crate::usage::UsageRecord`]
    /// fields, so the account ledger, the per-model tally and the wire session's quota totals
    /// never disagree about what a response carried.
    ///
    /// `base_input`, `cache_5m` and `cache_1h` (`UsageRecord::input`/`cache_5m`/`cache_1h`) fold
    /// into the session's `input_tokens`/`cache_creation_tokens` totals and into the per-model
    /// pricing tally kept in [`crate::session_wire::WireSession::by_model`] — wire 2. The QUOTA
    /// figure (`UsageRecord::input_total()`) is deliberately NOT passed here: it already folds
    /// cache reads in, and folding it into `input_tokens` too was the 2026-09-14 double-count
    /// incident (see [`crate::session_wire::WireSession::input_tokens`]).
    #[allow(clippy::too_many_arguments)]
    pub fn record_wire_session_usage(
        &self,
        session_id: Option<&str>,
        model: Option<&str>,
        base_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_usage(
            session_id,
            model,
            base_input,
            cache_5m,
            cache_1h,
            cache_read_tokens,
            output_tokens,
        );
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Count one Codex request on the wire session its `session-id` header named, attributed
    /// to `account`. A no-op when `session_id` is `None`: a Codex request that names no session
    /// has nothing for this table to key on.
    ///
    /// Called once per client request at the terminal outcome, in place of
    /// [`Self::record_wire_session`] (same call site in `proxy.rs`, same once-not-per-retry
    /// rule). Its arrival closes every call the session's previous response started, because
    /// it carries their outputs in a body tcr does not read. A request tcr answers itself
    /// never reaches the terminal outcome, so it is not counted and closes nothing, just as a
    /// Claude `tool_result` in such a request closes nothing.
    pub fn record_codex_wire_session(
        &self,
        session_id: Option<&str>,
        account: Option<String>,
        now: OffsetDateTime,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let now_ms = odt_to_ms(now);
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_codex_request(session_id, account, now_ms);
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Start the tool calls a Codex response's finished output items asked for, as running
    /// from `now` (the instant its stream ended), each with its own timeout. The Codex twin of
    /// [`Self::record_wire_session_tool_uses`], and a no-op on the same terms: `session_id` is
    /// `None` or the call list is empty.
    pub fn record_codex_wire_session_calls(
        &self,
        session_id: Option<&str>,
        now: OffsetDateTime,
        calls: &[CodexToolCall],
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        if calls.is_empty() {
            return;
        }
        let now_ms = odt_to_ms(now);
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_codex_tool_calls(session_id, now_ms, calls);
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// Add a Codex response's usage to its wire session. The arguments are the same
    /// [`crate::usage::UsageRecord`] fields [`Self::record_wire_session_usage`] takes, plus the
    /// turn's reasoning tokens, so the account ledger and the session never disagree about
    /// what a response carried. Unlike its Claude twin it stamps no Anthropic cache TTL, and
    /// the usage's model names the session — see
    /// [`crate::session_wire::WireSessionTracker::record_codex_usage`].
    #[allow(clippy::too_many_arguments)]
    pub fn record_codex_wire_session_usage(
        &self,
        session_id: Option<&str>,
        model: Option<&str>,
        base_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let mut tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker.record_codex_usage(
            session_id,
            model,
            base_input,
            cache_5m,
            cache_1h,
            cache_read_tokens,
            output_tokens,
            reasoning_tokens,
        );
        drop(tracker);
        self.mark_wire_sessions_dirty();
    }

    /// The live wire-session table, projected onto [`tcr_status_wire::SessionRow`] — what
    /// [`Self::snapshot`] hangs onto [`crate::stats::StatsSnapshot::wire_sessions`].
    pub(super) fn wire_sessions_snapshot(
        &self,
        now: OffsetDateTime,
    ) -> Vec<tcr_status_wire::SessionRow> {
        let now_ms = odt_to_ms(now);
        let tracker = self
            .wire_sessions
            .lock()
            .expect("wire sessions lock poisoned");
        tracker
            .snapshot(now_ms)
            .into_iter()
            .map(|(session_id, s)| {
                let (cost_usd, unpriced) = Self::price_session(&self.usage, &s.by_model);
                tcr_status_wire::SessionRow {
                    session_id,
                    account: s.account,
                    model: s.model,
                    first_seen_ms: s.first_seen_ms,
                    last_seen_ms: s.last_seen_ms,
                    requests: s.requests,
                    input_tokens: s.input_tokens,
                    output_tokens: s.output_tokens,
                    cache_read_tokens: s.cache_read_tokens,
                    cache_creation_tokens: s.cache_creation_tokens,
                    tools: tcr_status_wire::SessionToolsRow {
                        calls: s.tools.calls,
                        errors: s.tools.errors,
                        timeouts: s.tools.timeouts,
                        timeouts_by_class: s.tools.timeouts_by_class.clone(),
                        timed_out: s
                            .tools
                            .timed_out
                            .iter()
                            .map(|t| tcr_status_wire::SlowToolRow {
                                tool: t.tool.clone(),
                                seconds: t.seconds,
                                command_head: t.command_head.clone(),
                                command_class: t.command_class.map(|c| c.as_str().to_string()),
                                ended_ms: t.ended_ms,
                            })
                            .collect(),
                        over_one_minute: s.tools.over_one_minute,
                        subagents_running: s
                            .tools
                            .running
                            .values()
                            .filter(|r| {
                                (r.tool == "Agent" || r.tool == "Task")
                                    && !crate::session_wire::running_tool_is_lost(
                                        &r.tool,
                                        r.started_ms,
                                        now_ms,
                                    )
                            })
                            .count() as u64,
                        // Belt and braces beside `WireSessionTracker::restore`'s own drop: an
                        // entry too old to still be running is a LOST result, whatever put it
                        // there — see `crate::session_wire::running_tool_is_lost`.
                        running: s
                            .tools
                            .running
                            .into_values()
                            .filter(|r| {
                                !crate::session_wire::running_tool_is_lost(
                                    &r.tool,
                                    r.started_ms,
                                    now_ms,
                                )
                            })
                            .map(|r| tcr_status_wire::RunningToolRow {
                                tool: r.tool,
                                started_ms: r.started_ms,
                                command_head: r.command_head,
                                command_class: r.command_class.map(|c| c.as_str().to_string()),
                                timeout_seconds: r.timeout_seconds,
                            })
                            .collect(),
                        slowest: s
                            .tools
                            .slowest
                            .into_iter()
                            .map(|t| tcr_status_wire::SlowToolRow {
                                tool: t.tool,
                                seconds: t.seconds,
                                command_head: t.command_head,
                                command_class: t.command_class.map(|c| c.as_str().to_string()),
                                ended_ms: t.ended_ms,
                            })
                            .collect(),
                        by_tool: s
                            .tools
                            .by_tool
                            .iter()
                            .map(|(name, bucket)| tcr_status_wire::ToolBucketRow {
                                tool: name.clone(),
                                calls: bucket.calls,
                                errors: bucket.errors,
                                seconds_p50: bucket.seconds_p50(),
                                over_one_minute: bucket.over_one_minute,
                            })
                            .collect(),
                    },
                    req_per_minute: s.req_per_minute.projected(now_ms),
                    cost_usd,
                    cache_ttl_secs: s.cache_ttl_secs,
                    // Absent for Anthropic, so a Claude-only fleet's rows serialize as they
                    // always have.
                    provider: (!s.provider.is_anthropic()).then(|| s.provider.as_str().to_string()),
                    // Codex sessions only. A Claude session on a model this build cannot price
                    // keeps the zero it has always shown and no count: changing that changes a
                    // Claude-only fleet's output.
                    unpriced_requests: if s.provider.is_anthropic() {
                        0
                    } else {
                        unpriced
                    },
                    reasoning_tokens: s.reasoning_tokens,
                }
            })
            .collect()
    }

    /// Price one session's per-model token tallies against the account ledger's OWN pricing
    /// table (`crate::usage::UsageTracker::price_for`) and sum — one model's price at a time,
    /// so a session that spans two models is priced correctly rather than averaged. A model
    /// this table has no entry for contributes `0.0` to the cost, the same "unpriced" outcome
    /// `crate::pricing` documents for the account-level ledger, so `SessionRow::cost_usd` stays
    /// a plain total, never an `Option`. The second value counts the requests the cost leaves
    /// out, from each unpriced model's tally; the projection reports it as
    /// `SessionRow::unpriced_requests` on Codex sessions only, so a Claude session on a model
    /// this build cannot price keeps the bare zero it has always shown.
    fn price_session(
        usage: &crate::usage::UsageTracker,
        by_model: &std::collections::HashMap<String, crate::session_wire::ModelTokenTally>,
    ) -> (f64, u64) {
        let mut unpriced = 0;
        let nanos: u64 = by_model
            .iter()
            .filter_map(|(model, tally)| match usage.price_for(model) {
                Some(price) => Some(crate::pricing::cost_nanos(
                    &price,
                    tally.input,
                    tally.cache_5m,
                    tally.cache_1h,
                    tally.cache_read,
                    tally.output,
                )),
                None => {
                    unpriced += tally.requests;
                    None
                }
            })
            .sum();
        (nanos as f64 / 1_000_000_000.0, unpriced)
    }

    /// Fleet-wide tool-call totals, summed across every row `Self::wire_sessions_snapshot`
    /// just built — the ONE place this sum happens server-side, so a panel's headline and
    /// "BY TOOL" bars read the same numbers as the per-session rows (see the bridge, F3).
    pub(super) fn wire_sessions_summary(
        rows: &[tcr_status_wire::SessionRow],
    ) -> tcr_status_wire::SessionsSummary {
        let mut by_tool: std::collections::HashMap<String, tcr_status_wire::ToolBucketRow> =
            std::collections::HashMap::new();
        let mut summary = tcr_status_wire::SessionsSummary::default();
        for row in rows {
            summary.calls += row.tools.calls;
            summary.over_one_minute += row.tools.over_one_minute;
            summary.timeouts += row.tools.timeouts;
            for (class, count) in &row.tools.timeouts_by_class {
                *summary.timeouts_by_class.entry(class.clone()).or_insert(0) += count;
            }
            summary.cost_usd += row.cost_usd;
            for bucket in &row.tools.by_tool {
                let entry = by_tool.entry(bucket.tool.clone()).or_insert_with(|| {
                    tcr_status_wire::ToolBucketRow {
                        tool: bucket.tool.clone(),
                        ..Default::default()
                    }
                });
                entry.calls += bucket.calls;
                entry.errors += bucket.errors;
                entry.over_one_minute += bucket.over_one_minute;
            }
        }
        // `seconds_p50` cannot be summed across sessions — it is a median, not a total — so
        // the fleet-wide row leaves it at its default (0.0) rather than fabricating one from
        // an average of medians.
        summary.by_tool = by_tool.into_values().collect();
        summary
    }

    /// Flag the wire-session table as changed since the last flush. Mirrors
    /// [`Self::mark_affinity_dirty`]: a single relaxed store on the request path, with the
    /// actual write done off it by a debounced flusher.
    pub fn mark_wire_sessions_dirty(&self) {
        self.wire_sessions_dirty.store(true, Ordering::Relaxed);
    }

    /// Consume the dirty flag: `true` when something changed since the last call.
    pub fn take_wire_sessions_dirty(&self) -> bool {
        self.wire_sessions_dirty.swap(false, Ordering::Relaxed)
    }

    /// Write the wire-session table to `path`, atomically. Returns how many sessions
    /// landed. Never called on the request hot path — see `server.rs`'s debounced
    /// flusher, spawned only when a path was configured — and the caller logs the
    /// failure and carries on, same as [`Self::flush_affinity`]: a proxy that cannot
    /// write this cache still serves traffic exactly as it did before this file existed.
    pub fn flush_wire_sessions(&self, path: &Path) -> Result<usize, crate::config::ConfigError> {
        let now_ms = crate::now_ms();
        let sessions: std::collections::HashMap<String, crate::session_wire::WireSession> = {
            let tracker = self
                .wire_sessions
                .lock()
                .expect("wire sessions lock poisoned");
            tracker.snapshot(now_ms).into_iter().collect()
        };
        session_wire_persist::save(path, &sessions, now_ms)
    }

    /// Restore wire sessions from `path` into the live table, dropping anything stale
    /// (older than `ttl_ms`) or past [`session_wire_persist::PERSIST_CAP`].
    ///
    /// Existing in-memory sessions win — a session already tracked by a request served
    /// between boot and this call is fresher than anything on disk, same rule
    /// [`Self::restore_affinity`] follows for pins. In practice the table is empty here:
    /// this runs before the listener binds. Returns the store's report so the caller can
    /// state what was dropped.
    pub fn restore_wire_sessions(&self, path: &Path, ttl_ms: i64) -> LoadReport {
        let now_ms = crate::now_ms();
        let report = session_wire_persist::load(path, now_ms, ttl_ms);
        if !report.sessions.is_empty() {
            let mut tracker = self
                .wire_sessions
                .lock()
                .expect("wire sessions lock poisoned");
            tracker.restore(report.sessions.clone());
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;

    /// `WireSessionTracker` itself is unit-tested in `session_wire.rs` with canned bodies;
    /// these tests prove the `Manager` GLUE — the lock, the projection to `SessionRow`, and
    /// that an absent session id is a clean no-op rather than a panic. [`Manager::from_runtimes`]
    /// (also used by `pins.rs`'s tests) never touches the network.
    #[test]
    fn record_wire_session_round_trips_through_the_manager_snapshot() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(
            Some("sess-glue"),
            Some("alice@example.com".to_string()),
            Some("claude-fable-5".to_string()),
            now,
            &[],
            &[],
        );
        manager.record_wire_session_usage(
            Some("sess-glue"),
            Some("claude-fable-5"),
            10,
            0,
            0,
            5,
            20,
        );

        let snap = manager.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        let row = &snap.wire_sessions[0];
        assert_eq!(row.session_id, "sess-glue");
        assert_eq!(row.account.as_deref(), Some("alice@example.com"));
        assert_eq!(row.input_tokens, 10);
        assert_eq!(row.output_tokens, 20);
        assert_eq!(row.cache_read_tokens, 5);
    }

    /// The 2026-09-14 incident, pinned: `input_tokens` must hold BASE input only, never the
    /// quota figure (base + cache creation + cache read), or the panel's hit-ratio
    /// double-counts cache reads and every session renders ~50% regardless of its real hit
    /// rate. Cache creation gets its own counter rather than being dropped on the floor.
    #[test]
    fn session_input_tokens_excludes_cache_reads_and_creation() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(Some("sess-ratio"), None, None, now, &[], &[]);
        // base 100, cache_5m 20, cache_1h 30, cache_read 850 — the quota figure these fold
        // into (`UsageRecord::input_total()`) would be 1000, the number that used to land
        // wrongly in `input_tokens`.
        manager.record_wire_session_usage(
            Some("sess-ratio"),
            Some("claude-sonnet-5"),
            100,
            20,
            30,
            850,
            0,
        );

        let row = &manager.snapshot(now).wire_sessions[0];
        assert_eq!(
            row.input_tokens, 100,
            "base input only, not the quota total"
        );
        assert_eq!(row.cache_creation_tokens, 50, "cache_5m + cache_1h");
        assert_eq!(row.cache_read_tokens, 850);
    }

    /// `subagents_running` counts only `Agent`/`Task` entries among the running tools — a plain
    /// `Bash` call sitting alongside two subagents must not be counted, and a caller must be
    /// able to print "2 subagents" without scanning `running` itself.
    #[test]
    fn subagents_running_counts_agent_and_task_but_not_bash() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        let uses = vec![
            ToolUseEvent {
                id: "tu_bash".to_string(),
                name: Some("Bash".to_string()),
                command_head: Some("ls".to_string()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_agent".to_string(),
                name: Some("Agent".to_string()),
                command_head: Some("reviewer: check the wire fixtures".to_string()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_task".to_string(),
                name: Some("Task".to_string()),
                command_head: Some("review the diff".to_string()),
                command_class: None,
            },
        ];
        manager.record_wire_session(Some("sess-sub"), None, None, now, &uses, &[]);

        let snap = manager.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        let row = &snap.wire_sessions[0];
        assert_eq!(row.tools.running.len(), 3, "all three tools stay pending");
        assert_eq!(
            row.tools.subagents_running, 2,
            "only the Agent and Task entries count, not the Bash call"
        );
    }

    /// Belt and braces beside `WireSessionTracker::restore`'s drop: a `Bash` entry older than
    /// the tool's own timeout plus its grace is not a running call, it is a lost result, and
    /// the projection drops it however it got there. `Agent`/`Task` have no such deadline and
    /// stay — they are the calls that legitimately run for an hour.
    #[test]
    fn a_bash_entry_past_its_timeout_is_not_projected_as_running() {
        let manager = Manager::from_runtimes(vec![]);
        let started = OffsetDateTime::now_utc();
        let uses = vec![
            ToolUseEvent {
                id: "tu_bash".to_string(),
                name: Some("Bash".to_string()),
                command_head: Some("cargo test --release".to_string()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_agent".to_string(),
                name: Some("Agent".to_string()),
                command_head: Some("reviewer: check the wire fixtures".to_string()),
                command_class: None,
            },
        ];
        manager.record_wire_session(Some("sess-lost"), None, None, started, &uses, &[]);

        let grace_ms = crate::session_wire::RUNNING_BASH_LOST_MS;
        let inside = manager.snapshot(started + Duration::milliseconds(grace_ms));
        assert_eq!(
            inside.wire_sessions[0].tools.running.len(),
            2,
            "at the line both are still running"
        );

        let past = manager.snapshot(started + Duration::milliseconds(grace_ms + 1_000));
        let row = &past.wire_sessions[0];
        assert_eq!(
            row.tools.running.len(),
            1,
            "the Bash entry is dropped, the Agent stays"
        );
        assert_eq!(row.tools.running[0].tool, "Agent");
        assert_eq!(
            row.tools.subagents_running, 1,
            "the count and the list agree about what is running"
        );
    }

    #[test]
    fn record_wire_session_with_no_session_id_is_a_no_op() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(
            None,
            Some("alice@example.com".to_string()),
            None,
            now,
            &[],
            &[],
        );
        manager.record_wire_session_usage(None, Some("claude-fable-5"), 10, 0, 0, 5, 20);
        assert!(manager.snapshot(now).wire_sessions.is_empty());
    }

    /// The scoped ask: two requests on two different models, each with a known price, sum to
    /// the expected total to the cent — a session that spans models is priced per-model and
    /// summed, never averaged or priced against whichever model happened to be current.
    #[test]
    fn cost_usd_sums_two_models_priced_independently() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        manager.record_wire_session(Some("sess-cost"), None, None, now, &[], &[]);

        // 1,000,000 base input tokens on Opus 5 ($5.00/MTok) = $5.00 exactly.
        manager.record_wire_session_usage(
            Some("sess-cost"),
            Some("claude-opus-5"),
            1_000_000,
            0,
            0,
            0,
            0,
        );
        // 1,000,000 output tokens on Sonnet 5 ($10.00/MTok output) = $10.00 exactly.
        manager.record_wire_session_usage(
            Some("sess-cost"),
            Some("claude-sonnet-5"),
            0,
            0,
            0,
            0,
            1_000_000,
        );

        let snap = manager.snapshot(now);
        let row = &snap.wire_sessions[0];
        assert_eq!(
            row.cost_usd, 15.0,
            "$5.00 (opus input) + $10.00 (sonnet output)"
        );
        assert_eq!(
            snap.wire_sessions_summary.cost_usd, 15.0,
            "the fleet-wide summary sums the same per-session totals"
        );
    }

    /// The wire row, and the fleet summary above it, carry the per-class timeout split and
    /// the timed-out commands themselves — not just the headline count a panel could not
    /// act on. Two sessions, so the summary is proved to SUM rather than to copy one row.
    #[test]
    fn timeouts_by_class_and_the_timed_out_commands_reach_the_wire() {
        let manager = Manager::from_runtimes(vec![]);
        let now = OffsetDateTime::now_utc();
        let timed_out = |id: &str| ToolResultEvent {
            id: id.to_string(),
            is_error: true,
            timed_out: true,
        };
        let bash = |id: &str, command: &str| {
            crate::session_wire::tool_use_event_from_block(
                id.to_string(),
                Some("Bash".to_string()),
                Some(&serde_json::json!({ "command": command })),
            )
        };

        manager.record_wire_session(
            Some("sess-one"),
            None,
            None,
            now,
            &[
                bash("tu_wait", "until grep -q ready build.log; do sleep 5; done"),
                bash("tu_push", "git push origin main"),
            ],
            &[],
        );
        let later = now + Duration::seconds(1);
        manager.record_wire_session(
            Some("sess-one"),
            None,
            None,
            later,
            &[],
            &[timed_out("tu_wait")],
        );
        let later2 = now + Duration::seconds(2);
        manager.record_wire_session(
            Some("sess-one"),
            None,
            None,
            later2,
            &[],
            &[timed_out("tu_push")],
        );

        // A second session times out on another `git push`, so the fleet's `git-net` count
        // must read 2 while each session's own reads 1.
        manager.record_wire_session(
            Some("sess-two"),
            None,
            None,
            now,
            &[bash("tu_push2", "git push origin feature")],
            &[],
        );
        manager.record_wire_session(
            Some("sess-two"),
            None,
            None,
            later,
            &[],
            &[timed_out("tu_push2")],
        );

        let snap = manager.snapshot(later2);
        let one = snap
            .wire_sessions
            .iter()
            .find(|r| r.session_id == "sess-one")
            .expect("sess-one");
        assert_eq!(one.tools.timeouts, 2);
        assert_eq!(one.tools.timeouts_by_class.get("wait"), Some(&1));
        assert_eq!(one.tools.timeouts_by_class.get("git-net"), Some(&1));
        assert_eq!(one.tools.timed_out.len(), 2);
        assert_eq!(
            one.tools.timed_out[0].command_head.as_deref(),
            Some("git push origin main"),
            "newest first, with the command, not a bare tool name"
        );
        assert_eq!(
            one.tools.timed_out[0].command_class.as_deref(),
            Some("git-net")
        );

        let summary = &snap.wire_sessions_summary;
        assert_eq!(summary.timeouts, 3);
        assert_eq!(
            summary.timeouts_by_class.get("git-net"),
            Some(&2),
            "the fleet sums both sessions' git-net timeouts"
        );
        assert_eq!(summary.timeouts_by_class.get("wait"), Some(&1));
        assert_eq!(
            summary.timeouts_by_class.values().sum::<u64>(),
            summary.timeouts,
            "the headline and the section read one number"
        );
    }

    fn tmp_wire_sessions_path(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcr-wire-sessions-manager-test-{}-{label}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("session-wire.json")
    }

    /// The Manager-level round trip: a call tracked by one manager, flushed to disk,
    /// restored by a second (fresh) manager — the restart-survival contract, proven
    /// through the same `flush_wire_sessions`/`restore_wire_sessions` pair `server.rs`
    /// wires into boot and the debounced flusher, not by calling
    /// `session_wire_persist::save`/`load` directly.
    #[test]
    fn flush_and_restore_round_trip_through_two_managers() {
        let path = tmp_wire_sessions_path("round-trip");
        let now = OffsetDateTime::now_utc();

        let first = Manager::from_runtimes(vec![]);
        first.record_wire_session(
            Some("sess-restart"),
            Some("alice@example.com".to_string()),
            Some("claude-sonnet-5".to_string()),
            now,
            &[ToolUseEvent {
                id: "tu_1".to_string(),
                name: Some("Bash".to_string()),
                command_head: Some("ls".to_string()),
                command_class: None,
            }],
            &[ToolResultEvent {
                id: "tu_1".to_string(),
                is_error: false,
                timed_out: false,
            }],
        );
        let written = first
            .flush_wire_sessions(&path)
            .expect("flush to a writable temp path");
        assert_eq!(written, 1);

        let second = Manager::from_runtimes(vec![]);
        let report =
            second.restore_wire_sessions(&path, crate::session_wire_persist::RESTORE_TTL_MS);
        assert_eq!(report.degraded, None);
        assert_eq!(report.sessions.len(), 1);

        let snap = second.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        let row = &snap.wire_sessions[0];
        assert_eq!(row.session_id, "sess-restart");
        assert_eq!(row.account.as_deref(), Some("alice@example.com"));
        assert_eq!(
            row.tools.calls, 1,
            "the restored row is indistinguishable from a live one, tool counts included"
        );
    }

    /// Restoring must never clobber a session the SECOND manager already tracked
    /// between its own boot and this call — same "existing wins" rule
    /// `restore_affinity` follows for pins.
    #[test]
    fn restore_does_not_overwrite_an_existing_in_memory_session() {
        let path = tmp_wire_sessions_path("no-clobber");
        let now = OffsetDateTime::now_utc();

        let first = Manager::from_runtimes(vec![]);
        first.record_wire_session(
            Some("sess-shared"),
            Some("alice@example.com".to_string()),
            None,
            now,
            &[],
            &[],
        );
        first.flush_wire_sessions(&path).expect("flush");

        let second = Manager::from_runtimes(vec![]);
        // A request already served on the SECOND manager before restore runs.
        second.record_wire_session(
            Some("sess-shared"),
            Some("bob@example.com".to_string()),
            None,
            now,
            &[],
            &[],
        );
        second.restore_wire_sessions(&path, crate::session_wire_persist::RESTORE_TTL_MS);

        let snap = second.snapshot(now);
        assert_eq!(snap.wire_sessions.len(), 1);
        assert_eq!(
            snap.wire_sessions[0].account.as_deref(),
            Some("bob@example.com"),
            "the in-memory value, fresher than the file, must survive the restore"
        );
    }

    /// The dirty flag: unset on a fresh manager, set by a call that mutates the wire
    /// session table, and cleared by consuming it — the same debounce contract the
    /// flusher task in `server.rs` runs on.
    #[test]
    fn wire_sessions_dirty_flag_tracks_mutation_and_clears_on_take() {
        let manager = Manager::from_runtimes(vec![]);
        assert!(!manager.take_wire_sessions_dirty());

        manager.record_wire_session(
            Some("sess-dirty"),
            None,
            None,
            OffsetDateTime::now_utc(),
            &[],
            &[],
        );
        assert!(manager.take_wire_sessions_dirty());
        assert!(
            !manager.take_wire_sessions_dirty(),
            "a second read without an intervening write finds nothing new"
        );
    }

    /// A Codex session is unpriced until a price names its model. Instead of passing a zero off
    /// as measured, it says how many of its requests no price covers, while `costUsd` keeps
    /// its 0.0-for-unpriced meaning. Priced in part, it reports the priced part and the count it
    /// leaves out. A Claude session on a model this build cannot price reports no count and
    /// carries none of the Codex keys, so a Claude-only fleet's rows serialize as they did.
    #[test]
    fn codex_requests_count_as_unpriced() {
        let now = OffsetDateTime::now_utc();
        let manager = Manager::from_runtimes(vec![]);
        manager.record_codex_wire_session(
            Some("sess-codex"),
            Some("bob@example.com/codex".to_string()),
            now,
        );
        for _ in 0..3 {
            manager.record_codex_wire_session_usage(
                Some("sess-codex"),
                Some("gpt-5.1-codex"),
                1_000,
                0,
                0,
                0,
                10,
                2,
            );
        }
        manager.record_wire_session(
            Some("sess-claude"),
            Some("alice@example.com".to_string()),
            Some("claude-opus-4-5".to_string()),
            now,
            &[],
            &[],
        );
        manager.record_wire_session_usage(
            Some("sess-claude"),
            Some("claude-opus-4-5"),
            1_000,
            0,
            0,
            0,
            10,
        );
        let rows: std::collections::HashMap<String, tcr_status_wire::SessionRow> = manager
            .snapshot(now)
            .wire_sessions
            .into_iter()
            .map(|row| (row.session_id.clone(), row))
            .collect();
        let codex = &rows["sess-codex"];
        assert_eq!(codex.provider.as_deref(), Some("codex"));
        assert_eq!(codex.unpriced_requests, 3);
        assert_eq!(codex.cost_usd, 0.0);
        assert_eq!(codex.reasoning_tokens, 6);
        let claude = &rows["sess-claude"];
        assert_eq!(
            (claude.provider.as_deref(), claude.unpriced_requests),
            (None, 0)
        );
        let json = serde_json::to_value(claude).expect("a session row serializes");
        for key in ["provider", "unpricedRequests", "reasoningTokens"] {
            assert!(json.get(key).is_none(), "a Claude session carries no {key}");
        }

        let config: crate::config::Config =
            serde_json::from_str(r#"{"pricing":{"gpt-5.1-codex":{"input":1.25,"output":10}}}"#)
                .expect("a config with one price");
        let priced = Manager::with_live_refresher(config, None);
        priced.record_codex_wire_session(Some("sess-mixed"), None, now);
        for model in ["gpt-5.1-codex", "gpt-5.1-codex", "codex-mini-latest"] {
            priced.record_codex_wire_session_usage(
                Some("sess-mixed"),
                Some(model),
                1_000_000,
                0,
                0,
                0,
                0,
                0,
            );
        }
        let row = priced.snapshot(now).wire_sessions.remove(0);
        assert!(
            (row.cost_usd - 2.5).abs() < 1e-9,
            "two priced requests at $1.25: {}",
            row.cost_usd
        );
        assert_eq!(row.unpriced_requests, 1);
    }

    /// The producer proof for the canonical Codex session: the row TcrBar's decoder and the
    /// wire renderer are both tested against. Seven requests on one `session-id`, the calls
    /// their responses asked for, and their usage, fed through the Manager in time order,
    /// project onto exactly the fixture's row.
    ///
    /// Each call starts when its response's stream ends and closes when the session's next
    /// request arrives, and the last call is still running, with no timeout. Every
    /// `exec_command` call here, the last included, is codex's default interactive schema,
    /// which carries no timeout; the one-shot schema's timeout is proved inline, by
    /// `a_codex_timeout_is_whole_seconds_rounded_up` and
    /// `a_codex_function_call_starts_a_running_tool_with_its_timeout`.
    ///
    /// `byTool` is compared by tool name, because the server builds it from a map and promises
    /// no order.
    #[test]
    fn codex_sessions_are_produced_in_the_canonical_shape() {
        use crate::session_wire::{CodexToolCall, CommandClass, ToolUseEvent};
        const NOW: i64 = 1_767_225_600_000;
        const ID: &str = "019a3c5e-0000-7000-8000-000000000002";
        let at = |ms: i64| {
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
                .expect("a valid instant")
        };
        let call = |id: &str,
                    tool: &str,
                    head: &str,
                    class: Option<CommandClass>,
                    timeout_seconds: Option<u64>| {
            CodexToolCall {
                event: ToolUseEvent {
                    id: id.to_string(),
                    name: Some(tool.to_string()),
                    command_head: Some(head.to_string()),
                    command_class: class,
                },
                timeout_seconds,
            }
        };
        let build = Some(CommandClass::Build);
        // (request at, (its stream's end, the call it asked for), base input, cache read, output,
        // reasoning)
        type Turn = (i64, Option<(i64, CodexToolCall)>, u64, u64, u64, u64);
        // Every exec_command here is the default interactive schema, so none carries a timeout.
        let turns: [Turn; 7] = [
            (1_767_225_425_000, None, 35_000, 0, 1_100, 400),
            (
                1_767_225_470_000,
                Some((
                    1_767_225_481_000,
                    call("call_1", "exec_command", "cargo test", build, None),
                )),
                10_000,
                20_000,
                500,
                200,
            ),
            (1_767_225_485_000, None, 10_000, 20_000, 500, 200),
            (
                1_767_225_500_000,
                Some((
                    1_767_225_505_000,
                    call("call_2", "apply_patch", "apply_patch", None, None),
                )),
                10_000,
                20_000,
                500,
                200,
            ),
            (
                1_767_225_505_200,
                Some((
                    1_767_225_540_500,
                    call("call_3", "exec_command", "cargo check", build, None),
                )),
                10_000,
                20_000,
                500,
                200,
            ),
            (1_767_225_544_500, None, 10_000, 0, 500, 0),
            (
                1_767_225_550_000,
                Some((
                    1_767_225_555_000,
                    call("call_4", "exec_command", "cargo build", build, None),
                )),
                10_000,
                0,
                500,
                0,
            ),
        ];
        let manager = Manager::from_runtimes(vec![]);
        for (requested, started, input, cache_read, output, reasoning) in turns {
            manager.record_codex_wire_session(
                Some(ID),
                Some("bob@example.com/codex".to_string()),
                at(requested),
            );
            if let Some((ended, call)) = started {
                manager.record_codex_wire_session_calls(
                    Some(ID),
                    at(ended),
                    std::slice::from_ref(&call),
                );
            }
            manager.record_codex_wire_session_usage(
                Some(ID),
                Some("gpt-5.1-codex"),
                input,
                0,
                0,
                cache_read,
                output,
                reasoning,
            );
        }
        let row = manager
            .snapshot(at(NOW))
            .wire_sessions
            .into_iter()
            .find(|row| row.session_id == ID)
            .expect("the produced Codex session");
        let mut produced = serde_json::to_value(&row).expect("a session row serializes");
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("apps/macos/Tests/Fixtures/codex-sessions.json"),
            )
            .expect("the canonical sessions fixture"),
        )
        .expect("the fixture is JSON");
        let mut expected = fixture["sessions"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["sessionId"] == ID))
            .cloned()
            .expect("the fixture's Codex session");
        for value in [&mut produced, &mut expected] {
            if let Some(buckets) = value["tools"]["byTool"].as_array_mut() {
                buckets.sort_by(|a, b| a["tool"].as_str().cmp(&b["tool"].as_str()));
            }
        }
        let keys: std::collections::BTreeSet<String> = produced
            .as_object()
            .into_iter()
            .chain(expected.as_object())
            .flat_map(|fields| fields.keys().cloned())
            .collect();
        let differing: Vec<&String> = keys
            .iter()
            .filter(|key| produced.get(key.as_str()) != expected.get(key.as_str()))
            .collect();
        assert!(
            differing.is_empty(),
            "the produced Codex session differs from the fixture's in {differing:?}\nproduced: {produced:#}\nfixture:  {expected:#}"
        );
    }
}
