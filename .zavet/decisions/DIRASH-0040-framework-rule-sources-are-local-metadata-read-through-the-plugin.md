---
id: DIRASH-0040
title: Framework rule sources are local metadata read through the plugin's `spec-sources --json`, never text, never synced
status: active
guards:
  - cli/core/src/zavet_specs.rs
  - cli/core/migrations/0008_zavet_rule_sources.sql
  - cli/dirad/src/zavet_sources.rs
checks:
  - an old plugin is never asked for spec-sources --json :: cargo test -p dira-core --lib zavet_specs::tests::a_plugin_without_spec_sources_json_is_never_asked_for_rule_sources
  - the rule-source table has no text column :: cargo test -p dira-core --lib store::tests::migration_0008_keeps_no_text_column
  - a failed rule resolve changes nothing :: cargo test -p dirad --test zavet_sources a_failed_or_unsupported_rule_resolve_changes_nothing
  - a vanished rule source is marked removed, never deleted :: cargo test -p dirad --test zavet_sources rule_sources_are_upserted_shown_in_status_and_marked_removed_never_deleted
  - removals only come from the default-branch checkout :: cargo test -p dirad --test zavet_sources rule_source_removals_only_happen_from_the_default_branch_checkout
origin: recorded
verified: true
---

## Decision

A linked framework's standing-rule files reach dira only as metadata rows of
the installed plugin's `zavet spec-sources --json` (`"type":"rule-source"`),
gated on `spec_sources_json` in `zavet version --json`. These are the Spec Kit
constitution, OpenSpec `config.yaml` and `project.md`, and the BMAD context.
dira stores kind, path, blob oid, version, dates, rule count, state and reason
in `zavet_rule_sources`. It never stores, reads or sends the rule text. Nothing
in that table crosses any wire until a contract version carries rule-source
metadata. This applies DIRASH-0039 to rule sources.

## Why

The rules already reach agents through the plugin's own session context. dira
only needs to know which files are in play and whether they need a human. A
local copy of the text would be a second, stale source of the rules. Sending
anything would change the knowledge wire without a contract bump (D-0001,
DIRASH-0030).

## Rejected

- Parsing the constitution or `config.yaml` in dira. That forks the adapters
  DIRASH-0039 keeps in zavet, and the two parsers would drift.
- A new `dira` command or config line for rule sources. They follow the
  `spec-source:` declarations automatically, so there is nothing to configure.
- Riding the existing knowledge sync. Its contract has no field for rule
  sources, and a full-tier text path needs its own consent wording.

## Agent directives

- Never add a text, body or content column to `zavet_rule_sources`.
- Never give `zavet_rule_sources` a `touched_seq` or feed it to the knowledge
  sync before the contract carries rule-source metadata.
- Never run `spec-sources --json` unless the plugin advertises
  `spec_sources_json`. Use the shared runner and capability cache, not a fork.
- Mark a vanished row `removed_at`; never delete one. `refresh_sources` in
  `capture.rs` (its rule-source half) removes only from a successful resolve
  on the default branch.
  An empty answer while a declared default rule file is on disk removes nothing.
- Map kind and state into `RuleSourceKind` / `RuleSourceState`. Unknown words
  go to `Other`.

## Verification

The `checks:` entries cover the capability gate, the text-free schema, fail-open,
removed-not-deleted and the default-branch rule. "Nothing crosses the wire" is
not mechanically checked here. A reviewer confirms that no code under
`cli/dirad/src/knowledge_sync.rs` or `/contract` reads `zavet_rule_sources`.
