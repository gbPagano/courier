//! Backoff scheduling for polling sources.
//!
//! Sinks already get retry through `ManagedSink`. Polling sources need a
//! different shape: there is no envelope to dead-letter and no downstream
//! to propagate to — the only thing retry can decide is *when* to attempt
//! the next poll.
//!
//! Wait rule: `min(interval, backoff_for_failure_n)`. The polling
//! `interval` is a ceiling, not a floor — retry can only schedule a
//! *sooner* attempt, never one later than the normal cadence. When the
//! computed backoff would be longer than `interval`, retry is effectively
//! disregarded: there is no value in waiting longer than the configured
//! polling rate. After `max_attempts` consecutive failures the policy is
//! considered exhausted and we fall back to `interval` until the next
//! success resets the counter.
use std::time::Duration;

use crate::retry::RetryPolicy;

pub(crate) struct PollScheduler {
    interval: Duration,
    retry: Option<RetryPolicy>,
    consecutive_failures: u32,
}

impl PollScheduler {
    pub fn new(interval: Duration, retry: Option<RetryPolicy>) -> Self {
        Self {
            interval,
            retry,
            consecutive_failures: 0,
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Records a failure and returns the duration to wait before the next
    /// poll attempt. Caller is responsible for the actual sleep + cancel
    /// handling.
    pub fn record_failure(&mut self) -> Duration {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let Some(policy) = &self.retry else {
            return self.interval;
        };
        if self.consecutive_failures >= policy.max_attempts {
            return self.interval;
        }
        let backoff = policy.delay_for(self.consecutive_failures - 1);
        self.interval.min(backoff)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry::ExhaustedPolicy;

    fn policy(max_attempts: u32, initial_ms: u64, mult: f64, max_ms: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_delay_ms: initial_ms,
            backoff_multiplier: mult,
            max_delay_ms: max_ms,
            on_exhausted: ExhaustedPolicy::Propagate,
        }
    }

    #[test]
    fn no_retry_always_returns_interval() {
        let mut s = PollScheduler::new(Duration::from_secs(30), None);
        assert_eq!(s.record_failure(), Duration::from_secs(30));
        assert_eq!(s.record_failure(), Duration::from_secs(30));
    }

    #[test]
    fn retry_below_interval_overrides_interval() {
        // interval 60s, backoff 1s -> 2s -> 4s. Each is below interval, so
        // retry takes effect and recovery is attempted sooner.
        let mut s =
            PollScheduler::new(Duration::from_secs(60), Some(policy(5, 1_000, 2.0, 60_000)));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
        assert_eq!(s.record_failure(), Duration::from_secs(2));
        assert_eq!(s.record_failure(), Duration::from_secs(4));
    }

    #[test]
    fn interval_caps_backoff_when_backoff_exceeds_interval() {
        // interval 1s, backoff 5s. Retry is disregarded — interval wins.
        let mut s = PollScheduler::new(Duration::from_secs(1), Some(policy(5, 5_000, 2.0, 60_000)));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_grows_then_caps_at_interval() {
        // interval 5s, backoff 1s -> 2s -> 4s -> 8s (capped). Once backoff
        // exceeds interval the scheduler clips to interval.
        let mut s =
            PollScheduler::new(Duration::from_secs(5), Some(policy(10, 1_000, 2.0, 60_000)));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
        assert_eq!(s.record_failure(), Duration::from_secs(2));
        assert_eq!(s.record_failure(), Duration::from_secs(4));
        assert_eq!(s.record_failure(), Duration::from_secs(5));
        assert_eq!(s.record_failure(), Duration::from_secs(5));
    }

    #[test]
    fn success_resets_counter() {
        let mut s =
            PollScheduler::new(Duration::from_secs(60), Some(policy(5, 1_000, 2.0, 60_000)));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
        assert_eq!(s.record_failure(), Duration::from_secs(2));
        s.record_success();
        assert_eq!(s.consecutive_failures(), 0);
        // Backoff restarts at the initial delay.
        assert_eq!(s.record_failure(), Duration::from_secs(1));
    }

    #[test]
    fn exhaustion_falls_back_to_interval() {
        // max_attempts = 3 -> on the 3rd failure we already exhaust,
        // because attempts 1 and 2 plus the current pending one give us
        // three consecutive failures.
        let mut s =
            PollScheduler::new(Duration::from_secs(60), Some(policy(3, 1_000, 2.0, 60_000)));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
        assert_eq!(s.record_failure(), Duration::from_secs(2));
        // Third consecutive failure: policy is exhausted, fall back to interval.
        assert_eq!(s.record_failure(), Duration::from_secs(60));
        assert_eq!(s.record_failure(), Duration::from_secs(60));
    }

    #[test]
    fn max_delay_caps_backoff_below_interval() {
        // max_delay_ms (3s) caps the backoff before interval (60s) would.
        let mut s =
            PollScheduler::new(Duration::from_secs(60), Some(policy(10, 1_000, 2.0, 3_000)));
        assert_eq!(s.record_failure(), Duration::from_secs(1));
        assert_eq!(s.record_failure(), Duration::from_secs(2));
        assert_eq!(s.record_failure(), Duration::from_secs(3));
        assert_eq!(s.record_failure(), Duration::from_secs(3));
    }
}
