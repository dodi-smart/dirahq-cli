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
use common::{isolate_user_dirs, output_staged, MockGitHub};
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

/// `dira onboard <args…>` with `$HOME` isolated at `home` but the process
/// `cwd` set to a different directory (e.g. a git repo nested inside `home`,
/// for the `cloud:repo` step) — plus any extra env vars the cloud step needs
/// (namely `DIRA_DOWNLOAD_URL`, pointed at a [`MockGitHub`] so the digest
/// fetch never touches the network).
fn run_onboard_in(home: &Path, cwd: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dira"));
    cmd.arg("onboard")
        .arg("--no-service")
        .args(args)
        .current_dir(cwd);
    isolate_user_dirs(&mut cmd, home);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    output_staged(&mut cmd).expect("spawn dira onboard")
}

/// `git init -q` a fresh directory, with a local identity so the repo is
/// usable without touching the developer's real git config — mirrors
/// `cloud_init_e2e.rs`'s `git_init`.
fn git_init(dir: &Path) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("init")
        .arg("-q")
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init failed in {}", dir.display());
    for (key, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("config")
            .arg(key)
            .arg(value)
            .status()
            .expect("spawn git config");
        assert!(
            status.success(),
            "git config {key} failed in {}",
            dir.display()
        );
    }
}

/// A mock release server seeded with `.sha256` assets for both musl targets,
/// at the version this test binary was built with — the same version the
/// `cloud:repo` step's bootstrap pin uses (`CARGO_PKG_VERSION`). Mirrors
/// `cloud_init_e2e.rs`'s `mock_release`.
async fn mock_release() -> MockGitHub {
    let mock = MockGitHub::start().await;
    let version = env!("CARGO_PKG_VERSION");
    for (target, digest) in [
        ("x86_64-unknown-linux-musl", "a".repeat(64)),
        ("aarch64-unknown-linux-musl", "b".repeat(64)),
    ] {
        let archive = format!("dira-{version}-{target}.tar.gz");
        let sha_name = format!("dira-{version}-{target}.sha256");
        mock.put_asset(&sha_name, format!("{digest}  {archive}\n").into_bytes());
    }
    mock
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

    // A git repo nested under the isolated home, so the plan's cloud
    // detection path runs (cwd = home would report "not inside a git
    // repository" and never reach the `Wire` branch this asserts on).
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);

    let before = walk(home);
    let out = run_onboard_in(home, &repo, &[], &["--print"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = stdout(&out);
    assert!(text.contains("Nothing was changed"), "got:\n{text}");
    assert!(
        text.contains("for cloud agents"),
        "the plan must mention wiring the repo for cloud agents; got:\n{text}"
    );
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

/// The five paths a `cloud:repo` files list.
const CLOUD_REPO_PATHS: [&str; 5] = [
    ".dira/hook.sh",
    ".dira/bootstrap.sh",
    ".dira/.gitattributes",
    ".claude/settings.json",
    ".cursor/hooks.json",
];

/// `dira onboard --yes` inside a git repo wires it for cloud agents — the
/// same portable `.dira/` + hook-config artifacts as `dira cloud init` — and
/// a second run reports the repo as already wired rather than rewriting it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yes_wires_the_repo_for_cloud_agents_and_a_second_run_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);
    let mock = mock_release().await;
    let env = [("DIRA_DOWNLOAD_URL", mock.download_base())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let first = run_onboard_in(
        home,
        &repo,
        &env,
        &["--yes", "--no-zavet", "--knowledge", "off"],
    );
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out1 = stdout(&first);

    for rel in CLOUD_REPO_PATHS {
        assert!(
            repo.join(rel).exists(),
            "{rel} must be written; stdout:\n{out1}"
        );
    }

    let settings = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    assert!(settings.contains("hook.sh claude"), "{settings}");
    assert!(settings.contains("bootstrap.sh claude"), "{settings}");
    let hooks = std::fs::read_to_string(repo.join(".cursor/hooks.json")).unwrap();
    assert!(hooks.contains("hook.sh cursor"), "{hooks}");

    let bootstrap = std::fs::read_to_string(repo.join(".dira/bootstrap.sh")).unwrap();
    let ver = env!("CARGO_PKG_VERSION");
    assert!(
        bootstrap.contains(&format!("${{DIRA_VERSION:-{ver}}}")),
        "{bootstrap}"
    );
    assert!(bootstrap.contains(&"a".repeat(64)), "{bootstrap}");
    assert!(bootstrap.contains(&"b".repeat(64)), "{bootstrap}");

    assert!(
        out1.contains("wired") && out1.contains("for cloud agents"),
        "the cloud:repo summary line must say it wired the repo; got:\n{out1}"
    );

    let stamps: Vec<_> = CLOUD_REPO_PATHS
        .iter()
        .map(|rel| {
            std::fs::metadata(repo.join(rel))
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();

    let second = run_onboard_in(
        home,
        &repo,
        &env,
        &["--yes", "--no-zavet", "--knowledge", "off"],
    );
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let out2 = stdout(&second);
    assert!(
        out2.contains("already wired for cloud agents"),
        "a second run must report the repo as already wired; got:\n{out2}"
    );

    for (rel, stamp) in CLOUD_REPO_PATHS.iter().zip(stamps.iter()) {
        assert_eq!(
            *stamp,
            std::fs::metadata(repo.join(rel))
                .unwrap()
                .modified()
                .unwrap(),
            "{rel} must not be rewritten by a no-op run"
        );
    }
}

/// A `.dira/bootstrap.sh` pinned to a version older than this binary is
/// refreshed in place — only that file changes, and the pin is bumped back
/// up — rather than the whole wiring being rewritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_pin_is_refreshed_without_touching_current_files() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);
    let mock = mock_release().await;
    let env = [("DIRA_DOWNLOAD_URL", mock.download_base())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let args = ["--yes", "--no-zavet", "--knowledge", "off"];

    let first = run_onboard_in(home, &repo, &env, &args);
    assert!(first.status.success(), "{first:?}");

    let ver = env!("CARGO_PKG_VERSION");
    let bootstrap_path = repo.join(".dira/bootstrap.sh");
    let bootstrap = std::fs::read_to_string(&bootstrap_path).unwrap();
    let rewritten = bootstrap.replace(
        &format!("${{DIRA_VERSION:-{ver}}}"),
        "${DIRA_VERSION:-0.0.1}",
    );
    assert_ne!(
        bootstrap, rewritten,
        "the pin must actually be present to rewrite"
    );
    std::fs::write(&bootstrap_path, &rewritten).unwrap();

    let other_before: Vec<(PathBuf, Vec<u8>)> = CLOUD_REPO_PATHS
        .iter()
        .filter(|rel| **rel != ".dira/bootstrap.sh")
        .map(|rel| (repo.join(rel), std::fs::read(repo.join(rel)).unwrap()))
        .collect();

    let second = run_onboard_in(home, &repo, &env, &args);
    assert!(second.status.success(), "{second:?}");
    let out2 = stdout(&second);
    assert!(out2.contains("refreshed cloud wiring"), "got:\n{out2}");
    assert!(
        out2.contains(&format!("bootstrap pin v0.0.1 → v{ver}")),
        "got:\n{out2}"
    );

    let bootstrap_after = std::fs::read_to_string(&bootstrap_path).unwrap();
    assert!(
        bootstrap_after.contains(&format!("${{DIRA_VERSION:-{ver}}}")),
        "the pin must be back at {ver}: {bootstrap_after}"
    );
    assert_ne!(
        bootstrap_after, rewritten,
        "bootstrap.sh must actually have changed from the stale 0.0.1 pin"
    );
    // The refresh regenerates the file from the same inputs it was first
    // written with, so it lands back on the exact original bytes.
    assert_eq!(
        bootstrap_after, bootstrap,
        "the refreshed file must match what a fresh wire would have produced"
    );

    for (path, before) in &other_before {
        assert_eq!(
            *before,
            std::fs::read(path).unwrap(),
            "{} must be byte-identical after a refresh",
            path.display()
        );
    }
}

/// A `.dira/bootstrap.sh` pinned to a version newer than this binary is left
/// alone entirely — never ours to lower.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_pin_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);
    let mock = mock_release().await;
    let env = [("DIRA_DOWNLOAD_URL", mock.download_base())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let args = ["--yes", "--no-zavet", "--knowledge", "off"];

    let first = run_onboard_in(home, &repo, &env, &args);
    assert!(first.status.success(), "{first:?}");

    let ver = env!("CARGO_PKG_VERSION");
    let bootstrap_path = repo.join(".dira/bootstrap.sh");
    let bootstrap = std::fs::read_to_string(&bootstrap_path).unwrap();
    let rewritten = bootstrap.replace(
        &format!("${{DIRA_VERSION:-{ver}}}"),
        "${DIRA_VERSION:-99.0.0}",
    );
    assert_ne!(
        bootstrap, rewritten,
        "the pin must actually be present to rewrite"
    );
    std::fs::write(&bootstrap_path, rewritten).unwrap();

    let before = walk(&repo);
    let second = run_onboard_in(home, &repo, &env, &args);
    assert!(second.status.success(), "{second:?}");
    let out2 = stdout(&second);
    assert!(out2.contains("newer than this dira"), "got:\n{out2}");
    assert_eq!(
        before,
        walk(&repo),
        "a newer pin must be left completely alone"
    );
}

/// `--no-cloud` skips the `cloud:repo` step outright — no `.dira/` at all.
#[test]
fn no_cloud_skips_the_repo_wiring() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);

    let out = run_onboard_in(
        home,
        &repo,
        &[],
        &["--yes", "--no-zavet", "--knowledge", "off", "--no-cloud"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.join(".dira").exists(),
        "--no-cloud must not create .dira/"
    );
    let text = stdout(&out);
    assert!(text.contains("--no-cloud"), "got:\n{text}");
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
    assert!(
        text.contains("cloud wiring") || text.contains("for cloud agents"),
        "the plan must mention the cloud:repo step; got:\n{text}"
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

/// `--harness cursor` narrows the repo wiring to the cloud-capable harness
/// named: the repo gets `.cursor/hooks.json` and the scripts, and no
/// `.claude/settings.json` is invented for a harness nobody asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_filter_narrows_the_repo_cloud_wiring() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);
    let mock = mock_release().await;
    let env = [("DIRA_DOWNLOAD_URL", mock.download_base())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_onboard_in(
        home,
        &repo,
        &env,
        &[
            "--yes",
            "--no-zavet",
            "--knowledge",
            "off",
            "--harness",
            "cursor",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(repo.join(".dira/bootstrap.sh").is_file(), "{text}");
    assert!(repo.join(".cursor/hooks.json").is_file(), "{text}");
    assert!(
        !repo.join(".claude/settings.json").exists(),
        "--harness cursor must not wire claude in the repo; stdout:\n{text}"
    );
    assert!(text.contains("cursor 7 event(s)"), "{text}");
    assert!(!text.contains("claude 8 event(s)"), "{text}");
}

/// Regression: run from a subdirectory, the wiring lands at the git
/// toplevel — never a nested `.dira/` under wherever the shell happened to be.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_from_a_subdirectory_writes_at_the_repo_toplevel() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    let sub = repo.join("crates").join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    git_init(&repo);
    let mock = mock_release().await;
    let env = [("DIRA_DOWNLOAD_URL", mock.download_base())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let out = run_onboard_in(
        home,
        &sub,
        &env,
        &["--yes", "--no-zavet", "--knowledge", "off"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for rel in CLOUD_REPO_PATHS {
        assert!(repo.join(rel).is_file(), "{rel} at the toplevel");
        assert!(
            !sub.join(rel).exists(),
            "{rel} must not appear under the subdirectory"
        );
    }

    // And a second run from the toplevel agrees it is done.
    let again = run_onboard_in(
        home,
        &repo,
        &env,
        &["--yes", "--no-zavet", "--knowledge", "off"],
    );
    assert!(
        stdout(&again).contains("already wired for cloud agents"),
        "{}",
        stdout(&again)
    );
}

/// Regression: a hand-edited `.dira/hook.sh` is put back to this dira's
/// template on the next run, and nothing else is rewritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hand_edited_hook_sh_is_restored_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&repo);
    let mock = mock_release().await;
    let env = [("DIRA_DOWNLOAD_URL", mock.download_base())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let args = ["--yes", "--no-zavet", "--knowledge", "off"];

    assert!(run_onboard_in(home, &repo, &env, &args).status.success());
    let hook = repo.join(".dira/hook.sh");
    let pristine = std::fs::read(&hook).unwrap();
    std::fs::write(&hook, b"#!/bin/sh\n# edited by hand\nexit 0\n").unwrap();
    let before = walk(&repo);

    let out = run_onboard_in(home, &repo, &env, &args);
    let text = stdout(&out);
    assert!(text.contains("refreshed cloud wiring"), "{text}");
    assert!(text.contains("hook.sh differs"), "{text}");
    assert_eq!(
        std::fs::read(&hook).unwrap(),
        pristine,
        "restored to the template"
    );

    let after = walk(&repo);
    let changed: Vec<_> = before
        .iter()
        .zip(after.iter())
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.0.clone())
        .collect();
    assert_eq!(changed.len(), 1, "only hook.sh may change: {changed:?}");
    assert!(changed[0].ends_with("hook.sh"), "{changed:?}");
}
