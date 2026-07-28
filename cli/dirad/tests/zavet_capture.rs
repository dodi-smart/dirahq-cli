//! End-to-end zavet capture: a real temp git repo with `.zavet/` decision
//! records and trailered commits, walked by the production `capture_commits`
//! path, asserted against the store.

use dira_core::{Config, Store};
use dirad::state::AppState;
use std::path::Path;
use std::process::Command;

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let config = Config::default(); // modules.zavet = auto
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, config)
        .await
        .expect("build_state");
    state
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

const DECISION: &str = r#"---
id: D-0001
title: Poll git instead of watching the filesystem
status: active
guards:
  - src/capture/**
origin: recorded
verified: true
---

## Decision
Poll on events; no fs watcher.

## Why
Watchers add platform-specific failure modes.
"#;

fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(
        p,
        &["remote", "add", "origin", "git@github.com:acme/api.git"],
    );
    git(p, &["config", "user.email", "t@t.dev"]);
    git(p, &["config", "user.name", "T"]);
    std::fs::create_dir_all(p.join(".zavet/decisions")).unwrap();
    std::fs::write(p.join(".zavet/decisions/D-0001-poll.md"), DECISION).unwrap();
    // Scaffolding next to the records must NOT be captured as a decision
    // (regression: the plugin's template carries `id: D-0000` and was ingested).
    std::fs::write(
        p.join(".zavet/decisions/.template.md"),
        DECISION.replace("id: D-0001", "id: D-0000"),
    )
    .unwrap();
    git(p, &["add", "-A"]);
    git(
        p,
        &[
            "commit",
            "-q",
            "-m",
            "docs: record polling decision\n\nWhy: watchers add failure modes\nRefs: D-0001",
        ],
    );
    dir
}

#[tokio::test]
async fn capture_records_decisions_and_trailers_idempotently() {
    let state = test_state().await;
    let repo = setup_repo();
    let cwd = repo.path().display().to_string();
    let canonical = "github.com/acme/api";

    dirad::capture::capture_commits(&state, &cwd, canonical).await;

    // The decision record landed with parsed frontmatter + body + blob hash.
    let d = state
        .store
        .zavet_decision_get(canonical, "D-0001")
        .await
        .unwrap()
        .expect("decision captured");
    assert_eq!(
        d.title.as_deref(),
        Some("Poll git instead of watching the filesystem")
    );
    assert_eq!(d.status.as_deref(), Some("active"));
    assert_eq!(d.guards, vec!["src/capture/**"]);
    assert_eq!(d.slug.as_deref(), Some("poll"));
    assert!(d
        .body_md
        .unwrap()
        .contains("platform-specific failure modes"));
    assert!(d.content_hash.is_some(), "blob oid recorded");
    let first_commit = d.first_commit.clone().expect("first commit recorded");
    assert!(d.created_at.is_some(), "author date recorded");

    // Trailers were allowlisted + normalized (Why/Refs; no Signed-off noise),
    // and the `.template.md` scaffolding was NOT ingested as a decision.
    let counts = state.store.zavet_counts(canonical).await.unwrap();
    assert_eq!(counts.trailers, 2);
    assert_eq!(counts.decisions_total, 1);
    assert!(state
        .store
        .zavet_decision_get(canonical, "D-0000")
        .await
        .unwrap()
        .is_none());

    // Second capture with unchanged HEAD is a complete no-op.
    dirad::capture::capture_commits(&state, &cwd, canonical).await;
    let counts = state.store.zavet_counts(canonical).await.unwrap();
    assert_eq!(counts.trailers, 2);

    // A follow-up commit supersedes the record: living fields update, the
    // first-sight provenance stays on the introducing commit.
    let updated = DECISION.replace("status: active", "status: superseded");
    std::fs::write(repo.path().join(".zavet/decisions/D-0001-poll.md"), updated).unwrap();
    std::fs::create_dir_all(repo.path().join("src/capture")).unwrap();
    std::fs::write(repo.path().join("src/capture/poll.rs"), "fn poll() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(
        repo.path(),
        &[
            "commit",
            "-q",
            "-m",
            "feat: retire polling decision\n\nSupersedes: D-0001",
        ],
    );

    dirad::capture::capture_commits(&state, &cwd, canonical).await;
    let d = state
        .store
        .zavet_decision_get(canonical, "D-0001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.status.as_deref(), Some("superseded"));
    assert_eq!(d.first_commit.as_deref(), Some(first_commit.as_str()));
    assert_ne!(d.last_commit.as_deref(), Some(first_commit.as_str()));
    let counts = state.store.zavet_counts(canonical).await.unwrap();
    assert_eq!(counts.trailers, 3, "the Supersedes trailer joined");

    // Both commits are linked to the decision (trailer refs + record commits).
    let commits = state
        .store
        .zavet_commits_for_decision(canonical, "D-0001")
        .await
        .unwrap();
    assert_eq!(commits.len(), 2);
}

const SPEC: &str = r#"---
title: Commit capture pipeline
version: 1
origin: session          # designed | session | reverse-engineered
verified: false
confidence: high         # low | med | high
date: 2026-07-16
paths:
  - src/capture/**
decisions: [D-0001]
---

## Overview
The sweep walks new commits oldest-first, per D-1.

## Open Questions
- none
"#;

#[tokio::test]
async fn capture_records_specs_and_computes_staleness() {
    let state = test_state().await;
    let repo = setup_repo();
    let p = repo.path();
    let cwd = p.display().to_string();
    let canonical = "github.com/acme/api";

    std::fs::create_dir_all(p.join(".zavet/specs")).unwrap();
    std::fs::write(p.join(".zavet/specs/capture-pipeline.md"), SPEC).unwrap();
    // The dot-prefixed template next to the specs must NOT be captured.
    std::fs::write(p.join(".zavet/.spec-template.md"), SPEC).unwrap();
    std::fs::write(p.join(".zavet/specs/.spec-template.md"), SPEC).unwrap();
    git(p, &["add", "-A"]);
    git(
        p,
        &[
            "commit",
            "-q",
            "-m",
            "docs: spec the capture pipeline\n\nSpec: capture-pipeline",
        ],
    );

    dirad::capture::capture_commits(&state, &cwd, canonical).await;

    let s = state
        .store
        .zavet_spec_get(canonical, "capture-pipeline")
        .await
        .unwrap()
        .expect("spec captured");
    assert_eq!(s.title.as_deref(), Some("Commit capture pipeline"));
    assert_eq!(s.origin.as_deref(), Some("session"));
    assert_eq!(s.confidence.as_deref(), Some("high"));
    assert_eq!(s.verified, Some(false));
    assert_eq!(s.paths, vec!["src/capture/**"]);
    // Frontmatter list ∪ the body's `D-1` ref, canonicalized + deduped.
    assert_eq!(s.decisions, vec!["D-0001"]);
    assert!(s.content_hash.is_some());
    let first_commit = s.first_commit.clone().expect("first commit recorded");
    let counts = state.store.zavet_counts(canonical).await.unwrap();
    assert_eq!(counts.specs_total, 1, "templates must not be ingested");
    assert!(state
        .store
        .zavet_spec_get(canonical, ".spec-template")
        .await
        .unwrap()
        .is_none());

    // The reverse link: D-0001 knows which spec covers it.
    let covering = state
        .store
        .zavet_specs_for_decision(canonical, "D-0001")
        .await
        .unwrap();
    assert_eq!(covering.len(), 1);
    assert_eq!(covering[0].0, "capture-pipeline");

    // A commit touching a covered path — the spec goes stale.
    std::fs::create_dir_all(p.join("src/capture")).unwrap();
    std::fs::write(p.join("src/capture/poll.rs"), "fn poll() {}").unwrap();
    git(p, &["add", "-A"]);
    git(
        p,
        &["commit", "-q", "-m", "feat: rework poll\n\nRefs: D-0001"],
    );
    dirad::capture::capture_commits(&state, &cwd, canonical).await;

    let resp = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::ZavetWiki {
            topic: None,
            cwd: Some(cwd.clone()),
            repo: Some(canonical.into()),
        },
    )
    .await;
    let w = match resp {
        dira_core::protocol::Response::ZavetWiki(w) => w,
        other => panic!("expected ZavetWiki, got {other:?}"),
    };
    assert_eq!(w.specs_total, 1);
    assert_eq!(w.specs.len(), 1);
    assert_eq!(w.specs[0].slug, "capture-pipeline");
    assert_eq!(
        w.specs[0].stale_commits,
        Some(1),
        "the poll.rs commit touches src/capture/** after the spec's capture"
    );
    assert_eq!(w.specs[0].decisions, vec!["D-0001"]);

    // Updating the spec moves the living fields, keeps first-sight
    // provenance, and resets staleness.
    let updated = SPEC
        .replace("version: 1", "version: 2")
        .replace("decisions: [D-0001]", "decisions: [D-0001, D-0002]");
    std::fs::write(p.join(".zavet/specs/capture-pipeline.md"), updated).unwrap();
    git(p, &["add", "-A"]);
    git(
        p,
        &[
            "commit",
            "-q",
            "-m",
            "docs: refresh capture spec\n\nSpec: capture-pipeline",
        ],
    );
    dirad::capture::capture_commits(&state, &cwd, canonical).await;

    let s = state
        .store
        .zavet_spec_get(canonical, "capture-pipeline")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.version, 2);
    assert_eq!(s.first_commit.as_deref(), Some(first_commit.as_str()));
    assert_ne!(s.last_commit.as_deref(), Some(first_commit.as_str()));
    assert_eq!(
        s.decisions,
        vec!["D-0001", "D-0002"],
        "links replaced wholesale"
    );

    let resp = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::ZavetWiki {
            topic: None,
            cwd: Some(cwd),
            repo: Some(canonical.into()),
        },
    )
    .await;
    let w = match resp {
        dira_core::protocol::Response::ZavetWiki(w) => w,
        other => panic!("expected ZavetWiki, got {other:?}"),
    };
    assert_eq!(
        w.specs[0].stale_commits,
        Some(0),
        "refreshing the spec resets staleness"
    );
}

#[tokio::test]
async fn repos_without_zavet_capture_commits_but_no_knowledge() {
    let state = test_state().await;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(
        p,
        &["remote", "add", "origin", "git@github.com:acme/plain.git"],
    );
    git(p, &["config", "user.email", "t@t.dev"]);
    git(p, &["config", "user.name", "T"]);
    std::fs::write(p.join("a.txt"), "hi").unwrap();
    git(p, &["add", "-A"]);
    git(
        p,
        &[
            "commit",
            "-q",
            "-m",
            "feat: a\n\nWhy: should not be captured",
        ],
    );

    dirad::capture::capture_commits(&state, &p.display().to_string(), "github.com/acme/plain")
        .await;

    let counts = state
        .store
        .zavet_counts("github.com/acme/plain")
        .await
        .unwrap();
    assert_eq!(
        (counts.trailers, counts.decisions_total, counts.guard_events),
        (0, 0, 0),
        "zavet sweep must stay dormant without .zavet/",
    );
}
