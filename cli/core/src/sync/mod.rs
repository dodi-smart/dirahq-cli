//! Cloud sync: turn a window of the local event log into a signed-ready
//! [`dira_contract::AttestationBatch`]. The daemon owns scheduling + transport;
//! this module is the pure, testable derivation.

pub mod batch;
pub mod billing;
pub mod drift;
pub mod handshake;
pub mod health;
pub mod knowledge;
pub mod ratelimit;

pub use batch::{
    build_batch, build_batch_with_partials, build_chunked_batches, est_cost, fnv1a_bytes,
    ArtifactRow, ChunkBatch, PartialSession, TokenRow, CHUNK_ARTIFACTS, CHUNK_EVENTS, CHUNK_TOKENS,
};
pub use billing::{
    parse_billing_summary_response, BillingSummary, CachedBillingSummary, META_BILLING_SUMMARY,
};
pub use drift::{body_head, warn_unreadable_body};
pub use handshake::{parse_ingest_response, IngestResponse, SyncBlock};
pub use health::{parse_sync_health, SyncHealth, META_SYNC_HEALTH};
pub use ratelimit::{parse_retry_after_body, parse_retry_after_secs, Backoff};

/// `meta` key: the last event id confirmed-synced to the cloud. The window for a
/// flush is `(cursor, max_event_id]`. Lives here (rather than in the daemon) so
/// the store can reset it on `nuke` and the daemon + CLI share one definition.
pub const META_SYNC_CURSOR: &str = "sync_cursor_event_id";

/// `meta` key: the largest `artifacts.rowid` confirmed-synced to the cloud.
/// Artifacts aren't event-id ordered, so they ship on their own cursor; the
/// cloud dedups on `sha`, making an over-inclusive lower bound harmless.
pub const META_ARTIFACTS_CURSOR: &str = "sync_cursor_artifact_rowid";

/// `meta` key: the largest `token_usage.rowid` confirmed-synced to the cloud.
///
/// Deliberately a rowid and not an `at` watermark. Token rows carry the
/// *transcript's* timestamp, which is not monotonic with respect to capture
/// order: a turn is discovered by the `Stop` that follows it, so it is always
/// back-dated relative to its trigger, and a re-read after `nuke` re-imports a
/// whole transcript at its original historical timestamps. Selecting on `at`
/// therefore skips rows permanently — which is exactly the defect this cursor
/// replaces. `rowid` is insertion-ordered by construction, so it cannot.
///
/// The cloud dedups on `TokenUsage.id` (the transcript uuid), so an
/// over-inclusive lower bound is free; under-inclusion is the only direction
/// the id cannot protect against.
pub const META_TOKEN_CURSOR: &str = "sync_cursor_token_rowid";

/// `meta` key: the last `dataEpoch` the cloud reported. A change means the cloud's
/// durable log was reset → the daemon re-sends from scratch (see [`handshake`]).
pub const META_LAST_EPOCH: &str = "sync_last_data_epoch";

/// `meta` key: the cloud's last-reported persisted watermark (`syncedEventId`),
/// cached so `dira device status` can show "in sync / cloud behind" without a round
/// trip. Advisory/display only — never drives an automatic rewind.
pub const META_CLOUD_WATERMARK: &str = "sync_cloud_watermark";
