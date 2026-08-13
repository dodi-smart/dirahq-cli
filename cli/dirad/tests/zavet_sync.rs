//! `dira zavet sync` — the on-demand sweep, and the repo registration that
//! makes the idle ticker keep reaching a repo afterwards.
//!
//! The ticker only visits repos in `repo_dirs`, and that map is empty on a
//! freshly restarted daemon: a repo nobody has opened a session in is never
//! swept at all, and before this command there was no user-side remedy. The
//! boundary these tests pin is that closing the LATENCY hole does not widen
//! the SCOPE one — capture reads git objects, so sync can never pick up an
//! uncommitted record (DIRASH-0026), and it must not start trying.

use dira_core::protocol::{Request, Response, ZavetSyncView};
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::path::Path;
use std::process::Command;

const CANONICAL: &str = "github.com/acme/api";

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

fn decision(id: &str, title: &str) -> String {
    format!(
        "---\nid: {id}\ntitle: {title}\nstatus: active\nguards:\n  - src/**\n---\n\n## Decision\n{title}.\n"
    )
}

/// A repo with one COMMITTED decision record.
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
    std::fs::write(
        p.join(".zavet/decisions/D-0001-poll.md"),
        decision("D-0001", "Poll git instead of watching the filesystem"),
    )
    .unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "docs: record polling decision"]);
    dir
}

async fn sync_at(state: &AppState, cwd: &str) -> Box<ZavetSyncView> {
    let resp = dirad::control::dispatch(
        state,
        Request::ZavetSync {
            cwd: Some(cwd.to_string()),
            repo: None,
        },
    )
    .await;
    match resp {
        Response::ZavetSync(v) => v,
        other => panic!("expected ZavetSync, got {other:?}"),
    }
}

/// The hole this command exists for: a repo the daemon has never seen. Nothing
/// registers it, so the ticker never sweeps it, so its knowledge is invisible
/// forever. One sync both sweeps and registers it.
#[tokio::test]
async fn sync_sweeps_and_registers_a_repo_the_daemon_has_never_seen() {
    let state = test_state().await;
    let repo = setup_repo();
    // Run it from a SUBDIRECTORY: the registered dir must be the repo root, or
    // the ticker inherits a path that `knowledge_sync`'s `.zavet/` probe and
    // its cwd-relative pathspecs cannot resolve from.
    let sub = repo.path().join("src/capture");
    std::fs::create_dir_all(&sub).unwrap();

    assert!(
        dirad::control::lock_recover_map(&state.repo_dirs).is_empty(),
        "a fresh daemon knows no repos"
    );

    let v = sync_at(&state, &sub.display().to_string()).await;
    assert!(v.registered, "first sync registers the repo");
    assert!(v.active);
    assert_eq!(v.decisions_captured, 1, "and sweeps it in the same call");
    assert_eq!(v.decisions_total, 1);

    // Asserted by shape rather than by string equality: git reports a toplevel
    // as `C:/…` where `fs::canonicalize` yields `\\?\C:\…`, so comparing the
    // two forms only ever tested which platform the suite is on.
    let registered = dirad::control::lock_recover_map(&state.repo_dirs)
        .get(CANONICAL)
        .map(std::path::PathBuf::from)
        .expect("the idle ticker will now reach this repo");
    assert!(
        registered.join(".zavet").is_dir(),
        "registered {registered:?}, which is not the repo root"
    );
    assert_ne!(registered, sub, "resolved upward from the caller's cwd");
}

/// Sync reuses the ordinary capture path rather than forcing a re-read, so an
/// unchanged HEAD stays a complete no-op — the same invariant `zavet_capture`
/// pins for the ticker. A `--force` flag would break it and could not help
/// anyway: re-reading only re-ingests blobs already stored.
#[tokio::test]
async fn a_second_sync_with_unchanged_head_captures_nothing() {
    let state = test_state().await;
    let repo = setup_repo();
    let cwd = repo.path().display().to_string();

    sync_at(&state, &cwd).await;
    let again = sync_at(&state, &cwd).await;

    assert!(!again.registered, "already known");
    assert_eq!(again.decisions_captured, 0);
    assert_eq!(again.trailers_captured, 0);
    assert_eq!(again.decisions_total, 1);
}

/// The boundary. A record on disk but not committed is REPORTED by the sweep,
/// never ingested by it — and once committed, one sync picks it up without
/// waiting out the ticker, which is the whole point of the command.
#[tokio::test]
async fn sync_reports_an_uncommitted_record_then_captures_it_once_committed() {
    let state = test_state().await;
    let repo = setup_repo();
    let cwd = repo.path().display().to_string();

    std::fs::write(
        repo.path().join(".zavet/decisions/D-0002-precedence.md"),
        decision("D-0002", "Resolve configs by precedence"),
    )
    .unwrap();

    let before = sync_at(&state, &cwd).await;
    assert_eq!(
        before.decisions_captured, 1,
        "the committed D-0001 lands; the uncommitted one does not"
    );
    let pending: Vec<_> = before
        .uncaptured
        .iter()
        .map(|u| (u.id.as_deref().unwrap_or("?"), u.reason.as_str()))
        .collect();
    assert_eq!(
        pending,
        vec![("D-0002", "uncommitted")],
        "reported with the reason that names the real remedy"
    );

    git(repo.path(), &["add", "-A"]);
    git(
        repo.path(),
        &["commit", "-q", "-m", "docs(zavet): record D-0002"],
    );

    let after = sync_at(&state, &cwd).await;
    assert_eq!(after.decisions_captured, 1);
    assert_eq!(after.decisions_total, 2);
    assert!(
        after.uncaptured.is_empty(),
        "committing is what makes it capturable, and the sweep proves it"
    );
}

/// Naming a repo the daemon has no directory for cannot sweep anything, and
/// says so rather than reporting a successful zero-capture sync.
#[tokio::test]
async fn sync_without_a_known_directory_is_an_honest_error() {
    let state = test_state().await;
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetSync {
            cwd: None,
            repo: Some(CANONICAL.to_string()),
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(
            message.contains("no working directory known"),
            "unexpected message: {message}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// `--project` names a repo this process is not standing in, but the daemon
/// usually remembers a directory for it — and that repo's `id-width` decides
/// how a shorthand id canonicalizes. Falling back to the defaults re-padded
/// `CLOUD-42` to a width-4 `CLOUD-0042` and missed the stored `CLOUD-00042`,
/// reporting no such decision for a record sitting right there.
#[tokio::test]
async fn a_project_scoped_why_uses_the_named_repos_id_width() {
    let state = test_state().await;
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
    std::fs::write(p.join(".zavet/config"), "prefix: CLOUD\nid-width: 5\n").unwrap();
    std::fs::write(
        p.join(".zavet/decisions/CLOUD-00042-wide.md"),
        decision("CLOUD-00042", "Pad ids to five digits"),
    )
    .unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "docs: record CLOUD-00042"]);

    // Registering is what gives the daemon a directory for the named repo —
    // exactly what `zavet sync` (or any session in it) does.
    sync_at(&state, &p.display().to_string()).await;

    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "CLOUD-42".to_string(),
            cwd: None,
            repo: Some(CANONICAL.to_string()),
        },
    )
    .await;
    match resp {
        Response::ZavetWhy(v) => assert_eq!(v.decision.id, "CLOUD-00042"),
        other => panic!("expected ZavetWhy for the width-5 id, got {other:?}"),
    }
}
