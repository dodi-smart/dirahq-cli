-- Free-text human description for a manual session (`dira log`/`invoice`/`start`
-- `--note`, or the trailing comment). Surfaced on the session rollup and synced to
-- the cloud. `label` + `activity` already exist on this table.
ALTER TABLE events ADD COLUMN note TEXT;
