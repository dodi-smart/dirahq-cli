-- The local telemetry queue: an append-only, PII-free buffer of anonymous
-- usage events awaiting sync to the cloud ingest endpoint.
--
-- `props_json` is the already-scrubbed JSON of a `TelemetryEventWire` (see
-- `dira_core::telemetry::wire`) — the closed `TelemetryEvent` set is the only
-- producer, so nothing upstream of `Store::insert_telemetry_event` can hand
-- this table a prompt, a file path, or free text. `id` is a ULID (monotonic),
-- giving the sync channel the same id-cursor idiom every other channel here
-- uses (`events`, `zavet_guard_events`, ...).
CREATE TABLE IF NOT EXISTS telemetry_events (
    id         TEXT PRIMARY KEY,   -- ULID, monotonic
    created_at TEXT NOT NULL,      -- RFC 3339 UTC
    name       TEXT NOT NULL,      -- e.g. cli_command_executed
    props_json TEXT NOT NULL       -- serialized TelemetryEventWire
);

CREATE INDEX IF NOT EXISTS idx_telemetry_events_created_at
    ON telemetry_events (created_at);
