//! Commit capture — kept strictly off the writer's hot path.
//!
//! `capture_commits` shells out to `git` (`rev-parse`, `log`, `diff-tree |
//! patch-id`), which is blocking IO with no inherent timeout. If git stalls
//! (index.lock contention, a slow filesystem, a hung credential helper) a naive
//! inline call would freeze the single writer task — and with it every session's
//! `active_seconds` accrual and the `ManualTick` queue.
//!
//! So the blocking git walk runs in [`tokio::task::spawn_blocking`] wrapped in a
//! [`tokio::time::timeout`]: a capture that overruns the budget is logged and
//! dropped (the next commit-bearing event retries it). The whole capture is
//! `spawn`ed as a detached task by the writer, which returns to draining
//! immediately — git can never block timer accrual again.

use crate::state::AppState;
use dira_core::model::EventKind;
use dira_core::project::{self, CapturedCommit};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Minimum spacing between git polls for one repo. Keeps a tool-call burst from
/// shelling out to git on every event; the idle ticker covers the steady state.
const COMMIT_POLL_THROTTLE: Duration = Duration::from_secs(5);
/// Backfill depth the first time a repo is seen; afterwards only `<head>..HEAD`.
const COMMIT_BACKFILL_LIMIT: usize = 15;
/// Cap on commits captured in a single `<head>..HEAD` walk (a runaway guard).
const COMMIT_CAPTURE_LIMIT: usize = 200;
/// Wall-clock budget for one git capture's blocking work. A capture that exceeds
/// it is abandoned (logged + dropped); the next commit-bearing event retries.
/// Comfortably above a healthy `git log`, low enough that a wedged git is shed
/// long before it could matter to a watching human.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-repo poll throttle. Lives in the single-threaded writer, so no lock.
#[derive(Default)]
pub struct Throttle {
    last: HashMap<String, Instant>,
}

impl Throttle {
    /// True if `repo` hasn't been polled within [`COMMIT_POLL_THROTTLE`]; records
    /// the poll time when it returns true.
    pub fn ready(&mut self, repo: &str) -> bool {
        let now = Instant::now();
        match self.last.get(repo) {
            Some(t) if now.duration_since(*t) < COMMIT_POLL_THROTTLE => false,
            _ => {
                self.last.insert(repo.to_string(), now);
                true
            }
        }
    }
}

/// Events after which a commit may have just landed — a tool call returned, an
/// agent turn ended, or a session/manual session closed.
pub fn captures_commits(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::PostTool
            | EventKind::Stop
            | EventKind::SessionEnd
            | EventKind::ManualStart
            | EventKind::ManualStop
            | EventKind::ManualTick
    )
}

/// Spawn a detached commit capture for `(cwd, canonical)`. Returns immediately —
/// the caller (the writer) never awaits git. The blocking git walk inside is
/// timeout-bounded so a wedged git is shed instead of leaking a task forever.
pub fn spawn_capture(state: &AppState, cwd: &str, canonical: &str) {
    let state = state.clone();
    let cwd = cwd.to_string();
    let canonical = canonical.to_string();
    tokio::spawn(async move {
        capture_commits(&state, &cwd, &canonical).await;
    });
}

/// The result of the blocking git portion of a capture, computed entirely inside
/// `spawn_blocking` so no `git` call ever runs on a runtime worker inline.
struct GitWalk {
    head: String,
    commits: Vec<CapturedCommit>,
    git_ref: Option<String>,
    /// Squash-resilient session anchoring signals over the cumulative diff
    /// (`merge-base(upstream, HEAD)..HEAD`), computed once per walk inside the
    /// same `spawn_blocking` so this extra git work never touches the hot path.
    signals: project::SessionSignals,
}

/// Run the blocking git walk for a repo: resolve HEAD, and (unless HEAD is
/// unchanged vs `baseline`) list the new commits + current branch. `None` when
/// the dir isn't a git repo, HEAD is unchanged, or git is unavailable.
fn git_walk(cwd: &str, baseline: Option<&str>) -> Option<GitWalk> {
    let root = Path::new(cwd);
    let head = project::head_sha(root)?;
    if baseline == Some(head.as_str()) {
        return None; // HEAD unchanged since last poll — nothing to do
    }
    let range = baseline.map(|b| format!("{b}..HEAD"));
    let limit = if baseline.is_some() {
        COMMIT_CAPTURE_LIMIT
    } else {
        COMMIT_BACKFILL_LIMIT
    };
    let commits = project::log_commits(root, range.as_deref(), limit);
    let git_ref = project::current_branch(root);
    // Cumulative session signals — best-effort, all-None on merge/detached HEAD,
    // missing upstream, or git failure. Runs here so it shares the blocking budget.
    let signals = project::session_signals(root);
    Some(GitWalk {
        head,
        commits,
        git_ref,
        signals,
    })
}

/// Poll a repo for new commits and record them locally. Best-effort: a non-git
/// dir or a git failure simply captures nothing. On a repo's first sight it does a
/// bounded backfill of recent commits, then only commits past the recorded HEAD
/// watermark. Nudges sync when anything new was recorded.
///
/// The git portion runs in [`spawn_blocking`](tokio::task::spawn_blocking) under
/// a [`timeout`](tokio::time::timeout); the surrounding async DB work is cheap.
/// This whole future is itself spawned detached by the writer (see
/// [`spawn_capture`]), so neither the blocking git nor a hung capture can stall
/// the drain loop.
pub async fn capture_commits(state: &AppState, cwd: &str, canonical: &str) {
    let baseline = match state.store.repo_baseline_get(canonical).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("repo baseline read failed for {canonical}: {e}");
            return;
        }
    };

    // The blocking git work, off the runtime worker and time-boxed.
    let walk = {
        let cwd = cwd.to_string();
        let baseline = baseline.clone();
        let join = tokio::task::spawn_blocking(move || git_walk(&cwd, baseline.as_deref()));
        match tokio::time::timeout(CAPTURE_TIMEOUT, join).await {
            Ok(Ok(walk)) => walk,
            Ok(Err(e)) => {
                tracing::warn!("commit capture task failed for {canonical}: {e}");
                return;
            }
            Err(_) => {
                // git overran the budget — drop this capture; the next
                // commit-bearing event retries. Never blocks the writer.
                tracing::warn!(
                    repo = %canonical,
                    timeout_secs = CAPTURE_TIMEOUT.as_secs(),
                    "commit capture timed out — dropping (will retry on next event)"
                );
                return;
            }
        }
    };
    let Some(walk) = walk else {
        return; // not a git repo, or HEAD unchanged
    };

    // The session the daemon currently observes for this repo, IFF unambiguous
    // (exactly one active). Concurrent or absent sessions yield None and the
    // cloud anchors on author + time instead.
    let source_session = crate::control::lock_recover(&state.sessions).session_for_repo(canonical);

    let mut recorded = 0usize;
    for c in &walk.commits {
        match state
            .store
            .record_commit(
                c,
                Some(canonical),
                walk.git_ref.as_deref(),
                source_session.as_deref(),
                Some(&walk.signals),
            )
            .await
        {
            Ok(true) => recorded += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!("record commit {} failed: {e}", c.sha),
        }
    }
    if let Err(e) = state.store.repo_baseline_set(canonical, &walk.head).await {
        tracing::warn!("repo baseline set failed for {canonical}: {e}");
    }
    if recorded > 0 {
        tracing::info!(commits = recorded, repo = %canonical, "captured commits");
        let _ = state.sync.trigger.try_send(());
        // A commit landing is activity — wake the heartbeat too (WP-A3).
        state.presence_wake.notify_waiters();
    }
}
