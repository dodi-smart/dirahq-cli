//! End-to-end tests for `dira onboard`, driving the real compiled binary.
//!
//! ## Why these run the binary rather than calling the module
//!
//! `dira onboard` makes persistent, machine-scoped changes — it wires harness
//! configs under `$HOME`, and (unguarded) would register a launchd/systemd
//! service and talk to a real daemon. The unit tests in `src/onboard/` cover
//! the decision logic behind an injected `Ui`; what they cannot cover is
//! whether the assembled command, clap wiring included, actually writes the
//! files it claims to and stays idempotent across two real runs.
//!
//! ## Containment
//!
//! Two things keep these tests off the developer's real machine, and both are
//! load-bearing (D-0021):
//!
//! 1. **`isolate_user_dirs`** points `HOME` and every XDG variable at the
//!    test's own tempdir, so `dira_core::config::home_dir()` — which is what
//!    `init::run(global: true, …)` writes relative to — resolves inside the
//!    fixture. D-0021 is explicit that any new env var steering a write path
//!    belongs in that one helper, not inline in a single test; issue #90 was
//!    exactly a test polluting the developer's real cache.
//! 2. **`--no-service` on every invocation.** Nothing here may register a
//!    launchd agent or a systemd unit. `DIRA_SOCKET_PATH` additionally points
//!    at a path that is never created, so the daemon probes resolve to "not
//!    running" instead of finding the developer's real dirad — the control
//!    socket is machine-global, and a worktree does not isolate it.
//!
//! ## Staging discipline
//!
//! Every spawn goes through `common::output_staged`, and the isolation comes
//! from `common::isolate_user_dirs` — shared with `update_e2e.rs` rather than
//! copied, because D-0021's "one helper" rule exists precisely to stop the two
//! from drifting apart.

#![cfg(unix)]

mod common;
use common::{isolate_user_dirs, output_staged};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `dira onboard <args…>` inside an isolated `$HOME`, with `cwd` set to it.
fn run_onboard(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dira"));
    cmd.arg("onboard")
        // Non-negotiable in tests: no launchd agent, no systemd unit.
        .arg("--no-service")
        .args(args)
        .current_dir(home);
    isolate_user_dirs(&mut cmd, home);
    output_staged(&mut cmd).expect("spawn dira onboard")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// `--print` is a dry run. The strong assertion is not that it says so, but
/// that the isolated home is byte-for-byte unchanged afterwards.
#[test]
fn print_changes_nothing_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    // A real, migrated DB at the exact path `isolate_user_dirs` points
    // `DIRA_DB_PATH` at. Without this, that path never exists, so
    // `detect::device_linked` short-circuits on the file's absence before
    // ever reaching an open call — vacuous for the branch this test exists
    // to cover (DIRASH-0029 rule 3): a *present* database must also not be
    // written to by detection (`Store::open_readonly`, never `Store::open`).
    let db_path = home.join("isolated.db");
    tokio::runtime::Runtime::new()
        .expect("build a tokio runtime")
        .block_on(async {
            let store = dira_core::Store::open(&db_path)
                .await
                .expect("create the isolated db");
            // Fold the WAL into the main file (and truncate it) before this
            // test's own before/after snapshot, rather than racing sqlx's
            // asynchronous pool-close checkpoint — an uncheckpointed WAL is
            // exactly what `Store::open_readonly`'s `immutable(true)` never
            // looks at (see its doc comment), so a race here would make this
            // assertion pass for the wrong reason.
            store
                .wal_checkpoint_truncate()
                .await
                .expect("checkpoint the isolated db");
        });
    // `wal_checkpoint_truncate` zeroes the WAL but does not delete the
    // (now-empty) `-wal`/`-shm` sidecars, and this runtime's own pool-close
    // cleanup — which does delete them, on the last connection closing — runs
    // asynchronously on no promised schedule. Remove them here, synchronously,
    // so the "before" snapshot below isn't racing that cleanup: whether it
    // wins or loses would make this test flaky for a reason that has nothing
    // to do with what it exists to check.
    let _ = std::fs::remove_file(home.join("isolated.db-wal"));
    let _ = std::fs::remove_file(home.join("isolated.db-shm"));

    let before = walk(home);
    let out = run_onboard(home, &["--print"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = stdout(&out);
    assert!(text.contains("Nothing was changed"), "got:\n{text}");
    assert_eq!(before, walk(home), "--print must not touch the filesystem");
}

/// The idempotency property, end to end: a first `--yes` run wires the
/// harness, and an immediate second run reports it as already done rather
/// than rewriting it. This is what makes `dira onboard` safe to re-run, which
/// is the whole resume story — there is no state file, so if this breaks,
/// re-running silently redoes work.
#[test]
fn yes_wires_a_detected_harness_and_a_second_run_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    // A `.claude` directory is one of the two presence signals, so this
    // fixture makes Claude Code "detected" without needing a CLI on PATH.
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    let first = run_onboard(home, &["--yes", "--no-zavet", "--knowledge", "off"]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let settings = home.join(".claude/settings.json");
    assert!(
        settings.is_file(),
        "onboard --yes must wire the detected harness; stdout:\n{}",
        stdout(&first)
    );

    // The hooks must actually name this binary's `hook claude` shim, not just
    // be *some* JSON — a file that exists but wires nothing is the exact
    // failure mode this whole command was built to prevent.
    let text = std::fs::read_to_string(&settings).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        json["hooks"]["SessionStart"].is_array(),
        "expected wired hooks, got:\n{text}"
    );
    assert!(text.contains("hook claude"), "got:\n{text}");

    let stamp = std::fs::metadata(&settings).unwrap().modified().unwrap();

    let second = run_onboard(home, &["--yes", "--no-zavet", "--knowledge", "off"]);
    assert!(second.status.success());
    let out2 = stdout(&second);
    assert!(
        out2.contains("already wired"),
        "a second run must report the harness as already wired; got:\n{out2}"
    );
    assert_eq!(
        stamp,
        std::fs::metadata(&settings).unwrap().modified().unwrap(),
        "a no-op run must not rewrite the file"
    );
}

/// `--knowledge` must land in the real `config.toml`, in the spelling the
/// daemon deserializes — the knob was file/env-only before this work, so the
/// whole consent step depends on this write actually happening.
#[test]
fn the_knowledge_tier_is_written_to_config_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    let out = run_onboard(home, &["--yes", "--no-zavet", "--knowledge", "full"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let config = find_config_toml(home).unwrap_or_else(|| {
        panic!(
            "no config.toml written under {}; stdout:\n{}",
            home.display(),
            stdout(&out)
        )
    });
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("[sync]"), "got:\n{text}");
    assert!(text.contains("knowledge = \"full\""), "got:\n{text}");
}

/// A non-interactive run without `--yes` must not hang and must not act. CI
/// invoking `dira onboard` by accident should be a no-op, not a wedged job or
/// a machine that quietly grew a service.
#[test]
fn a_non_interactive_run_without_yes_prints_the_plan_and_exits_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    let before = walk(home);
    // stdin is `Stdio::null()` via `output_staged`, so this is exactly the
    // piped/CI shape.
    let out = run_onboard(home, &[]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("not a terminal"), "got:\n{text}");
    assert!(
        text.contains("--yes"),
        "must say how to proceed; got:\n{text}"
    );
    assert_eq!(before, walk(home), "must not act without a decision");
}

/// An unknown `--harness` fails before any step runs, rather than five steps
/// in with half the machine already changed.
#[test]
fn an_unknown_harness_fails_before_touching_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let before = walk(home);

    let out = run_onboard(home, &["--yes", "--harness", "emacs"]);
    assert!(!out.status.success(), "an unknown harness must be an error");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown harness 'emacs'"), "got:\n{err}");
    assert_eq!(before, walk(home));
}

/// Every path in `home`, with contents, for before/after comparison.
fn walk(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push((p.clone(), None));
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).ok()));
            }
        }
    }
    out.sort();
    out
}

/// The XDG config dir differs by platform (`~/Library/Application Support/…`
/// on macOS, `$XDG_CONFIG_HOME/…` on Linux), so find the file rather than
/// hard-coding a layout this test does not own.
fn find_config_toml(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "config.toml") {
                return Some(p);
            }
        }
    }
    None
}
