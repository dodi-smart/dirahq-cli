-- Orchestration lineage (DIRASH-0041). Sessions are derived from the event
-- log, so — like `spec` in 0006 — the facts live on events and roll up per
-- session: `host_session` / `workspace` / `parent_session` first-non-null,
-- `spawned` per event (one child per `start_session` call). Ids and an opaque
-- workspace digest only; never titles, paths, or tool arguments.
ALTER TABLE events ADD COLUMN host_session TEXT;
ALTER TABLE events ADD COLUMN workspace TEXT;
ALTER TABLE events ADD COLUMN spawned TEXT;
ALTER TABLE events ADD COLUMN parent_session TEXT;

-- A child resolves its parent by looking up the event that spawned its host id.
CREATE INDEX IF NOT EXISTS events_spawned_idx ON events (spawned) WHERE spawned IS NOT NULL;
