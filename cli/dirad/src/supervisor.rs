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
//!    can't be re-spawned with a fresh channel. As of WP-B7 that matters far
//!    less than it used to: the writer itself catches and drops any panic
//!    tripped by a single message (see `writer::process_message`), so it
//!    survives the class of fault that used to end accrual for good. A
//!    `JoinHandle` resolving here now means something escaped that per-message
//!    boundary entirely (e.g. the channel closed) — still logged loudly, since
//!    it's the daemon shutting down or a genuinely unexpected fault, not the
//!    routine case.
//!  - **stall watchdog:** a periodic check compares each task's last-progress
//!    timestamp (tracked in [`crate::state::ProgressTracker`]) against a threshold
//!    and flags diagnostics when a task hasn't made progress — catching a
//!    *wedged* (non-panicking) task that a JoinHandle watch never would. The
//!    writer branch escalates to `error!` and bumps `ProgressTracker::writer_stalls`
//!    (surfaced on `dira status`), since an un-self-healing task that stops
//!    advancing is a page-worthy condition, not a warning.

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
    // re-spawn (a fresh channel would orphan every existing `tx`). Per-message
    // panics no longer reach this handle at all (WP-B7) — they're caught inside
    // the writer loop itself — so a resolution here is the channel closing
    // (shutdown) or a fault outside the per-message boundary.
    let mut writer_handle = tokio::spawn(writer::writer(rx, state.clone()));
    // The ticker is restartable — it only needs a cloned `AppState`.
    let mut ticker_handle = tokio::spawn(crate::idle_ticker(state.clone()));

    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Writer finished. A clean end means the channel closed (shutdown); an
            // error means a panic escaped the per-message `catch_unwind` inside
            // the writer loop itself (WP-B7) — every routine per-message fault is
            // already caught and counted in `writer_panics` without ever reaching
            // here, so this arm firing is now the rare case: a bug in the
            // channel-drain loop itself, not a bad event. Still logged loudly and
            // still unrecoverable without a restart (the receiver is gone).
            res = &mut writer_handle => {
                match res {
                    Ok(()) => tracing::info!("writer task ended (channel closed)"),
                    Err(e) => tracing::error!(
                        error = %e,
                        "writer task PANICKED outside per-message isolation — event accrual has stopped; restart the daemon"
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

/// Flag either task if it hasn't made progress within its threshold. A writer
/// stall only matters when there's a backlog to drain, so it's gated on a
/// non-empty queue to avoid false alarms on a genuinely quiet daemon.
///
/// The writer branch is escalated to `error!` (was `warn!`): unlike the ticker,
/// the writer can't self-heal by re-spawning (it owns the sole receiver), so a
/// wedge here — as opposed to a caught per-message panic, which the writer
/// already recovers from on its own — means the drain loop itself stopped
/// advancing and every session's accrual is stuck. That's a page-worthy
/// condition, not a routine warning, so it also bumps
/// `ProgressTracker::writer_stalls`, surfaced on `dira status`.
fn check_stalls(state: &AppState) {
    if writer_wedged(state) {
        state.progress.mark_writer_stall();
        tracing::error!(
            writer_idle_secs = state.progress.writer_idle_secs(),
            queue_depth = writer::QUEUE_CAPACITY.saturating_sub(state.tx.capacity()),
            writer_stalls = state.progress.writer_stalls(),
            "watchdog: writer has not drained the queue — stalled"
        );
    }
    // Same baseline as the writer: a ticker that has NEVER ticked since the
    // daemon started (hung inside its first tick) is just as stalled as one
    // that ticked once and then stopped.
    let ticker_idle = state.progress.ticker_idle_or_start_secs();
    if ticker_idle >= TICKER_STALL_SECS {
        // The ticker self-heals (re-spawned on panic, see `run` above), so a
        // stall here — while still worth knowing about — isn't the same
        // unrecoverable-without-a-restart condition the writer's is.
        tracing::warn!(
            ticker_idle_secs = ticker_idle,
            "watchdog: idle ticker has not ticked — possible stall"
        );
    }
}

/// Whether the writer currently looks wedged: no progress within
/// [`WRITER_STALL_SECS`] while messages are backed up in the queue. Shared by
/// the watchdog's periodic check and `dira status`'s health line so both use
/// the exact same definition of "wedged".
pub fn writer_wedged(state: &AppState) -> bool {
    let queue_depth = writer::QUEUE_CAPACITY.saturating_sub(state.tx.capacity());
    queue_depth > 0 && state.progress.writer_idle_or_start_secs() >= WRITER_STALL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EventMsg;

    fn queued_event() -> EventMsg {
        EventMsg::Raw(Box::new(dira_core::model::RawEvent {
            id: ulid::Ulid::new().to_string(),
            at: time::OffsetDateTime::now_utc(),
            session_id: "s1".to_string(),
            harness: dira_contract::Harness::Manual,
            kind: dira_core::model::EventKind::ManualTick,
            cwd: None,
            project: Some("github.com/acme/api".to_string()),
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }))
    }

    /// A writer that hangs on its very FIRST message (a wedged `store.append`,
    /// a stuck SQLite lock at cold start) must still be flagged. Before the
    /// fix, `writer_idle_secs()` stayed `None` until the first fully-drained
    /// message, so `writer_wedged` short-circuited to `false` forever — the
    /// watchdog was blind in exactly the window (cold start) where a hang is
    /// most likely.
    #[tokio::test]
    async fn writer_wedged_flags_a_hang_on_the_very_first_message() {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, dira_core::Config::default())
                .await
                .unwrap();
        // A message is backed up (nothing is draining `_rx`)…
        state.tx.try_send(queued_event()).unwrap();
        // …and the daemon started longer ago than the stall threshold with
        // the writer never having drained anything.
        state
            .progress
            .backdate_start_for_test(WRITER_STALL_SECS + 1);
        assert!(
            writer_wedged(&state),
            "a queue backlog with zero writer progress since daemon start is a wedge"
        );
    }

    /// The happy-path guards still hold: a fresh daemon (started moments ago)
    /// with a backlog is NOT yet wedged, and an old daemon with an EMPTY queue
    /// is quiet, not wedged.
    #[tokio::test]
    async fn writer_wedged_stays_quiet_on_fresh_start_or_empty_queue() {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, dira_core::Config::default())
                .await
                .unwrap();
        // Old daemon, empty queue: quiet.
        state
            .progress
            .backdate_start_for_test(WRITER_STALL_SECS + 1);
        assert!(!writer_wedged(&state), "an empty queue is never a wedge");
        // Backlogged but with recent writer progress: not wedged.
        state.tx.try_send(queued_event()).unwrap();
        state.progress.mark_writer();
        assert!(
            !writer_wedged(&state),
            "recent writer progress means the backlog is being drained"
        );
    }
}
