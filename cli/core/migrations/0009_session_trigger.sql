-- Session trigger (DIRASH-0046). A session a trigger started rather than a person
-- (today: a Claude Desktop scheduled task) is marked on the event whose
-- transcript pass found it, and rolls up first-non-null per session like the
-- lineage columns in 0007. `trigger` is an enum word ('scheduled'); `trigger_name`
-- is the slug of an EXISTING ~/.claude/scheduled-tasks/<slug>/ directory. Never
-- prompt text, a task description or SKILL.md content.
ALTER TABLE events ADD COLUMN trigger TEXT;
ALTER TABLE events ADD COLUMN trigger_name TEXT;
