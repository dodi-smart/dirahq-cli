//! The local store: an append-only SQLite event log plus a small meta table.
//!
//! WAL mode lets the daemon write while the CLI reads concurrently. Derived state
//! is intentionally *not* persisted in Phase 1 — reports are computed from the log
//! on demand, which keeps the source of truth singular and corruption recovery
//! trivial (just replay).

use crate::accounting;
use crate::model::{EventKind, RawEvent};
use crate::project::CapturedCommit;
use crate::sync::{ArtifactRow, TokenRow};
use crate::tokens::TokenTurn;
use crate::Error;
use dira_contract::Harness;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Handle to the local SQLite store. Cheap to clone (wraps a pool).
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if needed) the store at `path`, enable WAL, and migrate.
    pub async fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Pre-create the db file owner-only (0600) so it never briefly exists
        // world-readable between SQLite's create and our post-connect chmod — it
        // holds the keychain-fallback secret + bearer. Best-effort, unix-only;
        // SQLite then opens the already-restricted file.
        #[cfg(unix)]
        if !path.exists() {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path);
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;

        // The db holds the keychain-fallback device secret (see `identity`) and the
        // bearer token, so it must not be world-readable. Tighten to owner-only on
        // unix; a no-op elsewhere. Best-effort (a failure to chmod must not stop the
        // daemon from starting), but we tighten the WAL/SHM sidecars too since they
        // mirror the main file's pages.
        #[cfg(unix)]
        restrict_to_owner_0600(path);

        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Open an in-memory store (tests).
    pub async fn open_in_memory() -> Result<Self, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Append a normalized event. This is the only write on the capture path.
    pub async fn append(&self, ev: &RawEvent) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO events
                (id, at, session_id, harness, kind, cwd, project, identity_email, branch, tool, label, activity, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(&ev.id)
        .bind(ev.at.format(&Rfc3339).map_err(Error::time)?)
        .bind(&ev.session_id)
        .bind(enum_str(&ev.harness))
        .bind(enum_str(&ev.kind))
        .bind(&ev.cwd)
        .bind(&ev.project)
        .bind(&ev.identity_email)
        .bind(&ev.branch)
        .bind(&ev.tool)
        .bind(&ev.label)
        .bind(&ev.activity)
        .bind(&ev.note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load events with `at >= since`, ordered by time. `None` loads all.
    pub async fn events_since(
        &self,
        since: Option<OffsetDateTime>,
    ) -> Result<Vec<RawEvent>, Error> {
        let rows = match since {
            Some(ts) => {
                sqlx::query("SELECT * FROM events WHERE at >= ?1 ORDER BY at ASC")
                    .bind(ts.format(&Rfc3339).map_err(Error::time)?)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("SELECT * FROM events ORDER BY at ASC")
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        rows.iter().map(row_to_event).collect()
    }

    /// Load events whose id is strictly greater than `since` and ≤ `until`,
    /// ordered by id. Both bounds are event ids (ULIDs, lexicographically
    /// monotonic), so this is the sync window `(cursor, until]`. `since = None`
    /// means "from the beginning".
    ///
    /// Ordering by `id` (not `at`) makes the window a clean cursor range: the
    /// cursor is the last id ingested, so re-running with the same bounds yields
    /// the same rows regardless of clock skew on `at`.
    pub async fn events_between(
        &self,
        since: Option<&str>,
        until: &str,
    ) -> Result<Vec<RawEvent>, Error> {
        let rows = match since {
            Some(cursor) => {
                sqlx::query("SELECT * FROM events WHERE id > ?1 AND id <= ?2 ORDER BY id ASC")
                    .bind(cursor)
                    .bind(until)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("SELECT * FROM events WHERE id <= ?1 ORDER BY id ASC")
                    .bind(until)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        rows.iter().map(row_to_event).collect()
    }

    /// All retained events for `session_ids` with `id <= until`, per session ordered
    /// by `at`. Used by the daemon to rebuild an *ended* session's rollup from its
    /// full retained history rather than the current sync window (issue #40): the
    /// sync cursor consumes early events window-by-window, but the rows stay on disk
    /// until compaction, so a just-ended multi-window session can be re-aggregated
    /// completely. Best-effort by design — events already compacted (synced AND
    /// older than retention) are gone, so a session outliving retention yields a
    /// partial (but still superset-of-tail) history.
    ///
    /// Queried per session id (not a single `IN (...)`) so each lookup rides the
    /// `idx_events_session_at (session_id, at)` index; results are concatenated with
    /// no ordering guarantee across different sessions.
    pub async fn events_for_sessions(
        &self,
        session_ids: &[String],
        until: &str,
    ) -> Result<Vec<RawEvent>, Error> {
        let mut out = Vec::new();
        for session_id in session_ids {
            let rows = sqlx::query(
                "SELECT * FROM events WHERE session_id = ?1 AND id <= ?2 ORDER BY at ASC",
            )
            .bind(session_id)
            .bind(until)
            .fetch_all(&self.pool)
            .await?;
            for row in &rows {
                out.push(row_to_event(row)?);
            }
        }
        Ok(out)
    }

    /// Human-signal "seed" for a sync flush: the already-synced human signals (id ≤
    /// `cursor`) whose `at` falls in `[at_lo, at_hi]`, ordered by `at` ascending.
    ///
    /// These are gap ANCHORS for the next flush window — they let the interval build
    /// count a counted gap that straddles the flush boundary, which the per-window
    /// build otherwise drops (issue #21). They are read-only context, never re-emitted
    /// as events and never advance the cursor. `cursor = None` ⇒ empty.
    ///
    /// The caller passes the window's human-signal `at`-span padded by one `idle` on
    /// each side (`[min_window_at - idle, max_window_at + idle]`): a pre-cursor signal
    /// can only bound a counted (≤ idle) gap with a window signal if it lies within
    /// that band. Bounding by `at` (NOT id) is essential — the window is an id-range
    /// but a gap is an `at`-relation, and `dira log` backdates `at` below the cursor,
    /// so a relevant anchor can be a higher-id row an id-window would miss.
    pub async fn human_signal_seed(
        &self,
        cursor: Option<&str>,
        at_lo: OffsetDateTime,
        at_hi: OffsetDateTime,
    ) -> Result<Vec<RawEvent>, Error> {
        let Some(cursor) = cursor else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query(
            "SELECT * FROM events WHERE id <= ?1 AND kind IN \
             ('user_prompt','permission_decision','manual_start','manual_tick','manual_stop') \
             AND at >= ?2 AND at <= ?3 ORDER BY at ASC",
        )
        .bind(cursor)
        .bind(at_lo.format(&Rfc3339).map_err(Error::time)?)
        .bind(at_hi.format(&Rfc3339).map_err(Error::time)?)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_event).collect()
    }

    /// The largest event id in the log, or `None` when the log is empty. This is
    /// the snapshot upper bound for a sync window (`until`).
    pub async fn max_event_id(&self) -> Result<Option<String>, Error> {
        let row = sqlx::query("SELECT MAX(id) AS m FROM events")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<Option<String>, _>("m"))
    }

    /// Count events with id strictly greater than `cursor` (the un-synced
    /// backlog). `cursor = None` counts the whole log.
    pub async fn count_events_after(&self, cursor: Option<&str>) -> Result<u64, Error> {
        let row = match cursor {
            Some(c) => {
                sqlx::query("SELECT COUNT(*) AS n FROM events WHERE id > ?1")
                    .bind(c)
                    .fetch_one(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("SELECT COUNT(*) AS n FROM events")
                    .fetch_one(&self.pool)
                    .await?
            }
        };
        Ok(row.get::<i64, _>("n") as u64)
    }

    /// Load per-row token usage in the time window `(since_at, until_at]`, ordered
    /// by time. Unlike [`Self::token_totals_since`] (which sums), this yields one
    /// [`TokenRow`] per stored turn so the batch builder can emit individual
    /// `TokenUsage` records.
    ///
    /// Token rows aren't ULID-ordered against events, so the caller bounds them by
    /// the `at`-range of the event sync-window rather than by the event-id cursor.
    /// A slightly over-inclusive lower bound is harmless: the cloud dedups
    /// `token_usage` by id, so a row that ships in two batches is a no-op the
    /// second time.
    pub async fn token_usage_between(
        &self,
        since_at: Option<&str>,
        until_at: &str,
    ) -> Result<Vec<TokenRow>, Error> {
        let rows = match since_at {
            Some(start) => {
                sqlx::query(
                    "SELECT id, session_id, project, model, input, output, \
                     cache_read, cache_create, est_cost, at \
                     FROM token_usage WHERE at > ?1 AND at <= ?2 ORDER BY at ASC",
                )
                .bind(start)
                .bind(until_at)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, session_id, project, model, input, output, \
                     cache_read, cache_create, est_cost, at \
                     FROM token_usage WHERE at <= ?1 ORDER BY at ASC",
                )
                .bind(until_at)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(row_to_token_row).collect()
    }

    /// Read a meta value by key.
    pub async fn meta_get(&self, key: &str) -> Result<Option<String>, Error> {
        let row = sqlx::query("SELECT value FROM meta WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Upsert a meta value.
    pub async fn meta_set(&self, key: &str, value: &str) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the per-repo zavet override: `Some(true)` = forced on, `Some(false)`
    /// = forced off, `None` = follow the global `modules.zavet` knob. Keyed in
    /// `meta` as `zavet_override:<canonical repo>`.
    pub async fn zavet_override_get(&self, repo: &str) -> Result<Option<bool>, Error> {
        Ok(self
            .meta_get(&format!("zavet_override:{repo}"))
            .await?
            .map(|v| v == "on"))
    }

    /// Set (`Some`) or clear (`None`) the per-repo zavet override.
    pub async fn zavet_override_set(&self, repo: &str, on: Option<bool>) -> Result<(), Error> {
        let key = format!("zavet_override:{repo}");
        match on {
            Some(v) => self.meta_set(&key, if v { "on" } else { "off" }).await,
            None => {
                sqlx::query("DELETE FROM meta WHERE key = ?1")
                    .bind(&key)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }
        }
    }

    /// Wipe all captured statistics — the whole event log and token usage — for a
    /// clean slate, returning `(events_deleted, tokens_deleted)`. Runs in a single
    /// transaction so the two tables are never left half-cleared.
    ///
    /// Device identity (`device_id`, `device_pubkey_b64`, `device_secret_b64`) and
    /// the `bearer` token in `meta` are deliberately **kept** — only stats are
    /// nuked, so the device stays linked. The sync cursor is reset (cleared) so
    /// the next sync run starts from an empty log rather than skipping the (now
    /// gone) backlog.
    pub async fn nuke(&self) -> Result<(u64, u64), Error> {
        let mut tx = self.pool.begin().await?;
        let events = sqlx::query("DELETE FROM events")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let tokens = sqlx::query("DELETE FROM token_usage")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // Captured commits + their per-repo watermarks are stats too — wipe them so
        // a clean slate re-backfills from the current HEAD rather than re-shipping.
        sqlx::query("DELETE FROM artifacts")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM repo_baseline")
            .execute(&mut *tx)
            .await?;
        // Compacted daily rollups are derived stats too — wipe them so a nuked
        // slate reports from an empty history rather than retaining old summaries.
        sqlx::query("DELETE FROM session_rollup_daily")
            .execute(&mut *tx)
            .await?;
        // Per-session token offsets (Phase 1c capture watermarks, meta keys
        // `token_offset:<session_id>`) are stats too — clear them so a nuked
        // slate re-captures token usage from the start of each session's
        // transcript rather than skipping past the (now gone) baseline.
        sqlx::query("DELETE FROM meta WHERE key LIKE 'token_offset:%'")
            .execute(&mut *tx)
            .await?;
        // Reset the sync cursors in the same transaction so they can't point past
        // the wiped tables. We blank them rather than delete the keys to keep reads
        // simple.
        for key in [
            crate::sync::META_SYNC_CURSOR,
            crate::sync::META_ARTIFACTS_CURSOR,
            crate::sync::META_LAST_EPOCH,
            crate::sync::META_CLOUD_WATERMARK,
            crate::sync::knowledge::META_KNOWLEDGE_DECISION_CURSOR,
            crate::sync::knowledge::META_KNOWLEDGE_SPEC_CURSOR,
            crate::sync::knowledge::META_KNOWLEDGE_TRAILER_CURSOR,
            crate::sync::knowledge::META_KNOWLEDGE_GUARD_CURSOR,
        ] {
            sqlx::query(
                "INSERT INTO meta (key, value) VALUES (?1, '')
                 ON CONFLICT(key) DO UPDATE SET value = ''",
            )
            .bind(key)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok((events, tokens))
    }

    /// Upsert one assistant turn's token usage. Idempotent by transcript uuid, so
    /// re-parsing a transcript on every turn-boundary hook never double-counts.
    pub async fn upsert_token_usage(
        &self,
        turn: &TokenTurn,
        session_id: &str,
        project: Option<&str>,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO token_usage
                (id, at, session_id, project, model, input, output, cache_read, cache_create, est_cost)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(&turn.id)
        .bind(&turn.at)
        .bind(session_id)
        .bind(project)
        .bind(&turn.model)
        .bind(turn.input as i64)
        .bind(turn.output as i64)
        .bind(turn.cache_read as i64)
        .bind(turn.cache_create as i64)
        .bind(turn.est_cost_usd())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record one captured commit. Idempotent by `sha`, so re-walking `git log`
    /// (overlapping ranges, restarts) never duplicates a commit. Returns whether a
    /// new row was inserted.
    ///
    /// `signals` are the session-level squash-resilient anchoring signals (computed
    /// once over the *cumulative* diff for the whole capture poll, so every commit
    /// recorded in that poll shares them). The `Vec` fields are stored as JSON text
    /// — metadata only (hashes and paths), never a diff or file content. `None`
    /// signals (merge HEAD, detached HEAD, no upstream, git failure) store NULL.
    pub async fn record_commit(
        &self,
        commit: &CapturedCommit,
        repo: Option<&str>,
        git_ref: Option<&str>,
        source_session: Option<&str>,
        signals: Option<&crate::project::SessionSignals>,
    ) -> Result<bool, Error> {
        // Encode the cumulative path/blob sets as JSON text for single-column
        // storage; the batch builder decodes them back to the wire shape.
        let touched_json = signals
            .and_then(|s| s.touched_paths.as_ref())
            .and_then(|p| {
                serde_json::to_string(p)
                    .map_err(|e| tracing::warn!("encode touched_paths failed: {e}"))
                    .ok()
            });
        let blobs_json = signals.and_then(|s| s.blobs.as_ref()).and_then(|b| {
            // Store the wire shape ({path, blob}) so reads map straight to BlobRef.
            let refs: Vec<dira_contract::BlobRef> = b
                .iter()
                .map(|tb| dira_contract::BlobRef {
                    path: tb.path.clone(),
                    blob: tb.blob.clone(),
                })
                .collect();
            serde_json::to_string(&refs)
                .map_err(|e| tracing::warn!("encode blobs failed: {e}"))
                .ok()
        });
        let session_change_id = signals.and_then(|s| s.change_id.as_deref());

        let res = sqlx::query(
            "INSERT INTO artifacts
                (sha, repo, git_ref, kind, authored_at, author_email, author_name, source_session, message, additions, deletions, patch_id, session_change_id, touched_paths, blobs)
             VALUES (?1, ?2, ?3, 'commit', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(sha) DO NOTHING",
        )
        .bind(&commit.sha)
        .bind(repo)
        .bind(git_ref)
        .bind(&commit.authored_at)
        .bind(&commit.author_email)
        .bind(&commit.author_name)
        .bind(source_session)
        .bind(&commit.message)
        .bind(commit.additions as i64)
        .bind(commit.deletions as i64)
        .bind(&commit.patch_id)
        .bind(session_change_id)
        .bind(touched_json)
        .bind(blobs_json)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Read a repo's captured HEAD watermark, or `None` if never seen.
    pub async fn repo_baseline_get(&self, repo: &str) -> Result<Option<String>, Error> {
        let row = sqlx::query("SELECT head_sha FROM repo_baseline WHERE repo = ?1")
            .bind(repo)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("head_sha")))
    }

    /// Upsert a repo's HEAD watermark to `head_sha`.
    pub async fn repo_baseline_set(&self, repo: &str, head_sha: &str) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO repo_baseline (repo, head_sha) VALUES (?1, ?2)
             ON CONFLICT(repo) DO UPDATE SET head_sha = excluded.head_sha",
        )
        .bind(repo)
        .bind(head_sha)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load artifact rows in the rowid window `(cursor, until]`, ordered by
    /// `rowid` — the un-synced artifact backlog for a flush. `cursor = None` means
    /// "from the beginning". Bounding by a snapshot `until` (taken with
    /// [`Self::max_artifact_rowid`]) makes the window a clean cursor range, so a
    /// commit recorded mid-flush ships in the *next* window rather than being
    /// skipped past when the cursor advances.
    pub async fn unsynced_artifacts(
        &self,
        cursor: Option<i64>,
        until: i64,
    ) -> Result<Vec<ArtifactRow>, Error> {
        let rows = match cursor {
            Some(c) => {
                sqlx::query(
                    "SELECT sha, repo, git_ref, kind, authored_at, author_email, author_name, source_session, patch_id, session_change_id, touched_paths, blobs \
                     FROM artifacts \
                     WHERE rowid > ?1 AND rowid <= ?2 ORDER BY rowid ASC",
                )
                .bind(c)
                .bind(until)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT sha, repo, git_ref, kind, authored_at, author_email, author_name, source_session, patch_id, session_change_id, touched_paths, blobs \
                     FROM artifacts \
                     WHERE rowid <= ?1 ORDER BY rowid ASC",
                )
                .bind(until)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.iter().map(row_to_artifact_row).collect())
    }

    /// The largest `artifacts.rowid`, or `None` when no commits are captured. The
    /// upper bound for advancing the artifacts sync cursor after a 2xx.
    pub async fn max_artifact_rowid(&self) -> Result<Option<i64>, Error> {
        let row = sqlx::query("SELECT MAX(rowid) AS m FROM artifacts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<Option<i64>, _>("m"))
    }

    /// Sum token usage with `at >= since` (or all when `None`) for reporting.
    pub async fn token_totals_since(
        &self,
        since: Option<OffsetDateTime>,
    ) -> Result<TokenTotals, Error> {
        let row = match since {
            Some(ts) => {
                sqlx::query(
                    "SELECT COALESCE(SUM(input),0) i, COALESCE(SUM(output),0) o, \
                     COALESCE(SUM(cache_read),0) cr, COALESCE(SUM(cache_create),0) cc, \
                     COALESCE(SUM(est_cost),0.0) cost FROM token_usage WHERE at >= ?1",
                )
                .bind(ts.format(&Rfc3339).map_err(Error::time)?)
                .fetch_one(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT COALESCE(SUM(input),0) i, COALESCE(SUM(output),0) o, \
                     COALESCE(SUM(cache_read),0) cr, COALESCE(SUM(cache_create),0) cc, \
                     COALESCE(SUM(est_cost),0.0) cost FROM token_usage",
                )
                .fetch_one(&self.pool)
                .await?
            }
        };
        Ok(TokenTotals {
            input: row.get::<i64, _>("i") as u64,
            output: row.get::<i64, _>("o") as u64,
            cache_read: row.get::<i64, _>("cr") as u64,
            cache_create: row.get::<i64, _>("cc") as u64,
            est_cost_usd: row.get::<f64, _>("cost"),
        })
    }

    /// The current sync cursor (last event id confirmed-synced), or `None` if
    /// nothing has been synced yet. Compaction is gated on this so un-synced rows
    /// are never pruned.
    pub async fn sync_cursor(&self) -> Result<Option<String>, Error> {
        Ok(self
            .meta_get(crate::sync::META_SYNC_CURSOR)
            .await?
            .filter(|s| !s.is_empty()))
    }

    /// Roll up and prune raw events that are both **synced** (`id <= cursor`) and
    /// **old** (`at < cutoff`), returning the number of raw event rows deleted.
    ///
    /// Crash-safe: the summarize + delete runs in one transaction, so the log is
    /// never left with rows that were summarized-but-not-deleted (double counting)
    /// or deleted-but-not-summarized (data loss). Un-synced or recent rows are
    /// never touched — `cursor` is the last id the cloud has confirmed, and
    /// `cutoff` keeps the recent window intact for `report --today/--week`.
    ///
    /// The rollup is computed with [`crate::accounting`] — the same gap model the
    /// reports use — so the summarized totals match what `report` would have
    /// produced over those raw rows. Rollup rows accumulate (additive upsert), so
    /// compacting a day in two passes (a later batch ages out) is safe.
    ///
    /// `cursor = None` means nothing is synced yet → nothing is eligible, returns 0.
    pub async fn compact(
        &self,
        cursor: Option<&str>,
        cutoff: OffsetDateTime,
        idle: time::Duration,
    ) -> Result<u64, Error> {
        let Some(cursor) = cursor else {
            return Ok(0); // nothing synced yet — never prune un-synced data
        };
        let cutoff_str = cutoff.format(&Rfc3339).map_err(Error::time)?;

        let mut tx = self.pool.begin().await?;

        // Snapshot the eligible rows (synced AND old). Ordered by time so the
        // accounting gap model sees them in timeline order.
        let rows = sqlx::query("SELECT * FROM events WHERE id <= ?1 AND at < ?2 ORDER BY at ASC")
            .bind(cursor)
            .bind(&cutoff_str)
            .fetch_all(&mut *tx)
            .await?;
        if rows.is_empty() {
            return Ok(0);
        }
        let events: Vec<RawEvent> = rows.iter().map(row_to_event).collect::<Result<_, _>>()?;

        // Group by (day, session_id): one rollup row per UTC day a session touches.
        use std::collections::BTreeMap;
        type Key = (String, String);
        struct Acc {
            project: Option<String>,
            signals: Vec<accounting::Signal>,
            activity: Vec<OffsetDateTime>,
            prompts: i64,
        }
        let mut groups: BTreeMap<Key, Acc> = BTreeMap::new();
        for ev in &events {
            let day = ev
                .at
                .format(&time::format_description::well_known::Iso8601::DATE)
                .map_err(Error::time)?;
            let acc = groups
                .entry((day, ev.session_id.clone()))
                .or_insert_with(|| Acc {
                    project: ev.project.clone(),
                    signals: Vec::new(),
                    activity: Vec::new(),
                    prompts: 0,
                });
            if acc.project.is_none() && ev.project.is_some() {
                acc.project = ev.project.clone();
            }
            if ev.kind.is_human_signal() {
                acc.signals.push(accounting::Signal {
                    at: ev.at,
                    project: ev.project.clone(),
                });
                acc.prompts += 1;
            }
            if ev.kind.is_agent_activity() {
                acc.activity.push(ev.at);
            }
        }

        // Token totals per (day, session) for the same eligible window. Tokens
        // aren't ULID-ordered, so we bound them by the same `at < cutoff` only
        // (the cloud-sync cursor doesn't apply); recent tokens stay live.
        for ((day, session_id), acc) in &groups {
            let human = accounting::total_human_seconds(&acc.signals, idle);
            let active = accounting::active_seconds(&acc.activity, idle);
            sqlx::query(
                "INSERT INTO session_rollup_daily
                    (day, session_id, project, human_seconds, active_seconds, prompts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(day, session_id) DO UPDATE SET
                    human_seconds  = human_seconds  + excluded.human_seconds,
                    active_seconds = active_seconds + excluded.active_seconds,
                    prompts        = prompts        + excluded.prompts,
                    project        = COALESCE(session_rollup_daily.project, excluded.project)",
            )
            .bind(day)
            .bind(session_id)
            .bind(&acc.project)
            .bind(human)
            .bind(active)
            .bind(acc.prompts)
            .execute(&mut *tx)
            .await?;
        }

        // Delete the summarized raw rows in the same transaction.
        let deleted = sqlx::query("DELETE FROM events WHERE id <= ?1 AND at < ?2")
            .bind(cursor)
            .bind(&cutoff_str)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        tx.commit().await?;
        Ok(deleted)
    }

    /// Run `PRAGMA wal_checkpoint(TRUNCATE)` to fold the WAL back into the main
    /// file and reset it to zero length. Best-effort: a busy checkpoint is fine
    /// (the next one catches up). Called after compaction so deletes actually
    /// shrink the on-disk footprint rather than just growing the WAL.
    pub async fn wal_checkpoint_truncate(&self) -> Result<(), Error> {
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Reclaim free pages with `VACUUM`. Heavier than a checkpoint (rewrites the
    /// file), so the maintenance task runs it sparingly.
    pub async fn vacuum(&self) -> Result<(), Error> {
        sqlx::query("VACUUM").execute(&self.pool).await?;
        Ok(())
    }

    /// Sum compacted rollup rows whose `day >= since_day` (or all when `None`),
    /// grouped per project, for the report's historical (pre-retention) window.
    /// `since_day` is a `YYYY-MM-DD` string compared lexicographically (ISO dates
    /// sort chronologically).
    pub async fn rollup_totals_since(
        &self,
        since_day: Option<&str>,
    ) -> Result<Vec<RollupLine>, Error> {
        let rows = match since_day {
            Some(d) => {
                sqlx::query(
                    "SELECT project, \
                        COALESCE(SUM(human_seconds),0)  h, \
                        COALESCE(SUM(active_seconds),0) a \
                     FROM session_rollup_daily WHERE day >= ?1 GROUP BY project",
                )
                .bind(d)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT project, \
                        COALESCE(SUM(human_seconds),0)  h, \
                        COALESCE(SUM(active_seconds),0) a \
                     FROM session_rollup_daily GROUP BY project",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows
            .iter()
            .map(|r| RollupLine {
                project: r.get("project"),
                human_seconds: r.get::<i64, _>("h"),
                agent_wall_seconds: r.get::<i64, _>("a"),
            })
            .collect())
    }

    /// Count distinct sessions captured in the rollup for `day >= since_day`
    /// (or all). Used to keep the report's `session_count` honest after compaction.
    pub async fn rollup_session_count(&self, since_day: Option<&str>) -> Result<usize, Error> {
        let row = match since_day {
            Some(d) => {
                sqlx::query(
                    "SELECT COUNT(DISTINCT session_id) n \
                     FROM session_rollup_daily WHERE day >= ?1",
                )
                .bind(d)
                .fetch_one(&self.pool)
                .await?
            }
            None => {
                sqlx::query("SELECT COUNT(DISTINCT session_id) n FROM session_rollup_daily")
                    .fetch_one(&self.pool)
                    .await?
            }
        };
        Ok(row.get::<i64, _>("n") as usize)
    }

    // ---- zavet knowledge module (all LOCAL-ONLY in M1) ----------------------

    /// Upsert a decision record captured from `commit_sha`. First-sight fields
    /// (`first_commit`, `created_at`, `source_session`) are preserved on
    /// conflict — provenance stays with the commit that INTRODUCED the record.
    /// Guards are replaced wholesale in the same transaction so a narrowed
    /// guard set actually narrows.
    pub async fn zavet_upsert_decision(
        &self,
        repo: &str,
        cap: &ZavetDecisionCapture,
        commit_sha: &str,
        authored_at: Option<&str>,
        source_session: Option<&str>,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO zavet_decisions
                (repo, id, slug, title, status, path, supersedes, body_md,
                 first_commit, last_commit, created_at, updated_at,
                 source_session, content_hash, origin, verified, touched_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?10, ?11, ?12, ?13, ?14,
                     (SELECT COALESCE(MAX(touched_seq), 0) + 1 FROM zavet_decisions))
             ON CONFLICT(repo, id) DO UPDATE SET
                slug = excluded.slug,
                title = excluded.title,
                status = excluded.status,
                path = excluded.path,
                supersedes = excluded.supersedes,
                body_md = excluded.body_md,
                last_commit = excluded.last_commit,
                updated_at = excluded.updated_at,
                content_hash = excluded.content_hash,
                origin = excluded.origin,
                verified = excluded.verified,
                touched_seq = (SELECT COALESCE(MAX(touched_seq), 0) + 1 FROM zavet_decisions)",
        )
        .bind(repo)
        .bind(&cap.id)
        .bind(&cap.slug)
        .bind(&cap.title)
        .bind(&cap.status)
        .bind(&cap.path)
        .bind(&cap.supersedes)
        .bind(&cap.body_md)
        .bind(commit_sha)
        .bind(authored_at)
        .bind(source_session)
        .bind(&cap.content_hash)
        .bind(&cap.origin)
        .bind(cap.verified)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM zavet_guards WHERE repo = ?1 AND decision_id = ?2")
            .bind(repo)
            .bind(&cap.id)
            .execute(&mut *tx)
            .await?;
        for glob in &cap.guards {
            sqlx::query(
                "INSERT OR IGNORE INTO zavet_guards (repo, decision_id, glob) VALUES (?1, ?2, ?3)",
            )
            .bind(repo)
            .bind(&cap.id)
            .bind(glob)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Record a commit's parsed trailers, keyed `(sha, seq)` so re-walking a
    /// range (overlaps, restarts) never duplicates.
    pub async fn zavet_record_trailers(
        &self,
        repo: Option<&str>,
        sha: &str,
        trailers: &[ZavetTrailer],
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        for (seq, t) in trailers.iter().enumerate() {
            sqlx::query(
                "INSERT OR IGNORE INTO zavet_trailers (sha, repo, key, value, decision_id, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .bind(sha)
            .bind(repo)
            .bind(&t.key)
            .bind(&t.value)
            .bind(&t.decision_id)
            .bind(seq as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Store one guard event (already attributed by the caller), returning its id.
    pub async fn zavet_record_guard_event(
        &self,
        at: &str,
        repo: Option<&str>,
        decision_id: &str,
        kind: &str,
        file_path: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<String, Error> {
        let id = ulid::Ulid::new().to_string();
        sqlx::query(
            "INSERT INTO zavet_guard_events (id, at, repo, decision_id, kind, file_path, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&id)
        .bind(at)
        .bind(repo)
        .bind(decision_id)
        .bind(kind)
        .bind(file_path)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// One decision with its guards, or `None`.
    pub async fn zavet_decision_get(
        &self,
        repo: &str,
        id: &str,
    ) -> Result<Option<ZavetDecisionRow>, Error> {
        let row = sqlx::query(
            "SELECT repo, id, slug, title, status, path, supersedes, body_md,
                    first_commit, last_commit, created_at, updated_at,
                    source_session, content_hash, origin, verified
             FROM zavet_decisions WHERE repo = ?1 AND id = ?2",
        )
        .bind(repo)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let mut decision = row_to_zavet_decision(&row);
        decision.guards = self.zavet_guards_for(repo, id).await?;
        Ok(Some(decision))
    }

    /// All decisions for a repo (with guards), ordered by id. Guards come from
    /// one bulk query grouped in memory, not a per-decision lookup.
    pub async fn zavet_decisions_list(&self, repo: &str) -> Result<Vec<ZavetDecisionRow>, Error> {
        let rows = sqlx::query(
            "SELECT repo, id, slug, title, status, path, supersedes, body_md,
                    first_commit, last_commit, created_at, updated_at,
                    source_session, content_hash, origin, verified
             FROM zavet_decisions WHERE repo = ?1 ORDER BY id ASC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let guard_rows = sqlx::query(
            "SELECT decision_id, glob FROM zavet_guards WHERE repo = ?1
             ORDER BY decision_id, glob",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let mut guards: HashMap<String, Vec<String>> = HashMap::new();
        for r in &guard_rows {
            guards
                .entry(r.get("decision_id"))
                .or_default()
                .push(r.get("glob"));
        }
        Ok(rows
            .iter()
            .map(|row| {
                let mut d = row_to_zavet_decision(row);
                if let Some(g) = guards.remove(&d.id) {
                    d.guards = g;
                }
                d
            })
            .collect())
    }

    // ---- knowledge sync selection (M2) — "changed since cursor" windows ----

    /// Decisions touched after `seq`, oldest-touch first, with guards bulk-
    /// joined; each row carries its own `touched_seq` so the caller can
    /// advance the cursor to the window's high-water only after a 2xx.
    pub async fn zavet_decisions_since(
        &self,
        seq: i64,
        limit: i64,
    ) -> Result<Vec<(i64, ZavetDecisionRow)>, Error> {
        let rows = sqlx::query(
            "SELECT repo, id, slug, title, status, path, supersedes, body_md,
                    first_commit, last_commit, created_at, updated_at,
                    source_session, content_hash, origin, verified, touched_seq
             FROM zavet_decisions WHERE touched_seq > ?1
             ORDER BY touched_seq ASC LIMIT ?2",
        )
        .bind(seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        // Guards for exactly this window, one bulk query grouped in memory
        // (the window spans repos, so the key is (repo, id)).
        let guard_rows = sqlx::query(
            "SELECT g.repo, g.decision_id, g.glob
             FROM zavet_guards g
             JOIN (SELECT repo, id FROM zavet_decisions
                   WHERE touched_seq > ?1 ORDER BY touched_seq ASC LIMIT ?2) w
               ON w.repo = g.repo AND w.id = g.decision_id
             ORDER BY g.repo, g.decision_id, g.glob",
        )
        .bind(seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut guards: HashMap<(String, String), Vec<String>> = HashMap::new();
        for r in &guard_rows {
            guards
                .entry((r.get("repo"), r.get("decision_id")))
                .or_default()
                .push(r.get("glob"));
        }
        Ok(rows
            .iter()
            .map(|row| {
                let mut d = row_to_zavet_decision(row);
                if let Some(g) = guards.remove(&(d.repo.clone(), d.id.clone())) {
                    d.guards = g;
                }
                (row.get::<i64, _>("touched_seq"), d)
            })
            .collect())
    }

    /// Specs touched after `seq`, oldest-touch first, with paths + decision
    /// links bulk-joined (two child queries for the whole window, grouped in
    /// memory — the window spans repos, so the key is (repo, slug)).
    pub async fn zavet_specs_since(
        &self,
        seq: i64,
        limit: i64,
    ) -> Result<Vec<(i64, ZavetSpecRow)>, Error> {
        let rows = sqlx::query(
            "SELECT repo, slug, title, version, origin, verified, confidence, date,
                    path, body_md, content_hash,
                    first_commit, last_commit, created_at, updated_at,
                    source_session, touched_seq
             FROM zavet_specs WHERE touched_seq > ?1
             ORDER BY touched_seq ASC LIMIT ?2",
        )
        .bind(seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let path_rows = sqlx::query(
            "SELECT p.repo, p.slug, p.glob
             FROM zavet_spec_paths p
             JOIN (SELECT repo, slug FROM zavet_specs
                   WHERE touched_seq > ?1 ORDER BY touched_seq ASC LIMIT ?2) w
               ON w.repo = p.repo AND w.slug = p.slug
             ORDER BY p.repo, p.slug, p.glob",
        )
        .bind(seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut paths: HashMap<(String, String), Vec<String>> = HashMap::new();
        for r in &path_rows {
            paths
                .entry((r.get("repo"), r.get("slug")))
                .or_default()
                .push(r.get("glob"));
        }
        let link_rows = sqlx::query(
            "SELECT d.repo, d.slug, d.decision_id
             FROM zavet_spec_decisions d
             JOIN (SELECT repo, slug FROM zavet_specs
                   WHERE touched_seq > ?1 ORDER BY touched_seq ASC LIMIT ?2) w
               ON w.repo = d.repo AND w.slug = d.slug
             ORDER BY d.repo, d.slug, d.decision_id",
        )
        .bind(seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut links: HashMap<(String, String), Vec<String>> = HashMap::new();
        for r in &link_rows {
            links
                .entry((r.get("repo"), r.get("slug")))
                .or_default()
                .push(r.get("decision_id"));
        }
        Ok(rows
            .iter()
            .map(|row| {
                let mut s = row_to_zavet_spec(row);
                let key = (s.repo.clone(), s.slug.clone());
                if let Some(p) = paths.remove(&key) {
                    s.paths = p;
                }
                if let Some(d) = links.remove(&key) {
                    s.decisions = d;
                }
                (row.get::<i64, _>("touched_seq"), s)
            })
            .collect())
    }

    /// Trailer rows inserted after `rowid` (the table is insert-only, keyed
    /// `(sha, seq)` — rowid is a valid monotonic cursor), oldest first.
    pub async fn zavet_trailers_since(
        &self,
        rowid: i64,
        limit: i64,
    ) -> Result<Vec<ZavetTrailerSyncRow>, Error> {
        let rows = sqlx::query(
            "SELECT rowid, sha, repo, key, value, decision_id, seq
             FROM zavet_trailers WHERE rowid > ?1
             ORDER BY rowid ASC LIMIT ?2",
        )
        .bind(rowid)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ZavetTrailerSyncRow {
                rowid: r.get("rowid"),
                sha: r.get("sha"),
                repo: r.get("repo"),
                key: r.get("key"),
                value: r.get("value"),
                decision_id: r.get("decision_id"),
                seq: r.get("seq"),
            })
            .collect())
    }

    /// Guard events after `id` (ULIDs sort lexicographically by time), oldest
    /// first.
    pub async fn zavet_guard_events_since(
        &self,
        id: &str,
        limit: i64,
    ) -> Result<Vec<ZavetGuardEventSyncRow>, Error> {
        let rows = sqlx::query(
            "SELECT id, at, repo, decision_id, kind, file_path, session_id
             FROM zavet_guard_events WHERE id > ?1
             ORDER BY id ASC LIMIT ?2",
        )
        .bind(id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ZavetGuardEventSyncRow {
                id: r.get("id"),
                at: r.get("at"),
                repo: r.get("repo"),
                decision_id: r.get("decision_id"),
                kind: r.get("kind"),
                file_path: r.get("file_path"),
                session_id: r.get("session_id"),
            })
            .collect())
    }

    /// Distinct commit shas carrying at least one captured trailer for `repo`
    /// — the numerator input of the knowledge capture-ratio stat.
    pub async fn zavet_trailer_shas(&self, repo: &str) -> Result<Vec<String>, Error> {
        let rows = sqlx::query("SELECT DISTINCT sha FROM zavet_trailers WHERE repo = ?1")
            .bind(repo)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("sha")).collect())
    }

    /// The decision that superseded `id` (the reverse of a record's own
    /// `supersedes` link), if captured.
    pub async fn zavet_superseded_by(&self, repo: &str, id: &str) -> Result<Option<String>, Error> {
        let row = sqlx::query(
            "SELECT id FROM zavet_decisions WHERE repo = ?1 AND supersedes = ?2 LIMIT 1",
        )
        .bind(repo)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("id")))
    }

    async fn zavet_guards_for(&self, repo: &str, decision_id: &str) -> Result<Vec<String>, Error> {
        let rows = sqlx::query(
            "SELECT glob FROM zavet_guards WHERE repo = ?1 AND decision_id = ?2 ORDER BY glob",
        )
        .bind(repo)
        .bind(decision_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("glob")).collect())
    }

    /// The distinct session ids evidencing a decision: attributed guard events,
    /// plus the source sessions of commits that reference it via trailers or
    /// that introduced/last-touched the record itself. This is the session set
    /// `zavet why` prices.
    pub async fn zavet_sessions_for_decision(
        &self,
        repo: &str,
        id: &str,
    ) -> Result<Vec<String>, Error> {
        let rows = sqlx::query(
            "SELECT DISTINCT s FROM (
                SELECT session_id AS s FROM zavet_guard_events
                    WHERE repo = ?1 AND decision_id = ?2 AND session_id IS NOT NULL
                UNION
                SELECT a.source_session AS s FROM artifacts a
                    JOIN zavet_trailers t ON t.sha = a.sha
                    WHERE t.repo = ?1 AND t.decision_id = ?2
                      AND a.source_session IS NOT NULL
                UNION
                SELECT a.source_session AS s FROM artifacts a
                    WHERE a.source_session IS NOT NULL AND a.sha IN (
                        SELECT first_commit FROM zavet_decisions
                            WHERE repo = ?1 AND id = ?2 AND first_commit IS NOT NULL
                        UNION
                        SELECT last_commit FROM zavet_decisions
                            WHERE repo = ?1 AND id = ?2 AND last_commit IS NOT NULL
                    )
             ) ORDER BY s",
        )
        .bind(repo)
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("s")).collect())
    }

    /// Commits linked to a decision (trailer refs plus the record's own
    /// first/last commits), newest first. Commits never captured into
    /// `artifacts` still appear, with only their sha.
    pub async fn zavet_commits_for_decision(
        &self,
        repo: &str,
        id: &str,
    ) -> Result<Vec<ZavetCommitRef>, Error> {
        let rows = sqlx::query(
            "SELECT shas.sha AS sha, a.message AS message, a.authored_at AS authored_at,
                    a.source_session AS source_session
             FROM (
                SELECT DISTINCT sha FROM zavet_trailers
                    WHERE repo = ?1 AND decision_id = ?2
                UNION
                SELECT first_commit AS sha FROM zavet_decisions
                    WHERE repo = ?1 AND id = ?2 AND first_commit IS NOT NULL
                UNION
                SELECT last_commit AS sha FROM zavet_decisions
                    WHERE repo = ?1 AND id = ?2 AND last_commit IS NOT NULL
             ) shas
             LEFT JOIN artifacts a ON a.sha = shas.sha
             ORDER BY a.authored_at DESC, shas.sha",
        )
        .bind(repo)
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_zavet_commit_ref).collect())
    }

    /// Every trailer of a repo — `(sha, key, value, decision_id)` — for search:
    /// trailers referencing a decision boost that record, and orphan trailers
    /// (the pure micro-decisions) are searchable hits of their own.
    #[allow(clippy::type_complexity)]
    pub async fn zavet_all_trailers(
        &self,
        repo: &str,
    ) -> Result<Vec<(String, String, String, Option<String>)>, Error> {
        let rows =
            sqlx::query("SELECT sha, key, value, decision_id FROM zavet_trailers WHERE repo = ?1")
                .bind(repo)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get("sha"),
                    r.get("key"),
                    r.get("value"),
                    r.get("decision_id"),
                )
            })
            .collect())
    }

    /// The most recent trailers for a repo (rowid-descending), for the wiki's
    /// "recent knowledge" chronicle.
    pub async fn zavet_recent_trailers(
        &self,
        repo: &str,
        limit: u32,
    ) -> Result<Vec<(String, String, String)>, Error> {
        let rows = sqlx::query(
            "SELECT sha, key, value FROM zavet_trailers WHERE repo = ?1
             ORDER BY rowid DESC LIMIT ?2",
        )
        .bind(repo)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("sha"), r.get("key"), r.get("value")))
            .collect())
    }

    /// Guard-event tallies for a repo (optionally one decision): per kind,
    /// `(total, unattributed)`.
    pub async fn zavet_guard_event_stats(
        &self,
        repo: &str,
        decision_id: Option<&str>,
    ) -> Result<Vec<ZavetGuardStat>, Error> {
        let rows = match decision_id {
            Some(d) => {
                sqlx::query(
                    "SELECT kind, COUNT(*) AS total,
                            SUM(CASE WHEN session_id IS NULL THEN 1 ELSE 0 END) AS unattributed
                     FROM zavet_guard_events WHERE repo = ?1 AND decision_id = ?2
                     GROUP BY kind ORDER BY kind",
                )
                .bind(repo)
                .bind(d)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT kind, COUNT(*) AS total,
                            SUM(CASE WHEN session_id IS NULL THEN 1 ELSE 0 END) AS unattributed
                     FROM zavet_guard_events WHERE repo = ?1
                     GROUP BY kind ORDER BY kind",
                )
                .bind(repo)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows
            .iter()
            .map(|r| ZavetGuardStat {
                kind: r.get("kind"),
                total: r.get::<i64, _>("total") as u64,
                unattributed: r.get::<i64, _>("unattributed") as u64,
            })
            .collect())
    }

    /// One session's compacted-history and token contributions: the daily-rollup
    /// sums (data whose raw events may already be pruned) plus the live
    /// `token_usage` sums. Raw-event time for the recent window is computed by
    /// the caller over the event log (global human attribution can't be done
    /// per-session in SQL) and added on top — compaction deletes the raw rows
    /// it rolls up, so the two parts never double-count.
    pub async fn zavet_session_totals(
        &self,
        session_id: &str,
    ) -> Result<ZavetSessionTotals, Error> {
        let r = sqlx::query(
            "SELECT COALESCE(SUM(human_seconds), 0) AS h,
                    COALESCE(SUM(active_seconds), 0) AS a,
                    COALESCE(SUM(input_tokens), 0) AS i,
                    COALESCE(SUM(output_tokens), 0) AS o
             FROM session_rollup_daily WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        let t = sqlx::query(
            "SELECT COALESCE(SUM(input), 0) AS i, COALESCE(SUM(output), 0) AS o
             FROM token_usage WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(ZavetSessionTotals {
            rollup_human_seconds: r.get::<i64, _>("h"),
            rollup_agent_seconds: r.get::<i64, _>("a"),
            input_tokens: (r.get::<i64, _>("i") + t.get::<i64, _>("i")) as u64,
            output_tokens: (r.get::<i64, _>("o") + t.get::<i64, _>("o")) as u64,
        })
    }

    /// Upsert a living spec captured from `commit_sha`. First-sight fields
    /// (`first_commit`, `created_at`, `source_session`) are preserved on
    /// conflict — provenance stays with the commit that INTRODUCED the spec.
    /// Path globs and decision links are replaced wholesale in the same
    /// transaction (both are derived from the living document).
    pub async fn zavet_upsert_spec(
        &self,
        repo: &str,
        cap: &ZavetSpecCapture,
        commit_sha: &str,
        authored_at: Option<&str>,
        source_session: Option<&str>,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO zavet_specs
                (repo, slug, title, version, origin, verified, confidence, date,
                 path, body_md, content_hash,
                 first_commit, last_commit, created_at, updated_at, source_session,
                 touched_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?13, ?14,
                     (SELECT COALESCE(MAX(touched_seq), 0) + 1 FROM zavet_specs))
             ON CONFLICT(repo, slug) DO UPDATE SET
                title = excluded.title,
                version = excluded.version,
                origin = excluded.origin,
                verified = excluded.verified,
                confidence = excluded.confidence,
                date = excluded.date,
                path = excluded.path,
                body_md = excluded.body_md,
                content_hash = excluded.content_hash,
                last_commit = excluded.last_commit,
                updated_at = excluded.updated_at,
                touched_seq = (SELECT COALESCE(MAX(touched_seq), 0) + 1 FROM zavet_specs)",
        )
        .bind(repo)
        .bind(&cap.slug)
        .bind(&cap.title)
        .bind(cap.version)
        .bind(&cap.origin)
        .bind(cap.verified)
        .bind(&cap.confidence)
        .bind(&cap.date)
        .bind(&cap.path)
        .bind(&cap.body_md)
        .bind(&cap.content_hash)
        .bind(commit_sha)
        .bind(authored_at)
        .bind(source_session)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM zavet_spec_paths WHERE repo = ?1 AND slug = ?2")
            .bind(repo)
            .bind(&cap.slug)
            .execute(&mut *tx)
            .await?;
        for glob in &cap.paths {
            sqlx::query(
                "INSERT OR IGNORE INTO zavet_spec_paths (repo, slug, glob) VALUES (?1, ?2, ?3)",
            )
            .bind(repo)
            .bind(&cap.slug)
            .bind(glob)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM zavet_spec_decisions WHERE repo = ?1 AND slug = ?2")
            .bind(repo)
            .bind(&cap.slug)
            .execute(&mut *tx)
            .await?;
        for id in &cap.decisions {
            sqlx::query(
                "INSERT OR IGNORE INTO zavet_spec_decisions (repo, slug, decision_id)
                 VALUES (?1, ?2, ?3)",
            )
            .bind(repo)
            .bind(&cap.slug)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// One spec with its paths and decision links, or `None`.
    pub async fn zavet_spec_get(
        &self,
        repo: &str,
        slug: &str,
    ) -> Result<Option<ZavetSpecRow>, Error> {
        let row = sqlx::query(
            "SELECT repo, slug, title, version, origin, verified, confidence, date,
                    path, body_md, content_hash,
                    first_commit, last_commit, created_at, updated_at, source_session
             FROM zavet_specs WHERE repo = ?1 AND slug = ?2",
        )
        .bind(repo)
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let mut spec = row_to_zavet_spec(&row);
        let paths = sqlx::query(
            "SELECT glob FROM zavet_spec_paths WHERE repo = ?1 AND slug = ?2 ORDER BY glob",
        )
        .bind(repo)
        .bind(slug)
        .fetch_all(&self.pool)
        .await?;
        spec.paths = paths.iter().map(|r| r.get::<String, _>("glob")).collect();
        let links = sqlx::query(
            "SELECT decision_id FROM zavet_spec_decisions
             WHERE repo = ?1 AND slug = ?2 ORDER BY decision_id",
        )
        .bind(repo)
        .bind(slug)
        .fetch_all(&self.pool)
        .await?;
        spec.decisions = links
            .iter()
            .map(|r| r.get::<String, _>("decision_id"))
            .collect();
        Ok(Some(spec))
    }

    /// All specs for a repo (with paths and decision links), ordered by slug.
    /// Children come from two bulk queries grouped in memory, not per-spec
    /// lookups.
    pub async fn zavet_specs_list(&self, repo: &str) -> Result<Vec<ZavetSpecRow>, Error> {
        let rows = sqlx::query(
            "SELECT repo, slug, title, version, origin, verified, confidence, date,
                    path, body_md, content_hash,
                    first_commit, last_commit, created_at, updated_at, source_session
             FROM zavet_specs WHERE repo = ?1 ORDER BY slug ASC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let path_rows = sqlx::query(
            "SELECT slug, glob FROM zavet_spec_paths WHERE repo = ?1 ORDER BY slug, glob",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let mut paths: HashMap<String, Vec<String>> = HashMap::new();
        for r in &path_rows {
            paths.entry(r.get("slug")).or_default().push(r.get("glob"));
        }
        let link_rows = sqlx::query(
            "SELECT slug, decision_id FROM zavet_spec_decisions WHERE repo = ?1
             ORDER BY slug, decision_id",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let mut links: HashMap<String, Vec<String>> = HashMap::new();
        for r in &link_rows {
            links
                .entry(r.get("slug"))
                .or_default()
                .push(r.get("decision_id"));
        }
        Ok(rows
            .iter()
            .map(|row| {
                let mut s = row_to_zavet_spec(row);
                if let Some(p) = paths.remove(&s.slug) {
                    s.paths = p;
                }
                if let Some(d) = links.remove(&s.slug) {
                    s.decisions = d;
                }
                s
            })
            .collect())
    }

    /// The specs that link a decision — the reverse direction, for
    /// `zavet why D-NNNN`. `(slug, title)` pairs, ordered by slug.
    pub async fn zavet_specs_for_decision(
        &self,
        repo: &str,
        decision_id: &str,
    ) -> Result<Vec<(String, Option<String>)>, Error> {
        let rows = sqlx::query(
            "SELECT s.slug AS slug, s.title AS title
             FROM zavet_spec_decisions d
             JOIN zavet_specs s ON s.repo = d.repo AND s.slug = d.slug
             WHERE d.repo = ?1 AND d.decision_id = ?2
             ORDER BY s.slug",
        )
        .bind(repo)
        .bind(decision_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("slug"), r.get("title")))
            .collect())
    }

    /// Commits linked to a spec: `Spec: <slug>` trailer commits plus the
    /// spec's own first/last commits, newest first. Commits never captured
    /// into `artifacts` still appear, with only their sha.
    pub async fn zavet_commits_for_spec(
        &self,
        repo: &str,
        slug: &str,
    ) -> Result<Vec<ZavetCommitRef>, Error> {
        let rows = sqlx::query(
            "SELECT shas.sha AS sha, a.message AS message, a.authored_at AS authored_at,
                    a.source_session AS source_session
             FROM (
                SELECT DISTINCT sha FROM zavet_trailers
                    WHERE repo = ?1 AND key = 'spec' AND TRIM(value) = ?2
                UNION
                SELECT first_commit AS sha FROM zavet_specs
                    WHERE repo = ?1 AND slug = ?2 AND first_commit IS NOT NULL
                UNION
                SELECT last_commit AS sha FROM zavet_specs
                    WHERE repo = ?1 AND slug = ?2 AND last_commit IS NOT NULL
             ) shas
             LEFT JOIN artifacts a ON a.sha = shas.sha
             ORDER BY a.authored_at DESC, shas.sha",
        )
        .bind(repo)
        .bind(slug)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_zavet_commit_ref).collect())
    }

    /// The distinct session ids evidencing a spec: source sessions of its
    /// `Spec:`-trailer commits and of the commits that introduced/last touched
    /// the file. The session set `zavet why` prices for a spec.
    pub async fn zavet_sessions_for_spec(
        &self,
        repo: &str,
        slug: &str,
    ) -> Result<Vec<String>, Error> {
        let rows = sqlx::query(
            "SELECT DISTINCT s FROM (
                SELECT a.source_session AS s FROM artifacts a
                    JOIN zavet_trailers t ON t.sha = a.sha
                    WHERE t.repo = ?1 AND t.key = 'spec' AND TRIM(t.value) = ?2
                      AND a.source_session IS NOT NULL
                UNION
                SELECT a.source_session AS s FROM artifacts a
                    WHERE a.source_session IS NOT NULL AND a.sha IN (
                        SELECT first_commit FROM zavet_specs
                            WHERE repo = ?1 AND slug = ?2 AND first_commit IS NOT NULL
                        UNION
                        SELECT last_commit FROM zavet_specs
                            WHERE repo = ?1 AND slug = ?2 AND last_commit IS NOT NULL
                    )
             ) ORDER BY s",
        )
        .bind(repo)
        .bind(slug)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("s")).collect())
    }

    /// Capture-health counters for `dira zavet status`.
    pub async fn zavet_counts(&self, repo: &str) -> Result<ZavetCounts, Error> {
        let d = sqlx::query(
            "SELECT COUNT(*) AS total,
                    SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END) AS active
             FROM zavet_decisions WHERE repo = ?1",
        )
        .bind(repo)
        .fetch_one(&self.pool)
        .await?;
        let t = sqlx::query("SELECT COUNT(*) AS n FROM zavet_trailers WHERE repo = ?1")
            .bind(repo)
            .fetch_one(&self.pool)
            .await?;
        let g = sqlx::query("SELECT COUNT(*) AS n FROM zavet_guard_events WHERE repo = ?1")
            .bind(repo)
            .fetch_one(&self.pool)
            .await?;
        let s = sqlx::query("SELECT COUNT(*) AS n FROM zavet_specs WHERE repo = ?1")
            .bind(repo)
            .fetch_one(&self.pool)
            .await?;
        Ok(ZavetCounts {
            decisions_total: d.get::<i64, _>("total") as u64,
            decisions_active: d.get::<Option<i64>, _>("active").unwrap_or(0) as u64,
            trailers: t.get::<i64, _>("n") as u64,
            guard_events: g.get::<i64, _>("n") as u64,
            specs_total: s.get::<i64, _>("n") as u64,
        })
    }
}

/// Tighten `path` (and its SQLite `-wal`/`-shm` sidecars) to owner-only `0600`
/// on unix. Best-effort: a missing file or a chmod failure is ignored — this is a
/// hardening step, not a correctness gate. No-op on non-unix targets.
#[cfg(unix)]
fn restrict_to_owner_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    for p in [
        path.to_path_buf(),
        with_suffix(path, "-wal"),
        with_suffix(path, "-shm"),
    ] {
        let _ = std::fs::set_permissions(&p, perms.clone());
    }
}

/// Append a suffix to a path's file name (e.g. `dira.db` → `dira.db-wal`).
#[cfg(unix)]
fn with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// One per-project line summed from the compacted daily rollup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollupLine {
    pub project: Option<String>,
    pub human_seconds: i64,
    pub agent_wall_seconds: i64,
}

/// Aggregate token totals over a window (for `dira report`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub est_cost_usd: f64,
}

/// A decision record as parsed from a `.zavet/decisions/*.md` blob at capture
/// time — the input to [`Store::zavet_upsert_decision`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetDecisionCapture {
    pub id: String,
    pub slug: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    /// Repo-relative file path.
    pub path: String,
    pub supersedes: Option<String>,
    /// Full record body — local by default; crosses the wire only at the
    /// knowledge channel's consent-gated full tier (M2, `[sync] knowledge =
    /// "full"` + workspace opt-in).
    pub body_md: Option<String>,
    pub guards: Vec<String>,
    /// Git blob sha of the file at the capturing commit.
    pub content_hash: Option<String>,
    /// `recorded` | `reverse-engineered` (frontmatter `origin`).
    pub origin: Option<String>,
    /// Frontmatter `verified`; reverse-engineered records start `false`.
    pub verified: Option<bool>,
}

/// A living spec as parsed from a `.zavet/specs/*.md` blob at capture time —
/// the input to [`Store::zavet_upsert_spec`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetSpecCapture {
    /// Filename stem — the spec's identity.
    pub slug: String,
    pub title: Option<String>,
    /// Frontmatter `version` (bumped on regeneration); parse defaults to 1.
    pub version: i64,
    /// `designed` | `session` | `reverse-engineered`; parse defaults to the
    /// most skeptical (`reverse-engineered`).
    pub origin: String,
    /// `true` only after a human confirms the spec matches the code.
    pub verified: Option<bool>,
    /// `low` | `med` | `high`; parse defaults to `low`.
    pub confidence: String,
    /// Frontmatter `date` (spec's own last-touched claim), verbatim.
    pub date: Option<String>,
    /// Git pathspecs the spec covers — the staleness domain.
    pub paths: Vec<String>,
    /// Linked decision ids: frontmatter `decisions:` ∪ body D-refs, canonical.
    pub decisions: Vec<String>,
    /// Repo-relative file path.
    pub path: String,
    /// Full spec body — local by default; never rides `AttestationBatch`
    /// (D-0001) and crosses only the knowledge channel's consent-gated full
    /// tier (M2).
    pub body_md: Option<String>,
    /// Git blob sha of the file at the capturing commit.
    pub content_hash: Option<String>,
}

/// A stored decision row plus its guard globs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetDecisionRow {
    pub repo: String,
    pub id: String,
    pub slug: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub path: String,
    pub supersedes: Option<String>,
    pub body_md: Option<String>,
    pub first_commit: Option<String>,
    pub last_commit: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub source_session: Option<String>,
    pub content_hash: Option<String>,
    pub origin: Option<String>,
    pub verified: Option<bool>,
    pub guards: Vec<String>,
}

/// One parsed commit trailer (`key` normalized to lowercase; `decision_id` is
/// the first `D-NNNN` referenced in the value, if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZavetTrailer {
    pub key: String,
    pub value: String,
    pub decision_id: Option<String>,
}

/// A stored trailer row with its rowid cursor, as selected for knowledge sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetTrailerSyncRow {
    pub rowid: i64,
    pub sha: String,
    pub repo: Option<String>,
    pub key: String,
    pub value: Option<String>,
    pub decision_id: Option<String>,
    pub seq: i64,
}

/// A stored guard event as selected for knowledge sync (id is the cursor).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetGuardEventSyncRow {
    pub id: String,
    pub at: String,
    pub repo: Option<String>,
    pub decision_id: String,
    pub kind: String,
    pub file_path: Option<String>,
    pub session_id: Option<String>,
}

/// A commit linked to a decision (trailer ref or the record's own commits).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetCommitRef {
    pub sha: String,
    pub message: Option<String>,
    pub authored_at: Option<String>,
    pub source_session: Option<String>,
}

/// Per-kind guard-event tallies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZavetGuardStat {
    pub kind: String,
    pub total: u64,
    pub unattributed: u64,
}

/// One session's compacted + token contributions (see
/// [`Store::zavet_session_totals`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZavetSessionTotals {
    pub rollup_human_seconds: i64,
    pub rollup_agent_seconds: i64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Capture-health counters for a repo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZavetCounts {
    pub decisions_total: u64,
    pub decisions_active: u64,
    pub trailers: u64,
    pub guard_events: u64,
    pub specs_total: u64,
}

/// A stored spec row plus its path globs and decision links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZavetSpecRow {
    pub repo: String,
    pub slug: String,
    pub title: Option<String>,
    pub version: i64,
    pub origin: Option<String>,
    pub verified: Option<bool>,
    pub confidence: Option<String>,
    pub date: Option<String>,
    pub path: String,
    pub body_md: Option<String>,
    pub content_hash: Option<String>,
    pub first_commit: Option<String>,
    pub last_commit: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub source_session: Option<String>,
    pub paths: Vec<String>,
    pub decisions: Vec<String>,
}

fn row_to_zavet_spec(row: &sqlx::sqlite::SqliteRow) -> ZavetSpecRow {
    ZavetSpecRow {
        repo: row.get("repo"),
        slug: row.get("slug"),
        title: row.get("title"),
        version: row.get("version"),
        origin: row.get("origin"),
        verified: row.get("verified"),
        confidence: row.get("confidence"),
        date: row.get("date"),
        path: row.get("path"),
        body_md: row.get("body_md"),
        content_hash: row.get("content_hash"),
        first_commit: row.get("first_commit"),
        last_commit: row.get("last_commit"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        source_session: row.get("source_session"),
        paths: Vec::new(),
        decisions: Vec::new(),
    }
}

fn row_to_zavet_commit_ref(row: &sqlx::sqlite::SqliteRow) -> ZavetCommitRef {
    ZavetCommitRef {
        sha: row.get("sha"),
        message: row.get("message"),
        authored_at: row.get("authored_at"),
        source_session: row.get("source_session"),
    }
}

fn row_to_zavet_decision(row: &sqlx::sqlite::SqliteRow) -> ZavetDecisionRow {
    ZavetDecisionRow {
        repo: row.get("repo"),
        id: row.get("id"),
        slug: row.get("slug"),
        title: row.get("title"),
        status: row.get("status"),
        path: row.get("path"),
        supersedes: row.get("supersedes"),
        body_md: row.get("body_md"),
        first_commit: row.get("first_commit"),
        last_commit: row.get("last_commit"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        source_session: row.get("source_session"),
        content_hash: row.get("content_hash"),
        origin: row.get("origin"),
        verified: row.get("verified"),
        guards: Vec::new(),
    }
}

/// Serialize a unit enum to its snake_case wire string for storage.
fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|val| val.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, Error> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| Error::Decode(e.to_string()))
}

/// Map a row from `unsynced_artifacts` to an [`ArtifactRow`], decoding the JSON
/// text columns (`touched_paths`, `blobs`) back to their typed shape. A malformed
/// or NULL JSON column decodes to `None` (best-effort: a decode failure drops just
/// that signal, never the whole row).
fn row_to_artifact_row(row: &sqlx::sqlite::SqliteRow) -> ArtifactRow {
    let touched_paths = row
        .get::<Option<String>, _>("touched_paths")
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
    let blobs = row
        .get::<Option<String>, _>("blobs")
        .and_then(|s| serde_json::from_str::<Vec<dira_contract::BlobRef>>(&s).ok());
    ArtifactRow {
        sha: row.get("sha"),
        repo: row.get("repo"),
        git_ref: row.get("git_ref"),
        kind: row.get("kind"),
        authored_at: row.get("authored_at"),
        author_email: row.get("author_email"),
        author_name: row.get("author_name"),
        source_session: row.get("source_session"),
        patch_id: row.get("patch_id"),
        session_change_id: row.get("session_change_id"),
        touched_paths,
        blobs,
    }
}

fn row_to_token_row(row: &sqlx::sqlite::SqliteRow) -> Result<TokenRow, Error> {
    Ok(TokenRow {
        id: row.get("id"),
        session_id: row.get("session_id"),
        project: row.get("project"),
        model: row.get("model"),
        input: row.get::<i64, _>("input") as u64,
        output: row.get::<i64, _>("output") as u64,
        cache_read: row.get::<i64, _>("cache_read") as u64,
        cache_create: row.get::<i64, _>("cache_create") as u64,
        est_cost_usd: row.get::<Option<f64>, _>("est_cost"),
        at: row.get("at"),
    })
}

fn row_to_event(row: &sqlx::sqlite::SqliteRow) -> Result<RawEvent, Error> {
    let at: String = row.get("at");
    let harness: String = row.get("harness");
    let kind: String = row.get("kind");
    Ok(RawEvent {
        id: row.get("id"),
        at: OffsetDateTime::parse(&at, &Rfc3339).map_err(Error::parse)?,
        session_id: row.get("session_id"),
        harness: parse_enum::<Harness>(&harness)?,
        kind: parse_enum::<EventKind>(&kind)?,
        cwd: row.get("cwd"),
        project: row.get("project"),
        identity_email: row.get("identity_email"),
        branch: row.get("branch"),
        tool: row.get("tool"),
        label: row.get("label"),
        activity: row.get("activity"),
        note: row.get("note"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventKind;

    fn ev(id: &str, kind: EventKind) -> RawEvent {
        RawEvent {
            id: id.to_string(),
            at: OffsetDateTime::UNIX_EPOCH,
            session_id: "s1".to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some("github.com/acme/api".to_string()),
            identity_email: Some("dev@acme.com".to_string()),
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    #[tokio::test]
    async fn append_and_read_roundtrips() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .append(&ev("01A", EventKind::SessionStart))
            .await
            .unwrap();
        store
            .append(&ev("01B", EventKind::UserPrompt))
            .await
            .unwrap();
        let all = store.events_since(None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, EventKind::SessionStart);
        assert_eq!(all[1].project.as_deref(), Some("github.com/acme/api"));
    }

    #[tokio::test]
    async fn human_signal_seed_selects_by_at_within_idle() {
        let store = Store::open_in_memory().await.unwrap();
        let mk = |id: &str, secs: i64, kind: EventKind| RawEvent {
            id: id.to_string(),
            at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs),
            session_id: "s1".to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some("p".to_string()),
            identity_email: Some("d@e.com".to_string()),
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        };
        // id-order != at-order: 01C has a HIGHER id but an OLDER `at` than 01B.
        store
            .append(&mk("01A", 0, EventKind::UserPrompt))
            .await
            .unwrap();
        store
            .append(&mk("01B", 1250, EventKind::UserPrompt))
            .await
            .unwrap(); // newest at
        store
            .append(&mk("01C", 1000, EventKind::UserPrompt))
            .await
            .unwrap(); // higher id, older at
        store
            .append(&mk("01D", 1240, EventKind::PreTool))
            .await
            .unwrap(); // non-human → ignored

        let at = |secs: i64| OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs);
        let seed = store
            .human_signal_seed(Some("01D"), at(900), at(1300))
            .await
            .unwrap();

        // Human signals with id ≤ 01D and at ∈ [900,1300]: 01C@1000 and 01B@1250;
        // 01A@0 is out of band, PreTool 01D is non-human. Returned at-ASC — so the
        // HIGHER-id 01C@1000 sorts before the LOWER-id 01B@1250 (at, not id, decides).
        let ids: Vec<&str> = seed.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["01C", "01B"]);

        // A far-backdated anchor (01A@0) is reachable when the band reaches it — this
        // is the `dira log` case where the relevant pre-cursor signal is > idle from
        // the cursor's newest signal but ≤ idle from a backdated window signal.
        let far = store
            .human_signal_seed(Some("01D"), at(-100), at(1300))
            .await
            .unwrap();
        assert_eq!(
            far.first().unwrap().id,
            "01A",
            "far anchor included when in band"
        );

        // No cursor ⇒ no seed.
        assert!(store
            .human_signal_seed(None, at(0), at(9999))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn events_for_sessions_filters_ids_and_bounds_by_until() {
        let store = Store::open_in_memory().await.unwrap();
        let mk = |id: &str, session: &str, secs: i64| RawEvent {
            id: id.to_string(),
            at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs),
            session_id: session.to_string(),
            harness: Harness::ClaudeCode,
            kind: EventKind::UserPrompt,
            cwd: None,
            project: Some("p".to_string()),
            identity_email: Some("d@e.com".to_string()),
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        };
        // Session s1 spans multiple ids; s2 is a different session entirely.
        store.append(&mk("01A", "s1", 0)).await.unwrap();
        store.append(&mk("01C", "s1", 200)).await.unwrap(); // out of at-order vs 01B
        store.append(&mk("01B", "s1", 100)).await.unwrap();
        store.append(&mk("01D", "s1", 300)).await.unwrap(); // above `until` — excluded
        store.append(&mk("01X", "s2", 50)).await.unwrap(); // other session — excluded

        let got = store
            .events_for_sessions(&["s1".to_string()], "01C")
            .await
            .unwrap();

        // Only s1's events with id <= "01C" come back, ordered by `at` ascending.
        let ids: Vec<&str> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["01A", "01B", "01C"]);
        assert!(got.iter().all(|e| e.session_id == "s1"));
    }

    #[tokio::test]
    async fn meta_roundtrips() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.meta_get("device_id").await.unwrap(), None);
        store.meta_set("device_id", "01DEVICE").await.unwrap();
        assert_eq!(
            store.meta_get("device_id").await.unwrap().as_deref(),
            Some("01DEVICE")
        );
        store.meta_set("device_id", "01OTHER").await.unwrap();
        assert_eq!(
            store.meta_get("device_id").await.unwrap().as_deref(),
            Some("01OTHER")
        );
    }

    #[tokio::test]
    async fn nuke_wipes_stats_but_keeps_identity() {
        let store = Store::open_in_memory().await.unwrap();
        // Two events + one token row.
        store
            .append(&ev("01A", EventKind::SessionStart))
            .await
            .unwrap();
        store
            .append(&ev("01B", EventKind::UserPrompt))
            .await
            .unwrap();
        let turn = TokenTurn {
            id: "tok1".to_string(),
            at: "2026-06-27T10:00:00Z".to_string(),
            model: "claude-opus-4-8".to_string(),
            input: 10,
            output: 20,
            cache_read: 0,
            cache_create: 0,
        };
        store
            .upsert_token_usage(&turn, "s1", Some("github.com/acme/api"))
            .await
            .unwrap();

        // Identity + a stale sync cursor that should be reset.
        store.meta_set("device_id", "01DEVICE").await.unwrap();
        store
            .meta_set(crate::sync::META_SYNC_CURSOR, "01B")
            .await
            .unwrap();

        let (events, tokens) = store.nuke().await.unwrap();
        assert_eq!(events, 2);
        assert_eq!(tokens, 1);

        // Both stats tables are empty.
        assert!(store.events_since(None).await.unwrap().is_empty());
        assert!(store
            .token_usage_between(None, "2099-01-01T00:00:00Z")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.max_event_id().await.unwrap(), None);

        // Identity is preserved; the sync cursor is cleared.
        assert_eq!(
            store.meta_get("device_id").await.unwrap().as_deref(),
            Some("01DEVICE")
        );
        assert_eq!(
            store
                .meta_get(crate::sync::META_SYNC_CURSOR)
                .await
                .unwrap()
                .as_deref(),
            Some("")
        );
    }

    #[tokio::test]
    async fn artifacts_capture_is_idempotent_and_windowed() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/acme/api";
        let c1 = CapturedCommit {
            sha: "aaa".into(),
            authored_at: Some("2026-06-27T10:00:00Z".into()),
            author_email: Some("dev@acme.com".into()),
            author_name: Some("Dev One".into()),
            message: "feat: one".into(),
            additions: 5,
            deletions: 1,
            patch_id: Some("p1".into()),
        };
        // Session-level squash-resilient signals shared across the capture poll.
        let signals = crate::project::SessionSignals {
            change_id: Some("scid-cumulative".into()),
            touched_paths: Some(vec!["src/a.rs".into(), "src/b.rs".into()]),
            blobs: Some(vec![
                crate::project::TouchedBlob {
                    path: "src/a.rs".into(),
                    blob: "blob-a".into(),
                },
                crate::project::TouchedBlob {
                    path: "src/b.rs".into(),
                    blob: "blob-b".into(),
                },
            ]),
        };
        // First insert records; a re-insert of the same sha is a no-op.
        assert!(store
            .record_commit(
                &c1,
                Some(repo),
                Some("main"),
                Some("sess-1"),
                Some(&signals)
            )
            .await
            .unwrap());
        assert!(!store
            .record_commit(
                &c1,
                Some(repo),
                Some("main"),
                Some("sess-1"),
                Some(&signals)
            )
            .await
            .unwrap());

        let until = store.max_artifact_rowid().await.unwrap().unwrap();
        let rows = store.unsynced_artifacts(None, until).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sha, "aaa");
        assert_eq!(rows[0].repo.as_deref(), Some(repo));
        assert_eq!(rows[0].git_ref.as_deref(), Some("main"));
        assert_eq!(rows[0].kind, "commit");
        // Author + observed session round-trip through the artifacts table.
        assert_eq!(rows[0].authored_at.as_deref(), Some("2026-06-27T10:00:00Z"));
        assert_eq!(rows[0].author_email.as_deref(), Some("dev@acme.com"));
        assert_eq!(rows[0].author_name.as_deref(), Some("Dev One"));
        assert_eq!(rows[0].source_session.as_deref(), Some("sess-1"));
        // Squash-resilient signals round-trip through the JSON columns.
        assert_eq!(
            rows[0].session_change_id.as_deref(),
            Some("scid-cumulative")
        );
        assert_eq!(
            rows[0].touched_paths.as_deref(),
            Some(["src/a.rs".to_string(), "src/b.rs".to_string()].as_slice())
        );
        let blobs = rows[0].blobs.as_ref().expect("blobs decoded");
        assert_eq!(blobs.len(), 2);
        assert_eq!(blobs[0].path, "src/a.rs");
        assert_eq!(blobs[0].blob, "blob-a");
        assert_eq!(blobs[1].path, "src/b.rs");

        // After advancing the cursor to `until`, the window is empty until a new
        // commit lands; then only the new one is in `(until, until2]`.
        assert!(store
            .unsynced_artifacts(Some(until), until)
            .await
            .unwrap()
            .is_empty());
        let c2 = CapturedCommit {
            sha: "bbb".into(),
            authored_at: None,
            author_email: None,
            author_name: None,
            message: "fix: two".into(),
            additions: 0,
            deletions: 0,
            patch_id: None,
        };
        // A commit with no resolvable signals stores NULLs that decode to None.
        assert!(store
            .record_commit(&c2, Some(repo), None, None, None)
            .await
            .unwrap());
        let until2 = store.max_artifact_rowid().await.unwrap().unwrap();
        let fresh = store.unsynced_artifacts(Some(until), until2).await.unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].sha, "bbb");
        assert_eq!(fresh[0].session_change_id, None);
        assert_eq!(fresh[0].touched_paths, None);
        assert_eq!(fresh[0].blobs, None);
    }

    #[tokio::test]
    async fn repo_baseline_roundtrips() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.repo_baseline_get("r").await.unwrap(), None);
        store.repo_baseline_set("r", "head1").await.unwrap();
        assert_eq!(
            store.repo_baseline_get("r").await.unwrap().as_deref(),
            Some("head1")
        );
        store.repo_baseline_set("r", "head2").await.unwrap();
        assert_eq!(
            store.repo_baseline_get("r").await.unwrap().as_deref(),
            Some("head2")
        );
    }

    fn ev_at(id: &str, session: &str, at: OffsetDateTime, kind: EventKind) -> RawEvent {
        RawEvent {
            id: id.to_string(),
            at,
            session_id: session.to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some("github.com/acme/api".to_string()),
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    #[tokio::test]
    async fn compact_summarizes_old_synced_rows_and_keeps_the_rest() {
        use crate::accounting;
        let store = Store::open_in_memory().await.unwrap();
        let idle = time::Duration::minutes(5);
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(30);
        // Old window: 20 days ago, four human signals 60s apart (id <= cursor).
        let old_base = now - time::Duration::days(20);
        let mut old_events = Vec::new();
        for i in 0..4 {
            let at = old_base + time::Duration::seconds(60 * i);
            // ULIDs sort lexicographically; "01OLD0".."01OLD3" are all < cursor.
            old_events.push(ev_at(
                &format!("01OLD{i}"),
                "s_old",
                at,
                EventKind::UserPrompt,
            ));
        }
        for e in &old_events {
            store.append(e).await.unwrap();
        }
        // Recent window: today, two prompts (NOT eligible — too new).
        let recent_a = ev_at("01REC0", "s_new", now, EventKind::UserPrompt);
        let recent_b = ev_at(
            "01REC1",
            "s_new",
            now + time::Duration::seconds(30),
            EventKind::UserPrompt,
        );
        store.append(&recent_a).await.unwrap();
        store.append(&recent_b).await.unwrap();
        // Un-synced old row: old enough by age, but past the cursor — must be kept.
        let unsynced_old = ev_at(
            "01ZZZZ", // > cursor "01OLD3"
            "s_unsynced",
            old_base + time::Duration::seconds(10),
            EventKind::UserPrompt,
        );
        store.append(&unsynced_old).await.unwrap();

        // Expected human time for the old session via the same accounting code.
        let old_signals: Vec<accounting::Signal> = old_events
            .iter()
            .map(|e| accounting::Signal {
                at: e.at,
                project: e.project.clone(),
            })
            .collect();
        let expected_human = accounting::total_human_seconds(&old_signals, idle);
        assert_eq!(expected_human, 180); // 3 gaps * 60s

        // Cursor at the last OLD id; cutoff 14 days ago.
        let cutoff = now - time::Duration::days(14);
        let deleted = store.compact(Some("01OLD3"), cutoff, idle).await.unwrap();
        assert_eq!(deleted, 4, "only the 4 old+synced rows are pruned");

        // The old rows are gone; recent + un-synced survive.
        let remaining = store.events_since(None).await.unwrap();
        let mut ids: Vec<&str> = remaining.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["01REC0", "01REC1", "01ZZZZ"]);

        // The rollup preserves the old session's totals.
        let lines = store.rollup_totals_since(None).await.unwrap();
        let total_human: i64 = lines.iter().map(|l| l.human_seconds).sum();
        assert_eq!(total_human, expected_human);
        assert_eq!(store.rollup_session_count(None).await.unwrap(), 1);

        // A second compaction with the same bounds is a no-op (idempotent).
        let again = store.compact(Some("01OLD3"), cutoff, idle).await.unwrap();
        assert_eq!(again, 0);
        let total_human2: i64 = store
            .rollup_totals_since(None)
            .await
            .unwrap()
            .iter()
            .map(|l| l.human_seconds)
            .sum();
        assert_eq!(total_human2, expected_human, "no double counting on re-run");
    }

    #[tokio::test]
    async fn compact_noop_without_sync_cursor() {
        let store = Store::open_in_memory().await.unwrap();
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(30);
        store
            .append(&ev_at(
                "01OLD0",
                "s",
                now - time::Duration::days(20),
                EventKind::UserPrompt,
            ))
            .await
            .unwrap();
        // Nothing synced → nothing eligible, even though the row is old.
        let deleted = store
            .compact(
                None,
                now - time::Duration::days(14),
                time::Duration::minutes(5),
            )
            .await
            .unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.events_since(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn events_for_sessions_after_compact_returns_remainder() {
        let store = Store::open_in_memory().await.unwrap();
        let idle = time::Duration::minutes(5);
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(30);
        // A single long-lived session with an old, synced+aged window (eligible for
        // compaction) and a recent window (too new to prune).
        let old_base = now - time::Duration::days(20);
        let mut old_events = Vec::new();
        for i in 0..4 {
            let at = old_base + time::Duration::seconds(60 * i);
            old_events.push(ev_at(&format!("01OLD{i}"), "s1", at, EventKind::UserPrompt));
        }
        for e in &old_events {
            store.append(e).await.unwrap();
        }
        let recent_a = ev_at("01REC0", "s1", now, EventKind::UserPrompt);
        let recent_b = ev_at(
            "01REC1",
            "s1",
            now + time::Duration::seconds(30),
            EventKind::UserPrompt,
        );
        store.append(&recent_a).await.unwrap();
        store.append(&recent_b).await.unwrap();

        // Cursor covers the old rows only; cutoff prunes the old window.
        let cutoff = now - time::Duration::days(14);
        let deleted = store.compact(Some("01OLD3"), cutoff, idle).await.unwrap();
        assert_eq!(deleted, 4, "the 4 old+synced rows are pruned");

        // Best-effort semantics: the compacted rows are gone, so a rebuild over the
        // full retained history only sees the surviving (recent) tail — a partial
        // but still superset-of-tail history, not an error.
        let got = store
            .events_for_sessions(&["s1".to_string()], "01REC1")
            .await
            .unwrap();
        let ids: Vec<&str> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["01REC0", "01REC1"]);
    }

    #[tokio::test]
    async fn nuke_clears_token_offsets_and_rollups() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .append(&ev("01A", EventKind::SessionStart))
            .await
            .unwrap();
        // A per-session token offset (Phase 1c) and a rollup row.
        store.meta_set("token_offset:s1", "1234").await.unwrap();
        store.meta_set("token_offset:s2", "5678").await.unwrap();
        store
            .compact(
                Some("01A"),
                OffsetDateTime::UNIX_EPOCH + time::Duration::days(1),
                time::Duration::minutes(5),
            )
            .await
            .unwrap();

        store.nuke().await.unwrap();

        // Both token offsets are cleared so a nuked slate re-captures tokens.
        assert_eq!(store.meta_get("token_offset:s1").await.unwrap(), None);
        assert_eq!(store.meta_get("token_offset:s2").await.unwrap(), None);
        // Rollups are wiped too.
        assert!(store.rollup_totals_since(None).await.unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_creates_db_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dira-perm-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("dira.db");
        let _ = std::fs::remove_file(&db);

        let store = Store::open(&db).await.unwrap();
        // Force a write so the WAL sidecar exists too, then re-tighten via a fresh
        // open path is unnecessary — the file is already 0600 from `open`.
        store.meta_set("k", "v").await.unwrap();

        let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "db file must be owner-only (rw-------)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn decision(id: &str, guards: &[&str]) -> ZavetDecisionCapture {
        ZavetDecisionCapture {
            id: id.to_string(),
            slug: Some("poll".into()),
            title: Some("Poll git".into()),
            status: Some("active".into()),
            path: format!(".zavet/decisions/{id}-poll.md"),
            supersedes: None,
            body_md: Some("## Decision\npoll".into()),
            guards: guards.iter().map(|s| s.to_string()).collect(),
            content_hash: Some("blob1".into()),
            origin: Some("recorded".into()),
            verified: Some(true),
        }
    }

    #[tokio::test]
    async fn zavet_upsert_preserves_first_sight_and_replaces_guards() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/o/r";
        store
            .zavet_upsert_decision(
                repo,
                &decision("D-0001", &["a/**", "b.rs"]),
                "sha1",
                Some("2026-01-01T00:00:00Z"),
                Some("s1"),
            )
            .await
            .unwrap();

        // Second capture from a later commit, different session, narrowed guards.
        let mut cap = decision("D-0001", &["a/**"]);
        cap.status = Some("superseded".into());
        cap.content_hash = Some("blob2".into());
        store
            .zavet_upsert_decision(repo, &cap, "sha2", Some("2026-02-01T00:00:00Z"), Some("s2"))
            .await
            .unwrap();

        let d = store
            .zavet_decision_get(repo, "D-0001")
            .await
            .unwrap()
            .unwrap();
        // First-sight provenance survives the update…
        assert_eq!(d.first_commit.as_deref(), Some("sha1"));
        assert_eq!(d.created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(d.source_session.as_deref(), Some("s1"));
        // …while the living fields track the latest commit.
        assert_eq!(d.last_commit.as_deref(), Some("sha2"));
        assert_eq!(d.status.as_deref(), Some("superseded"));
        assert_eq!(d.content_hash.as_deref(), Some("blob2"));
        // Guards were replaced wholesale, not accumulated.
        assert_eq!(d.guards, vec!["a/**".to_string()]);
    }

    #[tokio::test]
    async fn knowledge_since_queries_follow_touched_seq() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/o/r";
        store
            .zavet_upsert_decision(repo, &decision("D-0001", &["a/**"]), "sha1", None, None)
            .await
            .unwrap();
        store
            .zavet_upsert_decision(repo, &decision("D-0002", &["b/**"]), "sha2", None, None)
            .await
            .unwrap();

        // Fresh cursor: both rows, oldest-touch first, guards joined.
        let win = store.zavet_decisions_since(0, 10).await.unwrap();
        assert_eq!(win.len(), 2);
        assert_eq!(win[0].1.id, "D-0001");
        assert_eq!(win[0].1.guards, vec!["a/**".to_string()]);
        let high = win.last().unwrap().0;

        // Cursor at high-water: empty window.
        assert!(store
            .zavet_decisions_since(high, 10)
            .await
            .unwrap()
            .is_empty());

        // Re-upserting an old row bumps it past the cursor — an edit after a
        // rebase/backfill can never be silently skipped.
        store
            .zavet_upsert_decision(repo, &decision("D-0001", &["a/**"]), "sha3", None, None)
            .await
            .unwrap();
        let win = store.zavet_decisions_since(high, 10).await.unwrap();
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].1.id, "D-0001");
        assert!(win[0].0 > high);

        // Trailers ride a rowid cursor; guard events ride their ULID.
        store
            .zavet_record_trailers(
                Some(repo),
                "shaT",
                &[ZavetTrailer {
                    key: "refs".into(),
                    value: "D-0001".into(),
                    decision_id: Some("D-0001".into()),
                }],
            )
            .await
            .unwrap();
        let trailers = store.zavet_trailers_since(0, 10).await.unwrap();
        assert_eq!(trailers.len(), 1);
        assert_eq!(trailers[0].sha, "shaT");
        assert!(store
            .zavet_trailers_since(trailers[0].rowid, 10)
            .await
            .unwrap()
            .is_empty());

        let ev_id = store
            .zavet_record_guard_event(
                "2026-07-17T10:00:00Z",
                Some(repo),
                "D-0001",
                "guard_blocked",
                None,
                None,
            )
            .await
            .unwrap();
        let events = store.zavet_guard_events_since("", 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, ev_id);
        assert!(store
            .zavet_guard_events_since(&ev_id, 10)
            .await
            .unwrap()
            .is_empty());
    }

    fn spec(slug: &str, paths: &[&str], decisions: &[&str]) -> ZavetSpecCapture {
        ZavetSpecCapture {
            slug: slug.to_string(),
            title: Some("Capture pipeline".into()),
            version: 1,
            origin: "session".into(),
            verified: Some(false),
            confidence: "high".into(),
            date: Some("2026-07-16".into()),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            decisions: decisions.iter().map(|s| s.to_string()).collect(),
            path: format!(".zavet/specs/{slug}.md"),
            body_md: Some("## Overview\nsweeps".into()),
            content_hash: Some("blobS1".into()),
        }
    }

    #[tokio::test]
    async fn zavet_spec_upsert_preserves_first_sight_and_replaces_children() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/o/r";
        store
            .zavet_upsert_spec(
                repo,
                &spec(
                    "capture-pipeline",
                    &["cli/dirad/**", "cli/core/src/zavet.rs"],
                    &["D-0001"],
                ),
                "shaS1",
                Some("2026-07-01T00:00:00Z"),
                Some("s1"),
            )
            .await
            .unwrap();

        // Second capture: later commit, other session, narrowed paths, relinked.
        let mut cap = spec("capture-pipeline", &["cli/dirad/**"], &["D-0002"]);
        cap.version = 2;
        cap.confidence = "med".into();
        cap.content_hash = Some("blobS2".into());
        store
            .zavet_upsert_spec(
                repo,
                &cap,
                "shaS2",
                Some("2026-07-16T00:00:00Z"),
                Some("s2"),
            )
            .await
            .unwrap();

        let s = store
            .zavet_spec_get(repo, "capture-pipeline")
            .await
            .unwrap()
            .unwrap();
        // First-sight provenance survives the update…
        assert_eq!(s.first_commit.as_deref(), Some("shaS1"));
        assert_eq!(s.created_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(s.source_session.as_deref(), Some("s1"));
        // …while the living fields track the latest commit.
        assert_eq!(s.last_commit.as_deref(), Some("shaS2"));
        assert_eq!(s.version, 2);
        assert_eq!(s.confidence.as_deref(), Some("med"));
        assert_eq!(s.content_hash.as_deref(), Some("blobS2"));
        // Paths and decision links were replaced wholesale, not accumulated.
        assert_eq!(s.paths, vec!["cli/dirad/**".to_string()]);
        assert_eq!(s.decisions, vec!["D-0002".to_string()]);

        // Upserting the same capture again is a no-op (idempotent).
        store
            .zavet_upsert_spec(
                repo,
                &cap,
                "shaS2",
                Some("2026-07-16T00:00:00Z"),
                Some("s2"),
            )
            .await
            .unwrap();
        let again = store
            .zavet_spec_get(repo, "capture-pipeline")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again, s);

        let counts = store.zavet_counts(repo).await.unwrap();
        assert_eq!(counts.specs_total, 1);
    }

    #[tokio::test]
    async fn zavet_specs_list_bulk_loads_children_and_reverse_lookup_joins() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/o/r";
        store
            .zavet_upsert_spec(
                repo,
                &spec("auth-flow", &["src/auth/**"], &["D-0001"]),
                "sA",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .zavet_upsert_spec(
                repo,
                &spec("capture-pipeline", &["cli/**"], &["D-0001", "D-0002"]),
                "sB",
                None,
                None,
            )
            .await
            .unwrap();

        let list = store.zavet_specs_list(repo).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].slug, "auth-flow");
        assert_eq!(list[0].paths, vec!["src/auth/**".to_string()]);
        assert_eq!(list[0].decisions, vec!["D-0001".to_string()]);
        assert_eq!(list[1].slug, "capture-pipeline");
        assert_eq!(
            list[1].decisions,
            vec!["D-0001".to_string(), "D-0002".to_string()]
        );

        // Reverse: which specs cover D-0001 / D-0002?
        let covering = store
            .zavet_specs_for_decision(repo, "D-0001")
            .await
            .unwrap();
        assert_eq!(
            covering.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            vec!["auth-flow", "capture-pipeline"]
        );
        let covering = store
            .zavet_specs_for_decision(repo, "D-0002")
            .await
            .unwrap();
        assert_eq!(covering.len(), 1);
        assert_eq!(covering[0].0, "capture-pipeline");
        assert_eq!(covering[0].1.as_deref(), Some("Capture pipeline"));
        // Another repo sees nothing.
        assert!(store
            .zavet_specs_for_decision("github.com/o/other", "D-0001")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn zavet_trailers_are_idempotent_by_sha_and_seq() {
        let store = Store::open_in_memory().await.unwrap();
        let ts = vec![
            ZavetTrailer {
                key: "why".into(),
                value: "polling beats watching".into(),
                decision_id: None,
            },
            ZavetTrailer {
                key: "refs".into(),
                value: "D-0001".into(),
                decision_id: Some("D-0001".into()),
            },
        ];
        let repo = Some("github.com/o/r");
        store
            .zavet_record_trailers(repo, "shaX", &ts)
            .await
            .unwrap();
        // A re-walk of the same range records the same trailers again — no dupes.
        store
            .zavet_record_trailers(repo, "shaX", &ts)
            .await
            .unwrap();
        let counts = store.zavet_counts("github.com/o/r").await.unwrap();
        assert_eq!(counts.trailers, 2);
    }

    #[tokio::test]
    async fn zavet_session_set_unions_guard_events_trailers_and_record_commits() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/o/r";

        // The record itself was introduced by sha1 (session s1).
        store
            .zavet_upsert_decision(repo, &decision("D-0001", &[]), "sha1", None, Some("s1"))
            .await
            .unwrap();
        // A captured commit references it via trailer (session s2)…
        let c = CapturedCommit {
            sha: "sha2".into(),
            authored_at: Some("2026-01-02T00:00:00Z".into()),
            author_email: None,
            author_name: None,
            message: "feat: x".into(),
            additions: 1,
            deletions: 0,
            patch_id: None,
        };
        store
            .record_commit(&c, Some(repo), None, Some("s2"), None)
            .await
            .unwrap();
        store
            .zavet_record_trailers(
                Some(repo),
                "sha2",
                &[ZavetTrailer {
                    key: "refs".into(),
                    value: "D-0001".into(),
                    decision_id: Some("D-0001".into()),
                }],
            )
            .await
            .unwrap();
        // …an attributed guard event adds s3, and an unattributed one adds nothing.
        store
            .zavet_record_guard_event(
                "2026-01-03T00:00:00Z",
                Some(repo),
                "D-0001",
                "guard_shown",
                Some("a.rs"),
                Some("s3"),
            )
            .await
            .unwrap();
        store
            .zavet_record_guard_event(
                "2026-01-03T00:01:00Z",
                Some(repo),
                "D-0001",
                "guard_blocked",
                None,
                None,
            )
            .await
            .unwrap();

        // sha1 (first_commit) was never captured into artifacts, so it cannot
        // contribute a session — but s2 (trailer) and s3 (guard event) do.
        let sessions = store
            .zavet_sessions_for_decision(repo, "D-0001")
            .await
            .unwrap();
        assert_eq!(sessions, vec!["s2".to_string(), "s3".to_string()]);

        // Once sha1 IS captured, its source session joins the set.
        let c1 = CapturedCommit {
            sha: "sha1".into(),
            authored_at: Some("2026-01-01T00:00:00Z".into()),
            author_email: None,
            author_name: None,
            message: "docs: decide".into(),
            additions: 1,
            deletions: 0,
            patch_id: None,
        };
        store
            .record_commit(&c1, Some(repo), None, Some("s1"), None)
            .await
            .unwrap();
        let sessions = store
            .zavet_sessions_for_decision(repo, "D-0001")
            .await
            .unwrap();
        assert_eq!(
            sessions,
            vec!["s1".to_string(), "s2".to_string(), "s3".to_string()]
        );

        // Stats keep the honest unattributed tally per kind.
        let stats = store
            .zavet_guard_event_stats(repo, Some("D-0001"))
            .await
            .unwrap();
        let blocked = stats.iter().find(|s| s.kind == "guard_blocked").unwrap();
        assert_eq!((blocked.total, blocked.unattributed), (1, 1));
        let shown = stats.iter().find(|s| s.kind == "guard_shown").unwrap();
        assert_eq!((shown.total, shown.unattributed), (1, 0));

        // Linked commits list both shas, captured or not.
        let commits = store
            .zavet_commits_for_decision(repo, "D-0001")
            .await
            .unwrap();
        let shas: Vec<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
        assert!(shas.contains(&"sha1") && shas.contains(&"sha2"));
    }

    #[tokio::test]
    async fn zavet_override_round_trips_and_clears() {
        let store = Store::open_in_memory().await.unwrap();
        let repo = "github.com/o/r";
        assert_eq!(store.zavet_override_get(repo).await.unwrap(), None);
        store.zavet_override_set(repo, Some(true)).await.unwrap();
        assert_eq!(store.zavet_override_get(repo).await.unwrap(), Some(true));
        store.zavet_override_set(repo, Some(false)).await.unwrap();
        assert_eq!(store.zavet_override_get(repo).await.unwrap(), Some(false));
        store.zavet_override_set(repo, None).await.unwrap();
        assert_eq!(store.zavet_override_get(repo).await.unwrap(), None);
    }

    #[tokio::test]
    async fn token_totals_empty_store_is_zero() {
        // Regression: `COALESCE(SUM(est_cost),0)` returned an integer 0 on an
        // empty table, which sqlx could not decode as f64 (panicking on the
        // `cost` get). `0.0` keeps the column a real.
        let store = Store::open_in_memory().await.unwrap();
        let totals = store.token_totals_since(None).await.unwrap();
        assert_eq!(totals.input, 0);
        assert_eq!(totals.output, 0);
        assert_eq!(totals.cache_read, 0);
        assert_eq!(totals.cache_create, 0);
        assert_eq!(totals.est_cost_usd, 0.0);
    }
}
