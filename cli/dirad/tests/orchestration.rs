//! Orchestration sessions end to end (DIRASH-0041): a Claude Desktop session
//! rooted in a scratch workspace reaches repos through its tool calls and
//! spawns child sessions. Driven through the real ingress (`IngestHook` →
//! normalize → writer → store → registry), then rolled up for the wire.

use dira_contract::{SessionRole, SessionRollup};
use dira_core::model::{EventKind, RawEvent};
use dira_core::protocol::Request;
use dira_core::{Config, Store};
use dirad::state::AppState;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration as StdDuration;

const ORCH_HOST: &str = "local_684b6117-0abc-44f3-9ce0-44f092702849";
const KID_HOST: &str = "local_f42e16b4-7364-4ba7-966e-6d2f2eaa2147";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A git repo at `dir` whose origin is `github.com/<slug>`. Returns the path
/// as a hook cwd.
fn git_repo(dir: &Path, slug: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    let origin = format!("git@github.com:{slug}.git");
    git(dir, &["remote", "add", "origin", &origin]);
    dir.display().to_string()
}

/// `<tmp>/Claude/scratch-workspaces/acct/org/scratch-…/` with a clone of
/// `acme/api` under `repos/api`. Returns (guard, scratch root, repo dir).
fn scratch_with_clone() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scratch = tmp
        .path()
        .join("Claude/scratch-workspaces/acct/org/scratch-2026-10-15-abc123");
    let repo = scratch.join("repos/api");
    git_repo(&repo, "acme/api");
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "T"]);
    (tmp, scratch, repo)
}

async fn hook(state: &AppState, payload: serde_json::Value) {
    dirad::control::dispatch(
        state,
        Request::IngestHook {
            harness: "claude".into(),
            payload,
        },
    )
    .await;
}

fn env(host: &str) -> serde_json::Value {
    json!({ "host_session_id": host })
}

/// A daemon over an in-memory store, with its writer running.
async fn start() -> (AppState, tokio::task::JoinHandle<()>) {
    let store = Store::open_in_memory().await.expect("store");
    let (state, rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    let writer_state = state.clone();
    let writer = tokio::spawn(async move {
        dirad::writer::writer_with(rx, writer_state, |_, _, _| {}).await;
    });
    (state, writer)
}

/// Every stored event in timestamp order, once the async writer has stored all
/// `n` of them. `events_since` orders by `at` as RFC3339 text, whose trimmed
/// fractions mis-sort events within one second, so sort by the parsed time
/// before a test indexes into it.
async fn wait_events(state: &AppState, n: usize) -> Vec<RawEvent> {
    let mut events = Vec::new();
    for _ in 0..200 {
        events = state.store.events_since(None).await.unwrap();
        events.sort_by_key(|e| e.at);
        if events.len() >= n {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    assert_eq!(events.len(), n, "every hook stored: {events:#?}");
    events
}

/// The wire rollups for `events`, with each session in `ended` closed by a
/// `SessionEnd` five seconds after the last event so its final rollup ships.
fn rollups_with_end(events: &[RawEvent], ended: &[&str]) -> Vec<SessionRollup> {
    let mut all = events.to_vec();
    let end_at = all.last().unwrap().at + time::Duration::seconds(5);
    for (i, sid) in ended.iter().enumerate() {
        let mut end = events
            .iter()
            .find(|e| e.session_id == *sid)
            .unwrap()
            .clone();
        end.id = format!("ZZZ{i}");
        end.kind = EventKind::SessionEnd;
        end.at = end_at;
        all.push(end);
    }
    dira_core::sync::build_batch(
        &all,
        &[],
        &[],
        "dev",
        time::Duration::minutes(5),
        Default::default(),
        end_at,
    )
    .sessions
}

fn rollup<'a>(rollups: &'a [SessionRollup], sid: &str) -> &'a SessionRollup {
    rollups
        .iter()
        .find(|s| s.session_id == sid)
        .unwrap_or_else(|| panic!("{sid} rollup"))
}

#[tokio::test]
async fn an_orchestrator_attributes_per_tool_call_and_links_its_children() {
    let (state, writer) = start().await;
    let (_tmp, scratch, repo) = scratch_with_clone();
    let cwd = scratch.display().to_string();

    // The orchestrator: prompted at the scratch root, then works through
    // tool calls whose hook cwd never leaves it.
    hook(
        &state,
        json!({ "hook_event_name": "SessionStart", "session_id": "orch",
        "cwd": cwd, "dira_env": env(ORCH_HOST) }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "orch",
        "cwd": cwd, "dira_env": env(ORCH_HOST) }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "PreToolUse", "session_id": "orch",
        "cwd": cwd, "dira_env": env(ORCH_HOST), "tool_name": "Bash",
        "tool_input": { "command": "cd repos/api && git status" } }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "PreToolUse", "session_id": "orch",
        "cwd": cwd, "dira_env": env(ORCH_HOST), "tool_name": "Bash",
        "tool_input": { "command": "gh pr list -R acme/web" } }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "PostToolUse", "session_id": "orch",
        "cwd": cwd, "dira_env": env(ORCH_HOST),
        "tool_name": "mcp__ccd_session__start_session",
        "tool_input": { "title": "fix web" },
        "tool_response": format!("Session started: {KID_HOST}") }),
    )
    .await;

    // The child it spawned, in an ordinary checkout.
    let kid_cwd = repo.display().to_string();
    hook(
        &state,
        json!({ "hook_event_name": "SessionStart", "session_id": "kid",
        "cwd": kid_cwd, "dira_env": env(KID_HOST) }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "kid",
        "cwd": kid_cwd, "dira_env": env(KID_HOST) }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "PreToolUse", "session_id": "kid",
        "cwd": kid_cwd, "dira_env": env(KID_HOST), "tool_name": "Read",
        "tool_input": { "file_path": "README.md" } }),
    )
    .await;

    let events = wait_events(&state, 8).await;

    // Per-event attribution: the prompt stays repo-less (the orchestration
    // bucket); each tool call lands on the repo it named.
    let orch: Vec<_> = events.iter().filter(|e| e.session_id == "orch").collect();
    assert_eq!(orch[1].project, None, "the prompt names no repo");
    assert_eq!(orch[2].project.as_deref(), Some("github.com/acme/api"));
    assert_eq!(orch[3].project.as_deref(), Some("github.com/acme/web"));
    assert!(orch.iter().all(|e| e.lineage.workspace.is_some()));
    assert_eq!(orch[4].lineage.spawned.as_deref(), Some(KID_HOST));
    // No tool argument survives into the log.
    let log = format!("{events:?}");
    assert!(!log.contains("git status") && !log.contains("fix web"));

    // The child resolved its parent from the spawn record.
    let kid: Vec<_> = events.iter().filter(|e| e.session_id == "kid").collect();
    assert!(kid
        .iter()
        .all(|e| e.lineage.parent_session.as_deref() == Some("orch")));

    // Only a directory that resolved enters repo_dirs — never the scratch root.
    let dirs = dirad::control::lock_recover_map(&state.repo_dirs).clone();
    let api_dir = dirs.get("github.com/acme/api").expect("api registered");
    assert!(api_dir.ends_with("repos/api"), "{api_dir}");
    assert!(
        !dirs.contains_key("github.com/acme/web"),
        "a named repo has no local dir"
    );

    // Live registry: no single repo pinned on the orchestrator.
    {
        let reg = dirad::control::lock_recover(&state.sessions);
        let live = reg
            .all()
            .into_iter()
            .find(|s| s.session_id == "orch")
            .unwrap();
        assert_eq!(live.project, None);
        assert!(live.lineage.orchestrator);
        assert_eq!(reg.parent_of("kid").as_deref(), Some("orch"));
    }

    // The wire: end both sessions' windows and roll up.
    let rollups = rollups_with_end(&events, &["orch", "kid"]);
    let o = rollup(&rollups, "orch");
    assert_eq!(o.role, Some(SessionRole::Orchestrator));
    assert_eq!(o.repo_canonical, None);
    assert_eq!(
        o.repos,
        Some(vec![
            "github.com/acme/api".into(),
            "github.com/acme/web".into()
        ])
    );
    assert_eq!(o.workspace_ref.as_deref().map(str::len), Some(16));
    let k = rollup(&rollups, "kid");
    assert_eq!(k.role, None);
    assert_eq!(k.parent_session.as_deref(), Some("orch"));
    assert_eq!(k.repo_canonical.as_deref(), Some("github.com/acme/api"));
    assert_eq!(
        k.repos, None,
        "an ordinary session's rollup stays pre-1.6 shaped"
    );

    writer.abort();
}

#[tokio::test]
async fn a_child_that_starts_in_the_parent_scratch_dir_keeps_its_own_repo() {
    let (state, writer) = start().await;
    let (tmp, scratch, _clone) = scratch_with_clone();
    let cwd = scratch.display().to_string();
    // The child's own worktree of a real repo, outside the scratch workspace.
    let wt_cwd = git_repo(
        &tmp.path().join("Workspace/app/.claude/worktrees/x"),
        "acme/app",
    );

    hook(
        &state,
        json!({ "hook_event_name": "PostToolUse", "session_id": "orch",
        "cwd": cwd, "dira_env": env(ORCH_HOST),
        "tool_name": "mcp__ccd_session__start_session",
        "tool_input": { "title": "fix app" },
        "tool_response": format!("Session started: {KID_HOST}") }),
    )
    .await;
    // Its first events run in the parent's scratch dir…
    for kind in ["SessionStart", "UserPromptSubmit"] {
        hook(
            &state,
            json!({ "hook_event_name": kind, "session_id": "kid",
            "cwd": cwd, "dira_env": env(KID_HOST) }),
        )
        .await;
    }
    // …then it moves to its own worktree.
    for file in ["README.md", "src/lib.rs"] {
        hook(
            &state,
            json!({ "hook_event_name": "PreToolUse", "session_id": "kid",
            "cwd": wt_cwd, "dira_env": env(KID_HOST), "tool_name": "Read",
            "tool_input": { "file_path": file } }),
        )
        .await;
    }

    let events = wait_events(&state, 5).await;
    let kid: Vec<_> = events.iter().filter(|e| e.session_id == "kid").collect();
    // The per-event data is unchanged: the preamble still carries the digest.
    assert!(kid[0].lineage.workspace.is_some());
    assert_eq!(kid[3].project.as_deref(), Some("github.com/acme/app"));

    {
        let reg = dirad::control::lock_recover(&state.sessions);
        let live = reg
            .all()
            .into_iter()
            .find(|s| s.session_id == "kid")
            .unwrap();
        assert!(!live.lineage.orchestrator);
        assert_eq!(live.project.as_deref(), Some("github.com/acme/app"));
        assert_eq!(reg.parent_of("kid").as_deref(), Some("orch"));
    }

    let rollups = rollups_with_end(&events, &["kid"]);
    let k = rollup(&rollups, "kid");
    assert_eq!(k.role, None);
    assert_eq!(k.parent_session.as_deref(), Some("orch"));
    assert_eq!(k.repo_canonical.as_deref(), Some("github.com/acme/app"));
    assert_eq!(k.workspace_ref, None);
    assert_eq!(k.repos, None);

    writer.abort();
}

#[tokio::test]
async fn a_worktree_session_that_spawns_a_child_keeps_its_repo() {
    let (state, writer) = start().await;
    // Seen in prod: a session rooted in a real worktree (no scratch events)
    // called `start_session` and lost its repo to the orchestrator rule.
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = git_repo(
        &tmp.path().join("Workspace/app/.claude/worktrees/p"),
        "acme/app",
    );
    let kid_wt = git_repo(
        &tmp.path().join("Workspace/web/.claude/worktrees/k"),
        "acme/web",
    );

    for kind in ["SessionStart", "UserPromptSubmit"] {
        hook(
            &state,
            json!({ "hook_event_name": kind, "session_id": "spawner",
            "cwd": wt, "dira_env": env(ORCH_HOST) }),
        )
        .await;
    }
    hook(
        &state,
        json!({ "hook_event_name": "PostToolUse", "session_id": "spawner",
        "cwd": wt, "dira_env": env(ORCH_HOST),
        "tool_name": "mcp__ccd_session__start_session",
        "tool_input": { "title": "fix web" },
        "tool_response": format!("Session started: {KID_HOST}") }),
    )
    .await;
    hook(
        &state,
        json!({ "hook_event_name": "SessionStart", "session_id": "kid",
        "cwd": kid_wt, "dira_env": env(KID_HOST) }),
    )
    .await;

    let events = wait_events(&state, 4).await;
    assert_eq!(events[2].lineage.spawned.as_deref(), Some(KID_HOST));

    {
        let reg = dirad::control::lock_recover(&state.sessions);
        let live = reg
            .all()
            .into_iter()
            .find(|s| s.session_id == "spawner")
            .unwrap();
        assert!(!live.lineage.orchestrator);
        assert_eq!(live.project.as_deref(), Some("github.com/acme/app"));
        assert_eq!(reg.parent_of("kid").as_deref(), Some("spawner"));
    }

    let rollups = rollups_with_end(&events, &["spawner"]);
    let p = rollup(&rollups, "spawner");
    assert_eq!(p.role, None);
    assert_eq!(p.repo_canonical.as_deref(), Some("github.com/acme/app"));
    assert_eq!(
        p.repos.as_deref(),
        Some(&["github.com/acme/app".to_string()][..])
    );
    assert_eq!(p.workspace_ref, None);
    writer.abort();
}
