//! Sync-health snapshot (WP-B1a introduces the shape + meta key; WP-B9 wires it
//! into every flush outcome). A compact, persisted summary of the sync task's
//! last attempt, so `dira status` / `dira device status` can show the daemon's
//! own honest view of "is sync actually working" without guessing from the
//! cursor alone (a stalled sync and a quiet-because-nothing-changed sync both
//! leave the cursor sitting still).

use serde::{Deserialize, Serialize};

/// `meta` key: the last-persisted [`SyncHealth`] snapshot (compact JSON),
/// written after every flush attempt (WP-B9). Lives here (not the daemon) so a
/// future CLI-side reader (`dira device status`) can parse it without pulling
/// in `dirad`, mirroring [`super::META_SYNC_CURSOR`]'s split.
pub const META_SYNC_HEALTH: &str = "sync_health";

/// A point-in-time snapshot of the sync task's health. Deliberately compact —
/// RFC 3339 wall times, no derived/redundant fields — so it's cheap to write on
/// every flush attempt (success or failure) and cheap to serve back on
/// `status`. Every field defaults, so an old/short-written snapshot from a
/// prior daemon version still parses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHealth {
    /// RFC 3339 timestamp of the most recent flush attempt (success or failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<String>,
    /// RFC 3339 timestamp of the most recent flush that fully succeeded
    /// (accepted or a no-op "nothing to sync" tick — see the daemon's
    /// `FlushOutcome`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<String>,
    /// A stable, short code for the most recent failure kind (e.g.
    /// `"signature_rejected"`, `"unknown_device"`, `"transient"`,
    /// `"schema_skew"`, `"payload_too_large"`, `"fatal"`), or `None` right after
    /// a success / before the first attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_kind: Option<String>,
    /// Consecutive failed attempts since the last success (0 right after one).
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The backoff the sync loop is currently sleeping (or just slept) for, in
    /// seconds. `0` in steady state (nothing to back off from).
    #[serde(default)]
    pub backoff_secs: u64,
    /// The sync cursor (last confirmed-synced event id) as of this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The cloud's last-reported persisted watermark, as of this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_watermark: Option<String>,
}

/// Parse a persisted [`SyncHealth`] snapshot. Tolerant of an empty or garbled
/// value (a fresh daemon has never written one, or a downgrade wrote an
/// incompatible shape) — degrades to `None` rather than failing `status`.
pub fn parse_sync_health(json: &str) -> Option<SyncHealth> {
    if json.trim().is_empty() {
        return None;
    }
    serde_json::from_str(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sync_health_tolerates_empty_and_garbage() {
        assert!(parse_sync_health("").is_none());
        assert!(parse_sync_health("   ").is_none());
        assert!(parse_sync_health("not json").is_none());
    }

    #[test]
    fn sync_health_roundtrips_camel_case() {
        let h = SyncHealth {
            last_attempt_at: Some("2026-07-09T10:00:00Z".into()),
            last_success_at: Some("2026-07-09T09:55:00Z".into()),
            last_error_kind: None,
            consecutive_failures: 0,
            backoff_secs: 0,
            cursor: Some("01J0EVENT".into()),
            cloud_watermark: Some("01J0WATERMARK".into()),
        };
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("\"lastAttemptAt\""));
        assert!(json.contains("\"cloudWatermark\""));
        assert!(!json.contains("lastErrorKind"), "None must be omitted");
        let back = parse_sync_health(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn sync_health_defaults_missing_fields() {
        let h: SyncHealth = serde_json::from_str("{}").unwrap();
        assert_eq!(h, SyncHealth::default());
    }
}
