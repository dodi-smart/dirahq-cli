//! Advisory cloud→daemon sync handshake, parsed from the ingest HTTP *response*.
//!
//! Response-only and **unsigned** — this is NOT part of the signed request
//! contract (`dira_contract`), so adding fields here never touches the signing
//! vector. Every field is `#[serde(default)]`, so an older cloud that returns a
//! bare `IngestAck` (no `sync` block) deserializes to all-`None` and the daemon
//! simply skips the handshake.
//!
//! The daemon acts on two signals (see `dirad::sync`):
//! - `data_epoch` changes → the cloud's durable log was reset → re-send from scratch.
//! - `synced_event_id` → cached for an honest `dira device status` (display only;
//!   never an automatic rewind, by design — recovery is via the cloud reconciler
//!   plus a manual `dira device resync`).

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncBlock {
    /// Cloud's persisted high-water mark for this device (a batch ULID). Advisory.
    #[serde(default)]
    pub synced_event_id: Option<String>,
    /// Opaque token that changes when the cloud's data was reset.
    #[serde(default)]
    pub data_epoch: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub batch_id: String,
    #[serde(default)]
    pub sync: SyncBlock,
}

/// Parse an ingest response body into the typed handshake. Tolerant: a missing,
/// empty, or non-JSON body yields all-default (the handshake is then a no-op).
pub fn parse_ingest_response(body: &str) -> IngestResponse {
    serde_json::from_str(body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_block() {
        let r = parse_ingest_response(
            r#"{"status":"accepted","batchId":"01ABC","sync":{"syncedEventId":"01XYZ","dataEpoch":"ep-1"}}"#,
        );
        assert_eq!(r.status, "accepted");
        assert_eq!(r.batch_id, "01ABC");
        assert_eq!(r.sync.synced_event_id.as_deref(), Some("01XYZ"));
        assert_eq!(r.sync.data_epoch.as_deref(), Some("ep-1"));
    }

    #[test]
    fn tolerates_legacy_ack_without_sync() {
        // An older cloud returns only the IngestAck shape — no `sync` block.
        let r = parse_ingest_response(r#"{"serverTime":"t","accepted":1,"duplicates":0}"#);
        assert!(r.sync.synced_event_id.is_none());
        assert!(r.sync.data_epoch.is_none());
    }

    #[test]
    fn tolerates_empty_body() {
        let r = parse_ingest_response("");
        assert_eq!(r.status, "");
        assert!(r.sync.data_epoch.is_none());
    }
}
