//! Claude's banked limit resets: one-time refills Anthropic grants an account, each with a
//! use-by date. Claude Code reads and spends them as `/limit-reset`; internally the program
//! is `cedar_ember`.
//!
//! **Read.** The usage probe asks for them by adding `?cedar_ember=1` to the request it
//! already makes ([`crate::records::probe::USAGE_QUERY`]), so no account gets a second request. Not
//! `skip_spend=1`, which Claude Code also sends: measured 2026-11-16 on one live account,
//! `skip_spend=1` answers `extra_usage: null`, which tcr reads for the overage figure, while
//! `?cedar_ember=1` alone keeps `extra_usage` and still carries the grants.
//!
//! **Unknown is not zero.** Without a Claude Code user agent the block answers
//! `eligible: false, ineligible_reason: "surface"` and no grants on every account (measured
//! by the lead, 2026-11-16), so the probe sends [`crate::records::probe::CLAUDE_CODE_USER_AGENT`]. An
//! ineligible answer is kept as exactly that and reads as unknown everywhere; a missing block
//! is not read at all.
//!
//! **Spend.** [`crate::manager::Manager::reset_claude`] alone posts a claim, and only for an
//! operator's confirmed request. Grant ids stay in this process: the status wire carries the
//! grants without them ([`ClaudeResetBank::wire`]).

use serde_json::Value;
use tcr_status_wire::{BankedReset, BankedResets, ResetKind};

/// The `program` a claim names.
pub const PROGRAM: &str = "cedar_ember";

/// One grant as Anthropic holds it: its id, which a claim names, and what the wire shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub reset: BankedReset,
}

/// What the last successful usage read said about an account's banked resets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeResetBank {
    pub eligible: bool,
    /// Anthropic's `ineligible_reason`, verbatim, on an ineligible answer.
    pub reason: Option<String>,
    pub grants: Vec<Grant>,
}

/// Why no grant can be picked for a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickError {
    /// The account holds no reset of the asked kind, or none at all.
    NoneHeld,
    /// No kind was asked and the account holds more than one.
    KindNeeded,
}

impl ClaudeResetBank {
    /// The grants without their ids, for the status wire.
    pub fn wire(&self) -> BankedResets {
        BankedResets {
            eligible: self.eligible,
            reason: self.reason.clone(),
            grants: self.grants.iter().map(|g| g.reset.clone()).collect(),
        }
    }

    /// The grant a claim spends: of the asked kind, or of the one kind held when none was
    /// asked, the one whose use-by comes first. A grant with no use-by sorts last. Paused
    /// grants and spent ones are never picked.
    pub fn pick(&self, kind: Option<ResetKind>) -> Result<&Grant, PickError> {
        let held: Vec<&Grant> = self
            .grants
            .iter()
            .filter(|g| g.reset.resets_left > 0 && !g.reset.paused)
            .collect();
        let kind = if let Some(kind) = kind {
            kind
        } else {
            let mut kinds: Vec<ResetKind> = held.iter().map(|g| g.reset.kind).collect();
            kinds.sort();
            kinds.dedup();
            match kinds.as_slice() {
                [] => return Err(PickError::NoneHeld),
                [one] => *one,
                _ => return Err(PickError::KindNeeded),
            }
        };
        held.into_iter()
            .filter(|g| g.reset.kind == kind)
            .min_by_key(|g| g.reset.ends_at_ms.unwrap_or(i64::MAX))
            .ok_or(PickError::NoneHeld)
    }
}

/// A grant id a claim may name: the alphabet and bound Claude Code itself checks before it
/// sends one (`^[a-z0-9_-]{1,40}$`).
pub fn valid_grant_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// A request id a claim may carry, Claude Code's own bound (`^[A-Za-z0-9_-]{1,64}$`).
pub fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The payload's `cedar_ember` block, read. `None` when the block is absent or not an
/// object, which learns nothing. A grant that does not parse (an id outside the alphabet,
/// no count) is dropped, as Claude Code drops it.
pub fn parse(block: Option<&Value>) -> Option<ClaudeResetBank> {
    let block = block?.as_object()?;
    let eligible = block.get("eligible").and_then(Value::as_bool)?;
    let reason = block
        .get("ineligible_reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let grants = if eligible {
        block
            .get("grants")
            .and_then(Value::as_array)
            .map_or_default(|grants| grants.iter().filter_map(parse_grant).collect())
    } else {
        Vec::new()
    };
    Some(ClaudeResetBank {
        eligible,
        reason,
        grants,
    })
}

fn parse_grant(grant: &Value) -> Option<Grant> {
    let id = grant.get("id").and_then(Value::as_str)?;
    if !valid_grant_id(id) {
        return None;
    }
    let resets_left = u32::try_from(grant.get("resets_left").and_then(Value::as_u64)?).ok()?;
    let flag = |key: &str, absent: bool| grant.get(key).and_then(Value::as_bool).unwrap_or(absent);
    Some(Grant {
        id: id.to_owned(),
        reset: BankedReset {
            kind: kind_of(grant),
            resets_left,
            usable_now: flag("usable_now", false),
            paused: flag("paused", false),
            // Claude Code defaults this one to true: unknown means "wait for the limit".
            requires_limit: flag("use_requires_limit", true),
            ends_at_ms: grant
                .get("ends_at")
                .and_then(Value::as_str)
                .and_then(rfc3339_ms),
            unavailable_reason: grant
                .get("unavailable_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
    })
}

/// Claude Code's rule for a grant's kind: Fable when it gives back weekly budget; else its
/// `type`; else from what it clears: the 5-hour window alone is a session reset, no 5-hour
/// window a weekly one, both a full one. Read from Claude Code 2.1.325, where a live Fable
/// grant carries `type: "full"` and clears only `seven_day_overage_included`.
pub fn kind_of(grant: &Value) -> ResetKind {
    if grant.get("gives_back_weekly").and_then(Value::as_bool) == Some(true) {
        return ResetKind::Fable;
    }
    let typed = grant
        .get("type")
        .and_then(Value::as_str)
        .and_then(ResetKind::parse)
        .filter(|kind| *kind != ResetKind::Fable);
    if let Some(kind) = typed {
        return kind;
    }
    let clears: Vec<&str> = grant
        .get("clears")
        .and_then(Value::as_array)
        .map_or_default(|c| c.iter().filter_map(Value::as_str).collect());
    let five = clears.contains(&"five_hour");
    let seven = clears.iter().any(|c| *c != "five_hour");
    match (five, seven) {
        (true, false) => ResetKind::Session,
        (false, _) => ResetKind::Weekly,
        (true, true) => ResetKind::Full,
    }
}

fn rfc3339_ms(text: &str) -> Option<i64> {
    let at =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grant(id: &str, extra: Value) -> Value {
        let mut g = json!({"id": id, "label": "fake label", "resets_left": 1, "resets_total": 1,
            "usable_now": true, "paused": false, "use_requires_limit": false,
            "ends_at": "2030-01-01T00:00:00Z", "clears": ["seven_day"], "type": "weekly"});
        for (k, v) in extra.as_object().unwrap() {
            g[k] = v.clone();
        }
        g
    }

    #[test]
    fn kinds_follow_claude_codes_rule() {
        let fable = json!({"type": "full", "gives_back_weekly": true, "clears": ["seven_day_overage_included"]});
        assert_eq!(kind_of(&fable), ResetKind::Fable);
        assert_eq!(kind_of(&json!({"type": "full"})), ResetKind::Full);
        assert_eq!(
            kind_of(&json!({"type": "weekly", "clears": ["five_hour", "seven_day"]})),
            ResetKind::Weekly
        );
        assert_eq!(
            kind_of(&json!({"clears": ["five_hour"]})),
            ResetKind::Session
        );
        assert_eq!(
            kind_of(&json!({"clears": ["seven_day", "seven_day_overage_included"]})),
            ResetKind::Weekly
        );
        assert_eq!(
            kind_of(&json!({"clears": ["five_hour", "seven_day"]})),
            ResetKind::Full
        );
        assert_eq!(
            kind_of(&json!({"type": "something-new", "clears": ["five_hour"]})),
            ResetKind::Session
        );
    }

    /// The trap the lead measured: without Claude Code's user agent every account answers
    /// ineligible with reason "surface". That is kept as ineligible, with no grants, and the
    /// wire says so: unknown, never a count of zero.
    #[test]
    fn a_surface_answer_is_ineligible_not_zero() {
        let bank = parse(Some(
            &json!({"eligible": false, "ineligible_reason": "surface", "grants": []}),
        ))
        .unwrap();
        assert!(!bank.eligible);
        assert_eq!(bank.reason.as_deref(), Some("surface"));
        let wire = bank.wire();
        assert!(!wire.eligible);
        assert!(wire.grants.is_empty());
        assert!(wire.held().is_empty());
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({"eligible": false, "reason": "surface", "grants": []})
        );
    }

    #[test]
    fn an_absent_or_malformed_block_learns_nothing() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some(&Value::Null)), None);
        assert_eq!(parse(Some(&json!({"grants": []}))), None);
    }

    #[test]
    fn grants_parse_without_ids_on_the_wire_and_bad_ids_are_dropped() {
        let block = json!({"eligible": true, "next_grant_id": "fake-weekly-1", "grants": [
            grant("fake-weekly-1", json!({})),
            grant("Fake-Upper", json!({})),
            grant("fake-fable-1", json!({"type": "full", "gives_back_weekly": true, "resets_left": 2,
                "ends_at": "2029-06-01T00:00:00Z"})),
        ]});
        let bank = parse(Some(&block)).unwrap();
        assert_eq!(
            bank.grants.len(),
            2,
            "an id outside the alphabet is dropped"
        );
        let wire = serde_json::to_string(&bank.wire()).unwrap();
        assert!(
            !wire.contains("fake-weekly-1") && !wire.contains("fake-fable-1"),
            "{wire}"
        );
        assert_eq!(
            bank.wire().held(),
            vec![
                (ResetKind::Weekly, 1, Some(1_893_456_000_000)),
                (ResetKind::Fable, 2, Some(1_874_966_400_000))
            ]
        );
    }

    #[test]
    fn pick_takes_the_asked_kind_earliest_and_needs_a_kind_when_several_are_held() {
        let block = json!({"eligible": true, "grants": [
            grant("fake-weekly-late", json!({"ends_at": "2031-01-01T00:00:00Z"})),
            grant("fake-weekly-early", json!({"ends_at": "2030-01-01T00:00:00Z"})),
            grant("fake-weekly-paused", json!({"ends_at": "2029-01-01T00:00:00Z", "paused": true})),
            grant("fake-weekly-spent", json!({"ends_at": "2028-01-01T00:00:00Z", "resets_left": 0})),
            grant("fake-fable", json!({"gives_back_weekly": true})),
        ]});
        let bank = parse(Some(&block)).unwrap();
        assert_eq!(
            bank.pick(Some(ResetKind::Weekly)).unwrap().id,
            "fake-weekly-early"
        );
        assert_eq!(bank.pick(Some(ResetKind::Fable)).unwrap().id, "fake-fable");
        assert_eq!(bank.pick(Some(ResetKind::Full)), Err(PickError::NoneHeld));
        assert_eq!(bank.pick(None), Err(PickError::KindNeeded));
        let one = parse(Some(
            &json!({"eligible": true, "grants": [grant("fake-weekly", json!({}))]}),
        ))
        .unwrap();
        assert_eq!(one.pick(None).unwrap().id, "fake-weekly");
        let none = parse(Some(
            &json!({"eligible": false, "ineligible_reason": "surface"}),
        ))
        .unwrap();
        assert_eq!(none.pick(None), Err(PickError::NoneHeld));
    }

    #[test]
    fn ids_are_bounded_like_claude_codes() {
        assert!(valid_grant_id("fake-grant_1"));
        assert!(!valid_grant_id("Fake"));
        assert!(!valid_grant_id(&"a".repeat(41)));
        assert!(valid_request_id("A-b_9"));
        assert!(!valid_request_id(&"a".repeat(65)));
        assert!(!valid_request_id("a b"));
    }
}
