//! Pure-function tests for [`divert_verdict`] (the divert-budget design notes
//! §4.4) — all three arms, driven by explicit `budget` values, exactly as
//! the brief requires. These are a SELF-authored oracle (this module wrote
//! both the fix and the test), so they cap at σ2: sensitivity to the
//! change is not independence. Unit F's replay over the retained logs
//! (§6) is the independent oracle that can promote this past σ2.
use super::*;

fn ep(pin: usize, until_ms: i64, destinations: u64, sticky: usize) -> DivertEpisode {
    DivertEpisode {
        pin,
        until_ms,
        destinations,
        sticky,
    }
}

#[test]
fn no_episode_is_fresh() {
    assert_eq!(divert_verdict(None, 0, 1_000, 1), DivertVerdict::Fresh);
}

#[test]
fn mismatched_pin_is_fresh_even_with_a_recorded_episode() {
    let episode = ep(0, 1_000, 0b10, 1);
    // Same deadline, different pin: a different account is now held, so
    // this is not a continuation of the same episode.
    assert_eq!(
        divert_verdict(Some(episode), 2, 1_000, 1),
        DivertVerdict::Fresh
    );
}

#[test]
fn mismatched_deadline_is_fresh_even_with_the_same_pin() {
    let episode = ep(0, 1_000, 0b10, 1);
    // Same pin, different deadline: a NEW hold on the same account — the
    // whole point of keying the episode on `(pin, until_ms)` rather than
    // the pin alone (§4.1).
    assert_eq!(
        divert_verdict(Some(episode), 0, 2_000, 1),
        DivertVerdict::Fresh
    );
}

#[test]
fn matching_episode_under_unlimited_budget_offers_sticky() {
    let episode = ep(0, 1_000, 0b10, 1);
    // budget = 0 is the kill switch (§4.6): never Block, regardless of
    // how many distinct destinations are already recorded.
    assert_eq!(
        divert_verdict(Some(episode), 0, 1_000, 0),
        DivertVerdict::Sticky(1)
    );
}

#[test]
fn matching_episode_under_a_nonzero_budget_offers_sticky() {
    let episode = ep(0, 1_000, 0b10, 1); // one destination recorded
    assert_eq!(
        divert_verdict(Some(episode), 0, 1_000, 2),
        DivertVerdict::Sticky(1)
    );
}

#[test]
fn matching_episode_at_the_budget_cap_blocks() {
    let episode = ep(0, 1_000, 0b10, 1); // count_ones() == 1
    assert_eq!(
        divert_verdict(Some(episode), 0, 1_000, 1),
        DivertVerdict::Block
    );
}

#[test]
fn matching_episode_over_the_budget_cap_blocks() {
    // Two distinct destinations already recorded (bits 1 and 3), budget 1.
    let episode = ep(0, 1_000, 0b1010, 1);
    assert_eq!(
        divert_verdict(Some(episode), 0, 1_000, 1),
        DivertVerdict::Block
    );
}
