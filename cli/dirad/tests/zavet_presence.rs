//! Branch presence for the zavet list views: does a captured record's file
//! actually exist on the branch the caller is standing on, and is there a
//! record on disk that capture has never seen?
//!
//! The failure this pins was a field report: `dira zavet decisions` printed
//! seven decisions on a branch whose tree held a *different* seven. Four came
//! from a branch the user had left weeks earlier and four freshly-written ones
//! were missing — capture reads git objects, not the working tree. Both halves
//! were invisible, which is what made it a bug report instead of a shrug.
//!
//! Nothing here asserts that a row is removed. Rows must not be: decision ids
//! are minted repo-wide, so an off-branch record has to stay reachable.

use dira_core::protocol::{Response, ZavetPresence};
use dira_core::store::ZavetDecisionCapture;
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::path::Path;
use std::process::Command;

const REPO: &str = "github.com/acme/api";

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    state
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A record file with the minimum frontmatter the parser accepts.
fn write_record(root: &Path, id: &str, title: &str) -> String {
    let rel = format!(".zavet/decisions/{id}-{}.md", title.replace(' ', "-"));
    std::fs::create_dir_all(root.join(".zavet/decisions")).unwrap();
    std::fs::write(
        root.join(&rel),
        format!("---\nid: {id}\ntitle: {title}\nstatus: active\nguards:\n  - src/**\n---\n\n## Decision\n\nBecause.\n"),
    )
    .unwrap();
    rel
}

/// A repo with `.zavet/`, one commit on `main`, and git identity configured.
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
    std::fs::write(dir.join("README.md"), "x").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "init"]);
}

async fn capture(state: &AppState, rel: &str, id: &str, sha: &str) {
    let cap = ZavetDecisionCapture {
        id: id.to_string(),
        title: Some(id.to_string()),
        status: Some("active".into()),
        path: rel.to_string(),
        ..Default::default()
    };
    state
        .store
        .zavet_upsert_decision(REPO, &cap, sha, None, None)
        .await
        .unwrap();
}

fn decisions_view(resp: Response) -> dira_core::protocol::ZavetDecisionsView {
    match resp {
        Response::ZavetDecisions(v) => *v,
        other => panic!("expected ZavetDecisions, got {other:?}"),
    }
}

/// The reported defect, both halves at once: a record committed on another
/// branch is marked off-branch (not deleted, not silently listed as governing
/// this tree), and a record sitting uncommitted in the working tree is reported
/// as uncaptured rather than simply missing.
#[tokio::test]
async fn off_branch_and_uncommitted_records_are_both_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    init_repo(root);

    // D-0001 lives only on `other`.
    git(root, &["checkout", "-q", "-b", "other"]);
    let other_rel = write_record(root, "D-0001", "from another branch");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "decide D-0001"]);

    // D-0005 lives on `feature`, which never saw D-0001.
    git(root, &["checkout", "-q", "main"]);
    git(root, &["checkout", "-q", "-b", "feature"]);
    let here_rel = write_record(root, "D-0005", "on this branch");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "decide D-0005"]);

    // Both are captured — the store pools by repo, with no branch predicate.
    let state = test_state().await;
    capture(&state, &other_rel, "D-0001", "sha-other").await;
    capture(&state, &here_rel, "D-0005", "sha-here").await;

    // D-0008 is written but never committed: on disk, invisible to capture.
    write_record(root, "D-0008", "still being written");

    let v = decisions_view(
        dirad::zavet::decisions(
            &state,
            Some(root.display().to_string()),
            Some(REPO.to_string()),
        )
        .await,
    );

    assert_eq!(v.branch.as_deref(), Some("feature"));

    let by_id = |id: &str| {
        v.decisions
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("{id} missing from the list — rows are never dropped"))
    };
    assert_eq!(by_id("D-0005").presence, Some(ZavetPresence::OnBranch));
    assert_eq!(by_id("D-0001").presence, Some(ZavetPresence::OffBranch));

    assert_eq!(v.uncaptured.len(), 1, "{:#?}", v.uncaptured);
    let u = &v.uncaptured[0];
    assert_eq!(u.id.as_deref(), Some("D-0008"));
    assert_eq!(u.reason, "uncommitted");
    assert_eq!(u.kind, "decision");
}

/// A record that IS committed but has not been swept yet is a different
/// problem with a different remedy — it needs time (or a daemon that watches
/// the repo), not a commit — so it must not be reported as uncommitted.
#[tokio::test]
async fn a_committed_but_unswept_record_says_awaiting_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    init_repo(root);
    write_record(root, "D-0002", "committed but unswept");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "decide D-0002"]);

    let state = test_state().await;
    let v = decisions_view(
        dirad::zavet::decisions(
            &state,
            Some(root.display().to_string()),
            Some(REPO.to_string()),
        )
        .await,
    );

    assert!(v.decisions.is_empty());
    assert_eq!(v.uncaptured.len(), 1);
    assert_eq!(v.uncaptured[0].reason, "awaiting sweep");
}

/// Without a working directory there is nothing to ask git about. Presence must
/// come back unknown rather than defaulting to off-branch, which would report
/// every record in the repo as belonging to somewhere else.
#[tokio::test]
async fn presence_is_unknown_without_a_working_directory() {
    let state = test_state().await;
    capture(&state, ".zavet/decisions/D-0001-x.md", "D-0001", "sha1").await;

    let v = decisions_view(dirad::zavet::decisions(&state, None, Some(REPO.to_string())).await);

    assert_eq!(v.decisions.len(), 1);
    assert_eq!(v.decisions[0].presence, None);
    assert!(v.uncaptured.is_empty());
    assert_eq!(v.branch, None);
}

/// A record renamed in the working tree is the same record — it is keyed by
/// id, not by path — so it must not be reported as an uncaptured duplicate of
/// itself.
#[tokio::test]
async fn a_renamed_record_is_not_reported_as_uncaptured() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    init_repo(root);
    let rel = write_record(root, "D-0003", "renamed since capture");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "decide D-0003"]);

    let state = test_state().await;
    // Captured under an older filename for the same id.
    capture(
        &state,
        ".zavet/decisions/D-0003-old-slug.md",
        "D-0003",
        "sha1",
    )
    .await;
    assert!(rel.contains("D-0003"));

    let v = decisions_view(
        dirad::zavet::decisions(
            &state,
            Some(root.display().to_string()),
            Some(REPO.to_string()),
        )
        .await,
    );

    assert!(
        v.uncaptured.is_empty(),
        "renamed record reported as uncaptured: {:#?}",
        v.uncaptured
    );
}

/// A brand-new repo with no commits at all: the first decision of a project is
/// written before the first commit, so this is the *first* thing a new user
/// does — and it is the case that most needs the uncaptured report.
///
/// An unborn HEAD makes presence unknown, but the file is still on disk, so the
/// scan must still run. Bailing out on "no HEAD" reported zero uncaptured
/// records here, which is exactly the "dira lost my decision" failure.
#[tokio::test]
async fn an_unborn_branch_still_reports_records_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    // No commit — HEAD does not resolve.
    write_record(root, "D-0001", "the very first decision");

    let state = test_state().await;
    let v = decisions_view(
        dirad::zavet::decisions(
            &state,
            Some(root.display().to_string()),
            Some(REPO.to_string()),
        )
        .await,
    );

    assert_eq!(v.uncaptured.len(), 1, "{:#?}", v.uncaptured);
    assert_eq!(v.uncaptured[0].id.as_deref(), Some("D-0001"));
    // Nothing is committed, so "uncommitted" is the truthful reason.
    assert_eq!(v.uncaptured[0].reason, "uncommitted");
}
