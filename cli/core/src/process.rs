//! Child processes: windowless, and never holding the caller's pipes.
//!
//! On windows `dirad` is a windows-subsystem binary with no console (DIRASH-0048).
//! A console program it spawns without `CREATE_NO_WINDOW` (`git`, a plugin
//! script) gets a brand-new visible console window. These spawns fire on the
//! capture and sweep paths, so a missing flag shows up as windows flashing in the
//! user's session. Every spawn reachable from the daemon goes through
//! [`command`]/[`hidden`] for that reason, and every long-lived child (the
//! daemon itself, the background updater, the update-check refresh) through
//! [`spawn_background`].

use std::ffi::OsStr;
use std::process::{Child, Command, Stdio};

/// `CREATE_NO_WINDOW`: the child gets no console window. Ignored for
/// windows-subsystem children, so it is safe to set unconditionally.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `CREATE_NEW_PROCESS_GROUP`: a Ctrl-C delivered to the parent's console does
/// not reach the child.
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// `CREATE_BREAKAWAY_FROM_JOB`: the child leaves the parent's job object, so it
/// outlives a parent the Task Scheduler ends.
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// A [`Command`] for `program` that never opens a window on windows. A no-op
/// wrapper elsewhere.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    hidden(&mut cmd);
    cmd
}

/// Mark an existing [`Command`] windowless. Returns it for chaining.
pub fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Start a long-lived background child: stdin null, stdout/stderr to `log`
/// (or null), windowless and in its own process group, plus any
/// `extra_windows_flags` (e.g. [`CREATE_BREAKAWAY_FROM_JOB`]). If the extra
/// flags are refused (a job that forbids breakaway), the spawn is retried
/// without them.
///
/// Callers should also have run [`disinherit_std_handles`] at startup, so the
/// child cannot hold this process's own stdout pipe open.
pub fn spawn_background(
    cmd: &mut Command,
    log: Option<std::fs::File>,
    extra_windows_flags: u32,
) -> std::io::Result<Child> {
    cmd.stdin(Stdio::null());
    match log.and_then(|f| f.try_clone().ok().map(|dup| (f, dup))) {
        Some((out, err)) => {
            cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(base | extra_windows_flags);
        match cmd.spawn() {
            Ok(child) => Ok(child),
            Err(_) if extra_windows_flags != 0 => {
                cmd.creation_flags(base);
                cmd.spawn()
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = extra_windows_flags;
        // Its own process group, so a Ctrl-C in the terminal that started the
        // parent (or launchd tearing down the parent's job group) does not
        // take the child with it.
        cmd.process_group(0);
        cmd.spawn()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = extra_windows_flags;
        cmd.spawn()
    }
}

/// Make this process's own standard handles non-inheritable. Call once, first
/// thing in `main`.
///
/// On windows `CreateProcess` runs with `bInheritHandles = TRUE`, so a child
/// inherits every inheritable handle the parent holds, including the pipe the
/// parent's own stdout is connected to, even when the child's stdio is
/// redirected. A `dirad` started that way held the pipe for its whole life, and
/// whatever read it never saw end-of-file:
/// - `install.ps1` piping `dira daemon install`;
/// - a harness reading `dira hook`'s stdout;
/// - any `dira daemon start | ...`.
///
/// A one-time clear is safe for children that DO want our stdio: Rust's
/// `Stdio::inherit()` hands the child a fresh inheritable duplicate of the std
/// handle rather than relying on the original's flag. Unlike a guard held around
/// each spawn, it cannot race a spawn on another thread. A no-op on unix, where
/// fds 0-2 are always replaced by the child's redirection and every other fd is
/// close-on-exec.
pub fn disinherit_std_handles() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own std handles; a
            // null or invalid handle is skipped.
            unsafe {
                let h = GetStdHandle(which);
                if !h.is_null() && h != INVALID_HANDLE_VALUE {
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}
