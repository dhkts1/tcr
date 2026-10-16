//! What each phase sends, and when: pure functions of the phase and its flags, so the
//! shapes the report names can be tested without a proxy.

use serde::{Deserialize, Serialize};

use crate::stats::Rng;

/// The four phases, in the order a full run takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Throttles off, the stub answers at once: what the proxy itself costs.
    Overhead,
    /// The fleet's five-day average, about 1.5 requests a second, default throttles.
    Steady,
    /// Bursts in the shape of the fleet's busiest second: 36 sends inside one second.
    Peak,
    /// Offered well past the fleet ceiling, to watch the throttles hold.
    Runaway,
}

impl Phase {
    pub const ALL: [Phase; 4] = [Self::Overhead, Self::Steady, Self::Peak, Self::Runaway];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overhead => "overhead",
            Self::Steady => "steady",
            Self::Peak => "peak",
            Self::Runaway => "runaway",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == raw)
    }

    /// Whether the proxy boots with its throttles switched off for this phase.
    pub fn throttles_off(self) -> bool {
        self == Self::Overhead
    }
}

/// Which endpoint one request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// `/v1/messages`, `stream: true`: most of what Claude Code sends.
    Streamed,
    /// `/v1/messages`, `stream: false`.
    Buffered,
    /// `/v1/messages/count_tokens`.
    CountTokens,
}

impl Kind {
    /// 85% streamed, 5% buffered, 10% `count_tokens`.
    pub fn draw(rng: &mut Rng) -> Self {
        let u = rng.unit();
        if u < 0.85 {
            Self::Streamed
        } else if u < 0.90 {
            Self::Buffered
        } else {
            Self::CountTokens
        }
    }
}

/// One planned send: when, from which session, to which endpoint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Planned {
    pub at_ms: f64,
    pub session: usize,
    pub kind: Kind,
}

/// The fleet's five-day average send rate (`src/config.rs`, `default_account_throttle`).
pub const STEADY_RATE: f64 = 1.5;
/// The busiest one-second window in the same replay: 36 sends.
pub const PEAK_BURST: usize = 36;
/// Seconds between peak bursts.
pub const PEAK_EVERY_S: f64 = 5.0;
/// The runaway offer: three times the fleet ceiling's sustained 10 a second.
pub const RUNAWAY_RATE: f64 = 30.0;

/// How many concurrent sessions each phase spreads its sends over.
pub fn sessions(phase: Phase) -> usize {
    match phase {
        Phase::Overhead => 16,
        Phase::Steady => 18,
        Phase::Peak => 48,
        Phase::Runaway => 64,
    }
}

/// The open-loop schedule for `phase` over `duration_s`. The overhead phase is closed-loop
/// (each worker sends its next request when the last one ends) and has no schedule.
pub fn schedule(phase: Phase, duration_s: f64, seed: u64) -> Vec<Planned> {
    let mut rng = Rng::new(seed);
    let end = duration_s * 1_000.0;
    let n = sessions(phase);
    let mut out = Vec::new();
    match phase {
        Phase::Overhead => {}
        Phase::Steady => poisson(&mut out, &mut rng, STEADY_RATE, end, n),
        Phase::Peak => {
            poisson(&mut out, &mut rng, STEADY_RATE, end, n);
            let mut burst_at = 1_000.0;
            while burst_at + 1_000.0 <= end {
                // One burst: 36 DISTINCT sessions, evenly inside one second.
                let first = rng.below(n);
                for i in 0..PEAK_BURST {
                    out.push(Planned {
                        at_ms: burst_at + i as f64 * (1_000.0 / PEAK_BURST as f64),
                        session: (first + i) % n,
                        kind: Kind::draw(&mut rng),
                    });
                }
                burst_at += PEAK_EVERY_S * 1_000.0;
            }
        }
        Phase::Runaway => {
            // Indexed rather than accumulated, so float error cannot add a send.
            let count = (duration_s * RUNAWAY_RATE).round() as usize;
            for i in 0..count {
                out.push(Planned {
                    at_ms: i as f64 * 1_000.0 / RUNAWAY_RATE,
                    session: i % n,
                    kind: Kind::draw(&mut rng),
                });
            }
        }
    }
    out.sort_by(|a, b| a.at_ms.total_cmp(&b.at_ms));
    out
}

fn poisson(out: &mut Vec<Planned>, rng: &mut Rng, rate: f64, end: f64, sessions: usize) {
    let mut at = rng.exp_gap_ms(rate);
    while at < end {
        out.push(Planned {
            at_ms: at,
            session: rng.below(sessions),
            kind: Kind::draw(rng),
        });
        at += rng.exp_gap_ms(rate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::max_in_window;

    fn times(plan: &[Planned]) -> Vec<u64> {
        plan.iter().map(|p| p.at_ms as u64).collect()
    }

    #[test]
    fn steady_offers_about_one_and_a_half_a_second() {
        let plan = schedule(Phase::Steady, 600.0, 1);
        let rate = plan.len() as f64 / 600.0;
        assert!((rate - STEADY_RATE).abs() < 0.15, "rate {rate}");
        assert!(plan.iter().all(|p| p.session < sessions(Phase::Steady)));
    }

    #[test]
    fn peak_bursts_reach_thirty_six_in_one_second() {
        let plan = schedule(Phase::Peak, 30.0, 1);
        let busiest = max_in_window(&times(&plan), 1_000);
        assert!(
            (PEAK_BURST..PEAK_BURST + 8).contains(&busiest),
            "busiest second {busiest}"
        );
        // Each burst uses distinct sessions: count sessions in the first burst window.
        let first: Vec<usize> = plan
            .iter()
            .filter(|p| (1_000.0..2_000.0).contains(&p.at_ms))
            .map(|p| p.session)
            .collect();
        let mut distinct = first.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(distinct.len() >= PEAK_BURST, "{} distinct", distinct.len());
    }

    #[test]
    fn runaway_offers_three_times_the_fleet_ceiling() {
        let plan = schedule(Phase::Runaway, 20.0, 1);
        assert_eq!(plan.len(), 600);
        assert_eq!(max_in_window(&times(&plan), 1_000), 30);
    }

    #[test]
    fn overhead_is_closed_loop_and_the_only_unthrottled_phase() {
        assert!(schedule(Phase::Overhead, 20.0, 1).is_empty());
        let off: Vec<Phase> = Phase::ALL
            .into_iter()
            .filter(|p| p.throttles_off())
            .collect();
        assert_eq!(off, vec![Phase::Overhead]);
    }

    #[test]
    fn the_endpoint_mix_is_mostly_streamed() {
        let mut rng = Rng::new(3);
        let n = 10_000;
        let streamed = (0..n)
            .filter(|_| Kind::draw(&mut rng) == Kind::Streamed)
            .count();
        let share = streamed as f64 / n as f64;
        assert!((share - 0.85).abs() < 0.02, "streamed share {share}");
    }
}
