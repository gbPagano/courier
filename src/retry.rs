use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// Configures retry behavior for components that support it.
///
/// On each failure the component logs a warning, waits for the computed
/// delay, then tries again. When all attempts are exhausted, `on_exhausted`
/// determines what happens next (see [`ExhaustedPolicy`]).
///
/// The delay before the Nth retry (0-indexed) is:
/// ```text
/// delay = min(initial_delay_ms * backoff_multiplier ^ N, max_delay_ms)
/// ```
///
/// Set `max_attempts = 1` to disable retries (first failure immediately
/// triggers `on_exhausted`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RetryPolicy {
    /// Total number of attempts, including the first. Must be ≥ 1.
    pub max_attempts: u32,
    /// Delay before the second attempt, in milliseconds.
    pub initial_delay_ms: u64,
    /// Multiplier applied to the delay after each failed attempt.
    /// `1.0` = constant delay; `2.0` = exponential doubling.
    pub backoff_multiplier: f64,
    /// Upper bound on any single inter-attempt delay, in milliseconds.
    pub max_delay_ms: u64,
    /// What to do after all attempts are exhausted.
    #[serde(default)]
    pub on_exhausted: ExhaustedPolicy,
}

impl RetryPolicy {
    /// Delay for the Nth retry (0-indexed: 0 = delay before 2nd attempt).
    pub fn delay_for(&self, retry: u32) -> Duration {
        let ms = (self.initial_delay_ms as f64 * self.backoff_multiplier.powi(retry as i32))
            .min(self.max_delay_ms as f64) as u64;
        Duration::from_millis(ms)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 100,
            backoff_multiplier: 2.0,
            max_delay_ms: 10_000,
            on_exhausted: ExhaustedPolicy::default(),
        }
    }
}

/// What to do when a [`RetryPolicy`] exhausts all attempts.
///
/// - [`Propagate`]: return the last error to the caller. The surrounding
///   `ManagedSink` then applies its own `ErrorPolicy` (drop or fail pipeline).
///
/// - [`DeadLetter`]: append the failed envelope as a JSON line to `path` and
///   return `Ok(())` so the pipeline keeps running. If the file write itself
///   fails, the original error is returned instead.
///
/// [`Propagate`]: ExhaustedPolicy::Propagate
/// [`DeadLetter`]: ExhaustedPolicy::DeadLetter
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExhaustedPolicy {
    /// Return the last error to the caller.
    #[default]
    Propagate,
    /// Append the failed envelope as a JSON line to the given file.
    DeadLetter { path: PathBuf },
}
