//! Spending a banked reset before it expires unused (Gil, 2026-11-17: "auto use reset
//! when its going to expire").
//!
//! A banked reset that reaches its use-by unspent is lost. So, once a minute, tcr looks at
//! what the last usage read said each account holds, and spends any reset whose use-by is
//! less than [`LEAD_MS`] away, through the same claim path the panel's Reset button and
//! `tcr reset` take ([`Manager::reset_claude_at`], [`Manager::reset_codex_at`]). Spending
//! late, not early, is deliberate: the later the spend, the more usage there is for the
//! reset to clear, and two hours still leaves room for a probe or a refresh to fail once.
//!
//! **Each reset is tried once.** Its key (slot, kind, use-by) goes into [`Ledger::attempted`]
//! before the claim, and the Claude claim sends that key as its `request_id`, so neither a
//! second tick nor a retry can spend two. Whatever the answer, including "nothing to
//! reset" for a reset that needs the account at its limit first, it is recorded and not
//! retried: hammering a refusal costs requests and changes nothing.
//!
//! It reads in-memory state only; the minute tick sends no request unless a reset is due.
//! `"autoUseExpiringResets": false` in the config turns it off.

use super::claude_reset::{ClaudeResetError, ClaudeResetOutcome};
use super::codex_reset::CodexResetOutcome;
use super::*;
use crate::claude_resets::ClaudeResetBank;
use crate::provider::Provider;
use std::time::Duration;
use tcr_status_wire::{AutoReset, AutoResetOutcome, ResetCredits, ResetKind};

/// How long before its use-by a reset is spent: two hours.
pub const LEAD_MS: i64 = 2 * 3_600_000;
/// How long the last attempt stays on the status wire: a day.
pub const SHOWN_MS: i64 = 86_400_000;
/// How often the bank is looked at.
pub const TICK: Duration = Duration::from_secs(60);

/// What the automatic spends have done in this process.
#[derive(Debug, Default)]
pub struct Ledger {
    /// Every reset already tried, by [`Due::key`].
    attempted: HashSet<String>,
    /// Each slot's last attempt.
    last: HashMap<usize, AutoReset>,
}

/// One reset to spend now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    pub idx: usize,
    pub provider: Provider,
    pub kind: ResetKind,
    pub ends_at_ms: i64,
}

impl Due {
    /// The attempt's key, also the Claude claim's `request_id`: inside both the Claude
    /// (`^[A-Za-z0-9_-]{1,64}$`) and the local Codex alphabet.
    pub fn key(&self) -> String {
        format!(
            "auto-{}-{}-{}",
            self.idx,
            self.kind.token(),
            self.ends_at_ms
        )
    }
}

const fn inside(ends_at_ms: i64, now_ms: i64) -> bool {
    ends_at_ms - LEAD_MS <= now_ms && now_ms < ends_at_ms
}

/// The resets one row should spend at `now_ms`: for a Claude row, of each kind held, the
/// grant whose use-by comes first, when that use-by is inside the window and the grant is
/// neither paused nor spent; for a Codex row, its one earliest credit. `claude` is the bank
/// the row may spend ([`crate::quota::Quota::eligible_claude_resets`]) and `codex` the
/// credits it holds ([`crate::quota::Quota::spendable_codex_reset_credits`]); nothing when
/// either is `None`.
pub fn due_in(
    idx: usize,
    provider: Provider,
    claude: Option<&ClaudeResetBank>,
    codex: Option<&ResetCredits>,
    now_ms: i64,
) -> Vec<Due> {
    match provider {
        Provider::Anthropic => {
            let Some(bank) = claude else {
                return Vec::new();
            };
            let mut first: Vec<Due> = Vec::new();
            for grant in &bank.grants {
                let reset = &grant.reset;
                let Some(ends_at_ms) = reset.ends_at_ms else {
                    continue;
                };
                if reset.resets_left == 0 || reset.paused {
                    continue;
                }
                match first.iter_mut().find(|due| due.kind == reset.kind) {
                    Some(due) if due.ends_at_ms <= ends_at_ms => {}
                    Some(due) => due.ends_at_ms = ends_at_ms,
                    None => first.push(Due {
                        idx,
                        provider,
                        kind: reset.kind,
                        ends_at_ms,
                    }),
                }
            }
            first.retain(|due| inside(due.ends_at_ms, now_ms));
            first
        }
        Provider::Codex => codex
            .and_then(|credits| credits.earliest_expiry_ms)
            .filter(|&ends_at_ms| inside(ends_at_ms, now_ms))
            .map(|ends_at_ms| Due {
                idx,
                provider,
                kind: ResetKind::Weekly,
                ends_at_ms,
            })
            .into_iter()
            .collect(),
    }
}

impl From<ClaudeResetOutcome> for AutoResetOutcome {
    fn from(outcome: ClaudeResetOutcome) -> Self {
        match outcome {
            ClaudeResetOutcome::Applied => Self::Applied,
            ClaudeResetOutcome::AlreadyUsed => Self::AlreadyUsed,
            ClaudeResetOutcome::NothingToReset => Self::NeedsLimit,
            ClaudeResetOutcome::NotUsableYet => Self::NotUsableYet,
            ClaudeResetOutcome::NoResetLeft => Self::NoneLeft,
        }
    }
}

impl From<ClaudeResetError> for AutoResetOutcome {
    fn from(error: ClaudeResetError) -> Self {
        match error {
            ClaudeResetError::NoneHeld => Self::NoneLeft,
            ClaudeResetError::Missing
            | ClaudeResetError::Ambiguous
            | ClaudeResetError::WrongProvider
            | ClaudeResetError::Unavailable
            | ClaudeResetError::NotRead
            | ClaudeResetError::KindNeeded
            | ClaudeResetError::Auth
            | ClaudeResetError::Failed
            | ClaudeResetError::Unknown => Self::Failed,
        }
    }
}

impl From<CodexResetOutcome> for AutoResetOutcome {
    fn from(outcome: CodexResetOutcome) -> Self {
        match outcome {
            CodexResetOutcome::Applied => Self::Applied,
            CodexResetOutcome::NothingToReset => Self::NeedsLimit,
            CodexResetOutcome::NoCredit => Self::NoneLeft,
            CodexResetOutcome::AlreadyRedeemed => Self::AlreadyUsed,
        }
    }
}

impl Manager {
    /// See [`crate::config::Config::auto_reset_enabled`].
    pub fn auto_reset_enabled(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .auto_reset_enabled()
    }

    /// Spends every reset due at `now_ms` that was not tried before, one at a time, and
    /// returns how many it tried. The only automatic caller of the claim paths.
    pub async fn spend_expiring_resets(&self, now_ms: i64) -> usize {
        if !self.auto_reset_enabled() {
            return 0;
        }
        let due: Vec<Due> = {
            let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
            accounts
                .iter()
                .enumerate()
                .filter(|(_, row)| !row.removed)
                .flat_map(|(idx, row)| {
                    due_in(
                        idx,
                        row.provider,
                        row.quota.eligible_claude_resets(),
                        row.quota.spendable_codex_reset_credits(),
                        now_ms,
                    )
                })
                .collect()
        };
        let mut tried = 0;
        for due in due {
            let key = due.key();
            if !self
                .auto_resets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .attempted
                .insert(key.clone())
            {
                continue;
            }
            tried += 1;
            let target = ResetTarget::Slot(due.idx);
            let outcome = match due.provider {
                Provider::Anthropic => {
                    match self.reset_claude_at(target, Some(due.kind), &key).await {
                        Ok(response) => response.outcome.into(),
                        Err(error) => error.into(),
                    }
                }
                Provider::Codex => match self.reset_codex_at(target, &key).await {
                    Ok(response) => response.outcome.into(),
                    Err(_) => AutoResetOutcome::Failed,
                },
            };
            tracing::info!(
                index = due.idx,
                kind = due.kind.token(),
                outcome = outcome.token(),
                "reset auto-used before its use-by"
            );
            self.auto_resets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .last
                .insert(
                    due.idx,
                    AutoReset {
                        kind: due.kind,
                        at_ms: now_ms,
                        outcome,
                    },
                );
        }
        tried
    }

    /// The slot's last automatic attempt, while it is under [`SHOWN_MS`] old.
    pub(super) fn auto_reset_for(&self, idx: usize, now_ms: i64) -> Option<AutoReset> {
        self.auto_resets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last
            .get(&idx)
            .filter(|last| now_ms - last.at_ms < SHOWN_MS)
            .cloned()
    }
}

/// The loop the server spawns: [`Manager::spend_expiring_resets`] every [`TICK`].
pub async fn run(manager: Arc<Manager>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        manager.spend_expiring_resets(crate::now_ms()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_resets::Grant;
    use tcr_status_wire::BankedReset;

    const ENDS: i64 = 1_900_000_000_000;

    fn grant(id: &str, kind: ResetKind, left: u32, paused: bool, ends: Option<i64>) -> Grant {
        Grant {
            id: id.to_owned(),
            reset: BankedReset {
                kind,
                resets_left: left,
                usable_now: true,
                paused,
                requires_limit: false,
                ends_at_ms: ends,
                unavailable_reason: None,
            },
        }
    }

    fn bank(grants: Vec<Grant>) -> ClaudeResetBank {
        ClaudeResetBank {
            eligible: true,
            reason: None,
            grants,
        }
    }

    fn claude(bank: &ClaudeResetBank, now_ms: i64) -> Vec<Due> {
        due_in(3, Provider::Anthropic, Some(bank), None, now_ms)
    }

    /// Due from two hours before the use-by until it, and not a millisecond outside.
    #[test]
    fn a_reset_is_due_only_in_the_two_hours_before_its_use_by() {
        let held = bank(vec![grant(
            "fake-a",
            ResetKind::Weekly,
            1,
            false,
            Some(ENDS),
        )]);
        let due = Due {
            idx: 3,
            provider: Provider::Anthropic,
            kind: ResetKind::Weekly,
            ends_at_ms: ENDS,
        };
        assert_eq!(claude(&held, ENDS - LEAD_MS - 1), vec![]);
        assert_eq!(claude(&held, ENDS - LEAD_MS), vec![due]);
        assert_eq!(claude(&held, ENDS - 1), vec![due]);
        assert_eq!(claude(&held, ENDS), vec![], "at its use-by it is gone");
        assert_eq!(due.key(), "auto-3-weekly-1900000000000");
        assert!(crate::claude_resets::valid_request_id(&due.key()));
        assert!(valid_reset_key(&due.key()));
    }

    /// Of each kind the earliest grant counts, so a later grant of a kind never hides an
    /// expiring one, and a spent, paused or undated grant is never due.
    #[test]
    fn each_kind_is_judged_by_its_earliest_spendable_grant() {
        let held = bank(vec![
            grant(
                "fake-late",
                ResetKind::Weekly,
                1,
                false,
                Some(ENDS + 86_400_000),
            ),
            grant("fake-early", ResetKind::Weekly, 1, false, Some(ENDS)),
            grant("fake-spent", ResetKind::Fable, 0, false, Some(ENDS)),
            grant("fake-paused", ResetKind::Full, 1, true, Some(ENDS)),
            grant("fake-undated", ResetKind::Session, 1, false, None),
        ]);
        let kinds: Vec<ResetKind> = claude(&held, ENDS - 60_000)
            .iter()
            .map(|d| d.kind)
            .collect();
        assert_eq!(kinds, vec![ResetKind::Weekly]);
        let ineligible = Quota {
            claude_resets: Some(ClaudeResetBank {
                eligible: false,
                ..held
            }),
            ..Quota::default()
        };
        assert_eq!(
            due_in(
                3,
                Provider::Anthropic,
                ineligible.eligible_claude_resets(),
                None,
                ENDS - 60_000
            ),
            vec![]
        );
        assert_eq!(
            due_in(3, Provider::Anthropic, None, None, ENDS - 60_000),
            vec![]
        );
    }

    /// A Codex row spends its earliest credit, and only while it holds one.
    #[test]
    fn a_codex_credit_is_due_by_its_earliest_expiry() {
        let credits = |count| ResetCredits {
            available_count: count,
            earliest_expiry_ms: Some(ENDS),
        };
        let due = due_in(5, Provider::Codex, None, Some(&credits(2)), ENDS - 60_000);
        assert_eq!(
            due,
            vec![Due {
                idx: 5,
                provider: Provider::Codex,
                kind: ResetKind::Weekly,
                ends_at_ms: ENDS
            }]
        );
        let mut spent = Quota::default();
        spent.codex.reset_credits = Some(credits(0));
        assert_eq!(
            due_in(
                5,
                Provider::Codex,
                None,
                spent.spendable_codex_reset_credits(),
                ENDS - 60_000
            ),
            vec![]
        );
        assert_eq!(
            due_in(
                5,
                Provider::Codex,
                None,
                Some(&credits(2)),
                ENDS - LEAD_MS - 1
            ),
            vec![]
        );
    }
}
