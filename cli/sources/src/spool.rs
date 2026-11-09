//! The hook spool: where a normalized hook waits when it cannot reach the daemon
//! right now (DIRASH-0049).
//!
//! A hook used to be dropped whenever the daemon was down, busy, starting, or
//! its ingest queue was full, leaving only a breadcrumb. Now the `dira hook`
//! shim writes the event here when it cannot deliver it, the daemon writes it
//! here when its queue is full, and the daemon drains the directory back into
//! its writer at startup and every few seconds after.
//!
//! **Only the normalized event is written**: session id, kind, cwd, tool name,
//! transcript path, orchestration hints. Never the raw harness payload, which
//! carries prompt text and tool input (D-0001 governs the wire, and the same
//! rule keeps that content off this disk).
//!
//! One file per event, written to a dot-prefixed temp name and renamed into
//! place, so concurrent hook processes never contend for a lock and a reader
//! never sees a half-written record. Names start with a zero-padded nanosecond
//! timestamp, so a lexical sort is arrival order.

use crate::Normalized;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Record format version. A daemon skips (and quarantines) a record whose
/// version it does not know, rather than misreading it.
pub const VERSION: u8 = 1;

/// Hard cap on spooled records. At ~400 bytes each this is ~40 MB, which is
/// weeks of heavy use with the daemon down. Past it the oldest are dropped and
/// counted, because an unbounded spool on a machine where the daemon never
/// comes back is its own outage.
pub const MAX_RECORDS: usize = 100_000;

/// One spooled hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub v: u8,
    /// Canonical harness id (`claude`, `codex`, ...).
    pub harness: String,
    /// When the hook fired (RFC 3339), so a late delivery keeps its real time.
    pub observed_at: String,
    pub event: Normalized,
}

impl Record {
    pub fn new(harness: &str, observed_at: String, event: Normalized) -> Self {
        Self {
            v: VERSION,
            harness: crate::canonical_harness_id(harness)
                .unwrap_or(harness)
                .to_string(),
            observed_at,
            event,
        }
    }
}

/// The spool directory for a store at `db_path`: `spool/` beside the database.
/// Derived from the database path, not a fixed location, so an isolated daemon
/// (`DIRA_DB_PATH`) gets an isolated spool for free.
pub fn dir_for(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("spool")
}

static SEQ: AtomicU32 = AtomicU32::new(0);

fn file_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:024}-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Write one record. Creates the directory on first use.
pub fn write(dir: &Path, record: &Record) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = file_name();
    let tmp = dir.join(format!(".{name}.tmp"));
    let dst = dir.join(&name);
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, &dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(dst)
}

/// Every complete record file, oldest first. Temp files and the `bad/`
/// subdirectory are skipped.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    files.sort();
    files
}

/// Read one record. `Err` for unreadable, malformed or unknown-version files.
pub fn read(path: &Path) -> Result<Record, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let record: Record = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if record.v != VERSION {
        return Err(format!("unknown spool record version {}", record.v));
    }
    Ok(record)
}

/// Move a record that cannot be read into `bad/`, so it is kept for diagnosis
/// but never retried in a loop.
pub fn quarantine(dir: &Path, path: &Path) {
    let bad = dir.join("bad");
    if std::fs::create_dir_all(&bad).is_ok() {
        if let Some(name) = path.file_name() {
            if std::fs::rename(path, bad.join(name)).is_ok() {
                return;
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Drop the oldest records beyond `max`. Returns how many were dropped.
pub fn trim(dir: &Path, max: usize) -> usize {
    let files = list(dir);
    if files.len() <= max {
        return 0;
    }
    let excess = files.len() - max;
    files
        .iter()
        .take(excess)
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::model::EventKind;

    fn event(session: &str) -> Normalized {
        Normalized {
            session_id: session.into(),
            kind: EventKind::PreTool,
            cwd: Some("/w/repo".into()),
            tool: Some("Edit".into()),
            transcript_path: None,
            source: None,
            reason: None,
            hints: Box::default(),
        }
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dira-spool-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trips_in_arrival_order() {
        let dir = tmp();
        for i in 0..5 {
            let r = Record::new(
                "claude-code",
                format!("2026-11-09T10:00:0{i}Z"),
                event(&format!("s{i}")),
            );
            write(&dir, &r).unwrap();
        }
        let files = list(&dir);
        assert_eq!(files.len(), 5);
        let sessions: Vec<String> = files
            .iter()
            .map(|p| read(p).unwrap().event.session_id)
            .collect();
        assert_eq!(sessions, ["s0", "s1", "s2", "s3", "s4"]);
        assert_eq!(read(&files[0]).unwrap().harness, "claude");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_lists_temp_files_and_quarantines_garbage() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".x.json.tmp"), b"{").unwrap();
        std::fs::write(dir.join("0001-bad.json"), b"not json").unwrap();
        let files = list(&dir);
        assert_eq!(files.len(), 1);
        assert!(read(&files[0]).is_err());
        quarantine(&dir, &files[0]);
        assert!(list(&dir).is_empty());
        assert!(dir.join("bad/0001-bad.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trim_drops_the_oldest() {
        let dir = tmp();
        for i in 0..4 {
            write(
                &dir,
                &Record::new("codex", String::new(), event(&format!("s{i}"))),
            )
            .unwrap();
        }
        assert_eq!(trim(&dir, 2), 2);
        let left: Vec<String> = list(&dir)
            .iter()
            .map(|p| read(p).unwrap().event.session_id)
            .collect();
        assert_eq!(left, ["s2", "s3"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_record_holds_no_raw_payload() {
        // The record type has no field that could carry prompt text or tool
        // input: serialising one yields only the normalized keys.
        let r = Record::new("claude", "t".into(), event("s"));
        let v = serde_json::to_value(&r).unwrap();
        let keys: Vec<&str> = v["event"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for k in &keys {
            assert!(
                [
                    "session_id",
                    "kind",
                    "cwd",
                    "tool",
                    "transcript_path",
                    "source",
                    "reason",
                    "hints"
                ]
                .contains(k),
                "unexpected field {k}"
            );
        }
    }
}
