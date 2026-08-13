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
        .env("DIRA_DB_PATH", home.join("isolated.db"));
}
