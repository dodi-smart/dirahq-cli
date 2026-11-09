//! Background auto-update scheduling (DIRASH-0050).
//!
//! `dirad` is the only process that is always running, so it keeps the clock:
//! ten minutes after start, then every six hours with up to half an hour of
//! jitter, it spawns a detached, windowless `dira update --auto` and moves on.
//! It does no network I/O itself. The CLI resolves, downloads, verifies, swaps
//! (D-0003) and restarts this daemon through its supervisor (D-0019), exactly
//! as a hand-run `dira update` does. The foreground rule of D-0006 is
//! unchanged: no user-facing command ever waits on the network for this.
//!
//! The child outlives this daemon on purpose: restarting the daemon is part of
//! its job. On unix it gets its own process group (launchd kills the job's
//! group when the job exits), the systemd unit uses `KillMode=process`, and on
//! windows it asks to break away from the task's job object.

use crate::state::AppState;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Delay before the first check after the daemon starts, so a login is not
/// greeted by an update.
const FIRST_CHECK: Duration = Duration::from_secs(10 * 60);
/// The interval between checks.
const INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// Each interval is spread by this fraction (±30 min of six hours), so a fleet
/// never checks in step.
const JITTER_FRAC: f64 = 30.0 / 360.0;
/// `update.log` is truncated past this size before each run.
const LOG_CAP: u64 = 1 << 20;

/// Start the schedule, unless auto-update is off, or this is a debug build
/// (a development daemon must never replace itself with a release).
pub fn spawn(state: AppState) {
    if cfg!(debug_assertions) || !dira_core::config::auto_update_enabled(&state.config) {
        tracing::debug!("auto-update disabled");
        return;
    }
    tokio::spawn(async move {
        let mut wait = FIRST_CHECK;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = state.shutdown.notified() => return,
            }
            // Re-read: `dira config set update.auto off` must take effect
            // without a daemon restart.
            let live = dira_core::Config::load().unwrap_or_else(|_| state.config.clone());
            if dira_core::config::auto_update_enabled(&live) {
                if let Err(e) = spawn_updater(&state.config) {
                    tracing::warn!("auto-update: could not start the updater: {e}");
                }
            }
            wait = crate::jitter::jittered(INTERVAL, JITTER_FRAC);
        }
    });
}

fn dira_beside_this_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?.join(dira_ipc::DIRA_BIN);
    bin.exists().then_some(bin)
}

fn log_path(config: &dira_core::Config) -> PathBuf {
    crate::logfile::log_dir()
        .unwrap_or_else(|| {
            config
                .db_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
                .join("logs")
        })
        .join("update.log")
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_CAP) {
        let _ = std::fs::remove_file(path);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Spawn `dira update --auto`, detached, and reap it on a thread so it never
/// lingers as a zombie of this daemon.
fn spawn_updater(config: &dira_core::Config) -> anyhow::Result<()> {
    let Some(dira) = dira_beside_this_exe() else {
        anyhow::bail!("no dira binary beside dirad");
    };
    let log = open_log(&log_path(config))?;
    let mut cmd = std::process::Command::new(&dira);
    cmd.args(["update", "--auto"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    dira_core::process::detached(&mut cmd);
    let child = spawn_breaking_away(&mut cmd)?;
    tracing::info!(
        pid = child.id(),
        "auto-update: started `dira update --auto`"
    );
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// On windows, ask for `CREATE_BREAKAWAY_FROM_JOB` so the updater survives the
/// task's job object when this daemon exits, and fall back without it when the
/// job forbids breakaway.
#[cfg(windows)]
fn spawn_breaking_away(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    cmd.creation_flags(
        dira_core::process::CREATE_NO_WINDOW
            | dira_core::process::CREATE_NEW_PROCESS_GROUP
            | CREATE_BREAKAWAY_FROM_JOB,
    );
    match dira_core::process::spawn_isolated(cmd) {
        Ok(c) => Ok(c),
        Err(_) => {
            cmd.creation_flags(
                dira_core::process::CREATE_NO_WINDOW | dira_core::process::CREATE_NEW_PROCESS_GROUP,
            );
            dira_core::process::spawn_isolated(cmd)
        }
    }
}

#[cfg(not(windows))]
fn spawn_breaking_away(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_truncated_past_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/update.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; (LOG_CAP + 1) as usize]).unwrap();
        let _f = open_log(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }
}
