//! [`Manager::routing_candidates`] is the selector's own answer for a new, unpinned
//! request: the pass that would serve it and every account that pass could choose,
//! read without changing anything a selection reads.
use super::select_test_support::{account, config_with, live_manager, pin};
use super::*;
use crate::quota::QuotaWindow;
use time::Duration;

const MESSAGES: &str = "/v1/messages";
const MODELS: [Option<&str>; 3] = [None, Some("claude-opus-5-5"), Some("claude-fable-5-1")];

/// Three healthy Anthropic accounts, `tune` applied to their config.
fn fleet(tune: fn(&mut Config)) -> Arc<Manager> {
    let mut config = config_with(vec![
        account("a@example.com"),
        account("b@example.com"),
        account("c@example.com"),
    ]);
    tune(&mut config);
    live_manager(config)
}

/// Change every listed account's runtime, as upstream answers and holds would.
fn each(manager: &Manager, slots: &[usize], change: impl Fn(&mut AccountRuntime)) {
    let mut accounts = manager
        .accounts
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    for &slot in slots {
        change(&mut accounts[slot]);
    }
}

fn window(utilization: f64, now: OffsetDateTime) -> Option<QuotaWindow> {
    Some(QuotaWindow {
        utilization,
        reset: Some(now + Duration::hours(2)),
    })
}

/// What a new, unpinned request for `model` is served from, through the calls the proxy
/// makes: `select`, then the revalidation serve when it is on, no lock is set and `select`
/// found nothing.
fn served(manager: &Manager, now: OffsetDateTime, model: Option<&str>) -> Option<usize> {
    let none = HashSet::new();
    manager
        .select(&none, now, model, None, MESSAGES, None)
        .or_else(|| {
            (manager.revalidation_serve_enabled()
                && manager
                    .locked_account_for(crate::provider::Provider::Anthropic)
                    .is_none())
            .then(|| manager.select_revalidation(&none, now, model, None, MESSAGES))
            .flatten()
        })
}

struct Case {
    name: &'static str,
    tune: fn(&mut Config),
    setup: fn(&Manager, OffsetDateTime),
    /// The pass and its candidates, for no model, an Opus model and a Fable model.
    expect: [(Option<PoolPass>, &'static [usize]); 3],
}

const ALL: &[usize] = &[0, 1, 2];

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "a healthy fleet",
            tune: |_| {},
            setup: |_, _| {},
            expect: [(Some(PoolPass::Paced), ALL); 3],
        },
        Case {
            name: "one held, one disabled",
            tune: |_| {},
            setup: |manager, now| {
                let until = odt_to_ms(now) + 600_000;
                each(manager, &[0], |a| a.rate_limited_until_ms = Some(until));
                each(manager, &[1], |a| a.disabled = true);
            },
            expect: [(Some(PoolPass::Paced), &[2]); 3],
        },
        Case {
            name: "Fable predicted out on two",
            tune: |_| {},
            setup: |manager, now| {
                each(manager, &[0, 1], |a| {
                    a.quota.seven_day_oi = window(0.97, now);
                });
            },
            expect: [
                (Some(PoolPass::Paced), ALL),
                (Some(PoolPass::Paced), ALL),
                (Some(PoolPass::Paced), &[2]),
            ],
        },
        Case {
            name: "Fable predicted out everywhere",
            tune: |_| {},
            setup: |manager, now| {
                each(manager, ALL, |a| a.quota.seven_day_oi = window(0.97, now));
            },
            expect: [
                (Some(PoolPass::Paced), ALL),
                (Some(PoolPass::Paced), ALL),
                (Some(PoolPass::FableLastResort), ALL),
            ],
        },
        Case {
            name: "Fable rejected everywhere",
            tune: |_| {},
            setup: |manager, now| {
                each(manager, ALL, |a| a.quota.seven_day_oi = window(1.0, now));
            },
            expect: [
                (Some(PoolPass::Paced), ALL),
                (Some(PoolPass::Paced), ALL),
                (None, &[]),
            ],
        },
        Case {
            name: "every account paced",
            tune: |config| {
                config.pacing.max_in_flight_per_account = Some(1);
            },
            setup: |manager, _| each(manager, ALL, |a| a.in_flight = 1),
            expect: [(Some(PoolPass::LeastLoaded), ALL); 3],
        },
        Case {
            name: "every account over the switch threshold",
            tune: |_| {},
            setup: |manager, now| {
                each(manager, ALL, |a| a.quota.five_hour = window(0.95, now));
            },
            expect: [(Some(PoolPass::Revalidation), ALL); 3],
        },
        Case {
            name: "every account over the switch threshold, revalidation off",
            tune: |config| {
                config
                    .extra
                    .insert("revalidationServe".to_string(), serde_json::json!(false));
            },
            setup: |manager, now| {
                each(manager, ALL, |a| a.quota.five_hour = window(0.95, now));
            },
            expect: [(None, &[]); 3],
        },
        Case {
            name: "every account held",
            tune: |_| {},
            setup: |manager, now| {
                let until = odt_to_ms(now) + 600_000;
                each(manager, ALL, |a| a.rate_limited_until_ms = Some(until));
            },
            expect: [(None, &[]); 3],
        },
        Case {
            name: "the control account is not the pool's",
            tune: |config| {
                config.control_account =
                    Some(config::ControlAccountRef::Name("a@example.com".to_string()));
            },
            setup: |_, _| {},
            expect: [(Some(PoolPass::Paced), &[1, 2]); 3],
        },
        Case {
            name: "a lock serves its account whatever its gates",
            tune: |config| config.lock_account = Some("b@example.com".to_string()),
            setup: |manager, _| each(manager, &[1], |a| a.disabled = true),
            expect: [(Some(PoolPass::Lock), &[1]); 3],
        },
    ]
}

/// **Routing reads what the picks serve.** For each fleet and model, the pass routing
/// names is the one that serves, a model is routable exactly when a new request for it
/// is served, and the account served is one routing counted.
///
/// Each model gets a fresh fleet: a pick stamps its account, and a revalidation serve
/// spends the window the next one would need.
#[test]
fn routing_reads_what_the_picks_serve() {
    for case in cases() {
        for (model, (pass, accounts)) in MODELS.into_iter().zip(case.expect) {
            let manager = fleet(case.tune);
            let now = OffsetDateTime::now_utc();
            (case.setup)(&manager, now);
            let found = manager.routing_candidates(now, model, MESSAGES);
            let name = case.name;
            assert_eq!(
                (found.pass, found.accounts.as_slice()),
                (pass, accounts),
                "{name}, {model:?}"
            );
            let pick = served(&manager, now, model);
            assert_eq!(
                pick.is_some(),
                found.pass.is_some(),
                "{name}, {model:?}: routable must be exactly what a new request is served"
            );
            if let Some(idx) = pick {
                assert!(
                    found.accounts.contains(&idx),
                    "{name}, {model:?}: served {idx}, routing counted {:?}",
                    found.accounts
                );
            }
        }
    }
}

/// Everything a selection reads and writes besides the gates themselves.
#[derive(Debug, PartialEq)]
struct SelectorState {
    stamps: Vec<u64>,
    select_seq: u64,
    pins: Vec<(u64, (usize, i64))>,
    revalidation_at: (i64, i64),
    connections: usize,
    diverts: usize,
}

fn selector_state(manager: &Manager) -> SelectorState {
    let mut pins: Vec<(u64, (usize, i64))> = manager
        .affinity
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(&key, &pin)| (key, pin))
        .collect();
    pins.sort_unstable();
    SelectorState {
        stamps: manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|a| a.last_selected_seq)
            .collect(),
        select_seq: manager.select_seq.load(Ordering::Relaxed),
        pins,
        revalidation_at: (
            manager.next_revalidation_at_ms.load(Ordering::Relaxed),
            manager
                .next_codex_revalidation_at_ms
                .load(Ordering::Relaxed),
        ),
        connections: manager
            .conn_affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        diverts: manager
            .divert_ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
    }
}

/// **A routing read changes nothing the selector reads**: no stamp, no pin, no
/// revalidation window spent. A fleet that was read selects exactly as its unread twin
/// does, on a healthy fleet and on one only revalidation can serve.
#[test]
fn a_routing_read_changes_no_selector_state() {
    for over_threshold in [false, true] {
        let now = OffsetDateTime::now_utc();
        let setup = |manager: &Manager| {
            pin(manager, 7, 0, odt_to_ms(now));
            if over_threshold {
                each(manager, ALL, |a| a.quota.five_hour = window(0.95, now));
            }
        };
        let read = fleet(|_| {});
        let twin = fleet(|_| {});
        setup(&read);
        setup(&twin);
        let before = selector_state(&read);
        for model in MODELS {
            assert_eq!(
                read.routing_candidates(now, model, MESSAGES),
                read.routing_candidates(now, model, MESSAGES),
                "over_threshold={over_threshold}, {model:?}: a read does not change the next read"
            );
        }
        assert_eq!(
            selector_state(&read),
            before,
            "over_threshold={over_threshold}"
        );
        for turn in 0..3 {
            assert_eq!(
                served(&read, now, MODELS[1]),
                served(&twin, now, MODELS[1]),
                "over_threshold={over_threshold}, turn {turn}: the read fleet selects as its twin"
            );
        }
        assert_eq!(selector_state(&read), selector_state(&twin));
    }
}
