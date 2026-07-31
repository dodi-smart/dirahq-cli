//! The maintenance sweep's ordering guarantees.
//!
//! Driven through `maintenance_sweep` directly rather than the hourly loop, so
//! these run in milliseconds instead of waiting out `MAINTENANCE_INTERVAL_SECS`.

use dira_core::{Config, Store};
use dirad::state::AppState;

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    state
}

/// **The regression.** The checkpoint used to sit behind an early `continue` on
/// `Ok(0)`, so an install younger than the retention window — which has nothing
/// eligible for compaction — never truncated its WAL at all. Reported in the
/// field as a 4 MiB `-wal` beside a 332 KiB database.
#[tokio::test]
async fn the_wal_is_checkpointed_even_when_compaction_deletes_nothing() {
    let state = test_state().await;
    let mut dirty = false;

    // A fresh store: nothing synced, nothing past retention, so `compact`
    // returns 0 — exactly the young-install case.
    let outcome = dirad::maintenance_sweep(&state, 1, &mut dirty).await;

    assert_eq!(outcome.deleted, 0, "nothing should be eligible yet");
    assert!(!outcome.compaction_failed);
    assert!(
        outcome.checkpointed,
        "the WAL checkpoint must run regardless of the compaction outcome"
    );
}

/// VACUUM rewrites the whole file, so it stays gated on there having been
/// deletes since the last one — a young install must not rewrite its database
/// daily for nothing, even on the sweep where the counter lines up.
#[tokio::test]
async fn vacuum_is_skipped_when_nothing_has_been_deleted() {
    let state = test_state().await;
    let mut dirty = false;

    // Sweep 24 is a VACUUM sweep by the cadence, but nothing is dirty.
    let outcome = dirad::maintenance_sweep(&state, 24, &mut dirty).await;

    assert!(!outcome.vacuumed, "a clean database needs no VACUUM");
    assert!(outcome.checkpointed, "the checkpoint still runs");
}

/// Once something *has* been deleted, the next cadence sweep vacuums and clears
/// the flag, so the work happens once rather than every sweep thereafter.
#[tokio::test]
async fn vacuum_runs_once_after_deletes_then_resets() {
    let state = test_state().await;
    let mut dirty = true; // pretend an earlier sweep compacted rows away

    let first = dirad::maintenance_sweep(&state, 24, &mut dirty).await;
    assert!(
        first.vacuumed,
        "a dirty database on a cadence sweep vacuums"
    );
    assert!(!dirty, "and the flag resets");

    let second = dirad::maintenance_sweep(&state, 48, &mut dirty).await;
    assert!(
        !second.vacuumed,
        "with nothing new deleted, the next cadence sweep must not vacuum again"
    );
}

/// Sweeps that are not on the VACUUM cadence never vacuum, dirty or not.
#[tokio::test]
async fn an_off_cadence_sweep_only_checkpoints() {
    let state = test_state().await;
    let mut dirty = true;

    let outcome = dirad::maintenance_sweep(&state, 5, &mut dirty).await;

    assert!(outcome.checkpointed);
    assert!(!outcome.vacuumed);
    assert!(dirty, "the flag survives until a cadence sweep consumes it");
}
