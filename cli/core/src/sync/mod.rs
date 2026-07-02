//! Cloud sync: turn a window of the local event log into a signed-ready
//! [`dira_contract::AttestationBatch`]. The daemon owns scheduling + transport;
//! this module is the pure, testable derivation.

pub mod batch;
pub mod billing;
pub mod handshake;

pub use batch::{
    build_batch, build_batch_with_partials, build_chunked_batches, est_cost, ArtifactRow,
    ChunkBatch, PartialSession, TokenRow, CHUNK_EVENTS,
};
pub use billing::{
    parse_billing_summary_response, BillingSummary, CachedBillingSummary, META_BILLING_SUMMARY,
};
pub use handshake::{parse_ingest_response, IngestResponse, SyncBlock};

/// `meta` key: the last event id confirmed-synced to the cloud. The window for a
/// flush is `(cursor, max_event_id]`. Lives here (rather than in the daemon) so
/// the store can reset it on `nuke` and the daemon + CLI share one definition.
pub const META_SYNC_CURSOR: &str = "sync_cursor_event_id";

/// `meta` key: the largest `artifacts.rowid` confirmed-synced to the cloud.
/// Artifacts aren't event-id ordered, so they ship on their own cursor; the
/// cloud dedups on `sha`, making an over-inclusive lower bound harmless.
pub const META_ARTIFACTS_CURSOR: &str = "sync_cursor_artifact_rowid";

/// `meta` key: the last `dataEpoch` the cloud reported. A change means the cloud's
/// durable log was reset → the daemon re-sends from scratch (see [`handshake`]).
pub const META_LAST_EPOCH: &str = "sync_last_data_epoch";

/// `meta` key: the cloud's last-reported persisted watermark (`syncedEventId`),
/// cached so `dira device status` can show "in sync / cloud behind" without a round
/// trip. Advisory/display only — never drives an automatic rewind.
pub const META_CLOUD_WATERMARK: &str = "sync_cloud_watermark";
