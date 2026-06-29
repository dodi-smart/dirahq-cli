---
name: new-migration
description: >-
  Create or amend a database migration for the Dira local capture store. Use when
  adding/altering a table, column, index, or constraint in the daemon's SQLite store. This
  repo uses sqlx migrations under cli/core/migrations. (The cloud's Drizzle migrations live
  in the separate dirahq-cloud repo.)
disable-model-invocation: true
---

# new-migration

The CLI daemon persists captured data in a local SQLite store via **sqlx** migrations under
`cli/core/migrations`. (Cloud dashboard data uses Drizzle and lives in the separate
`dirahq-cloud` repo — not here.)

## The edit-vs-create rule (from the user's standing instruction)

**Prefer editing the existing migration over adding a new one** — *unless* it is already
deployed to `main`.

- On a PR / feature branch, if the latest migration has **not** yet merged to `main`, edit
  it in place.
- If it is already on `main`, create a **new** migration instead. Never rewrite a migration
  that has shipped.
- When unsure whether it shipped: `git log origin/main -- <migration-path>`.

## sqlx (local capture store)

- Migrations are plain SQL, applied by `sqlx::migrate!("./migrations")` in
  `cli/core/src/store.rs` at daemon startup. Naming is zero-padded sequential:
  `0001_init.sql`, `0002_token_usage.sql`, → next is `0003_<snake_name>.sql`.
- SQLite dialect. No live DB is needed to build (runtime queries only).
- After writing: `just test` (the store tests run migrations against a temp DB) and, if you
  added a column the daemon reads/writes, update the corresponding `Store` methods.

## Verify before finishing

- `just test`.
- Never check `.env`/secrets into a migration. SQL files under `cli/core/migrations` are
  intentionally un-ignored in `.gitignore` — make sure the new file is actually tracked
  (`git status`).
