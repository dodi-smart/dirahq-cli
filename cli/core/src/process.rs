//! Child processes that must never show a window.
//!
//! On windows `dirad` is a windows-subsystem binary with no console (DIRASH-0048).
//! A console program it spawns without `CREATE_NO_WINDOW` (`git`, a plugin
//! script) gets a brand-new visible console window. These spawns fire on the
//! capture and sweep paths, so a missing flag shows up as windows flashing in the
//! user's session. Every spawn reachable from the daemon goes through
//! [`hidden`] (or [`command`]) for that reason.

use std::ffi::OsStr;
use std::process::Command;

/// `CREATE_NO_WINDOW`: the child gets no console window. Ignored for
/// windows-subsystem children, so it is safe to set unconditionally.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `CREATE_NEW_PROCESS_GROUP`: a Ctrl-C delivered to the parent's console does
/// not reach the child.
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

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

/// Mark a [`Command`] windowless and detached from the parent's console process
/// group, for a long-lived child such as `dirad` itself or a background
/// `dira update --auto`.
pub fn detached(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a Ctrl-C in the terminal that started the
        // parent does not take the child down with it.
        cmd.process_group(0);
    }
    cmd
}

/// Spawn a long-lived child without letting it inherit **this** process's own
/// standard handles.
///
/// On windows `Command::spawn` passes `bInheritHandles = TRUE`, so the child
/// inherits every inheritable handle the parent holds, including the pipe its
/// own stdout is connected to, even when the child's stdio is redirected to
/// null or a file. `dirad` then held that pipe for its whole life, and whatever
/// was reading it never saw end-of-file:
/// - `install.ps1` piping `dira daemon install` hung once install started the
///   daemon;
/// - a harness reading `dira hook`'s stdout would wait on a hook that started
///   the daemon;
/// - any `dira daemon start | ...` never returned.
///
/// The fix clears the inherit flag on the three standard handles for the
/// duration of the spawn, then restores it, so later `Stdio::inherit()`
/// children still work. On unix the child's fds 0-2 are replaced by the
/// redirection and every other fd is close-on-exec already.
pub fn spawn_isolated(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(windows)]
    {
        let _guard = windows_std::NoInherit::new();
        cmd.spawn()
    }
    #[cfg(not(windows))]
    {
        cmd.spawn()
    }
}

#[cfg(windows)]
mod windows_std {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Clears `HANDLE_FLAG_INHERIT` on the std handles that had it, and puts
    /// it back on drop.
    pub struct NoInherit(Vec<HANDLE>);

    impl NoInherit {
        pub fn new() -> Self {
            let mut cleared = Vec::new();
            for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                // SAFETY: plain Win32 calls on this process's own std handles;
                // a null or invalid handle is skipped.
                unsafe {
                    let h = GetStdHandle(which);
                    if h.is_null() || h == INVALID_HANDLE_VALUE || cleared.contains(&h) {
                        continue;
                    }
                    let mut flags = 0u32;
                    if GetHandleInformation(h, &mut flags) != 0
                        && flags & HANDLE_FLAG_INHERIT != 0
                        && SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) != 0
                    {
                        cleared.push(h);
                    }
                }
            }
            Self(cleared)
        }
    }

    impl Drop for NoInherit {
        fn drop(&mut self) {
            for h in &self.0 {
                // SAFETY: restoring the flag this guard cleared on a handle
                // this process still owns.
                unsafe {
                    SetHandleInformation(*h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
                }
            }
        }
    }
}
