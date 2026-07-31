-- Zavet verification bindings and the errata pointer.
--
-- A check binds a recorded invariant to the command that proves it, so a
-- record can state HOW it is verified instead of asserting it in prose. The
-- command is opaque here and everywhere else in dira: no framework is
-- detected, inferred or special-cased, and dira never executes it — running
-- checks is `zavet verify`'s job, gated on an explicit human invocation.
--
-- `corrected_by` is the lighter sibling of `supersedes`. Supersession replaces
-- a record wholesale, which is too heavy when ONE claim inside it turns out
-- wrong; a corrected record stays `active` and keeps its body (append-only
-- holds), and every recall path just leads with the correction.

-- One row per check, ordered by `seq` (declaration order in the frontmatter —
-- the only order the author expressed). Replaced wholesale on each upsert,
-- mirroring zavet_guards and zavet_spec_paths.
--
-- `subject_kind` + `subject_key` key a check to either a decision id or a spec
-- slug rather than splitting the table in two: the two subjects carry
-- identical columns and every consumer reads them the same way. No FK — a
-- check is captured with its record, so the parent always exists, but a repo
-- rewrite must not fail on ordering.
CREATE TABLE IF NOT EXISTS zavet_checks (
    repo         TEXT NOT NULL,     -- canonical repo ref, e.g. github.com/org/repo
    subject_kind TEXT NOT NULL,     -- 'decision' | 'spec'
    subject_key  TEXT NOT NULL,     -- canonical D-NNNN, or spec slug
    seq          INTEGER NOT NULL,  -- declaration order within the record
    label        TEXT NOT NULL,     -- human name; equals command when unlabeled
    command      TEXT NOT NULL,     -- opaque shell command; exit 0 is pass
    PRIMARY KEY (repo, subject_kind, subject_key, seq)
);

CREATE INDEX IF NOT EXISTS idx_zavet_checks_subject
    ON zavet_checks (repo, subject_kind, subject_key);

-- The errata forward pointer: the decision that corrects one claim in this
-- one. Canonical D-NNNN, deliberately unconstrained — a record may be
-- corrected by a decision captured later (or never), and a dangling pointer
-- renders as a plain id rather than failing the capture.
ALTER TABLE zavet_decisions ADD COLUMN corrected_by TEXT;
