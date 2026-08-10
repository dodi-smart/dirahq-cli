//! Integration tests for the zavet ingest + query path: real daemon state
//! (in-memory store, live registry) driven through `control::dispatch`, with a
//! real temp git repo so activation and repo resolution run the production code.

use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use dira_core::protocol::{Request, Response};
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::path::Path;
use time::OffsetDateTime;
use ulid::Ulid;

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let config = Config::default(); // modules.zavet = auto
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, config)
        .await
        .expect("build_state");
    state
}

/// A git repo with an `origin` remote (canonicalizes to github.com/acme/api)
/// and, optionally, a `.zavet/` directory.
fn temp_repo(zavet: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    run(&["init", "-q"]);
    run(&["remote", "add", "origin", "git@github.com:acme/api.git"]);
    if zavet {
        std::fs::create_dir(dir.path().join(".zavet")).expect("mkdir .zavet");
    }
    dir
}

fn guard_event(cwd: &Path, kind: &str) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "kind": kind,
        "decision_id": "D-0001",
        "file_path": "src/x.rs",
        "cwd": cwd.display().to_string(),
        "ts": "2026-07-15T12:00:00Z",
    })
}

/// Put one live agent session for `repo` into the registry.
fn open_session(state: &AppState, repo: &str) -> String {
    let session_id = Ulid::generate().to_string();
    let ev = RawEvent {
        id: Ulid::generate().to_string(),
        at: OffsetDateTime::now_utc(),
        session_id: session_id.clone(),
        harness: Harness::ClaudeCode,
        kind: EventKind::SessionStart,
        cwd: None,
        project: Some(repo.to_string()),
        identity_email: None,
        branch: None,
        tool: None,
        label: None,
        activity: None,
        note: None,
    };
    state
        .sessions
        .lock()
        .expect("registry lock")
        .observe(&ev, state.config.idle());
    session_id
}

#[tokio::test]
async fn guard_event_is_stored_and_attributed_to_the_unique_active_session() {
    let state = test_state().await;
    let repo = temp_repo(true);
    let sid = open_session(&state, "github.com/acme/api");

    let resp = dirad::control::dispatch(
        &state,
        Request::IngestZavet {
            payload: guard_event(repo.path(), "guard_shown"),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok), "got {resp:?}");

    let stats = state
        .store
        .zavet_guard_event_stats("github.com/acme/api", Some("D-0001"))
        .await
        .unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].kind, "guard_shown");
    assert_eq!((stats[0].total, stats[0].unattributed), (1, 0));

    let sessions = state
        .store
        .zavet_sessions_for_decision("github.com/acme/api", "D-0001")
        .await
        .unwrap();
    assert_eq!(sessions, vec![sid]);
}

#[tokio::test]
async fn guard_event_without_active_session_is_stored_unattributed() {
    let state = test_state().await;
    let repo = temp_repo(true);

    let resp = dirad::control::dispatch(
        &state,
        Request::IngestZavet {
            payload: guard_event(repo.path(), "guard_blocked"),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));

    let stats = state
        .store
        .zavet_guard_event_stats("github.com/acme/api", Some("D-0001"))
        .await
        .unwrap();
    assert_eq!((stats[0].total, stats[0].unattributed), (1, 1));
}

#[tokio::test]
async fn events_for_inactive_repos_and_malformed_payloads_are_dropped_ok() {
    let state = test_state().await;

    // No .zavet/ + knob auto ⇒ inactive: dropped, but still Ok (shim-friendly).
    let repo = temp_repo(false);
    let resp = dirad::control::dispatch(
        &state,
        Request::IngestZavet {
            payload: guard_event(repo.path(), "guard_shown"),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));

    // Malformed payload: Ok, nothing stored, no panic.
    let resp = dirad::control::dispatch(
        &state,
        Request::IngestZavet {
            payload: serde_json::json!({"totally": "unrelated"}),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));

    let counts = state
        .store
        .zavet_counts("github.com/acme/api")
        .await
        .unwrap();
    assert_eq!(counts.guard_events, 0);
}

#[tokio::test]
async fn per_repo_override_beats_the_auto_probe() {
    let state = test_state().await;
    let repo = temp_repo(false); // no .zavet/ — auto says inactive

    // Force-enable, as `dira zavet enable` would.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetSetMode {
            cwd: Some(repo.path().display().to_string()),
            repo: None,
            mode: "on".into(),
        },
    )
    .await;
    match resp {
        Response::ZavetModeSet { repo: r, mode } => {
            assert_eq!(r, "github.com/acme/api");
            assert_eq!(mode, "on");
        }
        other => panic!("expected ZavetModeSet, got {other:?}"),
    }

    let resp = dirad::control::dispatch(
        &state,
        Request::IngestZavet {
            payload: guard_event(repo.path(), "guard_shown"),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));
    let counts = state
        .store
        .zavet_counts("github.com/acme/api")
        .await
        .unwrap();
    assert_eq!(counts.guard_events, 1);

    // Status reflects the override + verdict.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetStatus {
            cwd: Some(repo.path().display().to_string()),
            repo: None,
        },
    )
    .await;
    match resp {
        Response::ZavetStatus(v) => {
            assert!(v.active);
            assert_eq!(v.override_mode.as_deref(), Some("on"));
            assert_eq!(v.zavet_dir, Some(false));
            assert_eq!(v.guard_events, 1);
        }
        other => panic!("expected ZavetStatus, got {other:?}"),
    }
}

/// A guard event's id is canonicalized against the REPO's config, not a
/// guessed default.
///
/// The split matters: `parse_guard_event` runs before `cwd` has been resolved
/// to a repo, so it only uppercases the prefix and leaves the digits alone;
/// `zavet::ingest` pads once it can read `.zavet/config`. Padding at the parse
/// layer would key a width-5 repo's ids at width 4, and the record captured
/// from the same repo would land under a different key — the decision would
/// silently show zero guard events.
#[tokio::test]
async fn guard_event_ids_canonicalize_at_the_repo_width() {
    let state = test_state().await;
    let repo = temp_repo(true);
    std::fs::write(
        repo.path().join(".zavet/config"),
        "prefix: CLOUD\nprefix-aliases: D\nid-width: 5\n",
    )
    .expect("write config");

    // Shorthand under the current prefix, and an id under the retired one.
    for (sent, stored) in [("CLOUD-42", "CLOUD-00042"), ("d-7", "D-00007")] {
        let mut payload = guard_event(repo.path(), "guard_shown");
        payload["decision_id"] = serde_json::json!(sent);
        let resp = dirad::control::dispatch(&state, Request::IngestZavet { payload }).await;
        assert!(matches!(resp, Response::Ok), "got {resp:?}");

        let stats = state
            .store
            .zavet_guard_event_stats("github.com/acme/api", Some(stored))
            .await
            .unwrap();
        assert_eq!(stats.len(), 1, "{sent} should have stored as {stored}");
        assert_eq!((stats[0].total, stats[0].kind.as_str()), (1, "guard_shown"));
    }
}

/// A repo with no `.zavet/config` keeps the historical width — the guarantee
/// that makes prefixes migration-free for every repo scaffolded before them.
#[tokio::test]
async fn guard_event_ids_keep_width_4_without_a_config() {
    let state = test_state().await;
    let repo = temp_repo(true);
    let mut payload = guard_event(repo.path(), "guard_shown");
    payload["decision_id"] = serde_json::json!("D-7");
    let resp = dirad::control::dispatch(&state, Request::IngestZavet { payload }).await;
    assert!(matches!(resp, Response::Ok), "got {resp:?}");

    let stats = state
        .store
        .zavet_guard_event_stats("github.com/acme/api", Some("D-0007"))
        .await
        .unwrap();
    assert_eq!(stats.len(), 1, "D-7 should have stored as D-0007");
}
