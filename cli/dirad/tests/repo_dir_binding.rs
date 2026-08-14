//! What may enter `repo_dirs`, and on whose word.
//!
//! The map decides what the idle ticker sweeps, so an entry is not a cache hint
//! — it is a standing instruction to run git in that directory and record what
//! it finds under that name. #119 closed this hole for `dira zavet sync`
//! (DIRASH-0027); these tests pin the other entry point that can carry a
//! caller-supplied repo name, `dira start --project`.
//!
//! The distinction under test is *provenance*, not correctness of the name: a
//! project name derived from a directory is evidence about that directory, a
//! name the caller asserted is not.

use dira_core::protocol::{Request, Response};
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::path::Path;
use std::process::Command;

const CANONICAL: &str = "github.com/acme/api";
const SOMEWHERE_ELSE: &str = "github.com/acme/unrelated-service";

/// Returns the receivers alongside the state, and every caller must bind them:
/// dropping the event receiver closes the channel, and `Request::Start` then
/// answers "daemon shutting down" instead of doing anything worth asserting on.
async fn test_state() -> (AppState, impl Sized) {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, rx, sync_rx, knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    (state, (rx, sync_rx, knowledge_rx))
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A checkout whose origin remote canonicalizes to [`CANONICAL`].
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
    std::fs::write(p.join("README.md"), "x").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "chore: init"]);
    dir
}

async fn start_at(state: &AppState, project: Option<&str>, cwd: &Path) -> Response {
    dirad::control::dispatch(
        state,
        Request::Start {
            project: project.map(str::to_string),
            label: None,
            activity: None,
            note: None,
            cwd: Some(cwd.display().to_string()),
        },
    )
    .await
}

fn registered(state: &AppState) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = dirad::control::lock_recover_map(&state.repo_dirs)
        .iter()
        .map(|(k, val)| (k.clone(), val.clone()))
        .collect();
    v.sort();
    v
}

/// The bug (#122). `--project` names a repo this checkout is not; the cwd
/// arrives alongside it purely because that is where the user happened to be
/// standing. Registering the pair would enrol a foreign checkout under that
/// name, and the ticker would then capture *this* repo's commits as the other
/// one's — the same poisoning DIRASH-0027 fixed for `zavet sync`.
#[tokio::test]
async fn an_unrelated_project_name_does_not_enrol_the_callers_checkout() {
    let (state, _guards) = test_state().await;
    let repo = setup_repo();

    let resp = start_at(&state, Some(SOMEWHERE_ELSE), repo.path()).await;
    assert!(
        matches!(resp, Response::Started { .. }),
        "the session itself must still start — only the registration is refused: {resp:?}"
    );

    assert!(
        registered(&state).is_empty(),
        "a checkout that is not {SOMEWHERE_ELSE} must not be swept as it, got {:?}",
        registered(&state)
    );
}

/// The name is derived from the directory, so the pair is a single fact and
/// needs no second opinion. This is the ordinary `dira start` path.
#[tokio::test]
async fn a_cwd_derived_project_registers_that_cwd() {
    let (state, _guards) = test_state().await;
    let repo = setup_repo();

    start_at(&state, None, repo.path()).await;

    assert_eq!(
        registered(&state),
        vec![(CANONICAL.to_string(), repo.path().display().to_string())],
        "a repo resolved from the cwd is exactly what the ticker should sweep"
    );
}

/// `--project` is not refused wholesale — only unverified. Naming the repo you
/// are actually standing in keeps working, which is what stops the fix from
/// being a silent feature removal.
#[tokio::test]
async fn an_explicit_project_matching_the_cwd_still_registers() {
    let (state, _guards) = test_state().await;
    let repo = setup_repo();

    start_at(&state, Some(CANONICAL), repo.path()).await;

    assert_eq!(
        registered(&state),
        vec![(CANONICAL.to_string(), repo.path().display().to_string())],
        "the cwd demonstrably belongs to the named repo, so it is trusted"
    );
}

/// A directory with no resolvable remote cannot vouch for any name. It must not
/// fall through to trusting the caller — that is the same unverified pairing by
/// another route.
#[tokio::test]
async fn a_non_repo_cwd_never_vouches_for_an_explicit_project() {
    let (state, _guards) = test_state().await;
    let plain = tempfile::tempdir().expect("tempdir");

    start_at(&state, Some(CANONICAL), plain.path()).await;

    assert!(
        registered(&state).is_empty(),
        "a plain directory resolves to no repo, so it vouches for nothing"
    );
}

/// A retroactive entry records time against a name and never asks the ticker to
/// sweep anything, so it must not register regardless of what it is handed.
#[tokio::test]
async fn a_retroactive_log_entry_registers_nothing() {
    let (state, _guards) = test_state().await;
    let repo = setup_repo();

    let resp = dirad::control::dispatch(
        &state,
        Request::Log {
            duration_secs: 600,
            project: Some(SOMEWHERE_ELSE.to_string()),
            note: None,
            activity: None,
            label: None,
            cwd: Some(repo.path().display().to_string()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Logged { .. }), "got {resp:?}");

    assert!(
        registered(&state).is_empty(),
        "`dira log` has never registered a directory and must not start"
    );
}
