//! Source-backed specs, end to end: a real temp git repo with a declared
//! `spec-source:`, a FAKE installed plugin (a shell script answering
//! `version --json` / `specs --json`), walked by the production
//! `capture_commits` path and asserted against the store.
//!
//! The fake plugin is reached through `DIRA_ZAVET_PLUGIN_ROOT`, the same seam a
//! user with a non-standard install uses — never through the repo's vendored
//! `.zavet/bin/zavet`, which one test plants as a tripwire.
#![cfg(unix)]

use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use time::OffsetDateTime;

const REPO: &str = "github.com/acme/api";

/// The plugin root is a process-global env var; tests that set it take turns.
static PLUGIN_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
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

/// A fake plugin root: `bin/zavet` serves whatever `rows.jsonl` holds.
struct FakePlugin {
    dir: tempfile::TempDir,
}

impl FakePlugin {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let bin = dir.path().join("bin/zavet");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             d=\"$(dirname \"$0\")/..\"\n\
             case \"$1\" in\n\
               version) cat \"$d/version.json\" ;;\n\
               specs) cat \"$d/rows.jsonl\" ;;\n\
               *) exit 2 ;;\n\
             esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("version.json"), r#"{"specs_json":1}"#).unwrap();
        std::fs::write(dir.path().join("rows.jsonl"), "").unwrap();
        Self { dir }
    }
    fn rows(&self, jsonl: &str) {
        std::fs::write(self.dir.path().join("rows.jsonl"), jsonl).unwrap();
    }
    fn version(&self, json: &str) {
        std::fs::write(self.dir.path().join("version.json"), json).unwrap();
    }
    fn activate(&self) {
        // SAFETY of intent: serialised by PLUGIN_ENV in every test that calls it.
        std::env::set_var(dira_core::zavet_specs::PLUGIN_ROOT_ENV, self.dir.path());
    }
}

fn row(kind: &str, id: &str, state: &str, extra: &str) -> String {
    format!(
        r#"{{"v":1,"slug":"{kind}:{id}","kind":"{kind}","id":"{id}","root":"openspec","title":"{id}","state":"{state}","nativeState":"{state}","primary":"openspec/changes/{id}/proposal.md","files":[{{"path":"openspec/changes/{id}/proposal.md","oid":"o-{id}-{state}"}}],"firstCommit":"aaa111","firstAt":"2026-09-01T00:00:00Z","lastCommit":"bbb222","lastAt":"2026-09-02T00:00:00Z","paths":[],"requirements":[{{"id":"R1","state":"added","scenarios":2}}],"tasks":[{{"id":"1.1","done":true}},{{"id":"1.2","done":false}}]{extra}}}"#
    )
}

/// A repo declaring an OpenSpec source, with one commit touching the root.
fn repo_with_source(branch: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", branch]);
    git(
        p,
        &["remote", "add", "origin", "git@github.com:acme/api.git"],
    );
    git(p, &["config", "user.email", "t@t.dev"]);
    git(p, &["config", "user.name", "T"]);
    std::fs::create_dir_all(p.join(".zavet")).unwrap();
    std::fs::write(
        p.join(".zavet/config"),
        "prefix: D\nspec-source: openspec openspec\n",
    )
    .unwrap();
    std::fs::create_dir_all(p.join("openspec/changes/add-csv-import")).unwrap();
    std::fs::write(
        p.join("openspec/changes/add-csv-import/proposal.md"),
        "# Add CSV import\n",
    )
    .unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "feat: propose csv import"]);
    dir
}

fn commit_touching_root(repo: &Path, msg: &str) {
    let f = repo.join("openspec/changes/add-csv-import/tasks.md");
    let prev = std::fs::read_to_string(&f).unwrap_or_default();
    std::fs::write(&f, format!("{prev}- [ ] step\n")).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", msg]);
}

async fn capture(state: &AppState, repo: &Path) {
    dirad::capture::capture_commits(state, &repo.display().to_string(), REPO).await;
}

#[tokio::test]
async fn a_source_unit_is_upserted_removed_and_never_deleted() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");

    capture(&state, repo.path()).await;
    let spec = state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .expect("the source unit became a spec");
    assert_eq!(spec.origin.as_deref(), Some("external"));
    assert_eq!(spec.verified, Some(false), "verified is permanently false");
    assert_eq!(spec.path, "openspec/changes/add-csv-import/proposal.md");
    assert_eq!(spec.first_commit.as_deref(), Some("aaa111"));
    assert_eq!(spec.last_commit.as_deref(), Some("bbb222"));
    let src = spec.source.as_ref().expect("source metadata");
    assert_eq!(
        (src.kind.as_str(), src.state.as_str()),
        ("openspec", "active")
    );
    assert_eq!(src.counts.tasks_total, 2);
    assert_eq!(src.counts.tasks_done, 1);
    assert_eq!(src.counts.scenarios, 2);
    assert!(spec.content_hash.is_some());
    let seq_after_first = state.store.zavet_specs_since(0, 100).await.unwrap();

    // A commit touching the root with an UNCHANGED unit writes nothing: the
    // knowledge cursor must not move, or every commit re-pushes every unit.
    commit_touching_root(repo.path(), "chore: tweak tasks");
    capture(&state, repo.path()).await;
    let seq_after_second = state.store.zavet_specs_since(0, 100).await.unwrap();
    assert_eq!(
        seq_after_first.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        seq_after_second.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        "an unchanged unit must not bump touched_seq"
    );

    // A real change (the unit's files move) updates the row in place.
    plugin.rows(&row("openspec", "add-csv-import", "done", ""));
    commit_touching_root(repo.path(), "feat: finish csv import");
    capture(&state, repo.path()).await;
    let moved = state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.source.as_ref().unwrap().state, "done");
    assert_eq!(
        moved.first_commit.as_deref(),
        Some("aaa111"),
        "first sight is kept"
    );

    // An EMPTY answer while the declared root is still on disk is a failed
    // resolve, not a deletion: nothing is removed.
    plugin.rows("");
    commit_touching_root(repo.path(), "chore: plugin hiccup");
    capture(&state, repo.path()).await;
    assert_eq!(
        state
            .store
            .zavet_spec_get(REPO, "openspec:add-csv-import")
            .await
            .unwrap()
            .unwrap()
            .source
            .unwrap()
            .state,
        "done",
        "an empty resolve with the root still present removes nothing"
    );

    // The root is really gone and the resolve is empty: marked removed,
    // never deleted.
    git(repo.path(), &["rm", "-r", "-q", "openspec"]);
    git(
        repo.path(),
        &["commit", "-q", "-m", "chore: drop the change"],
    );
    capture(&state, repo.path()).await;
    let gone = state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .expect("never deleted");
    assert_eq!(gone.source.as_ref().unwrap().state, "removed");
    // Native specs are untouched by all of this.
    assert!(state
        .store
        .zavet_specs_list(REPO)
        .await
        .unwrap()
        .iter()
        .all(|s| s.source.is_some()));
}

#[tokio::test]
async fn removals_only_happen_from_the_default_branch_checkout() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    capture(&state, repo.path()).await;
    let state_of = || async {
        state
            .store
            .zavet_spec_get(REPO, "openspec:add-csv-import")
            .await
            .unwrap()
            .unwrap()
            .source
            .unwrap()
            .state
    };
    assert_eq!(state_of().await, "active");

    // A feature branch cut BEFORE the unit existed would resolve to nothing
    // with its root absent. It must not flap the unit to removed.
    git(repo.path(), &["checkout", "-q", "-b", "feat/old-base"]);
    git(repo.path(), &["rm", "-r", "-q", "openspec"]);
    git(
        repo.path(),
        &["commit", "-q", "-m", "chore: branch without the unit"],
    );
    plugin.rows("");
    capture(&state, repo.path()).await;
    assert_eq!(
        state_of().await,
        "active",
        "off the default branch: no removals"
    );

    // A seen unit is still upserted from a non-default checkout.
    plugin.rows(&row("openspec", "add-csv-import", "review", ""));
    std::fs::create_dir_all(repo.path().join("openspec/changes/add-csv-import")).unwrap();
    commit_touching_root(repo.path(), "feat: bring the change back");
    capture(&state, repo.path()).await;
    assert_eq!(state_of().await, "review");

    // Back on the default branch the same empty answer (root gone) removes.
    git(repo.path(), &["checkout", "-q", "main"]);
    git(repo.path(), &["rm", "-r", "-q", "openspec"]);
    git(
        repo.path(),
        &["commit", "-q", "-m", "chore: drop the change"],
    );
    plugin.rows("");
    capture(&state, repo.path()).await;
    assert_eq!(state_of().await, "removed");
}

#[tokio::test]
async fn a_source_unit_without_commits_gets_a_null_origin_that_a_later_sighting_fills() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    let bare = row("openspec", "add-csv-import", "active", "")
        .replace(r#""firstCommit":"aaa111","#, "")
        .replace(r#""lastCommit":"bbb222","#, "");
    plugin.rows(&bare);
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    capture(&state, repo.path()).await;
    let spec = state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(spec.first_commit, None, "never an empty-string origin");
    assert_eq!(spec.last_commit, None);

    plugin.rows(&row("openspec", "add-csv-import", "done", ""));
    commit_touching_root(repo.path(), "feat: finish");
    capture(&state, repo.path()).await;
    let spec = state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(spec.first_commit.as_deref(), Some("aaa111"));
    assert_eq!(spec.last_commit.as_deref(), Some("bbb222"));
}

#[tokio::test]
async fn a_failed_or_unsupported_resolve_changes_nothing() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    capture(&state, repo.path()).await;
    assert_eq!(state.store.zavet_specs_list(REPO).await.unwrap().len(), 1);

    // An old plugin without the capability: fail open, remove nothing.
    plugin.version(r#"{"version":"1.7.1"}"#);
    plugin.rows("");
    commit_touching_root(repo.path(), "chore: more");
    capture(&state, repo.path()).await;
    let spec = state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        spec.source.as_ref().unwrap().state,
        "active",
        "absence of evidence is not removal"
    );

    // No plugin at all.
    std::env::set_var(
        dira_core::zavet_specs::PLUGIN_ROOT_ENV,
        "/nonexistent/plugin",
    );
    commit_touching_root(repo.path(), "chore: even more");
    capture(&state, repo.path()).await;
    assert_eq!(
        state
            .store
            .zavet_spec_get(REPO, "openspec:add-csv-import")
            .await
            .unwrap()
            .unwrap()
            .source
            .unwrap()
            .state,
        "active"
    );
}

#[tokio::test]
async fn the_repos_vendored_zavet_is_never_executed() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    // A tripwire in the repo: if dira ever ran this it would leave a marker.
    let marker: PathBuf = repo.path().join("PWNED");
    std::fs::create_dir_all(repo.path().join(".zavet/bin")).unwrap();
    let vendored = repo.path().join(".zavet/bin/zavet");
    std::fs::write(
        &vendored,
        format!("#!/bin/sh\ntouch {}\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&vendored, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "chore: vendor zavet"]);
    commit_touching_root(repo.path(), "chore: touch the root");
    capture(&state, repo.path()).await;
    assert!(!marker.exists(), "the repo's vendored zavet must never run");
    assert!(state
        .store
        .zavet_spec_get(REPO, "openspec:add-csv-import")
        .await
        .unwrap()
        .is_some());
}

fn session_event(id: &str, kind: EventKind, spec: Option<&str>) -> RawEvent {
    RawEvent {
        id: id.to_string(),
        at: OffsetDateTime::now_utc(),
        session_id: "s1".to_string(),
        harness: Harness::ClaudeCode,
        kind,
        cwd: None,
        project: Some(REPO.to_string()),
        identity_email: None,
        branch: None,
        tool: None,
        label: None,
        activity: None,
        note: None,
        spec: spec.map(str::to_string),
    }
}

/// Open session `s1` on REPO in both the store and the live registry, the way
/// the writer does for a hook session.
async fn open_session(state: &AppState, spec: Option<&str>) {
    for (id, kind, sp) in [
        ("e1", EventKind::SessionStart, spec),
        ("e2", EventKind::UserPrompt, None),
    ] {
        let ev = session_event(id, kind, sp);
        state.store.append(&ev).await.unwrap();
        dirad::control::lock_recover(&state.sessions).observe(&ev, time::Duration::minutes(5));
    }
}

async fn session_spec(state: &AppState) -> (Option<String>, Option<String>) {
    let live = dirad::control::lock_recover(&state.sessions).spec_of("s1");
    let stored = state
        .store
        .events_since(None)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| e.spec);
    (live, stored)
}

#[tokio::test]
async fn a_session_binds_to_the_spec_its_commit_trailer_names() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    open_session(&state, None).await;
    commit_touching_root(
        repo.path(),
        "feat: csv parser\n\nSpec: nonsense, openspec:add-csv-import",
    );
    capture(&state, repo.path()).await;
    assert_eq!(
        session_spec(&state).await,
        (
            Some("openspec:add-csv-import".into()),
            Some("openspec:add-csv-import".into())
        ),
        "the first trailer value that exactly matches a captured slug wins"
    );
    // Only exact matches count as references.
    let refs = state.store.zavet_spec_trailer_values(REPO).await.unwrap();
    assert!(refs.iter().any(|(_, v)| v == "openspec:add-csv-import"));
    assert!(
        refs.iter().any(|(_, v)| v == "nonsense"),
        "stored split, filtered at sync"
    );
}

#[tokio::test]
async fn a_session_binds_by_an_exact_branch_and_never_by_a_guess() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();

    // `<kind>/<id>` branch, no trailer.
    let state = test_state().await;
    let repo = repo_with_source("openspec/add-csv-import");
    open_session(&state, None).await;
    capture(&state, repo.path()).await;
    assert_eq!(
        session_spec(&state).await.0.as_deref(),
        Some("openspec:add-csv-import")
    );

    // A final segment that equals a unique source id.
    let state = test_state().await;
    let repo = repo_with_source("feat/add-csv-import");
    open_session(&state, None).await;
    capture(&state, repo.path()).await;
    assert_eq!(
        session_spec(&state).await.0.as_deref(),
        Some("openspec:add-csv-import")
    );

    // A near miss is not a match.
    let state = test_state().await;
    let repo = repo_with_source("feat/add-csv-import-v2");
    open_session(&state, None).await;
    capture(&state, repo.path()).await;
    assert_eq!(session_spec(&state).await, (None, None));

    // An ambiguous final segment (two kinds share the id) binds nothing.
    plugin.rows(&format!(
        "{}\n{}",
        row("openspec", "dup", "active", ""),
        row("speckit", "dup", "active", "")
    ));
    let state = test_state().await;
    let repo = repo_with_source("feat/dup");
    open_session(&state, None).await;
    capture(&state, repo.path()).await;
    assert_eq!(
        session_spec(&state).await,
        (None, None),
        "ambiguous -> none"
    );
}

#[tokio::test]
async fn an_explicit_spec_is_never_overridden_by_a_resolution() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("openspec/add-csv-import");
    open_session(&state, Some("capture-pipeline")).await;
    commit_touching_root(repo.path(), "feat: x\n\nSpec: openspec:add-csv-import");
    capture(&state, repo.path()).await;
    assert_eq!(
        session_spec(&state).await.0.as_deref(),
        Some("capture-pipeline"),
        "dira start --spec wins over trailer and branch"
    );
}

#[tokio::test]
async fn reindex_walks_the_declared_roots_and_a_repeat_run_writes_nothing() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    // Many later commits that do not touch any root, so the first-sight
    // backfill window cannot reach the introducing commit on its own.
    for i in 0..20 {
        std::fs::write(repo.path().join(format!("f{i}.txt")), "x").unwrap();
        git(repo.path(), &["add", "-A"]);
        git(
            repo.path(),
            &["commit", "-q", "-m", &format!("chore: f{i}")],
        );
    }
    let cwd = repo.path().display().to_string();
    let resp = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::ZavetReindex {
            cwd: Some(cwd.clone()),
            all_trailers: false,
        },
    )
    .await;
    // The reindex needs a canonical repo from the cwd's remote.
    let dira_core::protocol::Response::ZavetReindex(first) = resp else {
        panic!("expected a reindex view");
    };
    assert!(first.active);
    assert!(
        first.specs_indexed >= 1,
        "the source unit was indexed: {first:?}"
    );
    let seqs = |s: &AppState| {
        let s = s.clone();
        async move {
            s.store
                .zavet_specs_since(0, 100)
                .await
                .unwrap()
                .iter()
                .map(|(q, _)| *q)
                .collect::<Vec<_>>()
        }
    };
    let before = seqs(&state).await;
    assert!(!before.is_empty());
    let resp = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::ZavetReindex {
            cwd: Some(cwd),
            all_trailers: false,
        },
    )
    .await;
    let dira_core::protocol::Response::ZavetReindex(second) = resp else {
        panic!("expected a reindex view");
    };
    assert_eq!(
        seqs(&state).await,
        before,
        "a repeat reindex must not churn the cursor"
    );
    let _ = second;
}

#[tokio::test]
async fn zavet_why_and_specs_address_a_source_unit_by_its_slug() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();
    let state = test_state().await;
    let repo = repo_with_source("main");
    commit_touching_root(
        repo.path(),
        "feat: csv\n\nSpec: openspec:add-csv-import, capture-pipeline",
    );
    capture(&state, repo.path()).await;

    // Registers the repo dir so the by-project query ladder can resolve it.
    let why = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::ZavetWhy {
            query: "openspec:add-csv-import".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match why {
        dira_core::protocol::Response::ZavetSpec(v) => {
            assert_eq!(v.spec.slug, "openspec:add-csv-import");
            let src = v.spec.source.expect("source view");
            assert_eq!(src.kind, "openspec");
            assert_eq!((src.tasks_done, src.tasks_total), (1, 2));
            assert!(
                !v.commits.is_empty(),
                "the comma-list trailer commit links to the unit"
            );
        }
        other => panic!("expected a spec answer, got {other:?}"),
    }

    let list = dirad::control::dispatch(
        &state,
        dira_core::protocol::Request::ZavetSpecs {
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match list {
        dira_core::protocol::Response::ZavetSpecs(v) => {
            assert_eq!(v.specs.len(), 1);
            assert!(v.specs[0].paths.is_empty(), "paths: none yet");
        }
        other => panic!("expected a specs list, got {other:?}"),
    }
}

#[tokio::test]
async fn an_ended_session_is_not_bound_late_and_a_removed_unit_is_not_a_target() {
    let _g = PLUGIN_ENV.lock().await;
    let plugin = FakePlugin::new();
    plugin.rows(&row("openspec", "add-csv-import", "active", ""));
    plugin.activate();

    // Ended before the binding lands: the rollup is out, so no late binding.
    let state = test_state().await;
    let repo = repo_with_source("main");
    capture(&state, repo.path()).await; // the unit is known
    open_session(&state, None).await;
    let end = session_event("e3", EventKind::SessionEnd, None);
    state.store.append(&end).await.unwrap();
    dirad::control::lock_recover(&state.sessions).observe(&end, time::Duration::minutes(5));
    let bound = dirad::zavet_sources::attribute_session(
        &state.store,
        &state.sessions,
        "s1",
        REPO,
        &[],
        Some("openspec/add-csv-import"),
    )
    .await;
    assert_eq!(bound, None);
    assert_eq!(session_spec(&state).await, (None, None));

    // A unit already marked removed is not something a session binds to.
    let state = test_state().await;
    let repo = repo_with_source("main");
    capture(&state, repo.path()).await; // unit captured (no session yet)
    plugin.rows("");
    git(repo.path(), &["rm", "-r", "-q", "openspec"]);
    git(repo.path(), &["commit", "-q", "-m", "chore: drop"]);
    capture(&state, repo.path()).await;
    assert_eq!(
        state
            .store
            .zavet_spec_get(REPO, "openspec:add-csv-import")
            .await
            .unwrap()
            .unwrap()
            .source
            .unwrap()
            .state,
        "removed"
    );
    open_session(&state, None).await;
    std::fs::write(repo.path().join("notes.txt"), "x").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(
        repo.path(),
        &[
            "commit",
            "-q",
            "-m",
            "feat: more\n\nSpec: openspec:add-csv-import",
        ],
    );
    capture(&state, repo.path()).await;
    assert_eq!(session_spec(&state).await, (None, None));
}
