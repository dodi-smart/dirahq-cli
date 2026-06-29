//! Cloud sync: turn a window of the local event log into a signed-ready
//! [`dira_contract::AttestationBatch`]. The daemon owns scheduling + transport;
//! this module is the pure, testable derivation.

pub mod batch;

pub use batch::{
    build_batch, build_batch_with_partials, est_cost, ArtifactRow, PartialSession, TokenRow,
};

/// `meta` key: the last event id confirmed-synced to the cloud. The window for a
/// flush is `(cursor, max_event_id]`. Lives here (rather than in the daemon) so
/// the store can reset it on `nuke` and the daemon + CLI share one definition.
pub const META_SYNC_CURSOR: &str = "sync_cursor_event_id";

/// `meta` key: the largest `artifacts.rowid` confirmed-synced to the cloud.
/// Artifacts aren't event-id ordered, so they ship on their own cursor; the
/// cloud dedups on `sha`, making an over-inclusive lower bound harmless.
pub const META_ARTIFACTS_CURSOR: &str = "sync_cursor_artifact_rowid";
