//! A breadcrumb for harness hooks that never reached the daemon.
//!
//! `dira hook <harness>` must always exit 0 and print nothing — a tracker that
//! breaks an agent loop is worse than a tracker that misses an event, and the
//! harness-sources spec makes that a hard invariant. But "never tell the harness"
//! was implemented as "never tell *anyone*": the send result was discarded
//! entirely, so a dead capture channel looked exactly like a healthy one. One
//! machine ran for days with 303 events, every one from a manual timer, while
//! `dira status` reported a perfectly healthy daemon.
//!
//! The carve-out is narrow and worth stating precisely:
//!
//! - the **harness** contract is unchanged: still exit 0, still no stdout;
//! - a **transport** failure (could not reach the daemon, or it refused us)
//!   leaves a local breadcrumb, surfaced on `dira status`;
//! - a **semantic** non-result (unknown harness, unaccounted event kind) stays
//!   silent everywhere — that is not a failure.
//!
//! Modelled on `update::notice`: a small TTL'd JSON file in `cache_dir()`, read by
//! foreground commands and printed to **stderr** so stdout stays byte-identical
//! for anything parsing it. `cache_dir()` is deliberate — this is disposable
//! derived state, deleting it is always harmless, and it keeps the CLI out of the
//! daemon-owned data dir where the SQLite files live.

use dira_core::config::project_dirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Failures older than this stop being reported: a breadcrumb from last month is
/// noise, not news.
const TTL_SECS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Health {
    /// Unix seconds of the most recent failure.
    pub last_error_at: i64,
    pub last_error: String,
    pub harness: String,
    /// Approximate. Hooks run as concurrent one-shot processes, so this
    /// read-modify-write races; the timestamp and message are the load-bearing
    /// fields and this is only ever used to say "a lot" vs "one".
    pub consecutive: u64,
}

fn path() -> Option<PathBuf> {
    project_dirs().map(|d| d.cache_dir().join("hook-health.json"))
}

fn now_secs() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Record that a hook could not be delivered.
///
/// Every filesystem error here is deliberately swallowed: a breadcrumb that could
/// break the hook shim would defeat its own purpose.
pub fn record_failure(harness: &str, reason: &str) {
    let Some(p) = path() else { return };
    let previous = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice::<Health>(&b).ok())
        .unwrap_or_default();
    let health = Health {
        last_error_at: now_secs(),
        last_error: reason.to_string(),
        harness: harness.to_string(),
        consecutive: previous.consecutive.saturating_add(1),
    };
    let Ok(bytes) = serde_json::to_vec(&health) else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write-then-rename so a concurrent reader never sees a half-written file.
    // `fs::rename` replaces an existing destination on both unix and windows.
    let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Record that a hook was delivered — one unlink, self-healing.
pub fn record_success() {
    if let Some(p) = path() {
        let _ = std::fs::remove_file(p);
    }
}

/// The breadcrumb itself, TTL-filtered — for callers (`dira doctor --json`)
/// that want the structured fields rather than the rendered prose. `None` when
/// the file is absent, corrupt, or expired, which is the same set of states
/// [`warning`] stays silent for.
pub(crate) fn snapshot() -> Option<Health> {
    let raw = std::fs::read(path()?).ok()?;
    let h: Health = serde_json::from_slice(&raw).ok()?;
    // Expiry lives in `warning_for`; asking it is how the two stay consistent.
    warning_for(&h, now_secs()).map(|_| h)
}

/// The warning line for `dira status` / `dira version`, or `None`.
pub fn warning() -> Option<String> {
    warning_for(&snapshot()?, now_secs())
}

/// Pure so the TTL and wording are testable without touching the filesystem.
fn warning_for(h: &Health, now: i64) -> Option<String> {
    if h.last_error_at <= 0 || now - h.last_error_at > TTL_SECS {
        return None;
    }
    let ago = now - h.last_error_at;
    let when = if ago < 120 {
        format!("{ago}s ago")
    } else {
        format!("{}m ago", ago / 60)
    };
    Some(format!(
        "warning: {} {} hook(s) could not reach dirad (most recent {when}):\n  {}\n  \
         captured activity is being lost right now — check `dira daemon status`.",
        h.consecutive, h.harness, h.last_error
    ))
}

/// Print the warning to stderr, if any. Stdout is untouched.
pub fn maybe_warn() {
    if let Some(w) = warning() {
        eprintln!("{w}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(at: i64) -> Health {
        Health {
            last_error_at: at,
            last_error: "access denied".into(),
            harness: "claude".into(),
            consecutive: 12,
        }
    }

    #[test]
    fn a_recent_failure_warns_and_names_the_cause() {
        let w = warning_for(&health(1_000_000), 1_000_060).expect("recent failure warns");
        assert!(w.contains("12 claude hook(s)"));
        assert!(w.contains("access denied"));
        assert!(w.contains("60s ago"));
    }

    #[test]
    fn an_old_failure_is_not_news() {
        assert!(warning_for(&health(1_000_000), 1_000_000 + TTL_SECS + 1).is_none());
    }

    #[test]
    fn an_empty_breadcrumb_says_nothing() {
        assert!(warning_for(&Health::default(), 1_000_000).is_none());
    }

    /// The file is disposable: a corrupt or absent one must never surface an
    /// error, because this runs on the foreground path of ordinary commands.
    #[test]
    fn a_corrupt_breadcrumb_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hook-health.json");
        std::fs::write(&p, b"{not json").unwrap();
        let parsed = std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<Health>(&b).ok());
        assert!(parsed.is_none());
    }
}
