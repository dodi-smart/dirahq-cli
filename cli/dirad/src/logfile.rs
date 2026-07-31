//! A size-capped log file for `dirad` on windows.
//!
//! `dira daemon start` nulls all three stdio handles and adds `CREATE_NO_WINDOW`,
//! and under a `schtasks` logon task there is no console either. On macOS the
//! launchd plist sets `StandardOutPath`/`StandardErrorPath`, and on Linux systemd
//! captures the journal — **windows had nothing at all**. When a user's capture
//! channel died for days, there was no log anywhere to explain it.
//!
//! Deliberately hand-rolled rather than `tracing-appender`: that crate is absent
//! from `Cargo.lock` and would pull itself plus `crossbeam-channel` and
//! `crossbeam-utils`, and its `WorkerGuard` must be held for the process lifetime
//! or logs silently vanish on exit — a new footgun in exchange for ~60 lines.
//! `dirad` writes a handful of lines a minute, so a blocking write is fine.
//!
//! Windows-only by default (macOS and Linux already have a sink, and a second one
//! would duplicate output and invalidate the existing runbooks). `$DIRA_LOG_DIR`
//! opts unix in on demand.

use std::io::{self, Write};
use std::path::PathBuf;

/// Roll to `dirad.log.1` past this, keeping ~10 MiB across two files forever.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Where the daemon's log lives, or `None` to keep stdout-only behaviour.
pub fn log_dir() -> Option<PathBuf> {
    let default = if cfg!(windows) {
        dira_core::config::project_dirs().map(|d| d.data_dir().join("logs"))
    } else {
        None
    };
    resolve_log_dir(std::env::var("DIRA_LOG_DIR").ok(), default)
}

/// The resolution rule, with its inputs injected so it is testable on every
/// platform without touching process state — the same discipline D-0008 requires
/// of `default_socket_path`.
fn resolve_log_dir(env: Option<String>, platform_default: Option<PathBuf>) -> Option<PathBuf> {
    match env {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => platform_default,
    }
}

/// The file the daemon logs to, for `dira daemon status` to point at.
pub fn log_path() -> Option<PathBuf> {
    log_dir().map(|d| d.join("dirad.log"))
}

/// A `Write` that rolls one generation aside when it outgrows [`MAX_BYTES`].
pub struct RollingLog {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
}

impl RollingLog {
    /// Open (or create) the log under `dir`, appending to any existing file.
    pub fn open(dir: &std::path::Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("dirad.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RollingLog {
            path,
            file,
            written,
        })
    }

    fn roll_if_needed(&mut self) {
        if self.written < MAX_BYTES {
            return;
        }
        // Best-effort: if the rename fails (a tail holding the file open, say),
        // keep appending rather than losing the line. An oversized log beats no
        // log — the whole point of this module.
        let previous = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&previous);
        if std::fs::rename(&self.path, &previous).is_ok() {
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                self.file = f;
                self.written = 0;
            }
        }
    }
}

impl Write for RollingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.roll_if_needed();
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_log_dir_wins_over_the_platform_default() {
        let fallback = Some(PathBuf::from("/platform/default"));
        assert_eq!(
            resolve_log_dir(Some("/explicit".into()), fallback.clone()),
            Some(PathBuf::from("/explicit"))
        );
        // An empty env var is not a choice — fall through to the default.
        assert_eq!(
            resolve_log_dir(Some(String::new()), fallback.clone()),
            fallback
        );
        assert_eq!(resolve_log_dir(None, fallback.clone()), fallback);
    }

    /// Unix keeps stdout-only unless asked: launchd and journald already capture
    /// the daemon, and a second sink would duplicate output.
    #[test]
    fn unix_has_no_platform_default() {
        assert_eq!(resolve_log_dir(None, None), None);
    }

    #[test]
    fn writes_append_and_the_file_lands_where_expected() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RollingLog::open(dir.path()).unwrap();
        log.write_all(b"first\n").unwrap();
        log.flush().unwrap();
        drop(log);

        let mut log = RollingLog::open(dir.path()).unwrap();
        log.write_all(b"second\n").unwrap();
        log.flush().unwrap();

        let body = std::fs::read_to_string(dir.path().join("dirad.log")).unwrap();
        assert_eq!(
            body, "first\nsecond\n",
            "reopening must append, not truncate"
        );
    }

    /// The cap is what makes an unattended daemon safe to log from: two files,
    /// bounded, forever.
    #[test]
    fn the_log_rolls_once_it_outgrows_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RollingLog::open(dir.path()).unwrap();
        // Pretend we are already at the cap rather than writing 5 MiB.
        log.written = MAX_BYTES;
        log.write_all(b"after the roll\n").unwrap();
        log.flush().unwrap();

        assert!(
            dir.path().join("dirad.log.1").exists(),
            "previous generation kept"
        );
        let current = std::fs::read_to_string(dir.path().join("dirad.log")).unwrap();
        assert_eq!(current, "after the roll\n");
    }
}
