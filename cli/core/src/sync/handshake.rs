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
    /// Records the cloud newly wrote for this batch. `None` — not `0` — when the
    /// cloud doesn't report it, and that distinction is the entire fix for issue
    /// #72: the daemon used to deserialize the real 202 body into an all-zero
    /// typed ack and log `accepted=0 duplicates=0` after every *successful*
    /// flush, so its own telemetry claimed nothing had been accepted while sync
    /// was perfectly healthy. An absent count must read as "unknown", never "zero".
    #[serde(default)]
    pub accepted: Option<u64>,
    /// Records the cloud already had (idempotent duplicates). `None` when
    /// unreported, for the same reason as [`Self::accepted`].
    ///
    /// The cloud also sends a per-table `records` breakdown and a `failures`
    /// count; both are deliberately not modelled here until something reads
    /// them — unknown fields are ignored, so adding them later costs nothing.
    #[serde(default)]
    pub duplicates: Option<u64>,
}

/// Parse an ingest response body into the typed handshake.
///
/// An empty body stays `Ok(default)` — a cloud that acks with no payload is
/// back-compat, and the handshake is then simply a no-op. A *non-empty* body
/// that won't parse is contract drift, and the error goes back to the caller
/// rather than being swallowed: an all-default handshake means no `dataEpoch`
/// and no `syncedEventId`, so the epoch-reset re-send silently doesn't happen
/// and the flush log prints empty status/batch id (#104, and the same shape as
/// the `accepted=0` mystery in #72). See [`super::drift`].
pub fn parse_ingest_response(body: &str) -> Result<IngestResponse, serde_json::Error> {
    if body.trim().is_empty() {
        return Ok(IngestResponse::default());
    }
    serde_json::from_str(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_block() {
        let r = parse_ingest_response(
            r#"{"status":"accepted","batchId":"01ABC","sync":{"syncedEventId":"01XYZ","dataEpoch":"ep-1"}}"#,
        )
        .unwrap();
        assert_eq!(r.status, "accepted");
        assert_eq!(r.batch_id, "01ABC");
        assert_eq!(r.sync.synced_event_id.as_deref(), Some("01XYZ"));
        assert_eq!(r.sync.data_epoch.as_deref(), Some("ep-1"));
    }

    #[test]
    fn tolerates_legacy_ack_without_sync() {
        // An older cloud returns only the IngestAck shape — no `sync` block.
        let r = parse_ingest_response(r#"{"serverTime":"t","accepted":1,"duplicates":0}"#).unwrap();
        assert!(r.sync.synced_event_id.is_none());
        assert!(r.sync.data_epoch.is_none());
    }

    /// Issue #72: a cloud that reports no counters must leave them `None`, so the
    /// daemon can say "unknown" instead of the flatly false "accepted=0".
    #[test]
    fn absent_counters_are_unknown_not_zero() {
        let r =
            parse_ingest_response(r#"{"status":"accepted","batchId":"01ABC","sync":{}}"#).unwrap();
        assert_eq!(r.status, "accepted");
        assert_eq!(r.accepted, None, "absent must not collapse to 0");
        assert_eq!(r.duplicates, None);
    }

    /// …and a cloud that does report them is read verbatim, including a genuine
    /// zero, which is why the distinction has to live in the type.
    #[test]
    fn reported_counters_are_read_verbatim_including_zero() {
        let r = parse_ingest_response(
            r#"{"status":"accepted","batchId":"01ABC","accepted":0,"duplicates":7,
                "records":{"intervals":0,"sessions":0,"tokens":3,"artifacts":4},"sync":{}}"#,
        )
        .unwrap();
        assert_eq!(
            r.accepted,
            Some(0),
            "a reported 0 is a fact, not an absence"
        );
        assert_eq!(r.duplicates, Some(7));
    }

    #[test]
    fn tolerates_empty_body() {
        let r = parse_ingest_response("").expect("an empty ack body stays OK");
        assert_eq!(r.status, "");
        assert!(r.sync.data_epoch.is_none());
    }

    /// #104: a NON-empty body the contract can't read must report the error
    /// instead of collapsing to an all-default handshake. A silent default here
    /// drops `dataEpoch`, so a cloud that reset its log never triggers the
    /// re-send — the failure mode the caller now warns about.
    #[test]
    fn a_drifted_body_reports_an_error_instead_of_defaulting() {
        for body in [
            r#"{"sync":{"dataEpoch":42}}"#, // type drift
            r#"{"accepted":"many"}"#,       // type drift on a counter
            "<html>502 Bad Gateway</html>", // an intercepting proxy, not JSON
            "null",                         // JSON, but not the object
        ] {
            assert!(
                parse_ingest_response(body).is_err(),
                "unreadable body must not degrade silently: {body}"
            );
        }
    }
}
