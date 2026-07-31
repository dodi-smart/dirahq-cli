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

use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// mock GitHub: one route for `/repos/{repo}/releases/latest`, one generic
// filename-keyed asset route standing in for `DIRA_DOWNLOAD_URL`'s base.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockState {
    latest_body: Arc<Mutex<String>>,
    assets: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// When set, answer 401 to any request carrying an `Authorization` header
    /// while still serving anonymous ones — GitHub's behaviour for a stale or
    /// expired token.
    reject_authorized: Arc<Mutex<bool>>,
}

struct MockGitHub {
    base_url: String,
    state: MockState,
}

impl MockGitHub {
    async fn start() -> Self {
        let state = MockState::default();

        let latest_state = state.clone();
        let assets_state = state.clone();
        let router = Router::new()
            .route(
                "/repos/{repo}/releases/latest",
                get(
                    move |AxPath(_repo): AxPath<String>, headers: axum::http::HeaderMap| {
                        // Stands in for a stale/expired credential: GitHub 401s
                        // an request carrying a bad bearer while serving the
                        // very same endpoint anonymously. See
                        // `reject_authorized_requests`.
                        let reject = *latest_state.reject_authorized.lock().unwrap();
                        let authorized = headers.contains_key(axum::http::header::AUTHORIZATION);
                        let body = latest_state.latest_body.lock().unwrap().clone();
                        async move {
                            if reject && authorized {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                            ([("content-type", "application/json")], body).into_response()
                        }
                    },
                ),
            )
            .route(
                "/assets/{filename}",
                get(move |AxPath(filename): AxPath<String>| {
                    let assets_state = assets_state.clone();
                    async move {
                        let assets = assets_state.assets.lock().unwrap();
                        match assets.get(&filename) {
                            Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock GitHub server");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            base_url: format!("http://{addr}"),
            state,
        }
    }

    fn download_base(&self) -> String {
        format!("{}/assets", self.base_url)
    }

    /// The mock server's own root — what `DIRA_API_URL` should point at for
    /// a test that exercises `/repos/{repo}/releases/latest` (i.e. anything
    /// that resolves "latest" rather than pinning `--version`).
    fn api_base(&self) -> &str {
        &self.base_url
    }

    fn set_latest_tag(&self, tag: &str) {
        let body = serde_json::json!({
            "tag_name": tag,
            "prerelease": false,
            "draft": false,
            "assets": []
        });
        *self.state.latest_body.lock().unwrap() = body.to_string();
    }

    /// Make every authenticated request 401 while anonymous ones keep working.
    fn reject_authorized_requests(&self) {
        *self.state.reject_authorized.lock().unwrap() = true;
    }

    fn put_asset(&self, name: &str, bytes: Vec<u8>) {
        self.state
            .assets
            .lock()
            .unwrap()
            .insert(name.to_string(), bytes);
    }
}

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
    let status = status_staged(
        Command::new("tar")
            .arg("-czf")
            .arg(&tarball_path)
            .arg("-C")
            .arg(&root)
            .args(["dira", "dirad"]),
    )
    .expect("spawn tar to build the fixture archive");
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

/// Serialises *writing* an executable against *forking* a subprocess.
///
/// `execve` returns `ETXTBSY` when any process holds the target open for
/// writing — and that check is per **inode**, not per path. `cargo test` runs
/// these tests on several threads, so:
///
///   1. thread A opens `A/bin/dira` to stage it (`i_writecount` > 0);
///   2. thread B forks for its own subprocess and the child inherits A's fd —
///      `O_CLOEXEC` does not save us, because it is only applied on a
///      *successful* exec;
///   3. B's child keeps that inode pinned for as long as it lives;
///   4. thread A execs its freshly-staged binary → `ETXTBSY`.
///
/// Each test has its own tempdir, so this is not path contention — it is fd
/// inheritance across a fork inside one process. Go hit the identical race in
/// golang/go#22315.
///
/// Note this is also why write-then-rename would **not** be enough on its own,
/// which is what issue #80 originally proposed: the inherited fd refers to the
/// inode, and renaming merely gives that inode another name. The window has to
/// be closed on the fork side, which is what this lock does.
///
/// Only the `spawn` is serialised, never the wait, so tests still overlap while
/// their subprocesses run.
static EXEC_STAGING: Mutex<()> = Mutex::new(());

fn lock_staging() -> std::sync::MutexGuard<'static, ()> {
    // A panicking test elsewhere must not cascade into "all subsequent spawns
    // panic on a poisoned lock" — the guarded data is `()`, so there is no
    // invariant left to protect.
    EXEC_STAGING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`Command::output`] with the fork serialised against binary staging.
///
/// Mirrors `output()`'s own stdio setup (stdin null, stdout/stderr piped); the
/// only difference is that the `spawn` happens under [`EXEC_STAGING`].
fn output_staged(cmd: &mut Command) -> std::io::Result<Output> {
    let child = {
        let _staging = lock_staging();
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?
    };
    child.wait_with_output()
}

/// [`Command::status`] with the same serialisation. Inherits stdio, as
/// `status()` does.
fn status_staged(cmd: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    let mut child = {
        let _staging = lock_staging();
        cmd.spawn()?
    };
    child.wait()
}

/// Run `<bin_dir>/dira update <extra_args...>` with the mock GitHub wired up
/// and the daemon path hard-disabled (`--no-restart` + an isolated,
/// never-created socket path — see the module doc's safety section).
fn run_update(bin_dir: &Path, download_base: &str, extra_args: &[&str]) -> Output {
    let sock = bin_dir.join("isolated-never-created.sock");
    let mut cmd = Command::new(bin_dir.join("dira"));
    cmd.arg("update")
        .arg("--bin-dir")
        .arg(bin_dir)
        .arg("--no-restart")
        .args(extra_args)
        .env("DIRA_DOWNLOAD_URL", download_base)
        .env("DIRA_TARGET", TARGET)
        .env("DIRA_SOCKET_PATH", &sock)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN");
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

    let sock = bin_dir.join("isolated-never-created.sock");
    let out = output_staged(
        Command::new(bin_dir.join("dira"))
            .arg("update")
            .arg("--bin-dir")
            .arg(&bin_dir)
            .arg("--no-restart")
            .arg("--check")
            .env("DIRA_API_URL", mock.api_base())
            .env("DIRA_DOWNLOAD_URL", mock.download_base())
            .env("DIRA_REPO", "test-repo")
            .env("DIRA_TARGET", TARGET)
            .env("DIRA_SOCKET_PATH", &sock)
            // The whole point: a credential the server rejects.
            .env("GITHUB_TOKEN", "ghp_expiredAndNoLongerValid")
            .env_remove("GH_TOKEN"),
    )
    .expect("spawn the copied dira binary");

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

    let sock = bin_dir.join("isolated-never-created.sock");
    let out = output_staged(
        Command::new(bin_dir.join("dira"))
            .arg("update")
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
            .env("DIRA_SOCKET_PATH", &sock)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN"),
    )
    .expect("spawn the copied dira binary");
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

    let sock = bin_dir.join("isolated-never-created.sock");
    let out = output_staged(
        Command::new(bin_dir.join("dira"))
            .arg("update")
            .arg("--bin-dir")
            .arg(&bin_dir)
            .arg("--no-restart")
            .arg("--check")
            .env("DIRA_API_URL", "http://127.0.0.1:1") // nothing listens on port 1
            .env("DIRA_TARGET", TARGET)
            .env("DIRA_SOCKET_PATH", &sock)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN"),
    )
    .expect("spawn the copied dira binary");

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
