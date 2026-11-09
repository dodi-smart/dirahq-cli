//! Bring a down daemon back from the hook shim (DIRASH-0049).
//!
//! When `dira hook` finds nothing listening, it has already spooled the event.
//! This asks whatever should be running `dirad` to start it, so the spool drains
//! within seconds instead of at the next logon:
//!
//! - windows: `schtasks /Run` when the `DiraDaemon` task exists, so the daemon
//!   runs under the task's restart-on-failure; else a direct windowless spawn
//!   (the Run-key case, or nothing installed).
//! - macOS: `launchctl kickstart` when the agent plist exists; else a direct
//!   spawn.
//! - Linux: `systemctl --user start` when the unit exists; else a direct spawn.
//!
//! Starting through the supervisor where there is one matters. A bare spawn
//! beside a supervisor would hold the single-instance guard, and the
//! supervisor's own start would then fail its bind and respawn-loop (D-0009).
//!
//! Never from an elevated shell (an elevated daemon is the broken combination,
//! D-0016), never in a debug build (tests run the shim with no daemon and must
//! not leave strays behind), never with `DIRA_NO_AUTOSTART`, and at most once
//! per [`MIN_INTERVAL`] per user.

use dira_core::Config;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Minimum gap between two start attempts. A burst of hooks against a dead
/// daemon makes exactly one attempt.
const MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Whether the shim may try at all in this process.
pub fn allowed() -> bool {
    !cfg!(debug_assertions)
        && std::env::var_os("DIRA_NO_AUTOSTART").is_none_or(|v| v.is_empty() || v == "0")
        && !dira_ipc::elevation::is_elevated()
}

fn stamp_path(config: &Config) -> PathBuf {
    config
        .db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("autostart.stamp")
}

/// Claim the attempt slot: `true` if no attempt happened within
/// [`MIN_INTERVAL`]. Writing the stamp first means two racing hooks cannot both
/// start a daemon.
fn claim(stamp: &Path, now: SystemTime) -> bool {
    let recent = std::fs::metadata(stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age < MIN_INTERVAL);
    if recent {
        return false;
    }
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(stamp, b"").is_ok()
}

/// Is the daemon simply not running (as opposed to busy or refusing us)?
pub async fn is_down(config: &Config) -> bool {
    match dira_ipc::connect(&config.socket_path).await {
        Ok(_) => false,
        Err(e) => crate::client::classify(&e) == crate::client::Reach::Down,
    }
}

/// Try to bring the daemon back. Best effort and silent: the event is already
/// spooled, so nothing here can lose data.
pub fn kick(config: &Config) {
    if !claim(&stamp_path(config), SystemTime::now()) {
        return;
    }
    if !via_supervisor() {
        spawn_bare();
    }
}

#[cfg(windows)]
fn via_supervisor() -> bool {
    dira_core::process::command("schtasks")
        .args(["/Run", "/TN", "DiraDaemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn via_supervisor() -> bool {
    let Ok(home) = dira_core::config::home_dir() else {
        return false;
    };
    if !home
        .join("Library/LaunchAgents/sh.dirahq.dirad.plist")
        .exists()
    {
        return false;
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    std::process::Command::new("launchctl")
        .args(["kickstart", &format!("gui/{uid}/sh.dirahq.dirad")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
fn via_supervisor() -> bool {
    let Ok(home) = dira_core::config::home_dir() else {
        return false;
    };
    if !home.join(".config/systemd/user/dirad.service").exists() {
        return false;
    }
    std::process::Command::new("systemctl")
        .args(["--user", "start", "--no-block", "dirad.service"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn via_supervisor() -> bool {
    false
}

/// Spawn `dirad` detached and windowless, the sibling of this executable.
fn spawn_bare() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let bin = dir.join(dira_ipc::DIRAD_BIN);
    if !bin.exists() {
        return;
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    dira_core::process::detached(&mut cmd);
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_attempt_per_interval() {
        let dir = std::env::temp_dir().join(format!("dira-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stamp = dir.join("autostart.stamp");
        let now = SystemTime::now();
        assert!(claim(&stamp, now));
        assert!(!claim(&stamp, now + Duration::from_secs(5)));
        assert!(claim(&stamp, now + MIN_INTERVAL + Duration::from_secs(1)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(debug_assertions)]
    fn debug_builds_never_autostart() {
        // The test binary is a debug build; the shim it would run must not
        // leave stray daemons behind.
        assert!(!allowed());
    }
}
