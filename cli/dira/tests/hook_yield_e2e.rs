//! End-to-end proof that the portable hook yields to live user-scope wiring,
//! driving the real compiled binary (`src/hook_yield.rs` and `src/main.rs`
//! carry the unit-testable decision logic; this is the part only a real
//! process boundary can show — real env vars, a real stdin pipe, a real
//! process exit code).
//!
//! Same containment as `onboard_e2e.rs`/`cloud_init_e2e.rs` (D-0021):
//! `isolate_user_dirs` points `HOME` (and therefore the user-scope
//! `~/.claude/settings.json` [`hook_yield`] reads) and the daemon socket at a
//! fresh per-test tempdir, so this never touches the developer's real config
//! or the real machine-global `dirad`.
//!
//! The observable proxy for "did it try to forward" is the `hook_health`
//! breadcrumb (`hook-health.json` under the isolated cache dir): the socket
//! path `isolate_user_dirs` sets is never created, so any real forward
//! attempt fails fast and `hook_health::record_failure` writes it. A yielded
//! invocation never reaches that code at all, so the breadcrumb stays absent.
//! This also happens to be exactly the user-visible bug WP-B fixes: a
//! double-forwarding laptop leaves this same breadcrumb blinking between
//! "healthy" and "warning" for no reason a user can see.
//!
//! The yield is structural on *both* sides (DIRASH-0037): a live user-scope
//! entry AND a project-scope config that genuinely wires the event through
//! the portable wrapper. User scope and project scope are therefore always
//! two distinct directories here — `home` (via `isolate_user_dirs`/
//! `CLAUDE_CONFIG_DIR`) and `project_dir` (via `cwd`/`CLAUDE_PROJECT_DIR`) —
//! so a test can hold one signal live and the other absent.

#![cfg(unix)]

mod common;
use common::isolate_user_dirs;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// `dira hook claude` fed `payload` on stdin, with an isolated `$HOME` (user
/// scope) and a separate `$CLAUDE_PROJECT_DIR`/cwd (project scope),
/// optionally marked as a portable-wrapper invocation.
fn run_hook(
    home: &Path,
    project_dir: &Path,
    payload: &serde_json::Value,
    via_portable: bool,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dira"));
    cmd.arg("hook")
        .arg("claude")
        .current_dir(project_dir)
        .env("CLAUDE_PROJECT_DIR", project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_user_dirs(&mut cmd, home);
    if via_portable {
        cmd.env("DIRA_HOOK_VIA", "portable");
    } else {
        cmd.env_remove("DIRA_HOOK_VIA");
    }

    let child = {
        let _staging = common::lock_staging();
        cmd.spawn().expect("spawn dira hook")
    };
    // Feed stdin and close it before waiting — mirrors `output_staged`, but
    // that helper doesn't let us write to stdin first, so it's inlined here.
    {
        use std::io::Write;
        child
            .stdin
            .as_ref()
            .expect("piped stdin")
            .write_all(payload.to_string().as_bytes())
            .expect("write hook payload");
    }
    child.wait_with_output().expect("wait for dira hook")
}

/// Recursively find a file named `name` under `root`, if any.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

fn hook_health_recorded(home: &Path) -> bool {
    find_file(home, "hook-health.json").is_some()
}

/// Write a user-scope `~/.claude/settings.json` wiring `Stop` straight at
/// `exe` (no matcher — `Stop` isn't a tool event and doesn't take one),
/// matching the shape `dira init --global` writes.
fn wire_user_scope_stop(home: &Path, exe: &Path) {
    let dir = home.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let settings = json!({
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        { "type": "command", "command": format!("{} hook claude", exe.display()) }
                    ]
                }
            ]
        }
    });
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();
}

/// Write a project-scope `.claude/settings.json` wiring `Stop` through the
/// repo-committed portable wrapper (`sh .dira/hook.sh claude`), matching the
/// shape `dira cloud init` writes.
fn wire_project_scope_portable_stop(project_dir: &Path) {
    let dir = project_dir.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let settings = json!({
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        { "type": "command", "command": "sh .dira/hook.sh claude" }
                    ]
                }
            ]
        }
    });
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();
}

/// The portable path yields when the same event is wired live at user scope
/// AND wired, at project scope, through the portable wrapper: no breadcrumb,
/// because it never reaches the code that would write one.
#[test]
fn portable_invocation_yields_to_live_user_scope_wiring() {
    let home_tmp = tempfile::tempdir().unwrap();
    let project_tmp = tempfile::tempdir().unwrap();
    let home = home_tmp.path();
    let project_dir = project_tmp.path();
    // `dira` itself is guaranteed to exist on disk — stand in for the
    // user-scope binary path `dira init --global` would have written.
    let dira_bin = PathBuf::from(env!("CARGO_BIN_EXE_dira"));
    wire_user_scope_stop(home, &dira_bin);
    wire_project_scope_portable_stop(project_dir);

    let out = run_hook(
        home,
        project_dir,
        &json!({ "hook_event_name": "Stop" }),
        true,
    );

    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "a hook must never write stdout: {out:?}"
    );
    assert!(
        !hook_health_recorded(home),
        "a yielded invocation must never attempt to forward, so it must never \
         touch hook_health"
    );
}

/// The direct path (no `DIRA_HOOK_VIA=portable`) always forwards, even with
/// the exact same live wiring in place on both sides — the yield is
/// conditioned on the portable marker, never on wiring alone. The isolated
/// socket path is never created, so the forward attempt fails fast and
/// leaves the `hook_health` breadcrumb behind.
#[test]
fn direct_invocation_still_forwards_with_the_same_wiring_present() {
    let home_tmp = tempfile::tempdir().unwrap();
    let project_tmp = tempfile::tempdir().unwrap();
    let home = home_tmp.path();
    let project_dir = project_tmp.path();
    let dira_bin = PathBuf::from(env!("CARGO_BIN_EXE_dira"));
    wire_user_scope_stop(home, &dira_bin);
    wire_project_scope_portable_stop(project_dir);

    let out = run_hook(
        home,
        project_dir,
        &json!({ "hook_event_name": "Stop" }),
        false,
    );

    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "a hook must never write stdout: {out:?}"
    );
    assert!(
        hook_health_recorded(home),
        "the direct invocation must still attempt to forward and record the \
         transport failure — only the portable marker changes behaviour"
    );
}

/// A stray `DIRA_HOOK_VIA=portable` — inherited from a shell profile rather
/// than set by a real `.dira/hook.sh` invocation — must not be enough on its
/// own to yield, even with a live user-scope entry present: this project has
/// no portable wrapper wired at all, so there is no real portable delivery
/// to yield to. The invocation must still forward (DIRASH-0037's structural
/// requirement on the project-scope side).
#[test]
fn stray_marker_without_project_scope_portable_wiring_still_forwards() {
    let home_tmp = tempfile::tempdir().unwrap();
    let project_tmp = tempfile::tempdir().unwrap();
    let home = home_tmp.path();
    let project_dir = project_tmp.path();
    let dira_bin = PathBuf::from(env!("CARGO_BIN_EXE_dira"));
    wire_user_scope_stop(home, &dira_bin);
    // Deliberately no `wire_project_scope_portable_stop` call: this project
    // was never `dira cloud init`-wired.

    let out = run_hook(
        home,
        project_dir,
        &json!({ "hook_event_name": "Stop" }),
        true,
    );

    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "a hook must never write stdout: {out:?}"
    );
    assert!(
        hook_health_recorded(home),
        "a stray portable marker with no project-scope portable wiring must \
         still forward — the marker alone is never proof of a real portable \
         invocation"
    );
}
