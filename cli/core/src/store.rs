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
