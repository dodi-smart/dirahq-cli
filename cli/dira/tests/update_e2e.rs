//! End-to-end integration test for `dira update` (production-distribution
//! plan §A3 / task T4).
//!
//! unix-only: this test shells real `#!/bin/sh` fixture scripts (both the
//! "installed" `dira`/`dirad` and the ones packed into the fixture archive)
//! and asserts on `0o755` mode bits — neither the shebang trick nor
//! `PermissionsExt` has a windows equivalent. The windows swap path is
//! covered by `replace.rs`'s unit tests instead (`swap_in`'s
//! `#[cfg(windows)]` rename-around-a-locked-dest logic and
//! `artifact.rs`'s `#[cfg(windows)]` zip-extraction tests).
//!
//! `dira` is a bin-only crate (no `[lib]` target), so this can't call
//! `update::run` directly — it drives the real compiled binary as a
//! subprocess, exactly the way a user would, against a local mock GitHub
//! (an `axum` server — already a dev-dependency for `device.rs`'s mock cloud,
//! see `src/test_support.rs`) and a real `.tar.gz` built at test time with
//! `tar`.
//!
//! # Safety: this test NEVER touches the real daemon or `~/.local/bin`
//!
//! Every invocation below passes `--no-restart` and an explicit
//! `--bin-dir` pointing into a `tempfile::tempdir()`. `--no-restart` means
//! `update::run` returns before it ever calls `daemon::restart` — so even
//! though this machine may have a real launchd-supervised `dirad` running,
//! nothing here can reach it. `DIRA_SOCKET_PATH` is also isolated, purely as
//! defense in depth.
//!
//! The subprocess's HOME is redirected into the same tempdir by
//! [`isolate_user_dirs`], because `--bin-dir` does NOT cover everything
//! `dira update` writes: the update-check cache lives at
//! `project_dirs()?.cache_dir()/update-check.json`, resolved from the real
//! `$HOME` and reachable by no CLI flag. Without that redirect a `--check`
//! run here persisted the mock's fixture tag into the developer's own cache,
//! and every subsequent real `dira` invocation on that machine advertised an
//! upgrade to a version that does not exist. Any new env var that steers a
//! *write* path belongs in `isolate_user_dirs`, not in one test.
//!
//! # Why the subprocess is a *copy* of the test binary, not a symlink or the
//! # original path
//!
//! `update::replace::discover_install` refuses to run when the *running
//! process's own* `current_exe()` resolves under a `target/{release,debug}`
//! ancestor (`Guard::DevBuild` — never overridable, not even by `--force`;
//! see `replace.rs`). `CARGO_BIN_EXE_dira` (what `cargo test` builds) is
//! exactly such a path. So each test copies that binary into the fake
//! install's `bin_dir` first and runs *that* copy — its `current_exe()`
//! then resolves inside the tempdir, matching a real installed `dira`, and
//! the swap logic gets to run for real.

#![cfg(unix)]

mod common;
use common::{isolate_user_dirs, lock_staging, output_staged, status_staged, MockGitHub};

use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

// ---------------------------------------------------------------------------
// fixture archive construction
// ---------------------------------------------------------------------------

const TARGET: &str = "e2e-test-target";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Build `dira-<version>-<TARGET>.tar.gz` with two trivial scripts at its
/// root — `dira` echoes `dira <version>` (what `verify_installed_version`'s
/// `--version` check looks for) and `dirad` is an inert placeholder that is
/// never executed by this test. Returns `(tarball_bytes, tarball_name,
/// sha_file_contents, sha_name)`.
fn build_fixture_archive(workdir: &Path, version: &str) -> (Vec<u8>, String, Vec<u8>, String) {
    let root = workdir.join(format!("root-{version}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("dira"),
        format!("#!/bin/sh\necho \"dira {version}\"\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("dirad"),
        format!("#!/bin/sh\necho \"dirad {version}\"\n"),
    )
    .unwrap();
    for name in ["dira", "dirad"] {
        std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tarball_name = format!("dira-{version}-{TARGET}.tar.gz");
    let tarball_path = workdir.join(&tarball_name);
    // `tar` forks like any other subprocess, so its child can inherit a write fd
    // to a binary another thread is staging. Same lock, same reason.
    let mut tar = Command::new("tar");
    tar.arg("-czf")
        .arg(&tarball_path)
        .arg("-C")
        .arg(&root)
        .args(["dira", "dirad"]);
    let status = status_staged(&mut tar).expect("spawn tar to build the fixture archive");
    assert!(
        status.success(),
        "building the fixture archive with tar failed"
    );

    let tarball_bytes = std::fs::read(&tarball_path).unwrap();
    let digest = sha256_hex(&tarball_bytes);
    let sha_name = format!("dira-{version}-{TARGET}.sha256");
    let sha_contents = format!("{digest}  {tarball_name}\n").into_bytes();

    (tarball_bytes, tarball_name, sha_contents, sha_name)
}

// ---------------------------------------------------------------------------
// fake install directory
// ---------------------------------------------------------------------------

/// Populate `bin_dir` with a "currently installed" `dira` + `dirad`: `dira`
/// is a **copy of the real compiled test binary** (see the module docs on
/// why this matters for `discover_install`'s `DevBuild` guard), and `dirad`
/// is an inert placeholder. Returns their original byte content, for
/// byte-identical assertions after a failed update.
fn seed_install(bin_dir: &Path) -> (Vec<u8>, Vec<u8>) {
    std::fs::create_dir_all(bin_dir).unwrap();
    let dira_src = std::fs::read(env!("CARGO_BIN_EXE_dira")).unwrap();
    let dirad_src =
        b"#!/bin/sh\necho \"dirad (placeholder, never executed by this test)\"\n".to_vec();

    // Held across BOTH writes so no other test thread can fork while a write fd
    // to a staged executable is open. See `EXEC_STAGING`.
    let _staging = lock_staging();
    for (name, bytes) in [("dira", &dira_src), ("dirad", &dirad_src)] {
        let path = bin_dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    (dira_src, dirad_src)
}

fn run_update(bin_dir: &Path, download_base: &str, extra_args: &[&str]) -> Output {
    let mut cmd = Command::new(bin_dir.join("dira"));
    cmd.arg("update")
        .arg("--bin-dir")
        .arg(bin_dir)
        .arg("--no-restart")
        // `--no-restart` already means the D2 plugin-refresh arm (inside
        // `daemon::restart`'s `Ok(())` branch) can never run here, but
        // `--no-zavet` is added anyway on the same D-0021 principle as the
        // rest of this helper: a test that execs the real binary isolates
        // every user dir it touches, rather than relying on one guard
        // (`--no-restart`) to also cover a second, unrelated one. Without
        // this, a dev machine with a real `claude` on PATH would be one
        // refactor away from this test suite shelling out to it for real.
        .arg("--no-zavet")
        .args(extra_args)
        .env("DIRA_DOWNLOAD_URL", download_base)
        .env("DIRA_TARGET", TARGET)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN");
    // `bin_dir` is `<tempdir>/bin`, so its parent is the per-test tempdir.
    isolate_user_dirs(
        &mut cmd,
        bin_dir.parent().expect("bin_dir has a tempdir parent"),
    );
    output_staged(&mut cmd).expect("spawn the copied dira binary")
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_update_replaces_both_binaries_mode_0755_and_cleans_up_backups() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    let (tarball, tarball_name, sha, sha_name) = build_fixture_archive(tmp.path(), "9.9.9");
    mock.put_asset(&tarball_name, tarball);
    mock.put_asset(&sha_name, sha);

    let out = run_update(&bin_dir, &mock.download_base(), &["--version", "9.9.9"]);
    assert!(
        out.status.success(),
        "update failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let dira_out = std::fs::read_to_string(bin_dir.join("dira")).unwrap();
    assert!(
        dira_out.contains("9.9.9"),
        "swapped-in dira was: {dira_out}"
    );
    let dirad_out = std::fs::read_to_string(bin_dir.join("dirad")).unwrap();
    assert!(
        dirad_out.contains("9.9.9"),
        "swapped-in dirad was: {dirad_out}"
    );

    assert_eq!(mode_of(&bin_dir.join("dira")), 0o755);
    assert_eq!(mode_of(&bin_dir.join("dirad")), 0o755);

    assert!(
        !bin_dir.join(".dira.bak").exists(),
        "backup must be cleaned up on success"
    );
    assert!(
        !bin_dir.join(".dirad.bak").exists(),
        "backup must be cleaned up on success"
    );
}

/// `--no-zavet` (baked into every `run_update` call — see its comment) must
/// actually suppress the post-update zavet plugin refresh: a successful
/// update's stdout must carry no `zavet plugin:` line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_update_with_no_zavet_prints_no_plugin_refresh_line() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    let (tarball, tarball_name, sha, sha_name) = build_fixture_archive(tmp.path(), "9.9.8");
    mock.put_asset(&tarball_name, tarball);
    mock.put_asset(&sha_name, sha);

    let out = run_update(&bin_dir, &mock.download_base(), &["--version", "9.9.8"]);
    assert!(
        out.status.success(),
        "update failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("zavet plugin:"),
        "--no-zavet must suppress the plugin refresh line: stdout={stdout:?}"
    );
}

/// A stale `GITHUB_TOKEN` in the environment must not break `dira update`.
///
/// Real-world report: an expired token exported in the user's shell — common,
/// and nothing to do with dira — turned every release lookup into a hard 401
/// against a repo that needs no credentials at all. A token only lifts GitHub's
/// anonymous rate limit here, so a rejected one is dropped and resolution
/// continues anonymously. Same fix as `install.sh` / `install.ps1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_token_falls_back_to_anonymous_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    mock.set_latest_tag("v42.0.0");
    mock.reject_authorized_requests();

    let mut cmd = Command::new(bin_dir.join("dira"));
    cmd.arg("update")
        .arg("--bin-dir")
        .arg(&bin_dir)
        .arg("--no-restart")
        .arg("--check")
        .env("DIRA_API_URL", mock.api_base())
        .env("DIRA_DOWNLOAD_URL", mock.download_base())
        .env("DIRA_REPO", "test-repo")
        .env("DIRA_TARGET", TARGET)
        // The whole point: a credential the server rejects.
        .env("GITHUB_TOKEN", "ghp_expiredAndNoLongerValid")
        .env_remove("GH_TOKEN")
        .stdin(std::process::Stdio::null());
    isolate_user_dirs(
        &mut cmd,
        bin_dir.parent().expect("bin_dir has a tempdir parent"),
    );
    let out = output_staged(&mut cmd).expect("spawn the copied dira binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a rejected token must not fail the check: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("42.0.0"),
        "must still resolve the release anonymously: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("rejected by GitHub (401)"),
        "must say why the token was ignored: stderr={stderr:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_resolves_against_the_latest_endpoint_and_mutates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let (orig_dira, orig_dirad) = seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    mock.set_latest_tag("v42.0.0");
    // Deliberately never register any asset — `--check` must never download.

    let mut cmd = Command::new(bin_dir.join("dira"));
    cmd.arg("update")
        .arg("--bin-dir")
        .arg(&bin_dir)
        .arg("--no-restart")
        .arg("--check")
        .env("DIRA_API_URL", mock.api_base())
        .env("DIRA_DOWNLOAD_URL", mock.download_base())
        // The mock's `/repos/{repo}/releases/latest` route captures a single
        // path segment; the real default (`dodi-smart/dirahq-cli`) is two.
        .env("DIRA_REPO", "test-repo")
        .env("DIRA_TARGET", TARGET)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(std::process::Stdio::null());
    isolate_user_dirs(
        &mut cmd,
        bin_dir.parent().expect("bin_dir has a tempdir parent"),
    );
    let out = output_staged(&mut cmd).expect("spawn the copied dira binary");
    assert!(
        out.status.success(),
        "--check must exit 0: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("42.0.0"),
        "expected the resolved version in stdout, got stdout={stdout:?} stderr={stderr:?}"
    );

    assert_eq!(
        std::fs::read(bin_dir.join("dira")).unwrap(),
        orig_dira,
        "--check must not touch dira"
    );
    assert_eq!(
        std::fs::read(bin_dir.join("dirad")).unwrap(),
        orig_dirad,
        "--check must not touch dirad"
    );
    assert!(!bin_dir.join(".dira.bak").exists());
    assert!(!bin_dir.join(".dirad.bak").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_exits_zero_even_when_the_asset_host_is_unreachable() {
    // Point at a port nothing is listening on -- simulates "offline". `--check`
    // must still exit 0 (script-friendly), per plan §A3 item 6.
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    seed_install(&bin_dir);

    let mut cmd = Command::new(bin_dir.join("dira"));
    cmd.arg("update")
        .arg("--bin-dir")
        .arg(&bin_dir)
        .arg("--no-restart")
        .arg("--check")
        .env("DIRA_API_URL", "http://127.0.0.1:1") // nothing listens on port 1
        .env("DIRA_TARGET", TARGET)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(std::process::Stdio::null());
    isolate_user_dirs(
        &mut cmd,
        bin_dir.parent().expect("bin_dir has a tempdir parent"),
    );
    let out = output_staged(&mut cmd).expect("spawn the copied dira binary");

    assert!(
        out.status.success(),
        "--check must exit 0 even offline: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_sha256_aborts_leaving_originals_byte_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let (orig_dira, orig_dirad) = seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    let (tarball, tarball_name, _sha, sha_name) = build_fixture_archive(tmp.path(), "1.2.3");
    mock.put_asset(&tarball_name, tarball);
    // A deliberately wrong checksum -- well-formed, just not the tarball's.
    let wrong_sha =
        format!("0000000000000000000000000000000000000000000000000000000000000  {tarball_name}\n");
    mock.put_asset(&sha_name, wrong_sha.into_bytes());

    let out = run_update(&bin_dir, &mock.download_base(), &["--version", "1.2.3"]);
    assert!(
        !out.status.success(),
        "a checksum mismatch must abort the update"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("checksum"),
        "expected a checksum-mismatch error, got: {stderr}"
    );

    assert_eq!(
        std::fs::read(bin_dir.join("dira")).unwrap(),
        orig_dira,
        "dira must be byte-identical to before the aborted update"
    );
    assert_eq!(
        std::fs::read(bin_dir.join("dirad")).unwrap(),
        orig_dirad,
        "dirad must be byte-identical to before the aborted update"
    );
    assert!(
        !bin_dir.join(".dira.bak").exists(),
        "no swap ever started, so no backup either"
    );
    assert!(!bin_dir.join(".dirad.bak").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_the_tarball_asset_gives_a_clear_error_and_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let (orig_dira, orig_dirad) = seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    // Nothing registered for "9.0.0" -- every asset GET 404s.

    let out = run_update(&bin_dir, &mock.download_base(), &["--version", "9.0.0"]);
    assert!(!out.status.success(), "a 404'd asset must abort the update");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("404"),
        "expected a clear 404 error, got: {stderr}"
    );

    assert_eq!(std::fs::read(bin_dir.join("dira")).unwrap(), orig_dira);
    assert_eq!(std::fs::read(bin_dir.join("dirad")).unwrap(), orig_dirad);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_pin_downgrades_without_resistance() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    seed_install(&bin_dir);

    let mock = MockGitHub::start().await;
    // "Currently installed" is the real dev build (reports its own dev
    // version via --version); the requested pin is numerically far lower,
    // and there must be no guard rejecting that -- `dira update --version`
    // is documented to allow downgrades.
    let (tarball, tarball_name, sha, sha_name) = build_fixture_archive(tmp.path(), "0.0.1");
    mock.put_asset(&tarball_name, tarball);
    mock.put_asset(&sha_name, sha);

    let out = run_update(&bin_dir, &mock.download_base(), &["--version", "0.0.1"]);
    assert!(
        out.status.success(),
        "downgrade must succeed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let dira_out = std::fs::read_to_string(bin_dir.join("dira")).unwrap();
    assert!(
        dira_out.contains("0.0.1"),
        "expected the downgraded version, got: {dira_out}"
    );
}

/// The regression this exists to prevent (#101): a D-0004 dev-install
/// refusal is deterministic and permanent-by-design -- no amount of retrying
/// `dira update` fixes it -- so it must never bump the passive notice's
/// consecutive-failure counter. Before the fix, a `just install` contributor
/// who ran an ordinary (unpinned) `dira update` against their dev build got
/// the refusal *and* a bumped counter, and two such runs alone escalated the
/// notice to "N recent update attempts failed" for a condition that was
/// never going away on its own.
///
/// Triggers `Guard::DevBuild` (not the PATH-symlink `DevSymlink` guard,
/// which is simpler to stage): running the copied test binary itself out of
/// a `target/debug` ancestor trips it unconditionally, regardless of
/// `--bin-dir` -- see `replace::discover_install`. The run is deliberately
/// *not* `--version`-pinned (unlike every other test in this file), because
/// a pinned run is excluded from the counter for an unrelated reason
/// (`update::run`'s `pinned` check) and would not exercise the `Refusal`
/// marker this test is actually pinning down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dev_build_refusal_does_not_bump_the_update_failure_counter() {
    let tmp = tempfile::tempdir().unwrap();

    let dev_dir = tmp.path().join("target").join("debug");
    std::fs::create_dir_all(&dev_dir).unwrap();
    let dira_copy = dev_dir.join("dira");
    {
        // Held across the write so no other test thread can fork while a
        // write fd to this staged executable is open (D-0021).
        let _staging = lock_staging();
        std::fs::copy(env!("CARGO_BIN_EXE_dira"), &dira_copy).unwrap();
        std::fs::set_permissions(&dira_copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    let mock = MockGitHub::start().await;
    // Ahead of any plausible running dev version, so the (unrelated)
    // downgrade guard never intervenes before the DevBuild guard is reached.
    mock.set_latest_tag("v999.0.0");

    let home = tmp.path().join("home");
    let mut cmd = Command::new(&dira_copy);
    cmd.arg("update")
        .arg("--bin-dir")
        .arg(&bin_dir)
        .arg("--no-restart")
        .env("DIRA_API_URL", mock.api_base())
        .env("DIRA_DOWNLOAD_URL", mock.download_base())
        .env("DIRA_REPO", "test-repo")
        .env("DIRA_TARGET", TARGET)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN");
    isolate_user_dirs(&mut cmd, &home);
    let out = output_staged(&mut cmd).expect("spawn the target/debug copy");

    assert!(
        !out.status.success(),
        "a DevBuild install must refuse: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("target/{release,debug}"),
        "expected the DevBuild refusal text, got: {stderr}"
    );

    // With the Refusal marker recognised, `update::run` never calls
    // `record_update_failure`, so the update-check cache is never written at
    // all along this path (nothing else in an unpinned, non---check run
    // touches it). Before the fix this file would exist with
    // `update_failures: 1`.
    assert!(
        find_cache_file(&home).is_none(),
        "a deterministic refusal must not create or bump the update-check cache"
    );
}

/// Recursively find `update-check.json` under `home` -- the update-check
/// cache resolves via `project_dirs()` (platform-specific: e.g.
/// `Library/Caches/sh.dirahq.dira` on macOS, `$XDG_CACHE_HOME/dira` on
/// Linux), so this walks rather than hard-coding one shape.
fn find_cache_file(home: &Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(home).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_cache_file(&path) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == "update-check.json") {
            return Some(path);
        }
    }
    None
}
