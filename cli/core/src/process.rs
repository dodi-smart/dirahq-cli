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
