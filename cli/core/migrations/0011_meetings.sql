-- Bot-less meeting detection (DIRASH-0060..0066, proposed).
--
-- A meeting is a SPAN, not events: a person listening in a call emits nothing.
-- Detected high-confidence meetings are `recorded`; low-confidence runs (and,
-- in `suggest` mode, every run) are `suggested` until the person confirms or
-- dismisses them. Accounting overlays `recorded` and `confirmed` spans on the
-- counted gaps (DIRASH-0063).
--
-- Content-free like the wire (D-0001): a closed provider class, never an app
-- name, window title, calendar title or attendee. Times use the canonical
-- fixed-width UTC form (DIRASH-0043). `version` is monotonic per meeting and
-- `updated_seq` is the store-wide change cursor sync reads, so a confirmation
-- or retraction (an update, which no rowid cursor sees) is re-sent.
CREATE TABLE IF NOT EXISTS meetings (
    id                              TEXT PRIMARY KEY,
    version                         INTEGER NOT NULL DEFAULT 1,
    updated_seq                     INTEGER NOT NULL,
    started_at                      TEXT NOT NULL,
    ended_at                        TEXT NOT NULL,
    state                           TEXT NOT NULL
        CHECK (state IN ('suggested', 'recorded', 'confirmed', 'dismissed', 'retracted')),
    source                          TEXT NOT NULL
        CHECK (source IN ('device', 'calendar', 'presence', 'user')),
    confidence                      TEXT NOT NULL CHECK (confidence IN ('high', 'low')),
    provider_class                  TEXT NOT NULL,
    evidence                        INTEGER NOT NULL DEFAULT 0,
    project                         TEXT,
    engaged_during_meeting_seconds  INTEGER NOT NULL DEFAULT 0,
    created_at                      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS meetings_started_idx ON meetings (started_at);
CREATE INDEX IF NOT EXISTS meetings_seq_idx ON meetings (updated_seq);
