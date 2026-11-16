//! Exponential backoff used by the watch supervisor when reconnecting to
//! etcd (spec §2: 1s → 2 → 4 → 8 → 16 → 32 → 60s max), with jitter.
//!
//! This is a pure data structure — it returns durations. The calling task is
//! responsible for actually sleeping.

use std::time::Duration;

pub const BASE_MS: u64 = 1_000;
pub const MAX_MS: u64 = 60_000;

#[derive(Debug, Clone)]
pub struct ExpBackoff {
    current_ms: u64,
    base_ms: u64,
    max_ms: u64,
    jitter: fn(Duration, Duration) -> Duration,
}

impl Default for ExpBackoff {
    fn default() -> Self {
        Self::new(BASE_MS, MAX_MS)
    }
}

impl ExpBackoff {
    pub const fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            current_ms: base_ms,
            base_ms,
            max_ms,
            jitter: random_jitter,
        }
    }

    /// Return the current delay and advance for the next call. Saturates at
    /// `max_ms` so long-running reconnect loops don't balloon past 60s.
    pub fn next_delay(&mut self) -> Duration {
        let d = Duration::from_millis(self.current_ms);
        self.current_ms = (self.current_ms.saturating_mul(2)).min(self.max_ms);
        (self.jitter)(d, Duration::from_millis(self.max_ms))
    }

    /// Reset the backoff after a successful reconnect so the next failure
    /// restarts at `base_ms`.
    pub fn reset(&mut self) {
        self.current_ms = self.base_ms;
    }
}

// 封顶前收窄到上限的 5/6，避免抖动后截断使大量实例又挤到同一上限。
fn jittered(step: Duration, maximum: Duration, unit: f64) -> Duration {
    step.min(maximum * 5 / 6)
        .mul_f64(1.0 + 0.2 * (2.0 * unit - 1.0))
}

fn random_jitter(step: Duration, maximum: Duration) -> Duration {
    jittered(step, maximum, rand::random::<f64>())
}

#[cfg(test)]
pub(crate) fn nominal_backoff() -> ExpBackoff {
    ExpBackoff {
        jitter: |step, _| step,
        ..ExpBackoff::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_then_saturates_at_max() {
        let mut b = ExpBackoff::new(1_000, 60_000);
        b.jitter = |step, _| step;
        let seq: Vec<u64> = (0..8).map(|_| b.next_delay().as_millis() as u64).collect();
        assert_eq!(
            seq,
            vec![1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000]
        );
    }

    #[test]
    fn reset_returns_to_base() {
        let mut b = ExpBackoff::new(500, 8_000);
        b.jitter = |step, _| step;
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay().as_millis() as u64, 500);
    }

    #[test]
    fn default_matches_spec_1s_to_60s() {
        let b = ExpBackoff::default();
        assert_eq!(b.base_ms, BASE_MS);
        assert_eq!(b.max_ms, MAX_MS);
    }

    #[test]
    fn jitter_spans_a_fifth_without_piling_up_at_the_ceiling() {
        let step = Duration::from_secs(4);
        let maximum = Duration::from_secs(60);
        for (unit, milliseconds) in [
            (0.0, 3_200),
            (0.25, 3_600),
            (0.5, 4_000),
            (0.75, 4_400),
            (1.0, 4_800),
        ] {
            assert_eq!(
                jittered(step, maximum, unit),
                Duration::from_millis(milliseconds)
            );
        }
        for (unit, seconds) in [(0.0, 40), (0.25, 45), (0.5, 50), (0.75, 55), (1.0, 60)] {
            assert_eq!(
                jittered(maximum, maximum, unit),
                Duration::from_secs(seconds)
            );
        }
    }

    #[test]
    fn distinct_draws_keep_reconnecting_clients_apart_at_every_step() {
        let mut first = ExpBackoff {
            jitter: |step, maximum| jittered(step, maximum, 0.25),
            ..ExpBackoff::default()
        };
        let mut second = ExpBackoff {
            jitter: |step, maximum| jittered(step, maximum, 0.75),
            ..ExpBackoff::default()
        };
        for (first_ms, second_ms) in [
            (900, 1_100),
            (1_800, 2_200),
            (3_600, 4_400),
            (7_200, 8_800),
            (14_400, 17_600),
            (28_800, 35_200),
            (45_000, 55_000),
            (45_000, 55_000),
        ] {
            assert_eq!(first.next_delay(), Duration::from_millis(first_ms));
            assert_eq!(second.next_delay(), Duration::from_millis(second_ms));
        }
        first.reset();
        second.reset();
        assert_eq!(first.next_delay(), Duration::from_millis(900));
        assert_eq!(second.next_delay(), Duration::from_millis(1_100));
    }

    #[test]
    fn default_draws_stay_within_the_reconnect_bounds() {
        for _ in 0..32 {
            let mut backoff = ExpBackoff::default();
            for (minimum_ms, maximum_ms) in [
                (800, 1_200),
                (1_600, 2_400),
                (3_200, 4_800),
                (6_400, 9_600),
                (12_800, 19_200),
                (25_600, 38_400),
                (40_000, 60_000),
                (40_000, 60_000),
            ] {
                let wait = backoff.next_delay();
                assert!(
                    (Duration::from_millis(minimum_ms)..=Duration::from_millis(maximum_ms))
                        .contains(&wait),
                    "{wait:?}"
                );
            }
        }
    }
}
