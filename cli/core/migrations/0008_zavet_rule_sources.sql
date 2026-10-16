-- Framework rule sources: the standing-rule files a linked spec framework
-- carries (the Spec Kit constitution, OpenSpec `config.yaml` context/rules
-- and the legacy `project.md`, BMAD's `AGENTS.md` block and the legacy
-- `project-context.md`), as the installed zavet plugin reports them through
-- `zavet spec-sources --json` (DIRASH-0040, under DIRASH-0039).
--
-- TEXT NEVER LANDS HERE. Every column is metadata: which file, its working-tree
-- blob oid, the version/dates/rule count the framework declares, and the
-- plugin's state word. The rules themselves reach an agent through the
-- plugin's own session context; dira reads none of them. Nothing in this
-- table rides any wire (no `touched_seq`: the knowledge sync never sees it)
-- until a contract version carries rule-source metadata.
--
-- Removed, never deleted: a source a fresh, successful resolve on the default
-- branch no longer lists gets `removed_at`; it is cleared again if the file
-- comes back. `kind` and `state` hold the closed vocabularies of
-- `zavet_specs::RuleSourceKind` / `RuleSourceState` (unknown words: `other`);
-- a source `.zavet/config` turned off is `state = 'off'`, with no flag of its own.
CREATE TABLE IF NOT EXISTS zavet_rule_sources (
    repo          TEXT    NOT NULL,  -- canonical repo
    kind          TEXT    NOT NULL,  -- speckit-constitution | openspec-config | … | other
    path          TEXT    NOT NULL,  -- repo-relative, `/`-separated
    oid           TEXT,              -- working-tree blob oid; NULL when absent
    version       TEXT,              -- framework-declared version, capped
    ratified      TEXT,              -- framework-declared date, verbatim, capped
    last_amended  TEXT,              -- framework-declared date, verbatim, capped
    rule_count    INTEGER,           -- NULL when the kind has no countable rules
    state         TEXT    NOT NULL,  -- ok | unratified | legacy | unresolved | file-missing | off | other
    reason        TEXT,              -- the plugin's one-line why, capped
    overridden    INTEGER NOT NULL DEFAULT 0,  -- path set by a `rule-source:` override
    first_seen_at TEXT    NOT NULL,  -- RFC 3339
    updated_at    TEXT    NOT NULL,  -- RFC 3339, last metadata change
    removed_at    TEXT,              -- RFC 3339; NULL while listed
    PRIMARY KEY (repo, kind, path)
);
