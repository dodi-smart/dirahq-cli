//! A hook the daemon could not take is replayed, not lost (DIRASH-0049).
//!
//! Drives the real writer over an in-memory store with the spool directory in a
//! throwaway dir (it is derived from `db_path`, so pointing `db_path` there
//! isolates it from the user's real spool).

use dira_core::model::EventKind;
use dira_core::{Config, Store};
use dira_sources::spool::{self, Record};
use dira_sources::Normalized;
use dirad::state::AppState;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

async fn daemon(dir: &std::path::Path) -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let config = Config {
        db_path: dir.join("dira.db"),
        ..Config::default()
    };
    let (state, rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, config)
        .await
        .expect("build_state");
    let writer_state = state.clone();
    tokio::spawn(async move { dirad::writer::writer(rx, writer_state).await });
    state
}

fn event(session: &str, kind: EventKind) -> Normalized {
    Normalized {
        session_id: session.into(),
        kind,
        cwd: None,
        tool: Some("Bash".into()),
        transcript_path: None,
        source: None,
        reason: None,
        hints: Box::default(),
    }
}

async fn wait_for_events(state: &AppState, n: usize) -> Vec<dira_core::model::RawEvent> {
    for _ in 0..100 {
        let evs = state.store.events_since(None).await.unwrap();
        if evs.len() >= n {
            return evs;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    state.store.events_since(None).await.unwrap()
}

#[tokio::test]
async fn spooled_hooks_replay_with_their_original_time_and_leave_the_spool_empty() {
    let dir = tempfile::tempdir().unwrap();
    let spool_dir = spool::dir_for(&dir.path().join("dira.db"));
    let observed = OffsetDateTime::now_utc() - time::Duration::minutes(42);
    // What the `dira hook` shim writes when the daemon is down.
    for i in 0..25 {
        let at = (observed + time::Duration::seconds(i))
            .format(&Rfc3339)
            .unwrap();
        spool::write(
            &spool_dir,
            &Record::new("claude", at, event("s-spooled", EventKind::PreTool)),
        )
        .unwrap();
    }
    // One unreadable record must not block the rest.
    std::fs::write(spool_dir.join("000-garbage.json"), b"{nope").unwrap();

    let state = daemon(dir.path()).await;
    let (replayed, remaining) = dirad::spool::drain_once(&state).await;
    assert_eq!(
        (replayed, remaining),
        (25, 1),
        "garbage counts as not replayed"
    );

    let evs = wait_for_events(&state, 25).await;
    assert_eq!(evs.len(), 25);
    assert!(evs.iter().all(|e| e.session_id == "s-spooled"));
    assert_eq!(evs[0].at.unix_timestamp(), observed.unix_timestamp());
    assert!(
        spool::list(&spool_dir).is_empty(),
        "replayed records are deleted"
    );
    assert!(spool_dir.join("bad/000-garbage.json").exists());
}

#[tokio::test]
async fn an_old_shim_without_observed_at_still_lands_at_receipt_time() {
    let dir = tempfile::tempdir().unwrap();
    let state = daemon(dir.path()).await;
    let resp = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::IngestHook {
            harness: "claude".into(),
            payload: serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": "s-live",
                "prompt": "never stored",
            }),
            observed_at: None,
        },
    )
    .await;
    assert!(matches!(resp, dira_core::protocol::Response::Ok));
    let evs = wait_for_events(&state, 1).await;
    assert_eq!(evs.len(), 1);
    assert!((OffsetDateTime::now_utc() - evs[0].at).whole_seconds() < 10);
}
