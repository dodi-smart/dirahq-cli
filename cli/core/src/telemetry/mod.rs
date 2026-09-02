//! Telemetry foundation (WP1): an anonymous, opt-out usage channel gated by
//! [`crate::config::TelemetryKnobs`].
//!
//! - [`identity`]: the per-install id + salt the rest of this module is keyed on.
//! - [`repo_facts`]: pure classification of a canonical repo remote (no I/O — the
//!   caller supplies the salt and any visibility it already knows).
//! - [`event`]: the closed set of events other crates are allowed to emit.
//! - [`wire`]: the serde-only batch shape POSTed to the cloud ingest endpoint.
//!
//! The local queue (`telemetry_events`, migration `0006_telemetry.sql`) is
//! append-only and PII-free by construction: every column is either an id, a
//! timestamp, an event name from the closed [`event::TelemetryEvent`] set, or a
//! JSON blob of the wire-shaped, already-scrubbed properties in
//! [`wire::TelemetryEventWire`]. Nothing upstream of `Store::insert_telemetry_event`
//! ever hands it a prompt, a file path, or free text.

pub mod event;
pub mod identity;
pub mod repo_facts;
pub mod wire;

/// `meta` key: highest `telemetry_events.id` (ULID) confirmed-synced. Same
/// id-cursor idiom as `sync::META_TOKEN_CURSOR` / the knowledge channel's
/// cursors — see [`crate::store::Store::telemetry_events_since`].
pub const META_TELEMETRY_CURSOR: &str = "telemetry_cursor_event_id";
/// `meta` key: the telemetry channel's health snapshot (same shape discipline
/// as `sync::META_SYNC_HEALTH`).
pub const META_TELEMETRY_HEALTH: &str = "telemetry_sync_health";
/// Filename, under the XDG config dir (beside `config.toml`), marking that the
/// one-time "telemetry is on by default, here's how to turn it off" notice has
/// already been shown on this machine. Presence alone is the signal — the file
/// carries no content.
pub const NOTICE_MARKER_FILE: &str = ".telemetry-notice-shown";
