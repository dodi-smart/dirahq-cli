//! Watchdog supervisor for the two long-lived accounting tasks.
//!
//! The writer (drains the ingest queue) and the idle ticker (keeps manual
//! sessions accruing) are the load-bearing tasks for timer accuracy. If either
//! silently stalls or dies, every session's `active_seconds` quietly stops
//! advancing — the exact failure Commit 1 sets out to make impossible.
//!
//! The supervisor:
//!  - **self-heals the ticker:** it re-spawns the idle ticker if its `JoinHandle`
//!    resolves to an error (a panic), so a one-off fault doesn't end accrual.
//!  - **monitors the writer:** the writer owns the single ingest receiver, so it
//!    can't be re-spawned with a fresh channel; instead a writer fault is logged
//!    loudly (operator signal) and the process keeps serving control requests.
//!  - **stall watchdog:** a periodic check compares each task's last-progress
//!    timestamp (tracked in [`crate::state::ProgressTracker`]) against a threshold
//!    and warns with diagnostics when a task hasn't made progress — catching a
//!    *wedged* (non-panicking) task that a JoinHandle watch never would.

use crate::state::{AppState, EventMsg};
use crate::writer;
use std::time::Duration;
use tokio::sync::mpsc;

/// How often the stall watchdog checks task liveness.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
/// Writer stall threshold. The writer makes progress on every drained message;
/// under a healthy idle ticker it sees a `ManualTick` (or nothing to do) well
/// within this window when manual sessions are open. A gap beyond this with a
/// non-empty queue means it's wedged. Generous so a genuinely quiet daemon
/// (no events at all) doesn't cry wolf — we only warn when the queue is backed up.
const WRITER_STALL_SECS: i64 = 120;
/// Idle-ticker stall threshold: it ticks every 30s, so two missed ticks is a stall.
const TICKER_STALL_SECS: i64 = 90;

/// Spawn the writer, the idle ticker, and the watchdog under one supervising task.
pub fn spawn(state: AppState, rx: mpsc::Receiver<EventMsg>) {
    tokio::spawn(run(state, rx));
}

async fn run(state: AppState, rx: mpsc::Receiver<EventMsg>) {
    // The writer owns the unique ingest receiver; monitor its handle but don't
    // re-spawn (a fresh channel would orphan every existing `tx`).
    let mut writer_handle = tokio::spawn(writer::writer(rx, state.clone()));
    // The ticker is restartable — it only needs a cloned `AppState`.
    let mut ticker_handle = tokio::spawn(crate::idle_ticker(state.clone()));

    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Writer finished. A clean end means the channel closed (shutdown);
            // an error means it panicked — log loudly. Either way stop watching it.
            res = &mut writer_handle => {
                match res {
                    Ok(()) => tracing::info!("writer task ended (channel closed)"),
                    Err(e) => tracing::error!(
                        error = %e,
                        "writer task PANICKED — event accrual has stopped; restart the daemon"
                    ),
                }
                // Park this branch forever so the supervisor keeps running the
                // ticker + watchdog without busy-looping on a resolved handle.
                writer_handle = tokio::spawn(std::future::pending::<()>());
            }
            // Ticker finished. It loops forever, so any resolution is a fault —
            // re-spawn it so manual-session accrual self-heals.
            res = &mut ticker_handle => {
                match res {
                    Ok(()) => tracing::warn!("idle ticker ended unexpectedly — re-spawning"),
                    Err(e) => tracing::error!(error = %e, "idle ticker PANICKED — re-spawning"),
                }
                ticker_handle = tokio::spawn(crate::idle_ticker(state.clone()));
            }
            _ = watchdog.tick() => {
                check_stalls(&state);
            }
        }
    }
}

/// Warn if either task hasn't made progress within its threshold. A writer stall
/// only matters when there's a backlog to drain, so it's gated on a non-empty
/// queue to avoid false alarms on a genuinely quiet daemon.
fn check_stalls(state: &AppState) {
    let queue_depth = writer::QUEUE_CAPACITY.saturating_sub(state.tx.capacity());
    if let Some(idle) = state.progress.writer_idle_secs() {
        if idle >= WRITER_STALL_SECS && queue_depth > 0 {
            tracing::warn!(
                writer_idle_secs = idle,
                queue_depth,
                "watchdog: writer has not drained the queue — possible stall"
            );
        }
    }
    if let Some(idle) = state.progress.ticker_idle_secs() {
        if idle >= TICKER_STALL_SECS {
            tracing::warn!(
                ticker_idle_secs = idle,
                "watchdog: idle ticker has not ticked — possible stall"
            );
        }
    }
}
