//! End-to-end proof that a headless run withholds human signals and forwards
//! everything else, driving the real compiled binary with real env vars
//! (`src/headless.rs` carries the unit-tested decision; DIRASH-0047).
//!
//! Same containment and forward proxy (`common::hook_forward_attempted`) as
//! `hook_yield_e2e.rs`. Every test sets both env knobs explicitly, because this suite itself
//! runs on GitHub Actions, where `GITHUB_ACTIONS=true` is inherited.

#![cfg(unix)]

mod common;
use common::isolate_user_dirs;
use serde_json::json;
use std::path::Path;
use std::process::{Command, Output};

fn run_hook(home: &Path, payload: &serde_json::Value, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dira"));
    cmd.arg("hook")
        .arg("claude")
        .current_dir(home)
        .env_remove("DIRA_HOOK_VIA")
        .env_remove("DIRA_HEADLESS")
        .env_remove("GITHUB_ACTIONS");
    isolate_user_dirs(&mut cmd, home);
    for (k, v) in env {
        cmd.env(k, v);
    }
    common::output_with_stdin(&mut cmd, payload.to_string().as_bytes()).expect("run dira hook")
}

fn prompt() -> serde_json::Value {
    json!({ "hook_event_name": "UserPromptSubmit", "session_id": "ci", "cwd": "/repo" })
}

#[test]
fn a_prompt_on_github_actions_is_withheld() {
    let home = tempfile::tempdir().unwrap();
    let out = run_hook(home.path(), &prompt(), &[("GITHUB_ACTIONS", "true")]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "a hook never writes stdout");
    assert!(
        !common::hook_forward_attempted(home.path()),
        "headless prompt must not be forwarded"
    );
}

#[test]
fn agent_hooks_on_github_actions_still_forward() {
    let home = tempfile::tempdir().unwrap();
    let stop = json!({ "hook_event_name": "Stop", "session_id": "ci", "cwd": "/repo" });
    let out = run_hook(home.path(), &stop, &[("GITHUB_ACTIONS", "true")]);
    assert!(out.status.success());
    assert!(
        common::hook_forward_attempted(home.path()),
        "agent activity is captured as before"
    );
}

#[test]
fn a_prompt_on_an_ordinary_machine_still_forwards() {
    let home = tempfile::tempdir().unwrap();
    let out = run_hook(home.path(), &prompt(), &[]);
    assert!(out.status.success());
    assert!(common::hook_forward_attempted(home.path()));
}

#[test]
fn the_explicit_switch_overrides_github_actions() {
    let home = tempfile::tempdir().unwrap();
    let env = [("GITHUB_ACTIONS", "true"), ("DIRA_HEADLESS", "0")];
    run_hook(home.path(), &prompt(), &env);
    assert!(
        common::hook_forward_attempted(home.path()),
        "DIRA_HEADLESS=0 forces interactive"
    );

    let home = tempfile::tempdir().unwrap();
    run_hook(home.path(), &prompt(), &[("DIRA_HEADLESS", "1")]);
    assert!(
        !common::hook_forward_attempted(home.path()),
        "DIRA_HEADLESS=1 forces headless"
    );
}
