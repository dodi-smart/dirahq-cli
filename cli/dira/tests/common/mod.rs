//! Shared fixtures for the tests that exec the real `dira` binary.
//!
//! D-0021 governs this file. Its two rules are the reason it exists as one
//! module rather than a copy per test binary:
//!
//! 1. **Every env var that steers a write path lives in [`isolate_user_dirs`],
//!    not inline in one test.** Issue #90 was a test that added a var in one
//!    place and polluted the developer's real update-check cache from another.
//!    Two copies of the helper reproduce that failure by construction — and
//!    had already begun to: the copy added with `dira onboard` set
//!    `USERPROFILE`, `DIRA_DB_PATH` and `DIRA_SOCKET_PATH`, and the original
//!    did not.
//! 2. **No bare `Command::output()`/`status()`.** Both go through
//!    [`output_staged`]/[`status_staged`], which serialise the *spawn* against
//!    binary staging. The fork window is what produced the `ETXTBSY` flakes in
//!    issue #80; the lock closes it, and is released before the wait so tests
//!    still overlap.

#![allow(dead_code)] // each test binary uses a different subset
#![allow(unused_imports)] // ditto — a binary that doesn't use MockGitHub doesn't need axum

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Mutex;

/// Serialises `spawn` against binary staging.
///
/// The race is not path contention — each test has its own tempdir — but fd
/// inheritance across a fork inside one process: thread A opens its staged
/// binary for writing while thread B forks, B's child inherits the write fd,
/// and A's exec of that inode hits `ETXTBSY`. Go hit the identical race in
/// golang/go#22315.
///
/// This is also why write-then-rename is not sufficient on its own (the
/// original proposal in issue #80): the inherited fd refers to the inode, and
/// renaming merely gives that inode another name. The window has to be closed
/// on the fork side.
static EXEC_STAGING: Mutex<()> = Mutex::new(());

/// Hold the staging lock directly — for a caller that *writes* staged
/// binaries and must keep other threads from forking across both writes.
pub fn lock_staging() -> std::sync::MutexGuard<'static, ()> {
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
pub fn output_staged(cmd: &mut Command) -> std::io::Result<Output> {
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
pub fn status_staged(cmd: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    let mut child = {
        let _staging = lock_staging();
        cmd.spawn()?
    };
    child.wait()
}

/// Point every user-directory lookup the subprocess makes at `home`.
///
/// `directories::ProjectDirs` reads `$HOME` on macOS and
/// `$XDG_CACHE_HOME`/`$HOME` on Linux; `dira_core::config::home_dir` also
/// consults `USERPROFILE`. None of these is exposed as a CLI flag, so
/// `--bin-dir` alone is not containment.
///
/// The socket and database paths are here too, and deliberately point at
/// files that are never created: the control socket is machine-global, so
/// without this a test would find the developer's real `dirad` — a worktree
/// does not isolate it.
///
/// **Add any new write-path env var here**, never inline in one test. That is
/// the whole point of the helper (D-0021).
pub fn isolate_user_dirs(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("DIRA_SOCKET_PATH", home.join("isolated-never-created.sock"))
        .env("DIRA_DB_PATH", home.join("isolated.db"))
        // A developer with a relocated Claude Code config (`CLAUDE_CONFIG_DIR`
        // set in their own shell) must not have it leak into a subprocess that
        // is supposed to see only the isolated `home` above — `hook_yield`
        // and `init::harness_config_paths` both honour this var to find
        // Claude's user-scope settings.json.
        .env_remove("CLAUDE_CONFIG_DIR");
}

// ---------------------------------------------------------------------------
// mock GitHub: one route for `/repos/{repo}/releases/latest`, one generic
// filename-keyed asset route standing in for `DIRA_DOWNLOAD_URL`'s base.
//
// Started life in `update_e2e.rs` (`dira update`'s own e2e suite); moved here
// so `cloud_init_e2e.rs` can reuse `download_base()` for the release-digest
// fetch (WP-G) without a second copy of the same mock server drifting from
// this one.
// ---------------------------------------------------------------------------

use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Default)]
struct MockState {
    latest_body: Arc<Mutex<String>>,
    assets: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// When set, answer 401 to any request carrying an `Authorization` header
    /// while still serving anonymous ones — GitHub's behaviour for a stale or
    /// expired token.
    reject_authorized: Arc<Mutex<bool>>,
}

pub struct MockGitHub {
    base_url: String,
    state: MockState,
}

impl MockGitHub {
    pub async fn start() -> Self {
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

    pub fn download_base(&self) -> String {
        format!("{}/assets", self.base_url)
    }

    /// The mock server's own root — what `DIRA_API_URL` should point at for
    /// a test that exercises `/repos/{repo}/releases/latest` (i.e. anything
    /// that resolves "latest" rather than pinning `--version`).
    pub fn api_base(&self) -> &str {
        &self.base_url
    }

    pub fn set_latest_tag(&self, tag: &str) {
        let body = serde_json::json!({
            "tag_name": tag,
            "prerelease": false,
            "draft": false,
            "assets": []
        });
        *self.state.latest_body.lock().unwrap() = body.to_string();
    }

    /// Make every authenticated request 401 while anonymous ones keep working.
    pub fn reject_authorized_requests(&self) {
        *self.state.reject_authorized.lock().unwrap() = true;
    }

    pub fn put_asset(&self, name: &str, bytes: Vec<u8>) {
        self.state
            .assets
            .lock()
            .unwrap()
            .insert(name.to_string(), bytes);
    }
}
