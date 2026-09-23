//! Background routine scheduler.
//!
//! Every tick the scheduler runs one [`RoutineService::tick`] pass: it claims
//! the routines that are due `now`, and for each decides whether to spawn a
//! fresh session or skip it (a prior run of the same routine is still active),
//! advancing every claimed routine's schedule. `claim_due` takes a
//! `FOR UPDATE SKIP LOCKED` row lock, so running more than one server instance
//! splits the work safely and never double-fires a routine.
//!
//! Modeled on [`crate::pr_tracking::PrPoller`]. vise-core is deliberately
//! log-free, so this loop owns the per-tick logging.

use std::sync::Arc;
use std::time::Duration;

use vise_core::routines::repository::RoutineRepository;
use vise_core::routines::service::{RoutineService, TickReport};
use vise_core::sessions::repository::SessionRepository;

/// Wakes every `interval`, runs one scheduler pass, and spawns a session per
/// due routine.
pub struct RoutineScheduler<R, S> {
    routines: Arc<RoutineService<R, S>>,
    interval: Duration,
}

impl<R, S> RoutineScheduler<R, S>
where
    R: RoutineRepository + Send + Sync + 'static,
    S: SessionRepository + Send + Sync + 'static,
{
    pub fn new(routines: Arc<RoutineService<R, S>>, interval: Duration) -> Self {
        Self { routines, interval }
    }

    /// Run one tick now: claim due routines, spawn/skip, log the outcome.
    /// Returns the report (handy for tests). Errors are logged, not propagated.
    pub async fn run_once(&self) -> TickReport {
        match self.routines.tick(chrono::Utc::now()).await {
            Ok(report) => {
                if report.spawned > 0 || report.skipped > 0 || report.failed > 0 {
                    tracing::info!(
                        spawned = report.spawned,
                        skipped = report.skipped,
                        failed = report.failed,
                        "routine scheduler tick",
                    );
                }
                report
            }
            Err(error) => {
                tracing::error!(%error, "routine scheduler tick failed");
                TickReport::default()
            }
        }
    }

    /// Schedule forever. Never returns while the server runs: every failure is
    /// logged inside [`run_once`](Self::run_once) and retried on the next tick.
    pub async fn run_forever(self) {
        tracing::info!(
            interval_secs = self.interval.as_secs(),
            "routine scheduler started"
        );

        let mut ticker = tokio::time::interval(self.interval);
        loop {
            ticker.tick().await;
            self.run_once().await;
        }
    }
}
