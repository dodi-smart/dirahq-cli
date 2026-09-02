//! End-to-end tests for `dira cloud init`, driving the real compiled binary.
//!
//! The unit tests in `src/cloud_init.rs` pin the injection semantics; what
//! they cannot cover is the assembled command — clap wiring, script
//! generation with the real pinned version, file modes, and idempotency
//! across two real runs in a real directory. Same containment as
//! `onboard_e2e.rs` (D-0021): `isolate_user_dirs` + a per-test tempdir cwd,
//! and `cloud init` itself only ever writes paths under the resolved repo
//! root.
//!
//! `cloud init` now resolves the repo root via `git rev-parse
//! --show-toplevel` (repo-root anchoring) and fetches release digests unless
//! `--no-pin` is passed, so every test here `git init`s its tempdir first and
//! points `DIRA_DOWNLOAD_URL` at a local mock — never the real network — via
//! [`common::MockGitHub`] (moved there from `update_e2e.rs` for this reuse).

#![cfg(unix)]

mod common;
use common::{isolate_user_dirs, output_staged, MockGitHub};
use std::path::Path;
use std::process::{Command, Output};

/// `git init -q` a fresh tempdir so `cloud init`'s repo-root resolution
/// (`dira_core::project::toplevel`) succeeds.
fn git_init(dir: &Path) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("init")
        .arg("-q")
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init failed in {}", dir.display());
}

/// A mock release server seeded with `.sha256` assets for both musl targets,
/// at the version this test binary was built with — the same version
/// `cloud init`'s own `CARGO_PKG_VERSION` pin uses, so a real `dira cloud
/// init` run resolves against it deterministically.
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

fn run_cloud_init(dir: &Path, args: &[&str], download_base: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dira"));
    cmd.arg("cloud").arg("init").args(args).current_dir(dir);
    isolate_user_dirs(&mut cmd, dir);
    // Set on every run — including --print — so nothing here ever reaches
    // the real network (see the module doc).
    cmd.env("DIRA_DOWNLOAD_URL", download_base);
    output_staged(&mut cmd).expect("spawn dira cloud init")
}

/// The plain `dira init` command (not `cloud init`) — never touches the
/// network, so no `DIRA_DOWNLOAD_URL` is needed. `home` and `cwd` are
/// separate so a `--global` run can be proven to write somewhere other than
/// the project directory.
fn run_dira_init(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dira"));
    cmd.arg("init").args(args).current_dir(cwd);
    isolate_user_dirs(&mut cmd, home);
    output_staged(&mut cmd).expect("spawn dira init")
}

/// Snapshot every file under `dir` as (relative path, bytes), sorted.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_portable_artifacts_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    let mock = mock_release().await;

    let out = run_cloud_init(dir, &[], &mock.download_base());
    assert!(out.status.success(), "{out:?}");

    // The five artifacts, all repo-root-relative.
    for rel in [
        ".dira/hook.sh",
        ".dira/bootstrap.sh",
        ".dira/.gitattributes",
        ".claude/settings.json",
        ".cursor/hooks.json",
    ] {
        assert!(dir.join(rel).exists(), "{rel} must be written");
    }
    assert_eq!(
        std::fs::read_to_string(dir.join(".dira/.gitattributes")).unwrap(),
        "*.sh text eol=lf\n"
    );

    // Scripts are executable and carry the pinned workspace version.
    use std::os::unix::fs::PermissionsExt;
    for rel in [".dira/hook.sh", ".dira/bootstrap.sh"] {
        let mode = std::fs::metadata(dir.join(rel))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{rel} must be executable (mode {mode:o})");
    }
    // .gitattributes is deliberately not executable.
    let gitattributes_mode = std::fs::metadata(dir.join(".dira/.gitattributes"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(gitattributes_mode & 0o111, 0);

    let bootstrap = std::fs::read_to_string(dir.join(".dira/bootstrap.sh")).unwrap();
    assert!(
        bootstrap.contains(env!("CARGO_PKG_VERSION")),
        "bootstrap pins the generating binary's version"
    );
    assert!(!bootstrap.contains("{{VERSION}}"));
    // Digests fetched from the mock are embedded, not the unpinned form.
    assert!(bootstrap.contains(&"a".repeat(64)), "{bootstrap}");
    assert!(bootstrap.contains(&"b".repeat(64)), "{bootstrap}");

    // The hook configs carry only portable commands — nothing machine-specific.
    let settings = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    // Repo-root anchored, with a working-directory fallback so an unset
    // CLAUDE_PROJECT_DIR can never resolve the hook to `/.dira/…`.
    assert!(settings.contains("${CLAUDE_PROJECT_DIR:-.}"), "{settings}");
    assert!(settings.contains(".dira/bootstrap.sh claude"), "{settings}");
    assert!(settings.contains(".dira/hook.sh claude"));
    // SessionStart carries the provisioning timeout.
    assert!(settings.contains("\"timeout\": 300"), "{settings}");
    let hooks = std::fs::read_to_string(dir.join(".cursor/hooks.json")).unwrap();
    assert!(hooks.contains("sh .dira/hook.sh cursor"));
    assert!(hooks.contains("sh .dira/bootstrap.sh cursor"));
    assert!(hooks.contains("\"timeout\": 300"), "{hooks}");
    for text in [&settings, &hooks] {
        assert!(
            !text.contains(env!("CARGO_BIN_EXE_dira")),
            "no absolute dira path may leak into a committed config"
        );
    }

    // Second run: byte-for-byte fixpoint.
    let before = snapshot(dir);
    let out = run_cloud_init(dir, &[], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(before, snapshot(dir), "re-run must change nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Deliberately no `git_init`: --print is a dry run and must work outside
    // a git work tree too.
    let mock = mock_release().await;
    let out = run_cloud_init(dir, &["--print"], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    assert!(
        snapshot(dir).is_empty(),
        "--print is a dry run; the directory must stay empty"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("bootstrap.sh"), "prints the scripts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_a_git_repo_refuses_without_print() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // No `git_init` here either — this is the case that must refuse.
    let mock = mock_release().await;
    let out = run_cloud_init(dir, &[], &mock.download_base());
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("git work tree"), "{stderr}");
    assert!(stderr.contains("--print"), "{stderr}");
    assert!(snapshot(dir).is_empty(), "a refusal must write nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_pin_skips_the_fetch_and_writes_unpinned() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    // A download base that would fail loudly if ever hit.
    let out = run_cloud_init(dir, &["--no-pin"], "http://127.0.0.1:1/unreachable");
    assert!(out.status.success(), "{out:?}");
    let bootstrap = std::fs::read_to_string(dir.join(".dira/bootstrap.sh")).unwrap();
    assert!(bootstrap.contains("expected_x86_64=\"\""), "{bootstrap}");
    assert!(bootstrap.contains("expected_aarch64=\"\""), "{bootstrap}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("could not fetch release digests"),
        "--no-pin must skip the fetch outright, not fail it: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_digest_fetch_warns_and_writes_unpinned_without_failing_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    let out = run_cloud_init(dir, &[], "http://127.0.0.1:1/unreachable");
    assert!(
        out.status.success(),
        "a failed fetch must warn, not fail: {out:?}"
    );
    let bootstrap = std::fs::read_to_string(dir.join(".dira/bootstrap.sh")).unwrap();
    assert!(bootstrap.contains("expected_x86_64=\"\""), "{bootstrap}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not fetch release digests"),
        "{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_filter_wires_only_the_named_harness() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    let mock = mock_release().await;
    let out = run_cloud_init(dir, &["--harness", "claude"], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    assert!(dir.join(".claude/settings.json").exists());
    assert!(
        !dir.join(".cursor/hooks.json").exists(),
        "unselected harnesses must not be touched"
    );
    // Unknown harness: a clear error naming the accepted set.
    let out = run_cloud_init(dir, &["--harness", "gemini"], &mock.download_base());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("claude, cursor"), "{stderr}");
}

/// A repo that already carries `dira init`'s machine-specific hooks gets them
/// upgraded to the portable form, not duplicated beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absolute_path_entries_are_replaced_not_duplicated() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    let mock = mock_release().await;
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::write(
        dir.join(".claude/settings.json"),
        serde_json::json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": "/home/me/.local/bin/dira hook claude" } ] },
                    { "hooks": [ { "type": "command", "command": "eslint --fix" } ] }
                ]
            }
        })
        .to_string(),
    )
    .unwrap();

    let out = run_cloud_init(dir, &["--harness", "claude"], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    let settings = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    assert!(!settings.contains("/home/me/.local/bin/dira"), "{settings}");
    assert!(settings.contains("eslint --fix"), "non-dira hooks survive");
    assert!(settings.contains(".dira/hook.sh claude"));
}

/// Bare `cloud init` (two harnesses selected) refuses a malformed harness
/// config rather than silently discarding it — same posture as `dira
/// onboard`. Naming a single `--harness` keeps the historical
/// overwrite-on-parse-failure behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_cloud_init_refuses_a_malformed_harness_config() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    let mock = mock_release().await;
    std::fs::create_dir_all(dir.join(".cursor")).unwrap();
    let malformed = "{ this is not json";
    std::fs::write(dir.join(".cursor/hooks.json"), malformed).unwrap();

    let out = run_cloud_init(dir, &[], &mock.download_base());
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not valid JSON"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join(".cursor/hooks.json")).unwrap(),
        malformed,
        "a refusal must leave the malformed file byte-identical"
    );

    // The single-harness form is still the historical overwrite-and-proceed.
    let out = run_cloud_init(dir, &["--harness", "cursor"], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    let hooks = std::fs::read_to_string(dir.join(".cursor/hooks.json")).unwrap();
    assert!(hooks.contains("sh .dira/hook.sh cursor"), "{hooks}");
}

/// `dira init` merged over a repo `dira cloud init` already wired must add
/// nothing — the portable wrapper counts as current — while `--global` still
/// writes its own (unrelated) file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dira_init_over_a_cloud_init_repo_adds_nothing_and_global_still_writes() {
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    // A HOME distinct from the project dir, so a `--global` write can be
    // shown to land somewhere other than the (already fully-wired) project
    // file rather than trivially matching it because both happen to be the
    // same directory.
    let home = tempfile::tempdir().unwrap();
    git_init(dir);
    let mock = mock_release().await;
    let out = run_cloud_init(dir, &["--harness", "claude"], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    let before = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();

    let out = run_dira_init(dir, home.path(), &[]);
    assert!(out.status.success(), "{out:?}");
    let after = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    assert_eq!(
        before, after,
        "dira init must add nothing over an already-wired portable wrapper"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dira cloud init"),
        "the note explaining why nothing was added must be printed: {stdout}"
    );

    // Every event still carries exactly one hook entry (no duplication) —
    // the reverse-order case: `cloud init` then `dira init` leaves one entry
    // per event, not two.
    let settings: serde_json::Value = serde_json::from_str(&after).unwrap();
    for event in [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "Stop",
        "SubagentStop",
        "Notification",
        "PreToolUse",
        "PostToolUse",
    ] {
        let groups = settings["hooks"][event].as_array().unwrap_or_else(|| {
            panic!("{event} must still be wired: {after}");
        });
        let entries: usize = groups
            .iter()
            .map(|g| g["hooks"].as_array().map_or(0, |h| h.len()))
            .sum();
        assert_eq!(entries, 1, "{event} must carry exactly one entry: {after}");
    }

    // `--global` still writes its own file at HOME — the project-scope
    // portable wrapper must not make it a no-op.
    let global_path = home.path().join(".claude/settings.json");
    assert!(!global_path.exists(), "sanity: not written yet");
    let out = run_dira_init(dir, home.path(), &["--global"]);
    assert!(out.status.success(), "{out:?}");
    assert!(global_path.exists(), "--global must still write");
    let global_after = std::fs::read_to_string(&global_path).unwrap();
    assert!(
        global_after.contains("dira hook claude"),
        "the global file carries the direct (non-portable) command: {global_after}"
    );
    // The project file, wired by `cloud init`, is untouched by the --global run.
    assert_eq!(
        before,
        std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap(),
        "a --global run must not touch the project file"
    );
}

/// A repo's own `.gitignore` excluding the committed artifacts is a silent
/// failure mode (the wiring is written but never actually reaches a
/// commit/push) — `cloud init` must warn, naming the path and the fix,
/// without failing the command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warns_when_a_committed_artifact_is_gitignored() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git_init(dir);
    std::fs::write(dir.join(".gitignore"), ".dira/\n").unwrap();
    let mock = mock_release().await;

    let out = run_cloud_init(dir, &["--harness", "claude"], &mock.download_base());
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(".dira/hook.sh"), "{stderr}");
    assert!(stderr.contains(".gitignore"), "{stderr}");
    // The unignored config is not warned about.
    assert!(
        !stderr.contains(".claude/settings.json is excluded"),
        "{stderr}"
    );
}
