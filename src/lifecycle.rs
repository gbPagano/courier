//! Runtime lifecycle state tracking for Courier pipelines.
//!
//! Each pipeline transitions through a well-defined set of states that
//! orchestrators (Kubernetes, systemd) can probe via health endpoints:
//!
//! ```text
//! Starting → Running → Draining → Stopped   (signal-triggered shutdown)
//!                ↘ Stopped                  (natural source exhaustion)
//!                ↘ Failed                   (FailPipeline error policy)
//! ```
//!
//! **State definitions:**
//!
//! - **Starting**: Tasks have been spawned but have not yet begun processing.
//! - **Running**: The pipeline is actively processing envelopes.
//! - **Failed**: An unrecoverable error (e.g. `FailPipeline` policy) terminated
//!   the pipeline. Once in `Failed`, the pipeline does not recover.
//! - **Draining**: A shutdown signal was received and in-flight envelopes are
//!   being flushed through sinks.
//! - **Stopped**: All tasks have completed. This is the terminal state for a
//!   graceful shutdown.
//!
//! **In-flight envelopes during shutdown:** When a `CancellationToken` fires,
//! sources stop pulling, transforms finish their current item, and sinks drain
//! their channel receivers until the upstream closes. Any envelopes still in
//! channel buffers are processed by sinks before they exit. If the shutdown
//! timeout expires before all tasks complete, remaining tasks are aborted and
//! in-flight envelopes are dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Lifecycle state of a single pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PipelineState {
    Starting = 0,
    Running = 1,
    Failed = 2,
    Draining = 3,
    Stopped = 4,
}

impl PipelineState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Starting,
            1 => Self::Running,
            2 => Self::Failed,
            3 => Self::Draining,
            4 => Self::Stopped,
            _ => Self::Starting,
        }
    }

    /// Ordinal position in the forward progression
    /// `Starting(0) → Running(1) → Draining(2) → Stopped(3)`.
    /// `Failed` uses `u8::MAX` so the backward-transition guard in
    /// `set_state` always allows it via the explicit `Failed` exception.
    fn ordinal(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Running => 1,
            Self::Draining => 2,
            Self::Stopped => 3,
            Self::Failed => u8::MAX,
        }
    }
}

impl std::fmt::Display for PipelineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Failed => write!(f, "failed"),
            Self::Draining => write!(f, "draining"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Thread-safe status tracker for a single pipeline.
///
/// Shared between the runtime (which transitions states) and the health
/// server (which reads them). `Failed` and `Stopped` are terminal:
/// `set_state` is a no-op once either is reached, preventing concurrent
/// shutdown signals from overwriting a `Failed` state with `Draining`.
pub struct PipelineStatus {
    name: String,
    state: AtomicU8,
}

impl PipelineStatus {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: AtomicU8::new(PipelineState::Starting as u8),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn state(&self) -> PipelineState {
        PipelineState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Transition to `new_state`. Guards:
    ///
    /// - Terminal states (`Failed`, `Stopped`) are never overwritten.
    /// - Backward transitions in the forward progression are rejected
    ///   (e.g. `Draining → Running`), preventing a race where the SIGINT
    ///   handler sets `Draining` before `spawn_pipeline` sets `Running`.
    /// - `Failed` is always reachable from any non-terminal state.
    pub fn set_state(&self, new_state: PipelineState) {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let current_state = PipelineState::from_u8(current);
            if matches!(
                current_state,
                PipelineState::Failed | PipelineState::Stopped
            ) {
                return;
            }
            if !matches!(new_state, PipelineState::Failed)
                && new_state.ordinal() <= current_state.ordinal()
            {
                return;
            }
            match self.state.compare_exchange_weak(
                current,
                new_state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn mark_failed(&self) {
        self.set_state(PipelineState::Failed);
    }

    pub fn is_failed(&self) -> bool {
        self.state() == PipelineState::Failed
    }
}

/// Aggregate lifecycle state for the entire Courier runtime.
///
/// Owns one `PipelineStatus` per pipeline and a global shutdown flag.
/// The health server reads from this; the runtime writes to it.
pub struct CourierState {
    pipelines: Vec<Arc<PipelineStatus>>,
    shutdown_requested: AtomicBool,
}

impl CourierState {
    pub fn new(pipeline_names: Vec<String>) -> Self {
        Self {
            pipelines: pipeline_names
                .into_iter()
                .map(|name| Arc::new(PipelineStatus::new(name)))
                .collect(),
            shutdown_requested: AtomicBool::new(false),
        }
    }

    pub fn pipeline_status(&self, index: usize) -> Arc<PipelineStatus> {
        Arc::clone(&self.pipelines[index])
    }

    /// Readiness check: returns `true` when every pipeline has reached at
    /// least `Running` and none are `Failed`, `Draining`, or `Stopped`,
    /// and no shutdown has been requested.
    pub fn is_ready(&self) -> bool {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return false;
        }
        if self.pipelines.is_empty() {
            return true;
        }
        self.pipelines
            .iter()
            .all(|p| matches!(p.state(), PipelineState::Running))
    }

    pub fn mark_shutdown_requested(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
    }

    pub fn has_failures(&self) -> bool {
        self.pipelines.iter().any(|p| p.is_failed())
    }

    pub fn pipeline_states(&self) -> Vec<(String, PipelineState)> {
        self.pipelines
            .iter()
            .map(|p| (p.name().to_string(), p.state()))
            .collect()
    }

    /// Transition every non-terminal pipeline to `Draining`.
    /// Called by the SIGINT handler before cancelling the token.
    /// `set_state` guards terminal states and backward transitions,
    /// so `Failed`/`Stopped` pipelines are left unchanged and a pipeline
    /// already in `Draining` is a no-op.
    pub fn transition_pipelines_to_draining(&self) {
        for status in &self.pipelines {
            status.set_state(PipelineState::Draining);
        }
    }

    /// Move every non-failed pipeline to `Stopped` after all tasks complete.
    /// `Failed` pipelines are left unchanged (terminal state is guarded by
    /// `set_state`).
    pub fn finalize_pipeline_states(&self) {
        for status in &self.pipelines {
            status.set_state(PipelineState::Stopped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_state_from_u8_round_trips() {
        for state in [
            PipelineState::Starting,
            PipelineState::Running,
            PipelineState::Failed,
            PipelineState::Draining,
            PipelineState::Stopped,
        ] {
            assert_eq!(PipelineState::from_u8(state as u8), state);
        }
    }

    #[test]
    fn pipeline_status_transitions() {
        let status = PipelineStatus::new("test");
        assert_eq!(status.state(), PipelineState::Starting);
        assert!(!status.is_failed());

        status.set_state(PipelineState::Running);
        assert_eq!(status.state(), PipelineState::Running);

        status.mark_failed();
        assert_eq!(status.state(), PipelineState::Failed);
        assert!(status.is_failed());
    }

    #[test]
    fn set_state_cannot_overwrite_failed() {
        let status = PipelineStatus::new("test");
        status.mark_failed();
        assert!(status.is_failed());
        status.set_state(PipelineState::Running);
        assert_eq!(status.state(), PipelineState::Failed);
    }

    #[test]
    fn set_state_cannot_overwrite_stopped() {
        let status = PipelineStatus::new("test");
        status.set_state(PipelineState::Running);
        status.set_state(PipelineState::Stopped);
        status.set_state(PipelineState::Running);
        assert_eq!(status.state(), PipelineState::Stopped);
    }

    #[test]
    fn mark_failed_wins_race_with_set_draining() {
        // mark_failed then set_state(Draining) — Draining must not overwrite Failed.
        let status = PipelineStatus::new("test");
        status.set_state(PipelineState::Running);
        status.mark_failed();
        status.set_state(PipelineState::Draining);
        assert_eq!(status.state(), PipelineState::Failed);
        assert!(status.is_failed());
    }

    #[test]
    fn set_state_cannot_overwrite_draining_with_running() {
        // Backward transition Draining → Running must be rejected.
        // This guards the race where transition_pipelines_to_draining fires
        // before spawn_pipeline calls set_state(Running).
        let status = PipelineStatus::new("test");
        status.set_state(PipelineState::Running);
        status.set_state(PipelineState::Draining);
        assert_eq!(status.state(), PipelineState::Draining);
        status.set_state(PipelineState::Running);
        assert_eq!(status.state(), PipelineState::Draining);
    }

    #[test]
    fn set_state_allows_draining_to_stopped() {
        let status = PipelineStatus::new("test");
        status.set_state(PipelineState::Running);
        status.set_state(PipelineState::Draining);
        status.set_state(PipelineState::Stopped);
        assert_eq!(status.state(), PipelineState::Stopped);
    }

    #[test]
    fn set_state_allows_failed_from_draining() {
        let status = PipelineStatus::new("test");
        status.set_state(PipelineState::Running);
        status.set_state(PipelineState::Draining);
        status.mark_failed();
        assert_eq!(status.state(), PipelineState::Failed);
    }

    #[test]
    fn courier_state_ready_with_no_pipelines() {
        let state = CourierState::new(vec![]);
        assert!(state.is_ready());
        assert!(!state.has_failures());
    }

    #[test]
    fn courier_state_ready_when_all_running() {
        let state = CourierState::new(vec!["p1".into(), "p2".into()]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.pipeline_status(1).set_state(PipelineState::Running);
        assert!(state.is_ready());
    }

    #[test]
    fn courier_state_not_ready_when_starting() {
        let state = CourierState::new(vec!["p1".into()]);
        assert!(!state.is_ready());
    }

    #[test]
    fn courier_state_not_ready_when_failed() {
        let state = CourierState::new(vec!["p1".into()]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        assert!(state.is_ready());
        state.pipeline_status(0).mark_failed();
        assert!(!state.is_ready());
        assert!(state.has_failures());
    }

    #[test]
    fn courier_state_not_ready_after_shutdown_requested() {
        let state = CourierState::new(vec!["p1".into()]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        assert!(state.is_ready());
        state.mark_shutdown_requested();
        assert!(!state.is_ready());
    }

    #[test]
    fn courier_state_not_ready_when_draining() {
        let state = CourierState::new(vec!["p1".into()]);
        state.pipeline_status(0).set_state(PipelineState::Draining);
        assert!(!state.is_ready());
    }
}
