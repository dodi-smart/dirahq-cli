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
//! The decision is `daemon::registration` / `daemon::start_registered`, the
//! same names and probes `dira daemon` uses.
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
/// [`MIN_INTERVAL`]. The stamp is created with `create_new`, so of two hooks
/// racing for a free slot exactly one wins; a stale stamp is removed first.
fn claim(stamp: &Path, now: SystemTime) -> bool {
    if let Ok(modified) = std::fs::metadata(stamp).and_then(|m| m.modified()) {
        let fresh = now
            .duration_since(modified)
            .map(|age| age < MIN_INTERVAL)
            .unwrap_or(true);
        if fresh {
            return false;
        }
        let _ = std::fs::remove_file(stamp);
    }
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stamp)
        .is_ok()
}

/// Is the daemon simply not running (as opposed to busy or refusing us)?
pub async fn is_down(config: &Config) -> bool {
    match dira_ipc::connect(&config.socket_path).await {
        Ok(_) => false,
        Err(e) => crate::client::classify(&e) == crate::client::Reach::Down,
    }
}

/// Try to bring the daemon back. Best effort and silent: the event is already
/// spooled, so nothing here can lose data. Whatever is registered to run the
/// daemon starts it (one decision, shared with `dira daemon`); with nothing
/// registered, it is spawned directly.
pub fn kick(config: &Config) {
    if !claim(&stamp_path(config), SystemTime::now()) {
        return;
    }
    if !crate::daemon::start_registered(crate::daemon::registration()) {
        let _ = dira_core::process::spawn_background(
            &mut std::process::Command::new(crate::daemon::locate_dirad()),
            None,
            0,
        );
    }
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
