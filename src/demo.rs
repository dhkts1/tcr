//! `tcr demo` — render the live TUI against fake, in-memory accounts.
//!
//! This exists purely to capture a clean, sanitized dashboard screenshot for the
//! public README: the real TUI shows `@token.security` account names that must
//! never ship. The demo seeds a [`Manager`] with six believable Claude accounts in
//! varied states (healthy / near-limit / throttled / dead-cred) and three Codex ones
//! (a Pro login with the weekly window only, a Plus login with both windows, a Pro
//! login at its weekly limit), and hands it to the real dashboard
//! ([`crate::tui::run_with_facts`]) — NO server, NO prober, NO warmer, NO network,
//! NO real tokens. Quitting (`q` / `Ctrl-C`) and terminal restore reuse the TUI's
//! own run loop unchanged; nothing here is ever exercised by the serving path.

use std::io;
use std::sync::Arc;

use tcr_status_wire::CodexRoute;
use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex as AsyncMutex;

use crate::manager::{AccountRuntime, AccountStatus, Manager};
use crate::probe::ProbeStatus;
use crate::provider::Provider;
use crate::quota::{CodexWindow, CodexWindows, Quota, QuotaWindow};
use crate::stats::{RequestLogEntry, SessionKind};
use crate::tui::DashboardFacts;

/// A 5-hour quota window at `util`, resetting a few hours out so it reads live
/// (a future reset means [`QuotaWindow::effective`] returns the seeded value).
fn five_hour(util: f64) -> QuotaWindow {
    QuotaWindow {
        utilization: util,
        reset: Some(OffsetDateTime::now_utc() + Duration::hours(3)),
    }
}

/// A weekly quota window at `util`, resetting a few days out.
fn seven_day(util: f64) -> QuotaWindow {
    QuotaWindow {
        utilization: util,
        reset: Some(OffsetDateTime::now_utc() + Duration::days(4)),
    }
}

/// A believable base runtime row; callers clone it and override the varying
/// fields. Every account carries obviously-fake credentials.
fn base(name: &str, priority: i64) -> AccountRuntime {
    let now = crate::now_ms();
    AccountRuntime {
        name: name.to_string(),
        account_type: crate::config::AccountType::OAuth,
        provider: Default::default(),
        account_uuid: None,
        org_uuid: None,
        org_name: Some("Demo Org".to_string()),
        priority,
        disabled: false,
        removed: false,
        groups: Vec::new(),
        // A believable default plan, overridden per row below so the demo shows
        // the mix a real fleet has — which is the whole reason the plan is on
        // the row at all: two accounts that look identical until one says Max
        // 20x and the other Team.
        organization_type: Some("claude_max".to_string()),
        rate_limit_tier: Some("default_claude_max_20x".to_string()),
        seat_tier: None,
        switch_threshold: None,
        access_token: "demo-access-token".to_string(),
        refresh_token: Some("demo-refresh-token".to_string()),
        expires_at_ms: Some(now + 3_600_000),
        status: AccountStatus::Active,
        quota: Quota::default(),
        // The demo's rows are presented as already-probed (`probe_status: Ok`), so
        // their quota reads as READ too — otherwise every demo row would render as
        // a permanently un-warmable account.
        quota_known: true,
        consecutive_probe_failures: 0,
        consecutive_5xx_probes: 0,
        consecutive_warms_without_evidence: 0,
        warm_evidence_retry_after_ms: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        requests: 0,
        last_used_ms: None,
        last_selected_seq: 0,
        in_flight: 0,
        last_served_ms: 0,
        rate_limited_until_ms: None,
        hold_cause: crate::manager::HoldCause::RateLimit,
        login_error: None,
        codex_home: None,
        refresh_retry_after_ms: None,
        error_retry_after_ms: None,
        error_backoff_ms: 0,
        probe_status: ProbeStatus::Ok,
        last_probe_ms: Some(now - 12_000),
        probe_error: None,
        probe_retry_after_ms: None,
        stream_error_times_ms: std::collections::VecDeque::new(),
        last_stream_error: None,
        refresh_lock: Arc::new(AsyncMutex::new(())),
        http: crate::manager::build_serving_client(false),
        serves_since_client_build: 0,
    }
}

/// An age `secs` seconds in the past, in epoch-ms — for `last_used` columns.
fn ago_ms(secs: i64) -> i64 {
    crate::now_ms() - secs * 1000
}

/// A five-hour window's length, as a Codex reading reports it.
const FIVE_HOURS: CodexWindow = CodexWindow { minutes: Some(300) };

/// A weekly window's length, as a Codex reading reports it.
const ONE_WEEK: CodexWindow = CodexWindow {
    minutes: Some(10_080),
};

/// A Codex row: a ChatGPT login on `plan`, whose readings carried `windows`, as a usage probe
/// records them. Its windows' utilization is set by the caller.
fn codex(name: &str, plan: &str, windows: CodexWindows) -> AccountRuntime {
    let mut row = base(name, 0);
    row.provider = Provider::Codex;
    row.organization_type = Some(plan.to_string());
    row.quota.codex.probed_windows = Some(windows);
    row
}

/// Build the fake account rows described in the demo spec: the six Claude rows, then the
/// three Codex rows.
fn demo_accounts() -> Vec<AccountRuntime> {
    // alice — green, healthy, actively serving.
    let mut alice = base("alice", 0);
    alice.quota.five_hour = Some(five_hour(0.45));
    alice.quota.seven_day = Some(seven_day(0.20));
    alice.requests = 1200;
    alice.input_tokens = 4_200_000;
    alice.output_tokens = 310_000;
    alice.last_used_ms = Some(ago_ms(3));

    // bob — green, low use.
    let mut bob = base("bob", 0);
    bob.quota.five_hour = Some(five_hour(0.12));
    bob.quota.seven_day = Some(seven_day(0.08));
    bob.requests = 340;
    bob.input_tokens = 1_100_000;
    bob.output_tokens = 88_000;
    bob.last_used_ms = Some(ago_ms(120));

    // carol — near-limit (yellow "near" on the weekly bar). Her own lower
    // threshold makes 0.88 read as near without touching the fleet default.
    let mut carol = base("carol", 0);
    carol.switch_threshold = Some(0.85);
    carol.quota.five_hour = Some(five_hour(0.88));
    carol.quota.seven_day = Some(seven_day(0.76));
    carol.requests = 2100;
    carol.input_tokens = 7_800_000;
    carol.output_tokens = 540_000;
    carol.last_used_ms = Some(ago_ms(3600));

    // team-prod — green, and the CURRENT serving account (▶ marker).
    let mut team_prod = base("team-prod", 0);
    team_prod.organization_type = Some("claude_team".to_string());
    team_prod.rate_limit_tier = Some("default_claude_max_5x".to_string());
    team_prod.seat_tier = Some("team_tier_1".to_string());
    team_prod.quota.five_hour = Some(five_hour(0.30));
    team_prod.quota.seven_day = Some(seven_day(0.15));
    team_prod.requests = 890;
    team_prod.input_tokens = 2_600_000;
    team_prod.output_tokens = 190_000;
    team_prod.last_used_ms = Some(ago_ms(1));

    // dave — 5h window full (red "full"), parked out on a 429 throttle hold.
    let mut dave = base("dave", 10);
    dave.status = AccountStatus::Throttled;
    dave.rate_limited_until_ms = Some(crate::now_ms() + 15 * 60_000);
    dave.quota.five_hour = Some(five_hour(1.00));
    dave.quota.seven_day = Some(seven_day(0.40));
    dave.requests = 60;
    dave.input_tokens = 300_000;
    dave.output_tokens = 22_000;
    dave.last_used_ms = Some(ago_ms(720));

    // backup — dead credential: red "error" status, failing probe, no quota.
    let mut backup = base("backup", 10);
    backup.rate_limit_tier = Some("default_claude_max_5x".to_string());
    backup.status = AccountStatus::Error;
    backup.probe_status = ProbeStatus::Error;
    backup.probe_error = Some("refresh token rejected (HTTP 401)".to_string());
    backup.last_probe_ms = Some(ago_ms(45));

    // bob@example.com/codex — ChatGPT Pro, whose plan has the weekly window only, so
    // its 5h cell reads `none`. The account serving Codex (the Codex section's ▶).
    let mut codex_bob = codex(
        "bob@example.com/codex",
        "pro",
        CodexWindows {
            five_hour: None,
            seven_day: Some(ONE_WEEK),
        },
    );
    codex_bob.quota.seven_day = Some(seven_day(0.34));

    // dana — ChatGPT Plus, with both windows.
    let mut dana = codex(
        "dana@example.com",
        "plus",
        CodexWindows {
            five_hour: Some(FIVE_HOURS),
            seven_day: Some(ONE_WEEK),
        },
    );
    dana.quota.five_hour = Some(five_hour(0.18));
    dana.quota.seven_day = Some(QuotaWindow {
        utilization: 0.25,
        reset: Some(OffsetDateTime::now_utc() + Duration::days(5)),
    });

    // carol — ChatGPT Pro at its weekly limit, out until the window resets. `rel` rounds
    // down, so a reset exactly 3 days out would already paint `7D 2d` when the frame is
    // drawn a moment later; the 30 seconds keep it at `7D 3d`.
    let mut codex_carol = codex(
        "carol@example.com",
        "pro",
        CodexWindows {
            five_hour: None,
            seven_day: Some(ONE_WEEK),
        },
    );
    codex_carol.quota.seven_day = Some(QuotaWindow {
        utilization: 1.00,
        reset: Some(OffsetDateTime::now_utc() + Duration::days(3) + Duration::seconds(30)),
    });

    vec![
        alice,
        bob,
        carol,
        team_prod,
        dave,
        backup,
        codex_bob,
        dana,
        codex_carol,
    ]
}

/// A recent-request log entry `secs` seconds ago, for a request to `provider`.
fn log_entry(
    secs: i64,
    method: &str,
    path: &str,
    status: u16,
    account: &str,
    provider: Provider,
) -> RequestLogEntry {
    RequestLogEntry {
        time: OffsetDateTime::now_utc() - Duration::seconds(secs),
        method: method.to_string(),
        path: path.to_string(),
        status,
        account: account.to_string(),
        provider,
    }
}

/// The demo's fleet with its sessions and log seeded: everything `tcr demo` draws. A test
/// draws it too, so the frames it checks are the ones a person sees.
pub(crate) fn demo_manager() -> Arc<Manager> {
    let manager = Manager::from_runtimes(demo_accounts());

    // bob@example.com/codex (index 6, the first row after the six Claude ones) serves
    // Codex, then team-prod (index 3) serves Claude, so team-prod is the account served
    // last and each section paints its own ▶ marker.
    let codex_bob = 6;
    manager.set_current(codex_bob);
    manager.set_current(3);

    // Two live stable sessions so the sessions pane isn't empty in the screenshot,
    // and one Codex session.
    manager.seed_session(0xA1F3, 3, 24, ago_ms(1), SessionKind::Stable);
    manager.seed_session(0xB2C7, 0, 12, ago_ms(3), SessionKind::Stable);
    manager.seed_session(0xC0DE, codex_bob, 7, ago_ms(1), SessionKind::Stable);

    // A handful of recent requests, pushed oldest-first (the ring buffer is
    // reversed to most-recent-first when the snapshot is built).
    for entry in [
        log_entry(300, "GET", "/v1/models", 200, "bob", Provider::Anthropic),
        log_entry(
            120,
            "POST",
            "/v1/messages",
            200,
            "carol",
            Provider::Anthropic,
        ),
        log_entry(30, "POST", "/v1/messages", 429, "dave", Provider::Anthropic),
        log_entry(3, "POST", "/v1/messages", 200, "alice", Provider::Anthropic),
        log_entry(
            1,
            "POST",
            "/v1/messages",
            200,
            "team-prod",
            Provider::Anthropic,
        ),
        log_entry(
            1,
            "POST",
            "/backend-api/codex/responses",
            200,
            "bob@example.com/codex",
            Provider::Codex,
        ),
    ] {
        manager.push_log(entry);
    }

    manager
}

/// The facts the demo draws with, fixed for the whole run: Codex pooled, and nothing else
/// known. The demo then reads none of this machine's MITM state, CA, codex login or home
/// folder, so it shows no not-pooled, refused or unpooled suffix, and a screenshot of it can
/// print no path of this machine.
pub(crate) fn demo_facts() -> DashboardFacts {
    DashboardFacts {
        codex_route: Some(CodexRoute {
            pooled: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `tcr demo` — seed a fake fleet and launch the real TUI. Purely for a sanitized
/// README screenshot; returns once the TUI's own quit handling restores the terminal.
pub async fn run_demo() -> io::Result<()> {
    crate::tui::run_with_facts(demo_manager(), demo_facts()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{AccountSnapshot, GateReason};

    /// `tcr demo` shows a fleet of both providers, so its screenshot shows what tcr draws for
    /// Codex accounts: three Codex rows beside the six Claude ones, one plan per kind of window
    /// a plan reports, a Codex session, a Codex request in the log, and the account serving each
    /// provider.
    #[test]
    fn the_demo_seeds_codex_rows_a_session_and_a_log_line() {
        let snapshot = demo_manager().snapshot(OffsetDateTime::now_utc());
        let of = |provider: Provider| -> Vec<&AccountSnapshot> {
            snapshot
                .accounts
                .iter()
                .filter(|a| a.provider == provider)
                .collect()
        };
        let names = |rows: &[&AccountSnapshot]| -> Vec<String> {
            rows.iter().map(|a| a.name.clone()).collect()
        };

        let codex = of(Provider::Codex);
        assert_eq!(
            names(&codex),
            [
                "bob@example.com/codex",
                "dana@example.com",
                "carol@example.com"
            ]
        );
        assert_eq!(
            codex.iter().map(|a| a.plan_label()).collect::<Vec<_>>(),
            [
                Some("ChatGPT Pro".to_string()),
                Some("ChatGPT Plus".to_string()),
                Some("ChatGPT Pro".to_string()),
            ]
        );
        assert_eq!(
            codex[0].codex.quota_windows,
            Some(vec!["sevenDay".to_string()])
        );
        assert_eq!(
            codex[1].codex.quota_windows,
            Some(vec!["fiveHour".to_string(), "sevenDay".to_string()])
        );
        assert_eq!(codex[2].gate, GateReason::SevenDay);
        assert!(
            codex[2]
                .free_at
                .is_some_and(|at| at > OffsetDateTime::now_utc()),
            "carol's weekly window resets in the future"
        );
        assert_eq!(
            names(&of(Provider::Anthropic)),
            ["alice", "bob", "carol", "team-prod", "dave", "backup"]
        );

        assert!(
            snapshot
                .sessions
                .iter()
                .any(|s| s.provider == Provider::Codex),
            "a Codex session is seeded"
        );
        assert!(
            snapshot
                .recent
                .iter()
                .any(|e| e.provider == Provider::Codex && e.path == "/backend-api/codex/responses"),
            "a Codex request is in the log"
        );

        let position = |name: &str| {
            snapshot
                .accounts
                .iter()
                .position(|a| a.name == name)
                .expect("the demo seeds it")
        };
        assert_eq!(
            snapshot.serving(Provider::Anthropic),
            Some(position("team-prod"))
        );
        assert_eq!(
            snapshot.serving(Provider::Codex),
            Some(position("bob@example.com/codex"))
        );
    }
}
