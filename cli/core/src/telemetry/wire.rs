//! Wire types for the telemetry batch shipped to the cloud ingest endpoint.
//!
//! Deliberately **not** part of `/contract`: unlike an attestation, a telemetry
//! batch is never signed and never verified byte-for-byte across languages, so
//! it carries no `schemars` derive and no JSON Schema is generated for it. Serde
//! only.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One flattened telemetry event, ready to serialize.
///
/// The five base fields (`event`, `timestamp`, `dira_version`, `os`, `arch`) are
/// always present; everything else is `Option` + `skip_serializing_if`, so a
/// given event's JSON carries only the keys its variant actually populated —
/// see the round-trip tests in `event.rs`, which assert exactly that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEventWire {
    pub event: String,
    pub timestamp: String,
    pub dira_version: String,
    pub os: String,
    pub arch: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Milliseconds. Doubles as `CommandExecuted`'s wall time and
    /// `DaemonStopped`'s uptime — both are "how long did this run" measured on
    /// the same clock, so one field carries both rather than adding a second
    /// duration-shaped column for a single variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_host_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_source: Option<String>,
}

impl TelemetryEventWire {
    /// The base shape every event carries; [`super::event::TelemetryEvent::into_wire`]
    /// fills in whichever optional fields its variant populates.
    pub(crate) fn base(event: &'static str, timestamp: String, dira_version: &str) -> Self {
        Self {
            event: event.to_string(),
            timestamp,
            dira_version: dira_version.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            command: None,
            duration_ms: None,
            success: None,
            error_kind: None,
            repo_host_class: None,
            repo_visibility: None,
            repo_hash: None,
            telemetry_enabled: None,
            consent_source: None,
        }
    }
}

/// A batch of telemetry events POSTed to the cloud ingest endpoint in one call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryBatch {
    /// Wire format version; `1` for this work package.
    pub v: u32,
    pub batch_id: String,
    pub install_id: String,
    pub generated_at: String,
    pub events: Vec<TelemetryEventWire>,
}

/// A deterministic batch id for idempotent resend (D-0020): hex SHA-256 of
/// `"{install_id}:{first_id}:{last_id}"`. Re-sending the same cursor window
/// after a dropped ack produces the same id, so the cloud can de-dupe on it
/// instead of double-counting a retried POST.
pub fn batch_id(install_id: &str, first_id: &str, last_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(install_id.as_bytes());
    hasher.update(b":");
    hasher.update(first_id.as_bytes());
    hasher.update(b":");
    hasher.update(last_id.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_id_is_deterministic() {
        let a = batch_id("inst1", "01A", "01Z");
        let b = batch_id("inst1", "01A", "01Z");
        assert_eq!(a, b);
    }

    #[test]
    fn batch_id_changes_with_any_input() {
        let base = batch_id("inst1", "01A", "01Z");
        assert_ne!(base, batch_id("inst2", "01A", "01Z"));
        assert_ne!(base, batch_id("inst1", "01B", "01Z"));
        assert_ne!(base, batch_id("inst1", "01A", "01Y"));
    }

    #[test]
    fn base_wire_carries_only_the_five_required_fields() {
        let wire =
            TelemetryEventWire::base("cli_daemon_started", "2026-01-01T00:00:00Z".into(), "0.5.1");
        let json = serde_json::to_value(&wire).unwrap();
        let obj = json.as_object().unwrap();
        let keys: std::collections::BTreeSet<_> = obj.keys().cloned().collect();
        let expected: std::collections::BTreeSet<_> =
            ["event", "timestamp", "diraVersion", "os", "arch"]
                .into_iter()
                .map(String::from)
                .collect();
        assert_eq!(keys, expected);
    }
}
