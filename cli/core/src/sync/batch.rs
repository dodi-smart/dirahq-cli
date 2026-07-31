//! Pure batch builder: turn a window of raw events + token rows into a signed-
//! ready [`AttestationBatch`].
//!
//! This is the producer side of the wire contract. It deliberately reuses the
//! same de-duplicated human-time math as local reporting
//! ([`crate::accounting::counted_gaps`]) so the billable seconds the cloud sees
//! match the seconds `dira report` shows — never a second source of truth.
//!
//! ## Partial windows
//! A sync window is a slice of the event log, so a session can straddle two
//! windows. Two rules keep that honest:
//! - **Sessions** are emitted only once they have *ended* (`SessionEnd` /
//!   `ManualStop`). The cloud does `onConflictDoNothing` on `sessions.id`, so a
//!   half-captured session emitted early could never be corrected. Waiting for
//!   the end means the rollup is final the first (and only) time it ships.
//! - **Intervals** carry unique ULIDs and are *always* emitted. They are the
//!   billable truth; a session split across windows just contributes intervals
//!   from each window, which sum correctly because the gaps never overlap.

use crate::model::{EventKind, RawEvent};
use crate::tokens;
use dira_contract::{
    ArtifactKind, ArtifactRef, AttestationBatch, Harness, Interval, SessionKind, SessionRollup,
    TokenUsage,
};
use std::collections::BTreeMap;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// One stored token-usage row, as read from `token_usage`. Mirrors the columns
/// 1:1 (the summing `TokenTotals` collapses them, so we need a per-row form).
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub id: String,
    pub session_id: String,
    pub project: Option<String>,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub est_cost_usd: Option<f64>,
    /// RFC 3339 timestamp string, stored verbatim.
    pub at: String,
}

/// One stored artifact row, as read from the local `artifacts` table. Mirrors the
/// captured columns the batch builder needs to emit an [`ArtifactRef`].
#[derive(Debug, Clone)]
pub struct ArtifactRow {
    /// Commit SHA; doubles as the wire `ArtifactRef.id` for idempotent ingest.
    pub sha: String,
    /// Canonical repo ref, e.g. `github.com/acme/api`.
    pub repo: Option<String>,
    /// Branch ref at capture time.
    pub git_ref: Option<String>,
    /// Stored wire kind string (`commit` / `pull_request`).
    pub kind: String,
    /// RFC 3339 author date of the commit.
    pub authored_at: Option<String>,
    /// Commit author email — shipped on the wire for cloud-side anchoring.
    pub author_email: Option<String>,
    /// Commit author name — local-only (PII); never shipped on the wire.
    pub author_name: Option<String>,
    /// The session the daemon observed for this repo at capture time, if unambiguous.
    pub source_session: Option<String>,
    /// `git patch-id --stable` — stable change-id shipped for rebase-resilient
    /// anchoring (the cloud finds the rewritten commit and confirms this id).
    pub patch_id: Option<String>,
    /// Squash-resilient session signal: `git patch-id --stable` over the session's
    /// *cumulative* diff. Equals a squash-merge commit's patch-id (base unmoved).
    pub session_change_id: Option<String>,
    /// Squash-resilient session signal: repo-relative paths the session changed.
    pub touched_paths: Option<Vec<String>>,
    /// Squash-resilient session signal: per touched path, the git post-image blob
    /// SHA at the session tip.
    pub blobs: Option<Vec<dira_contract::BlobRef>>,
}

/// A long-running, not-yet-ended session the daemon wants to ship a *partial*
/// rollup for (Phase 6c), described from the live registry rather than the event
/// window (the sync window only holds new events, not the whole session).
///
/// The emitted `SessionRollup` carries `ended_at: None` and `agent_wall_seconds`
/// from the session's rolling [`crate::accounting::active_seconds`] counter, with
/// a *stable* `session_id`.
///
/// ## Cloud idempotency contract (IMPORTANT)
/// A partial rollup re-ships the **same** `session_id` on every sync until the
/// session finally ends, each time with a larger `agent_wall_seconds`. The cloud
/// MUST therefore **UPSERT session rows by `session_id` (latest wins)** for
/// ongoing sessions, NOT `onConflictDoNothing` — otherwise the first (smallest)
/// partial would stick and later growth (and the final ended rollup) would be
/// dropped. The ended path stays final-once: when the daemon observes the end it
/// emits a rollup with `ended_at: Some(..)` via [`build_sessions`], and that
/// terminal write is the authoritative last UPSERT.
#[derive(Debug, Clone)]
pub struct PartialSession {
    pub session_id: String,
    pub harness: Harness,
    pub kind: SessionKind,
    pub repo_canonical: Option<String>,
    pub identity_email: Option<String>,
    pub started_at: OffsetDateTime,
    /// Rolling idle-trimmed active seconds for the whole session so far.
    pub active_seconds: u64,
    /// Human prompts observed so far, if tracked (else omitted on the wire).
    pub prompts: Option<u64>,
    pub branch: Option<String>,
    /// Free-text description for a manual session, if set (`--note`/comment).
    pub note: Option<String>,
    /// Operational tag for a manual session, if set (`--label`).
    pub label: Option<String>,
}

/// Build an attestation batch from a window of events, token rows, and artifacts.
///
/// `idle` is the human-time idle threshold; `now` stamps `generated_at` and is
/// the synthetic end for sessions that have agent activity but no recorded end
/// within the window (only matters for the wall-clock of *ended* sessions).
pub fn build_batch(
    events: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    device_id: &str,
    idle: Duration,
    agent: crate::accounting::AgentPolicy,
    now: OffsetDateTime,
) -> AttestationBatch {
    build_batch_with_partials(
        events,
        token_rows,
        artifact_rows,
        &[],
        device_id,
        idle,
        agent,
        now,
    )
}

/// Like [`build_batch`], but also emits a *partial* [`SessionRollup`]
/// (`ended_at: None`) for each [`PartialSession`] — a long-running session that
/// has not ended yet (Phase 6c). See [`PartialSession`] for the cloud UPSERT
/// contract this relies on.
///
/// A partial is suppressed when an *ended* rollup for the same `session_id` is
/// already in this batch (the session ended in this very window): the final
/// rollup supersedes the partial, so shipping both would be redundant.
#[allow(clippy::too_many_arguments)]
pub fn build_batch_with_partials(
    events: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    partials: &[PartialSession],
    device_id: &str,
    idle: Duration,
    agent: crate::accounting::AgentPolicy,
    now: OffsetDateTime,
) -> AttestationBatch {
    assemble_batch(
        events,
        &[], // single-window build: no pre-cursor seed
        &[], // no history — build_batch* has no store to fetch it from; the
        // daemon's chunked path (build_chunked_batches) is what feeds history
        // in production (issue #40).
        token_rows,
        artifact_rows,
        partials,
        device_id,
        idle,
        agent,
        now,
    )
}

/// Assemble one [`AttestationBatch`] from a window of events/tokens/artifacts/
/// partials, deriving the `batch_id` internally from the events, artifacts, AND the
/// built interval decomposition (see [`batch_id_for_chunk`]). Shared by the single-
/// window builder ([`build_batch_with_partials`]) and the chunked builder
/// ([`build_chunked_batches`]) so the fact-derivation is identical either way.
#[allow(clippy::too_many_arguments)]
fn assemble_batch(
    events: &[RawEvent],
    // Gap anchors from before the window's cursor (see `build_intervals_seeded`).
    // Empty for a single-window build and for every chunk after the first.
    seed: &[RawEvent],
    // Full retained history for sessions ending in THIS `events` slice, so their
    // terminal rollup aggregates over the whole session, not just the tail window
    // (issue #40). Feeds ONLY `build_sessions` below — intervals and the batch id
    // derive from `events` (+ `seed`) alone, so they stay byte-identical whether or
    // not a session's rollup ends up history-aggregated.
    history: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    partials: &[PartialSession],
    device_id: &str,
    idle: Duration,
    agent: crate::accounting::AgentPolicy,
    now: OffsetDateTime,
) -> AttestationBatch {
    let intervals = build_intervals_seeded(events, idle, seed);
    // Compute the batch id AFTER the intervals exist, folding the interval
    // decomposition in (see `batch_id_for_chunk`): a re-derivation that changes an
    // interval's (start,end) split of the same minutes yields a DIFFERENT batch id so
    // the cloud re-unpacks it, while a byte-identical rebuild stays stable (issue #21).
    let batch_id = batch_id_for_chunk(events, artifact_rows, &intervals);
    let mut sessions = build_sessions(events, history, agent);
    // Drop degenerate sessions before shipping: no engaged time AND no agent
    // activity is empty noise (a bare SessionStart with nothing after it), so it
    // never reaches the cloud. Manual/agent sessions with real time are kept.
    let mut engaged: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for iv in &intervals {
        *engaged.entry(iv.source_session.as_str()).or_insert(0) += iv.human_seconds;
    }
    sessions.retain(|s| {
        s.agent_wall_seconds > 0 || engaged.get(s.session_id.as_str()).copied().unwrap_or(0) > 0
    });

    // Phase 6c: append partial rollups for long-running un-ended sessions. Skip any
    // whose session already ended in *this* window (its final rollup, above,
    // supersedes the partial), and any with no active time yet (nothing to settle).
    let ended_ids: std::collections::HashSet<String> =
        sessions.iter().map(|s| s.session_id.clone()).collect();
    for p in partials {
        if p.active_seconds == 0 || ended_ids.contains(&p.session_id) {
            continue;
        }
        sessions.push(SessionRollup {
            session_id: p.session_id.clone(),
            harness: p.harness,
            kind: p.kind,
            repo_canonical: p.repo_canonical.clone(),
            identity_email: p.identity_email.clone().unwrap_or_default(),
            started_at: fmt(p.started_at),
            // The defining mark of a partial rollup: the session is still open.
            ended_at: None,
            agent_wall_seconds: p.active_seconds,
            prompts: p.prompts,
            branch: p.branch.clone(),
            note: p.note.clone(),
            label: p.label.clone(),
        });
    }

    let token_usage = build_token_usage(token_rows);
    let artifacts = build_artifacts(artifact_rows);

    AttestationBatch {
        batch_id,
        device_id: device_id.to_string(),
        generated_at: now.format(&Rfc3339).unwrap_or_default(),
        intervals,
        sessions,
        token_usage,
        artifacts,
    }
}

/// Default soft cap on events per sync sub-batch (see [`build_chunked_batches`]).
pub const CHUNK_EVENTS: usize = 1000;

/// Hard cap on artifacts per sync sub-batch (see [`build_chunked_batches`]).
///
/// Events were capped from the start; artifacts were not, and they are the fat
/// rows — each carries `touched_paths` and `blobs`, so a single row's size is
/// unbounded in practice. `dira device resync` rewinds the artifact cursor to the
/// beginning, so the very first flush after one tried to ship the entire backlog
/// in one request and was rejected `413 Payload Too Large` (issue #71). The limit
/// is the platform's request-body ceiling, not anything the cloud chooses, so the
/// only place it can be respected is here, at construction.
///
/// 100 is deliberately well under any plausible ceiling even for rows with large
/// path/blob lists: the cost of a low cap is one extra HTTP round-trip inside a
/// flush that is already looping over chunks, which is far cheaper than a wedged
/// resync.
pub const CHUNK_ARTIFACTS: usize = 100;

/// Hard cap on token-usage rows per sync sub-batch (see [`build_chunked_batches`]).
///
/// Tokens were nominally "bounded by the event window", which stops being a bound
/// on exactly the trigger that produced issue #71: `dira device resync` rewinds
/// the event cursor as well as the artifact one, so the window becomes the entire
/// log and every token row it spans rides one request. Rows are much smaller than
/// artifacts — ids, counts, a model name, no path or blob lists — hence the higher
/// cap. The cloud dedups `token_usage` by id, so spreading them is free.
pub const CHUNK_TOKENS: usize = 1000;

/// One deterministically-chunked sub-batch, paired with the bookkeeping the daemon
/// needs to advance its cursors after the cloud acks it.
pub struct ChunkBatch {
    pub batch: AttestationBatch,
    /// The highest event id this chunk covers — advance `META_SYNC_CURSOR` here on a
    /// 2xx. `None` for an artifact/partial-only chunk (no events), where only the
    /// artifact cursor moves.
    pub cursor_event_id: Option<String>,
    /// The final chunk carries the artifacts + partial rollups, so its ack also
    /// advances the artifact cursor and marks partials sent.
    pub is_last: bool,
}

/// Split a window of events into deterministic, size-bounded sub-batches **only at
/// idle breaks** — points where consecutive events are more than `idle` apart.
///
/// This is the key to lossless chunking: a *counted* human gap is, by definition,
/// ≤ `idle` between two human signals, so it can never contain a > `idle` break.
/// Cutting only at > `idle` breaks therefore never splits a billable interval —
/// each counted gap stays wholly within one chunk — while still bounding request
/// size. A chunk grows to at least `CHUNK_EVENTS` events, then closes at the next
/// idle break; a long continuous burst (no break) stays one chunk (it is genuinely
/// indivisible without loss). The result is a pure function of `(events, idle)`, so
/// a re-send reproduces identical chunk boundaries.
fn chunk_ranges(events: &[RawEvent], idle: Duration, min_chunk: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if events.is_empty() {
        return ranges;
    }
    let min_chunk = min_chunk.max(1);
    let mut start = 0usize;
    for i in 1..events.len() {
        let big_break = events[i].at - events[i - 1].at > idle;
        if i - start >= min_chunk && big_break {
            ranges.push((start, i - 1));
            start = i;
        }
    }
    ranges.push((start, events.len() - 1));
    ranges
}

/// Build deterministic, capped sub-batches for a window. Each chunk derives its own
/// intervals/sessions from its events (lossless thanks to [`chunk_ranges`]); the
/// final chunk additionally carries the token rows and partial rollups (the cloud
/// dedups tokens by id and sessions by id), so their cursors advance only once the
/// whole window has drained.
///
/// **Artifacts are spread across the chunks**, at most [`CHUNK_ARTIFACTS`] per
/// chunk, rather than all riding the final one (issue #71). Spreading rather than
/// capping the store read is what keeps the artifact cursor honest: it still
/// advances exactly once, on the true final chunk, to the same snapshot bound the
/// caller read — so there is never a window in which the cursor has moved past a
/// row that was never sent. A backlog larger than the event chunks can carry gets
/// extra artifact-only chunks appended after the last event chunk, so the whole
/// backlog still drains within ONE flush instead of one cap-sized slice per
/// backstop tick.
#[allow(clippy::too_many_arguments)]
pub fn build_chunked_batches(
    events: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    partials: &[PartialSession],
    device_id: &str,
    idle: Duration,
    agent: crate::accounting::AgentPolicy,
    now: OffsetDateTime,
    // Human signals at/before the flush cursor within one idle-window of the
    // boundary. Used only to recover the boundary-straddling gap in the FIRST chunk;
    // later chunks open on `> idle` breaks by construction, so nothing straddles them.
    seed: &[RawEvent],
    // Full retained history for sessions ENDING somewhere in this flush's window
    // (issue #40). Passed to EVERY chunk's `assemble_batch` call: each chunk derives
    // its own `ended_ids` from its own event slice, so history only actually merges
    // into whichever chunk holds that session's SessionEnd/ManualStop — passing it to
    // the others is a no-op for them, not a duplication risk.
    history: &[RawEvent],
) -> Vec<ChunkBatch> {
    // Size-bounded slices of the two collections that are NOT bounded by the
    // event window. `chunks` on an empty slice yields nothing, so a flush without
    // artifacts or tokens behaves exactly as before.
    let art_slices: Vec<&[ArtifactRow]> = artifact_rows.chunks(CHUNK_ARTIFACTS).collect();
    let tok_slices: Vec<&[TokenRow]> = token_rows.chunks(CHUNK_TOKENS).collect();

    // `chunk_ranges` returns empty for empty events, so an artifact/partial-only
    // flush falls out of the same loop: every per-chunk expression below degrades
    // to the no-events form. `.max(1)` keeps the one batch a partials-only flush
    // needs when there is neither an event nor an artifact to chunk.
    let ranges = chunk_ranges(events, idle, CHUNK_EVENTS);
    // Enough chunks to carry both the events and the artifacts: the artifact
    // backlog can outlast the event chunks (a resync rewinds only the artifact
    // cursor), and the surplus rides artifact-only chunks appended after them.
    let n = ranges
        .len()
        .max(art_slices.len())
        .max(tok_slices.len())
        .max(1);
    (0..n)
        .map(|i| {
            let is_last = i == n - 1;
            let chunk: &[RawEvent] = ranges.get(i).map_or(&[], |&(s, e)| &events[s..=e]);
            // Only the first chunk abuts the flush cursor, so only it seeds — and
            // only if it actually holds events; a seed exists to anchor a gap
            // against them.
            let chunk_seed: &[RawEvent] = if i == 0 && !chunk.is_empty() {
                seed
            } else {
                &[]
            };
            let arts: &[ArtifactRow] = art_slices.get(i).copied().unwrap_or(&[]);
            // History only ever merges into the chunk holding a session's
            // terminal event, so an event-free chunk would scan all of it to
            // produce nothing.
            let hist: &[RawEvent] = if chunk.is_empty() { &[] } else { history };
            let toks: &[TokenRow] = tok_slices.get(i).copied().unwrap_or(&[]);
            // Partials alone still ride the final chunk: they are a snapshot of
            // the live registry, bounded by the number of open sessions, and
            // their sent-watermark advances with `is_last`.
            let parts: &[PartialSession] = if is_last { partials } else { &[] };
            let batch = assemble_batch(
                chunk, chunk_seed, hist, toks, arts, parts, device_id, idle, agent, now,
            );
            ChunkBatch {
                batch,
                // `None` on an artifact-only chunk past the last event range —
                // there is no event high-water for it to advance.
                cursor_event_id: chunk.last().map(|ev| ev.id.clone()),
                is_last,
            }
        })
        .collect()
}

/// Map stored artifact rows to the contract's [`ArtifactRef`]. The wire `id` is
/// the sha, so the cloud's `onConflictDoNothing` on the artifact id makes a
/// re-shipped commit a no-op (idempotent anchoring).
fn build_artifacts(rows: &[ArtifactRow]) -> Vec<ArtifactRef> {
    rows.iter()
        .map(|r| ArtifactRef {
            id: r.sha.clone(),
            repo_canonical: r.repo.clone(),
            kind: parse_artifact_kind(&r.kind),
            git_ref: r.git_ref.clone(),
            sha: r.sha.clone(),
            authored_at: r.authored_at.clone(),
            author_email: r.author_email.clone(),
            source_session: r.source_session.clone(),
            patch_id: r.patch_id.clone(),
            session_change_id: r.session_change_id.clone(),
            touched_paths: r.touched_paths.clone(),
            blobs: r.blobs.clone(),
            // author_name is intentionally local-only (PII) — not on the wire.
        })
        .collect()
}

/// Decode a stored artifact-kind string, defaulting to `Commit`.
fn parse_artifact_kind(kind: &str) -> ArtifactKind {
    match kind {
        "pull_request" => ArtifactKind::PullRequest,
        _ => ArtifactKind::Commit,
    }
}

/// De-duplicated, idle-trimmed intervals — ONE per counted gap (no coalescing), so
/// the set of `(source_session, start, end)` intervals is a pure function of the
/// data, identical under every windowing (issue #21).
/// Convenience wrapper for the empty-seed (single-window) case. Production always
/// goes through `assemble_batch` → `build_intervals_seeded`; this is used by tests.
#[cfg(test)]
fn build_intervals(events: &[RawEvent], idle: Duration) -> Vec<Interval> {
    build_intervals_seeded(events, idle, &[])
}

/// Per-gap intervals over `events`, using `seed` — the human signals at/before the
/// flush cursor that neighbour the window's `at`-span — as gap anchors.
///
/// A gap `[a, b)` is emitted unless BOTH endpoints are seed signals (a gap wholly
/// within the seed was already emitted by an earlier window; re-counting it would
/// double-bill). Any gap touching a window signal on either side is derived here. This
/// recovers the ordinary boundary gap `[seed, first_window)` that the per-window build
/// dropped, AND the backdated case where a `dira log` signal lands (by `at`) between
/// two already-synced seed signals and re-splits their gap (issue #21). The `seed`
/// must be selected by `at` (not event id): the window is an id-range but gaps are an
/// `at`-relation, so a relevant earlier-by-`at` signal can be a higher-id row that
/// `dira log` backdated (see `store::human_signal_seed`).
fn build_intervals_seeded(events: &[RawEvent], idle: Duration, seed: &[RawEvent]) -> Vec<Interval> {
    struct Sig {
        at: OffsetDateTime,
        project: Option<String>,
        session_id: String,
        /// Did this signal come from the window (`events`) rather than the seed?
        from_window: bool,
    }
    // Collect human signals (seed first, then window), tagged with origin, then sort
    // by time exactly as `accounting::counted_gaps` does (stable on ties).
    let mut tagged: Vec<Sig> = seed
        .iter()
        .map(|e| (e, false))
        .chain(events.iter().map(|e| (e, true)))
        .filter(|(e, _)| e.kind.is_human_signal())
        .map(|(e, from_window)| Sig {
            at: e.at,
            project: e.project.clone(),
            session_id: e.session_id.clone(),
            from_window,
        })
        .collect();
    tagged.sort_by_key(|s| s.at);

    // Fallback email + first activity per session. Resolved over events ∪ seed so a
    // seed-opened boundary gap keeps its opener session's identity even when that
    // session has no event in this window. (Common case — empty seed — avoids the copy.)
    let (emails, activities) = if seed.is_empty() {
        (session_emails(events), session_activities(events))
    } else {
        let ctx: Vec<RawEvent> = seed.iter().chain(events.iter()).cloned().collect();
        (session_emails(&ctx), session_activities(&ctx))
    };

    let mut out: Vec<Interval> = Vec::new();
    for pair in tagged.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // Emit a gap unless BOTH endpoints are seed signals: a gap wholly within the
        // seed was already emitted by an earlier window. A gap touching a window signal
        // on EITHER side is (re-)derived now. That recovers the ordinary boundary gap
        // [seed, window) AND the backdated case: `dira log` stamps a past `at`, so a
        // window signal can land (by `at`) BETWEEN two already-synced seed signals and
        // re-split the gap they used to bound — the new gaps each touch the window
        // signal, so both are emitted and the cloud's overlap rebuild replaces the
        // stale wider interval (issue #21).
        if !a.from_window && !b.from_window {
            continue;
        }
        let delta = b.at - a.at;
        if delta <= Duration::ZERO || delta > idle {
            continue;
        }
        let human = (b.at - a.at).whole_seconds().max(0) as u64;

        // One interval per counted gap — NO coalescing. Each interval's endpoints are
        // a pair of globally-adjacent human signals, so the (session, start, end)
        // content key is intrinsic to the data and identical under every windowing
        // (incremental per-flush == one-shot). This is what makes the cloud's
        // content-key dedup sound — a coalesced run's endpoints depend on where the
        // flush boundary fell, so two windowings of the same minutes would decompose
        // differently, never dedup, and overlap → double-bill (issue #21).
        out.push(Interval {
            // Placeholder — the id is content-derived in a final pass below.
            id: String::new(),
            repo_canonical: a.project.clone(),
            identity_email: emails
                .get(&a.session_id)
                .cloned()
                .flatten()
                .unwrap_or_default(),
            started_at: fmt(a.at),
            ended_at: fmt(b.at),
            human_seconds: human,
            // Per-session activity classification (surfaced for manual sessions;
            // None for agent work that carries no activity).
            activity: activities.get(&a.session_id).cloned().flatten(),
            source_session: a.session_id.clone(),
        });
    }

    // Content-derived ids: an interval IS a counted human gap, so deriving its id
    // deterministically from its settled content (session + start + end + identity +
    // repo) — rather than a random ULID — makes re-deriving the SAME gap (a resync,
    // a re-chunk, a crash-retry) yield the SAME id. The cloud dedups intervals by
    // content too (its authority), so this is the daemon half of defense-in-depth:
    // no recovery path can ever double-count human_seconds. Computed in a final pass
    // over the settled per-gap intervals.
    for iv in &mut out {
        iv.id = interval_id(
            &iv.source_session,
            &iv.started_at,
            &iv.ended_at,
            &iv.identity_email,
            iv.repo_canonical.as_deref(),
        );
    }
    out
}

/// Deterministic, content-derived interval id (a ULID-shaped 128-bit hash of the
/// settled interval content). Stable across re-derivation — the basis for
/// idempotent re-send/replay (see [`build_intervals`]).
fn interval_id(
    source_session: &str,
    started_at: &str,
    ended_at: &str,
    identity_email: &str,
    repo_canonical: Option<&str>,
) -> String {
    // Order is significant; `str::hash` writes a boundary terminator so distinct
    // field splits never collide. Two FNV-1a passes give 128 bits, no extra dep.
    let parts: [&str; 5] = [
        source_session,
        started_at,
        ended_at,
        identity_email,
        repo_canonical.unwrap_or(""),
    ];
    let lo = fnv1a(&parts, 0xcbf29ce484222325);
    let hi = fnv1a(&parts, 0x100000001b3);
    let value = ((hi as u128) << 64) | lo as u128;
    Ulid::from(value).to_string()
}

/// Per-session rollups, emitted only for sessions that have ended in this window.
///
/// `idle` trims the session's `agent_wall_seconds`: rather than the raw `last -
/// first` span, the wall-clock is the idle-trimmed active time over *all* of the
/// session's event timestamps ([`crate::accounting::active_seconds`]), so a
/// session left open for hours but only sporadically active reports its active
/// spans, not the dead span between them.
///
/// ## History aggregation (issue #40)
/// A session that spans multiple sync flush windows previously rolled up only its
/// TAIL window: the daemon emits a session's `SessionRollup` once, on the flush
/// where it observes the end, built solely from that flush's `events` slice — so
/// `prompts`, `branch`, `started_at`, and `agent_wall_seconds` only ever reflected
/// the last window, silently dropping everything from earlier windows. `history`
/// fixes that: for each session that ends IN `events` (`ended_ids`, computed from
/// `events` alone), we merge in the session's full retained history — deduped by
/// event id against `events` — before running the accumulator, so the rollup
/// aggregates over the WHOLE session.
///
/// CRITICAL: history must NEVER mark a session ended. Claude Code emits
/// `SessionEnd` on compaction, so a still-*live* session's history can contain a
/// stale end event; if history could set `ended`, we'd emit a false terminal
/// rollup for a live session. This is enforced structurally, not by a runtime
/// check: history is only ever merged in for a session already in `ended_ids`
/// (i.e. one whose *window* slice carries a real end), so a live session's
/// history never enters `merged` at all.
///
/// Best-effort semantics: `history` comes from retained (not-yet-compacted)
/// storage, so a session that outlives retention yields a partial — but always a
/// superset of the old tail-only — rollup. Idempotency note: a retried chunk
/// after compaction narrows the available history, so it may produce a rollup
/// with slightly different content under the same `batch_id`; the cloud's
/// batch-id dedup makes the FIRST accepted version stick, which is acceptable —
/// it is still always a superset of the pre-#40 tail-only rollup.
fn build_sessions(
    events: &[RawEvent],
    history: &[RawEvent],
    agent: crate::accounting::AgentPolicy,
) -> Vec<SessionRollup> {
    // session_id -> accumulator. BTreeMap for deterministic output ordering.
    struct Acc {
        harness: Harness,
        kind: SessionKind,
        project: Option<String>,
        email: Option<String>,
        first: OffsetDateTime,
        last: OffsetDateTime,
        had_activity: bool,
        prompts: u64,
        ended: bool,
        ended_at: Option<OffsetDateTime>,
        /// Branch on the session's *earliest* observed event (session start).
        start_branch: Option<String>,
        /// Frequency of each branch across the session's events (fallback).
        branch_counts: BTreeMap<String, u64>,
        /// Every event timestamp in the session, for idle-trimmed wall-clock.
        event_times: Vec<crate::accounting::AgentSample>,
        /// First non-null free-text note / operational label across the session's
        /// events (manual sessions; agent sessions leave these None).
        note: Option<String>,
        label: Option<String>,
    }

    // Sessions that END in THIS window's slice ONLY — never from `history` (see the
    // CRITICAL note above).
    let ended_ids: std::collections::HashSet<&str> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::SessionEnd | EventKind::ManualStop))
        .map(|e| e.session_id.as_str())
        .collect();

    // Merge `events` with the subset of `history` that (a) belongs to a session
    // ending in this window and (b) isn't already in `events` (ids are ULIDs, the
    // events-table PK, so a plain set-membership check dedups exactly). Note that
    // the history query returns window events too — including this SAME session's
    // events sitting in OTHER chunks of this flush — so those get pulled in here
    // too, which is exactly the point: a session's rollup should aggregate over the
    // whole flush's worth of its events, not just the chunk that happened to carry
    // the end.
    let slice_ids: std::collections::HashSet<&str> = events.iter().map(|e| e.id.as_str()).collect();
    let mut merged: Vec<&RawEvent> = events.iter().collect();
    merged.extend(history.iter().filter(|e| {
        ended_ids.contains(e.session_id.as_str()) && !slice_ids.contains(e.id.as_str())
    }));
    // Sort by (at, id): the accumulator below is order-dependent (first-non-null
    // project/email/note/label, `start_branch` via `e.at < entry.first`), and a
    // history-carried stale end (e.g. a compaction `SessionEnd`) must lose to a
    // later real end on `ended_at` — sorting by `at` (ties broken by id) makes the
    // last-processed end win, which is always the temporally-latest one.
    merged.sort_by(|a, b| (a.at, a.id.as_str()).cmp(&(b.at, b.id.as_str())));

    let mut sessions: BTreeMap<&str, Acc> = BTreeMap::new();
    for e in merged.into_iter() {
        let entry = sessions
            .entry(e.session_id.as_str())
            .or_insert_with(|| Acc {
                harness: e.harness,
                kind: kind_of(e.harness),
                project: e.project.clone(),
                email: e.identity_email.clone(),
                first: e.at,
                last: e.at,
                had_activity: false,
                prompts: 0,
                ended: false,
                ended_at: None,
                start_branch: e.branch.clone(),
                branch_counts: BTreeMap::new(),
                event_times: Vec::new(),
                note: e.note.clone(),
                label: e.label.clone(),
            });
        entry.event_times.push(crate::accounting::AgentSample {
            at: e.at,
            opens_span: e.kind.opens_agent_span(),
        });
        if let Some(b) = &e.branch {
            *entry.branch_counts.entry(b.clone()).or_insert(0) += 1;
        }
        // Track the branch at the session's earliest event (its start).
        if e.at < entry.first {
            entry.first = e.at;
            entry.start_branch = e.branch.clone();
        }
        if e.at > entry.last {
            entry.last = e.at;
        }
        if e.kind.is_agent_activity() {
            entry.had_activity = true;
        }
        if matches!(e.kind, EventKind::UserPrompt) {
            entry.prompts += 1;
        }
        if entry.project.is_none() && e.project.is_some() {
            entry.project = e.project.clone();
        }
        if entry.email.is_none() && e.identity_email.is_some() {
            entry.email = e.identity_email.clone();
        }
        if entry.note.is_none() && e.note.is_some() {
            entry.note = e.note.clone();
        }
        if entry.label.is_none() && e.label.is_some() {
            entry.label = e.label.clone();
        }
        if matches!(e.kind, EventKind::SessionEnd | EventKind::ManualStop) {
            entry.ended = true;
            entry.ended_at = Some(e.at);
        }
    }

    sessions
        .into_iter()
        .filter(|(_, a)| a.ended)
        .map(|(id, a)| {
            // Agent wall-clock: the idle-trimmed active time over the session's
            // whole event timeline when it had activity (not the raw last - first
            // span), so dead spans between bursts of work don't inflate it.
            let agent_wall = if a.had_activity {
                crate::accounting::agent_active_seconds(&a.event_times, agent).max(0) as u64
            } else {
                0
            };
            // Branch at session start, falling back to the most frequent branch
            // among the session's events (ties broken by branch name for
            // determinism). `None` when no event carried a branch.
            let branch = a.start_branch.clone().or_else(|| {
                a.branch_counts
                    .iter()
                    .max_by(|(an, ac), (bn, bc)| ac.cmp(bc).then(bn.cmp(an)))
                    .map(|(name, _)| name.clone())
            });
            SessionRollup {
                session_id: id.to_string(),
                harness: a.harness,
                kind: a.kind,
                repo_canonical: a.project,
                identity_email: a.email.unwrap_or_default(),
                started_at: fmt(a.first),
                ended_at: a.ended_at.map(fmt),
                agent_wall_seconds: agent_wall,
                prompts: Some(a.prompts),
                branch,
                note: a.note,
                label: a.label,
            }
        })
        .collect()
}

/// Map stored token rows to the contract's `TokenUsage`.
fn build_token_usage(rows: &[TokenRow]) -> Vec<TokenUsage> {
    rows.iter()
        .map(|r| TokenUsage {
            id: r.id.clone(),
            session_id: r.session_id.clone(),
            repo_canonical: r.project.clone(),
            model: r.model.clone(),
            input: r.input,
            output: r.output,
            cache_read: r.cache_read,
            cache_create: r.cache_create,
            est_cost_usd: r.est_cost_usd,
            at: r.at.clone(),
        })
        .collect()
}

/// Resolve a fallback identity email per session from its events.
fn session_emails(events: &[RawEvent]) -> BTreeMap<String, Option<String>> {
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    for e in events {
        let entry = out.entry(e.session_id.clone()).or_insert(None);
        if entry.is_none() && e.identity_email.is_some() {
            *entry = e.identity_email.clone();
        }
    }
    out
}

/// Resolve the first non-null activity classification per session.
fn session_activities(events: &[RawEvent]) -> BTreeMap<String, Option<String>> {
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    for e in events {
        let entry = out.entry(e.session_id.clone()).or_insert(None);
        if entry.is_none() && e.activity.is_some() {
            *entry = e.activity.clone();
        }
    }
    out
}

/// Manual harness ⇒ manual session kind; everything else is an agent session.
fn kind_of(harness: Harness) -> SessionKind {
    match harness {
        Harness::Manual => SessionKind::Manual,
        _ => SessionKind::Agent,
    }
}

/// Deterministic batch id over the window's events + artifact shas + INTERVAL
/// decomposition, so a crash-retry of the *same* window (same events AND same
/// interval split) produces the *same* `batchId` and the cloud no-ops on it
/// (idempotent ingest), while a re-derivation that changes the interval split of the
/// same minutes (the per-gap fix, issue #21) produces a DIFFERENT `batchId` and
/// forces the cloud to re-unpack rather than dedup-and-drop it.
///
/// Events/artifacts are ULIDs/shas hashed into a 128-bit value rendered as a ULID.
/// Artifact shas are folded so an *artifact-only* flush still gets a distinct id.
/// The ULID's 48-bit timestamp is stamped with the chunk's **maximum covered event
/// time**, so `max(batchId)` — the cloud's persisted device watermark — stays
/// monotonic in covered event time and comparable to the daemon's event cursor.
/// Falls back to the pure content hash for an artifact-only chunk (no covered time).
fn batch_id_for_chunk(
    events: &[RawEvent],
    artifacts: &[ArtifactRow],
    intervals: &[Interval],
) -> String {
    // Fold each interval's content id into the hash so a decomposition change (the
    // per-gap fix, issue #21) produces a different batch id and forces a cloud re-
    // unpack, while a byte-identical rebuild yields the same id (crash-retry dedup).
    let mut ids: Vec<&str> = events
        .iter()
        .map(|e| e.id.as_str())
        .chain(artifacts.iter().map(|a| a.sha.as_str()))
        .chain(intervals.iter().map(|iv| iv.id.as_str()))
        .collect();
    ids.sort_unstable();
    let lo = fnv1a(&ids, 0xcbf29ce484222325);
    let hi = fnv1a(&ids, 0x100000001b3);
    let content = ((hi as u128) << 64) | lo as u128;

    // Highest event id in the chunk (events are ULIDs ⇒ lexicographic max = latest).
    let max_ts_ms = events
        .iter()
        .map(|e| e.id.as_str())
        .max()
        .and_then(|id| Ulid::from_string(id).ok())
        .map(|u| u.timestamp_ms());

    match max_ts_ms {
        // ULID = 48-bit timestamp (high) | 80-bit randomness (low). Stamp the time
        // and keep the low 80 bits of the content hash as the "randomness".
        Some(ts) => Ulid::from(((ts as u128) << 80) | (content & ((1u128 << 80) - 1))).to_string(),
        None => Ulid::from(content).to_string(),
    }
}

pub(crate) fn fnv1a(ids: &[&str], seed: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    // A tiny deterministic hasher (std's DefaultHasher is not stable across
    // releases, so we roll FNV-1a for a fixed, reproducible batch id).
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 ^= b as u64;
                self.0 = self.0.wrapping_mul(0x100000001b3);
            }
        }
    }
    let mut h = Fnv(seed);
    for id in ids {
        id.hash(&mut h);
    }
    h.finish()
}

/// Format a timestamp as RFC 3339 (the contract's wire encoding).
fn fmt(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

/// Re-export the est-cost helper so callers that build [`TokenRow`]s from raw
/// turns don't need to reach into [`crate::tokens`] directly.
pub fn est_cost(model: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> f64 {
    let turn = tokens::TokenTurn {
        id: String::new(),
        at: String::new(),
        model: model.to_string(),
        input,
        output,
        cache_read,
        cache_create,
    };
    turn.est_cost_usd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::{counted_gaps, Signal};
    use crate::report;

    fn ev(session: &str, secs: i64, kind: EventKind, project: &str) -> RawEvent {
        RawEvent {
            id: format!("{session}-{secs:08}"),
            at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs),
            session_id: session.to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some(project.to_string()),
            identity_email: Some("dev@example.com".to_string()),
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    /// Like [`ev`] but with a branch set — the plain helper hardcodes `branch:
    /// None`, so history-aggregation tests that need to track a branch across
    /// events use this instead.
    fn ev_b(session: &str, secs: i64, kind: EventKind, project: &str, branch: &str) -> RawEvent {
        RawEvent {
            branch: Some(branch.to_string()),
            ..ev(session, secs, kind, project)
        }
    }

    const IDLE: Duration = Duration::minutes(5);
    /// The shipped agent ceilings, so these fixtures exercise the same policy a
    /// real daemon runs with. `agent_const_matches_the_shipped_default` below
    /// fails if this ever drifts from `AgentPolicy::default()`.
    const AGENT: crate::accounting::AgentPolicy = crate::accounting::AgentPolicy {
        idle: Duration::minutes(5),
        max_span: Duration::hours(8),
    };

    #[test]
    fn agent_const_matches_the_shipped_default() {
        assert_eq!(AGENT, crate::accounting::AgentPolicy::default());
    }
    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    #[test]
    fn prompts_are_counted_per_session() {
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::PreTool, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
            ev("s1", 90, EventKind::SessionEnd, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        let s = batch
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("session s1 rolled up");
        assert_eq!(s.prompts, Some(2), "two user_prompt events → prompts = 2");
    }

    /// The canonical fixture: two interleaved sessions on the same project. The
    /// deduped human-seconds in the intervals must equal the report's total, and
    /// the intervals must sum to that same number.
    #[test]
    fn interleaved_two_session_intervals_match_report_total() {
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s2", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::UserPrompt, "p"),
            ev("s2", 20, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::PreTool, "p"),
            ev("s2", 40, EventKind::PreTool, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
            ev("s1", 90, EventKind::SessionEnd, "p"),
            ev("s2", 90, EventKind::SessionEnd, "p"),
        ];

        let report = report::build(&events, IDLE, crate::accounting::AgentPolicy::default());
        let batch = build_batch(&events, &[], &[], "01DEVICE", IDLE, AGENT, NOW);

        let interval_seconds: u64 = batch.intervals.iter().map(|i| i.human_seconds).sum();
        assert_eq!(
            interval_seconds as i64, report.total_human_seconds,
            "interval human-seconds must equal the deduped report total"
        );

        // Cross-check against accounting directly: the dedup must not double-count
        // the two concurrent sessions.
        let signals: Vec<Signal> = events
            .iter()
            .filter(|e| e.kind.is_human_signal())
            .map(|e| Signal {
                at: e.at,
                project: e.project.clone(),
            })
            .collect();
        let direct: i64 = counted_gaps(&signals, IDLE)
            .iter()
            .map(|g| g.seconds())
            .sum();
        assert_eq!(interval_seconds as i64, direct);
    }

    #[test]
    fn emits_one_interval_per_gap_for_one_session() {
        // One session, prompts at 0/30/60 — three signals, two abutting 30s gaps.
        // The decomposition is one interval PER counted gap (no coalescing), so the
        // set of (start,end) intervals is a pure function of the data and identical
        // under every windowing — the invariant the cloud's content-key dedup needs
        // (issue #21). Totals are unchanged; only the decomposition granularity is.
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.intervals.len(), 2, "one interval per counted gap");
        assert_eq!(batch.intervals[0].human_seconds, 30);
        assert_eq!(batch.intervals[1].human_seconds, 30);
        // Half-open + contiguous: the first ends exactly where the second starts.
        assert_eq!(batch.intervals[0].ended_at, batch.intervals[1].started_at);
        assert_eq!(batch.intervals[0].source_session, "s1");
        assert_eq!(batch.intervals[0].repo_canonical.as_deref(), Some("p"));
        assert_eq!(batch.intervals[0].identity_email, "dev@example.com");
        // Total still equals the deduped accounting truth.
        let total: u64 = batch.intervals.iter().map(|i| i.human_seconds).sum();
        assert_eq!(total, 60);
    }

    #[test]
    fn seed_recovers_boundary_gap_and_is_windowing_invariant() {
        // Signals 0,100,250,400 (all <= idle apart). Split into two flush windows at
        // 100; the second window seeds with the pre-cursor signal @100. The union of
        // the two windows' intervals must equal the one-shot build — same total AND
        // the same (start,end) id multiset (issue #21 windowing-invariance).
        let all = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 100, EventKind::UserPrompt, "p"),
            ev("s1", 250, EventKind::UserPrompt, "p"),
            ev("s1", 400, EventKind::UserPrompt, "p"),
        ];
        let mut oneshot: Vec<String> = build_intervals(&all, IDLE)
            .into_iter()
            .map(|i| i.id)
            .collect();
        let oneshot_total: u64 = build_intervals(&all, IDLE)
            .iter()
            .map(|i| i.human_seconds)
            .sum();
        assert_eq!(oneshot_total, 400);

        let w1 = build_intervals(&all[0..2], IDLE); // signals @0,@100
        let seed = vec![all[1].clone()]; // pre-cursor signal @100
        let w2 = build_intervals_seeded(&all[2..4], IDLE, &seed); // @250,@400 + seed @100

        let inc_total: u64 = w1.iter().chain(&w2).map(|i| i.human_seconds).sum();
        assert_eq!(
            inc_total, 400,
            "boundary gap [100,250) recovered via the seed"
        );

        let mut inc: Vec<String> = w1.iter().chain(&w2).map(|i| i.id.clone()).collect();
        oneshot.sort();
        inc.sort();
        assert_eq!(oneshot, inc, "seeded incremental == one-shot decomposition");
    }

    #[test]
    fn seed_selected_by_at_not_id_and_no_seed_internal_double_count() {
        // Pre-cursor human signals at 1000 (session sX) and 1250 (sY); the window's
        // first signal is at 1400 (sY). The TRUE boundary opener is 1250 (gap 150 <=
        // idle) — anchoring on 1000 instead would give 400 > idle and drop the gap.
        // And [1000,1250) must NOT be re-emitted: it belongs to the earlier window.
        let seed = vec![
            ev("sX", 1000, EventKind::UserPrompt, "p"),
            ev("sY", 1250, EventKind::UserPrompt, "p"),
        ];
        let win = vec![ev("sY", 1400, EventKind::UserPrompt, "p")];
        let out = build_intervals_seeded(&win, IDLE, &seed);
        assert_eq!(
            out.len(),
            1,
            "only the boundary gap into the window signal is emitted"
        );
        assert_eq!(
            out[0].human_seconds, 150,
            "gap [1250,1400) anchors on the max-AT seed"
        );
        assert_eq!(out[0].source_session, "sY");
    }

    #[test]
    fn seed_attributes_boundary_gap_to_opener_across_project_switch() {
        // Window opens on a different session/project than the seed; the boundary gap
        // is attributed to the OPENER (the seed signal), not the window signal.
        let seed = vec![ev("sA", 100, EventKind::UserPrompt, "pA")];
        let win = vec![ev("sB", 250, EventKind::UserPrompt, "pB")]; // gap 150 <= idle
        let out = build_intervals_seeded(&win, IDLE, &seed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source_session, "sA");
        assert_eq!(out[0].repo_canonical.as_deref(), Some("pA"));
        assert_eq!(out[0].human_seconds, 150);
        assert!(
            !out[0].identity_email.is_empty(),
            "opener identity resolved from the seed"
        );
    }

    #[test]
    fn batch_id_changes_when_interval_decomposition_changes() {
        // batch_id must fold the interval decomposition so a re-derivation that splits
        // the same minutes differently forces the cloud to re-unpack instead of dedup-
        // and-drop (issue #21 Part C); same input → same id (crash-retry dedup).
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
        ];
        let full = build_intervals(&events, IDLE); // two per-gap intervals
        assert_eq!(full.len(), 2);
        let coarser = full[..1].to_vec(); // a DIFFERENT decomposition of the same events
        let id_full = batch_id_for_chunk(&events, &[], &full);
        let id_coarser = batch_id_for_chunk(&events, &[], &coarser);
        assert_ne!(
            id_full, id_coarser,
            "decomposition change → different batch_id"
        );
        assert_eq!(
            id_full,
            batch_id_for_chunk(&events, &[], &full),
            "byte-identical rebuild → same batch_id"
        );
    }

    #[test]
    fn empty_seed_equals_plain_build() {
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
        ];
        let plain: Vec<String> = build_intervals(&events, IDLE)
            .into_iter()
            .map(|i| i.id)
            .collect();
        let seeded: Vec<String> = build_intervals_seeded(&events, IDLE, &[])
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(plain, seeded);
    }

    #[test]
    fn retro_log_backdated_events_are_reconciled() {
        // `dira log` backdates `at` below the sync cursor: a prior flush already sent
        // L[1000,1200)=200, then a manual session is logged with M@1050,1150 (fresh,
        // higher ids). Flush 2's window is {M@1050, M@1150}; the seed is [L@1000,
        // L@1200]. The backdated M signals re-split the L gap into [1000,1050)(L),
        // [1050,1150)(M), [1150,1200)(M) — each touches a window signal, so all three
        // are emitted (the "either endpoint from window" rule). Ground truth = 200.
        let seed = vec![
            ev("L", 1000, EventKind::UserPrompt, "p"),
            ev("L", 1200, EventKind::UserPrompt, "p"),
        ];
        let win = vec![
            ev("M", 1050, EventKind::ManualStart, "p"),
            ev("M", 1150, EventKind::ManualStop, "p"),
        ];
        let out = build_intervals_seeded(&win, IDLE, &seed);
        let total: u64 = out.iter().map(|i| i.human_seconds).sum();
        assert_eq!(total, 200, "backdated gap fully reconciled, no under-count");
        // The re-split [1150,1200) is attributed to the manual session M (its opener).
        assert!(
            out.iter()
                .any(|i| i.source_session == "M" && i.human_seconds == 50),
            "the [1150,1200) gap is re-attributed to M"
        );
        // And no interval double-counts: total never exceeds the wall span 1000..1200.
        assert!(total <= 200);
    }

    #[test]
    fn far_backdated_log_gap_is_recovered_with_band_seed() {
        // The pre-cursor signals L@800 and L@1200 are > idle apart (no synced L
        // interval). `dira log` injects M@1050,1150. The seed band reaches back to
        // L@800 (it is ≤ idle from the backdated M@1050), so the boundary gap
        // [800,1050) is recovered. Ground truth = 250(L) + 100(M) + 50(M) = 400.
        let seed = vec![
            ev("L", 800, EventKind::UserPrompt, "p"),
            ev("L", 1200, EventKind::UserPrompt, "p"),
        ];
        let win = vec![
            ev("M", 1050, EventKind::ManualStart, "p"),
            ev("M", 1150, EventKind::ManualStop, "p"),
        ];
        let out = build_intervals_seeded(&win, IDLE, &seed);
        let total: u64 = out.iter().map(|i| i.human_seconds).sum();
        assert_eq!(total, 400);
        assert!(
            out.iter()
                .any(|i| i.source_session == "L" && i.human_seconds == 250),
            "the far [800,1050) gap is anchored on L@800 and attributed to L"
        );
    }

    #[test]
    fn idle_gap_splits_intervals() {
        // 0s, +30s, then a 10-min idle gap, then +30s. The wide gap is excluded
        // and breaks the coalescing, yielding two short intervals.
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
            ev("s1", 30 + 600, EventKind::UserPrompt, "p"),
            ev("s1", 30 + 600 + 30, EventKind::UserPrompt, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.intervals.len(), 2);
        assert_eq!(batch.intervals[0].human_seconds, 30);
        assert_eq!(batch.intervals[1].human_seconds, 30);
    }

    #[test]
    fn only_ended_sessions_are_rolled_up() {
        // s1 ends, s2 does not. Only s1 should appear in the rollup.
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::PreTool, "p"),
            ev("s1", 20, EventKind::SessionEnd, "p"),
            ev("s2", 0, EventKind::SessionStart, "p"),
            ev("s2", 15, EventKind::PreTool, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.sessions.len(), 1);
        let s = &batch.sessions[0];
        assert_eq!(s.session_id, "s1");
        assert_eq!(s.agent_wall_seconds, 20); // span 0..20 with activity
        assert!(s.ended_at.is_some());
        assert_eq!(s.identity_email, "dev@example.com");
    }

    #[test]
    fn agent_wall_is_idle_trimmed_not_raw_span() {
        // A session opened, a short burst of activity (0..30s), then a long quiet
        // hour that is NOT a tool call, then a final end event. What this test
        // protects is that the raw ~3690s span is never what gets shipped.
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::PreTool, "p"),
            ev("s1", 30, EventKind::PostTool, "p"),
            ev("s1", 30 + 3600, EventKind::SessionEnd, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.sessions.len(), 1);
        let s = &batch.sessions[0];
        // started_at / ended_at remain first / last as before.
        assert_eq!(s.started_at, fmt(OffsetDateTime::UNIX_EPOCH));
        assert!(s.ended_at.is_some());
        // 10 (start→pre) + 20 (the tool call) + 300 (the quiet hour, clamped to
        // the agent idle ceiling rather than discarded, so the live dashboard can
        // predict this value instead of showing time it later revokes).
        assert_eq!(s.agent_wall_seconds, 330);
        assert!(s.agent_wall_seconds < 3690 / 10);
    }

    /// The regression the agent policy exists for: the gap after a `PreTool` is
    /// the tool call itself, and the rollup shipped to the cloud must bank it.
    /// Under the old shared-idle rule this session reported zero agent seconds.
    #[test]
    fn a_long_tool_call_reaches_the_cloud_rollup() {
        let two_hours = 2 * 60 * 60;
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 1, EventKind::PreTool, "p"),
            ev("s1", 1 + two_hours, EventKind::PostTool, "p"),
            ev("s1", 2 + two_hours, EventKind::SessionEnd, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        let s = &batch.sessions[0];
        // 1 (start→pre) + 7200 (the build) + 1 (post→end).
        assert_eq!(s.agent_wall_seconds, (two_hours + 2) as u64);
    }

    #[test]
    fn manual_session_kind_is_manual() {
        let mut start = ev("m1", 0, EventKind::ManualStart, "p");
        start.harness = Harness::Manual;
        let mut stop = ev("m1", 60, EventKind::ManualStop, "p");
        stop.harness = Harness::Manual;
        let batch = build_batch(&[start, stop], &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.sessions.len(), 1);
        assert_eq!(batch.sessions[0].kind, SessionKind::Manual);
        assert_eq!(batch.sessions[0].harness, Harness::Manual);
    }

    #[test]
    fn token_rows_map_to_contract() {
        let rows = vec![TokenRow {
            id: "t1".into(),
            session_id: "s1".into(),
            project: Some("p".into()),
            model: "claude-opus-4-8".into(),
            input: 10,
            output: 20,
            cache_read: 5,
            cache_create: 3,
            est_cost_usd: Some(0.5),
            at: "2026-06-27T10:00:00Z".into(),
        }];
        let batch = build_batch(&[], &rows, &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.token_usage.len(), 1);
        let t = &batch.token_usage[0];
        assert_eq!(t.id, "t1");
        assert_eq!(t.repo_canonical.as_deref(), Some("p"));
        assert_eq!(t.input, 10);
        assert_eq!(t.est_cost_usd, Some(0.5));
    }

    /// A minimal artifact row — only `sha` matters to the chunking tests.
    fn art(sha: &str) -> ArtifactRow {
        ArtifactRow {
            sha: sha.into(),
            repo: Some("github.com/acme/api".into()),
            git_ref: None,
            kind: "commit".into(),
            authored_at: None,
            author_email: None,
            author_name: None,
            source_session: None,
            patch_id: None,
            session_change_id: None,
            touched_paths: None,
            blobs: None,
        }
    }

    /// Every shipped artifact, in chunk order, plus the per-chunk counts.
    fn shipped_artifacts(chunks: &[ChunkBatch]) -> (Vec<String>, Vec<usize>) {
        let per_chunk: Vec<usize> = chunks.iter().map(|c| c.batch.artifacts.len()).collect();
        let all: Vec<String> = chunks
            .iter()
            .flat_map(|c| c.batch.artifacts.iter().map(|a| a.sha.clone()))
            .collect();
        (all, per_chunk)
    }

    /// Issue #71: artifacts used to ride the FINAL chunk in their entirety, so a
    /// `dira device resync` (which rewinds the artifact cursor to the beginning)
    /// built one request out of the whole backlog and got a 413. They must now be
    /// spread — losslessly, and without duplicating a single row.
    #[test]
    fn artifacts_are_spread_across_chunks_losslessly() {
        let rows: Vec<ArtifactRow> = (0..CHUNK_ARTIFACTS * 2 + 7)
            .map(|i| art(&format!("sha-{i:04}")))
            .collect();
        let events = vec![ev("s1", 0, EventKind::UserPrompt, "p")];

        let chunks =
            build_chunked_batches(&events, &[], &rows, &[], "d", IDLE, AGENT, NOW, &[], &[]);

        let (shipped, per_chunk) = shipped_artifacts(&chunks);
        assert!(
            per_chunk.iter().all(|&n| n <= CHUNK_ARTIFACTS),
            "no request may exceed the cap: {per_chunk:?}"
        );
        let expected: Vec<String> = rows.iter().map(|r| r.sha.clone()).collect();
        assert_eq!(
            shipped, expected,
            "every artifact ships exactly once, in order"
        );
    }

    /// The cursor contract: the artifact cursor advances on `is_last` alone, so
    /// exactly one chunk may carry it — and it must be the last one, or the cursor
    /// would jump past artifacts that were never sent.
    #[test]
    fn only_the_final_chunk_is_last_when_artifacts_outnumber_event_chunks() {
        let rows: Vec<ArtifactRow> = (0..CHUNK_ARTIFACTS * 3)
            .map(|i| art(&format!("sha-{i:04}")))
            .collect();
        let events = vec![ev("s1", 0, EventKind::UserPrompt, "p")];

        let chunks =
            build_chunked_batches(&events, &[], &rows, &[], "d", IDLE, AGENT, NOW, &[], &[]);

        assert_eq!(
            chunks.len(),
            3,
            "one event chunk grown to carry 3 artifact slices"
        );
        assert_eq!(
            chunks.iter().filter(|c| c.is_last).count(),
            1,
            "exactly one chunk advances the artifact cursor"
        );
        assert!(chunks.last().unwrap().is_last);
        // The surplus chunks carry no event high-water — there are no events left.
        assert!(chunks[0].cursor_event_id.is_some());
        assert!(chunks[1].cursor_event_id.is_none());
        assert!(chunks[2].cursor_event_id.is_none());
    }

    /// The `dira device resync` shape: a large artifact backlog with no new events
    /// at all. It must still be bounded, and still drain in ONE flush.
    #[test]
    fn an_artifact_only_flush_is_bounded_too() {
        let rows: Vec<ArtifactRow> = (0..CHUNK_ARTIFACTS * 2 + 1)
            .map(|i| art(&format!("sha-{i:04}")))
            .collect();

        let chunks = build_chunked_batches(&[], &[], &rows, &[], "d", IDLE, AGENT, NOW, &[], &[]);

        let (shipped, per_chunk) = shipped_artifacts(&chunks);
        assert_eq!(per_chunk, vec![CHUNK_ARTIFACTS, CHUNK_ARTIFACTS, 1]);
        assert_eq!(shipped.len(), rows.len(), "nothing dropped");
        assert!(chunks.iter().all(|c| c.cursor_event_id.is_none()));
        assert!(chunks.last().unwrap().is_last);
        assert_eq!(chunks.iter().filter(|c| c.is_last).count(), 1);
    }

    fn tok(id: &str) -> TokenRow {
        TokenRow {
            id: id.into(),
            session_id: "s1".into(),
            project: Some("p".into()),
            model: "claude-opus-4-8".into(),
            input: 10,
            output: 20,
            cache_read: 0,
            cache_create: 0,
            est_cost_usd: None,
            at: "2026-06-27T10:00:00Z".into(),
        }
    }

    /// Token rows were "bounded by the event window" — which stops bounding
    /// anything on a `dira device resync`, since that rewinds the event cursor
    /// too and the window becomes the whole log. Same 413 as issue #71, smaller
    /// rows, so they are spread the same way.
    #[test]
    fn token_rows_are_spread_across_chunks_losslessly() {
        let rows: Vec<TokenRow> = (0..CHUNK_TOKENS * 2 + 5)
            .map(|i| tok(&format!("t-{i:05}")))
            .collect();
        let events = vec![ev("s1", 0, EventKind::UserPrompt, "p")];

        let chunks =
            build_chunked_batches(&events, &rows, &[], &[], "d", IDLE, AGENT, NOW, &[], &[]);

        let per_chunk: Vec<usize> = chunks.iter().map(|c| c.batch.token_usage.len()).collect();
        assert_eq!(per_chunk, vec![CHUNK_TOKENS, CHUNK_TOKENS, 5]);
        let shipped: Vec<String> = chunks
            .iter()
            .flat_map(|c| c.batch.token_usage.iter().map(|t| t.id.clone()))
            .collect();
        let expected: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(shipped, expected, "every token row ships exactly once");
        assert_eq!(chunks.iter().filter(|c| c.is_last).count(), 1);
    }

    /// Spreading must not disturb the no-artifact path, which is the common case.
    #[test]
    fn a_flush_with_no_artifacts_chunks_exactly_as_before() {
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 10, EventKind::PreTool, "p"),
        ];
        let chunks = build_chunked_batches(&events, &[], &[], &[], "d", IDLE, AGENT, NOW, &[], &[]);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_last);
        assert!(chunks[0].cursor_event_id.is_some());
    }

    #[test]
    fn artifacts_map_to_contract_with_sha_as_id() {
        let rows = vec![ArtifactRow {
            sha: "abc123".into(),
            repo: Some("github.com/acme/api".into()),
            git_ref: Some("main".into()),
            kind: "commit".into(),
            authored_at: Some("2026-06-27T10:00:00Z".into()),
            author_email: Some("dev@example.com".into()),
            author_name: Some("Dev One".into()),
            source_session: Some("sess-1".into()),
            patch_id: Some("pid-1".into()),
            session_change_id: Some("scid-1".into()),
            touched_paths: Some(vec!["a.rs".into(), "b.rs".into()]),
            blobs: Some(vec![dira_contract::BlobRef {
                path: "a.rs".into(),
                blob: "blob-a".into(),
            }]),
        }];
        let batch = build_batch(&[], &[], &rows, "d", IDLE, AGENT, NOW);
        assert_eq!(batch.artifacts.len(), 1);
        let a = &batch.artifacts[0];
        // id == sha keeps cloud ingest idempotent on re-ship.
        assert_eq!(a.id, "abc123");
        assert_eq!(a.sha, "abc123");
        assert_eq!(a.repo_canonical.as_deref(), Some("github.com/acme/api"));
        assert_eq!(a.git_ref.as_deref(), Some("main"));
        assert_eq!(a.kind, dira_contract::ArtifactKind::Commit);
        // New anchoring fields propagate to the wire ref.
        assert_eq!(a.authored_at.as_deref(), Some("2026-06-27T10:00:00Z"));
        assert_eq!(a.author_email.as_deref(), Some("dev@example.com"));
        assert_eq!(a.source_session.as_deref(), Some("sess-1"));
        assert_eq!(a.patch_id.as_deref(), Some("pid-1"));
        // Squash-resilient session signals propagate too.
        assert_eq!(a.session_change_id.as_deref(), Some("scid-1"));
        assert_eq!(
            a.touched_paths.as_deref(),
            Some(["a.rs".to_string(), "b.rs".to_string()].as_slice())
        );
        assert_eq!(a.blobs.as_ref().unwrap()[0].path, "a.rs");
        assert_eq!(a.blobs.as_ref().unwrap()[0].blob, "blob-a");
    }

    #[test]
    fn session_branch_propagates_from_events() {
        // Branch on the start event wins over later branches.
        let mut start = ev("s1", 0, EventKind::SessionStart, "p");
        start.branch = Some("feat/x".into());
        let mut work = ev("s1", 10, EventKind::PreTool, "p");
        work.branch = Some("feat/y".into());
        let mut end = ev("s1", 20, EventKind::SessionEnd, "p");
        end.branch = Some("feat/y".into());
        let batch = build_batch(&[start, work, end], &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.sessions.len(), 1);
        assert_eq!(batch.sessions[0].branch.as_deref(), Some("feat/x"));
    }

    #[test]
    fn session_branch_falls_back_to_most_frequent() {
        // No branch on the start event ⇒ fall back to the most frequent branch.
        let start = ev("s1", 0, EventKind::SessionStart, "p"); // branch None
        let mut w1 = ev("s1", 10, EventKind::PreTool, "p");
        w1.branch = Some("feat/x".into());
        let mut w2 = ev("s1", 20, EventKind::PreTool, "p");
        w2.branch = Some("feat/x".into());
        let mut end = ev("s1", 30, EventKind::SessionEnd, "p");
        end.branch = Some("feat/y".into());
        let batch = build_batch(&[start, w1, w2, end], &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(batch.sessions.len(), 1);
        assert_eq!(batch.sessions[0].branch.as_deref(), Some("feat/x"));
    }

    #[test]
    fn batch_id_is_deterministic_for_same_window() {
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
        ];
        let a = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        let b = build_batch(&events, &[], &[], "d", IDLE, AGENT, NOW);
        assert_eq!(a.batch_id, b.batch_id, "same window ⇒ same batch id");

        // A different window ⇒ a different id.
        let mut more = events.clone();
        more.push(ev("s1", 60, EventKind::UserPrompt, "p"));
        let c = build_batch(&more, &[], &[], "d", IDLE, AGENT, NOW);
        assert_ne!(a.batch_id, c.batch_id);
    }

    #[test]
    fn empty_window_is_an_empty_batch() {
        let batch = build_batch(&[], &[], &[], "d", IDLE, AGENT, NOW);
        assert!(batch.intervals.is_empty());
        assert!(batch.sessions.is_empty());
        assert!(batch.token_usage.is_empty());
        assert!(batch.artifacts.is_empty());
    }

    #[test]
    fn long_unended_session_emits_a_partial_rollup() {
        // A multi-day session that hasn't ended: the event window for *this* sync
        // carries only a couple of new tool events, so build_sessions emits no
        // ended rollup. The partial — described from the registry — must appear
        // with ended_at=None and the registry's active_seconds.
        let window = vec![
            ev("long", 100, EventKind::PreTool, "p"),
            ev("long", 130, EventKind::PostTool, "p"),
        ];
        let partials = vec![PartialSession {
            session_id: "long".into(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            repo_canonical: Some("p".into()),
            identity_email: Some("dev@example.com".into()),
            started_at: OffsetDateTime::UNIX_EPOCH,
            active_seconds: 4242,
            prompts: Some(7),
            branch: Some("feat/x".into()),
            note: None,
            label: None,
        }];
        let batch = build_batch_with_partials(&window, &[], &[], &partials, "d", IDLE, AGENT, NOW);
        let s = batch
            .sessions
            .iter()
            .find(|s| s.session_id == "long")
            .expect("partial rollup present");
        assert_eq!(s.ended_at, None, "a partial rollup is open-ended");
        assert_eq!(
            s.agent_wall_seconds, 4242,
            "uses the rolling active_seconds"
        );
        assert_eq!(s.prompts, Some(7));
        assert_eq!(s.branch.as_deref(), Some("feat/x"));
        assert_eq!(s.identity_email, "dev@example.com");
    }

    #[test]
    fn partial_is_suppressed_when_session_ends_in_window() {
        // The session both has new activity AND ends in this window: the ended
        // rollup supersedes the partial, so only ONE rollup ships (ended).
        let window = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::PreTool, "p"),
            ev("s1", 20, EventKind::SessionEnd, "p"),
        ];
        let partials = vec![PartialSession {
            session_id: "s1".into(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            repo_canonical: Some("p".into()),
            identity_email: Some("dev@example.com".into()),
            started_at: OffsetDateTime::UNIX_EPOCH,
            active_seconds: 999,
            prompts: None,
            branch: None,
            note: None,
            label: None,
        }];
        let batch = build_batch_with_partials(&window, &[], &[], &partials, "d", IDLE, AGENT, NOW);
        let rollups: Vec<_> = batch
            .sessions
            .iter()
            .filter(|s| s.session_id == "s1")
            .collect();
        assert_eq!(rollups.len(), 1, "ended rollup supersedes the partial");
        assert!(
            rollups[0].ended_at.is_some(),
            "the surviving rollup is the ended one"
        );
    }

    #[test]
    fn partial_with_zero_active_is_skipped() {
        let partials = vec![PartialSession {
            session_id: "idle".into(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            repo_canonical: Some("p".into()),
            identity_email: Some("dev@example.com".into()),
            started_at: OffsetDateTime::UNIX_EPOCH,
            active_seconds: 0,
            prompts: None,
            branch: None,
            note: None,
            label: None,
        }];
        let batch = build_batch_with_partials(&[], &[], &[], &partials, "d", IDLE, AGENT, NOW);
        assert!(
            batch.sessions.is_empty(),
            "no active time ⇒ nothing to settle"
        );
    }

    #[test]
    fn interval_ids_are_deterministic_and_content_derived() {
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
            ev("s1", 600, EventKind::UserPrompt, "p"),
        ];
        let a = build_intervals(&events, IDLE);
        let b = build_intervals(&events, IDLE);
        assert!(!a.is_empty());
        let ids_a: Vec<&str> = a.iter().map(|i| i.id.as_str()).collect();
        let ids_b: Vec<&str> = b.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids_a, ids_b,
            "same content ⇒ same interval ids (idempotent re-send/replay)"
        );
        assert!(a.iter().all(|i| !i.id.is_empty()), "ids are filled in");
    }

    #[test]
    fn chunk_ranges_split_only_at_idle_breaks() {
        // Two tight clusters separated by a > idle gap.
        let events = vec![
            ev("s", 0, EventKind::UserPrompt, "p"),
            ev("s", 10, EventKind::UserPrompt, "p"),
            ev("s", 20, EventKind::UserPrompt, "p"),
            // > IDLE (5 min) break here:
            ev("s", 1000, EventKind::UserPrompt, "p"),
            ev("s", 1010, EventKind::UserPrompt, "p"),
        ];
        // min_chunk = 2: the first cluster reaches the cap, then closes at the break.
        assert_eq!(chunk_ranges(&events, IDLE, 2), vec![(0, 2), (3, 4)]);
        // A tight cluster with no > idle break stays one chunk (indivisible w/o loss).
        let tight = vec![
            ev("s", 0, EventKind::UserPrompt, "p"),
            ev("s", 5, EventKind::UserPrompt, "p"),
            ev("s", 10, EventKind::UserPrompt, "p"),
        ];
        assert_eq!(chunk_ranges(&tight, IDLE, 1), vec![(0, 2)]);
    }

    #[test]
    fn chunked_build_preserves_all_intervals_no_dup() {
        // A window spanning an idle break: the single-window build and the chunked
        // build (split at the break) must yield the SAME interval id multiset — no
        // counted gap is lost or duplicated across the chunk boundary.
        let events = vec![
            ev("s", 0, EventKind::UserPrompt, "p"),
            ev("s", 60, EventKind::UserPrompt, "p"),
            ev("s", 120, EventKind::UserPrompt, "p"),
            // idle break (> 5 min):
            ev("s", 2000, EventKind::UserPrompt, "p"),
            ev("s", 2060, EventKind::UserPrompt, "p"),
        ];
        let mut single: Vec<String> = build_intervals(&events, IDLE)
            .into_iter()
            .map(|i| i.id)
            .collect();
        single.sort();

        let ranges = chunk_ranges(&events, IDLE, 2);
        assert!(ranges.len() >= 2, "expected a split at the idle break");
        let mut chunked: Vec<String> = ranges
            .iter()
            .flat_map(|(s, e)| build_intervals(&events[*s..=*e], IDLE))
            .map(|i| i.id)
            .collect();
        chunked.sort();

        assert_eq!(
            single, chunked,
            "chunked union == single window (lossless, no double count)"
        );
    }

    #[test]
    fn manual_session_surfaces_note_label_and_activity() {
        // A manual session whose start carries note/label/activity: build_sessions
        // surfaces note + label (first non-null across events), and build_intervals
        // threads the session's activity onto its counted gap.
        let mut start = ev("m", 0, EventKind::ManualStart, "p");
        start.note = Some("Meeting with Fol".into());
        start.label = Some("standup".into());
        start.activity = Some("meeting".into());
        let stop = ev("m", 60, EventKind::ManualStop, "p");
        let batch = build_batch(&[start, stop], &[], &[], "d", IDLE, AGENT, NOW);

        let sess = batch
            .sessions
            .iter()
            .find(|s| s.session_id == "m")
            .expect("manual session rolled up");
        assert_eq!(sess.note.as_deref(), Some("Meeting with Fol"));
        assert_eq!(sess.label.as_deref(), Some("standup"));
        assert!(
            batch
                .intervals
                .iter()
                .any(|i| i.activity.as_deref() == Some("meeting")),
            "activity threads onto the interval"
        );

        // An agent session carries none of them.
        let a = ev("a", 0, EventKind::UserPrompt, "p");
        let b = ev("a", 30, EventKind::SessionEnd, "p");
        let abatch = build_batch(&[a, b], &[], &[], "d", IDLE, AGENT, NOW);
        if let Some(asess) = abatch.sessions.iter().find(|s| s.session_id == "a") {
            assert!(asess.note.is_none() && asess.label.is_none());
        }
    }

    // --- issue #40: history-aware build_sessions ---------------------------------

    #[test]
    fn history_merges_prompts_branch_started_at_into_ended_rollup() {
        // The window carries ONLY the bare SessionEnd; everything that makes the
        // rollup meaningful — its start, branch, and prompts — lives in history
        // from earlier flush windows only.
        let history = vec![
            ev_b("s1", 0, EventKind::SessionStart, "p", "main"),
            ev_b("s1", 10, EventKind::UserPrompt, "p", "main"),
            ev("s1", 20, EventKind::PreTool, "p"),
            ev("s1", 30, EventKind::PostTool, "p"),
            ev_b("s1", 40, EventKind::UserPrompt, "p", "main"),
        ];
        let window = vec![ev("s1", 600, EventKind::SessionEnd, "p")];

        // Go through the full chunked-build path (not `build_sessions` directly) so
        // this also exercises `assemble_batch`'s degenerate-session retain filter:
        // pre-fix, a window slice that's just a bare SessionEnd has no agent
        // activity and no interval-engaged time of its own, so the old (window-
        // only) rollup would have been silently dropped by that filter.
        let chunks =
            build_chunked_batches(&window, &[], &[], &[], "d", IDLE, AGENT, NOW, &[], &history);
        assert_eq!(chunks.len(), 1);
        let sessions = &chunks[0].batch.sessions;
        assert_eq!(
            sessions.len(),
            1,
            "the rollup must exist at all — regression for the degenerate-session \
             retain filter, which previously dropped a rollup whose window slice \
             (just the bare SessionEnd) had no activity of its own"
        );
        let s = &sessions[0];
        assert_eq!(
            s.prompts,
            Some(2),
            "two UserPrompt events across history → prompts = 2"
        );
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.started_at, fmt(OffsetDateTime::UNIX_EPOCH));
        assert!(
            s.agent_wall_seconds > 0,
            "agent wall-clock computed over the full history-merged span, not just \
             the bare SessionEnd"
        );
    }

    #[test]
    fn history_never_marks_live_session_ended() {
        // A stale SessionEnd (e.g. a Claude Code compaction end) sits ONLY in
        // history; the session is still live — its WINDOW slice carries no end at
        // all. History must never be able to terminate it.
        let window = vec![
            ev("s1", 100, EventKind::UserPrompt, "p"),
            ev("s1", 110, EventKind::PreTool, "p"),
        ];
        let history = vec![ev("s1", 50, EventKind::SessionEnd, "p")];
        let sessions = build_sessions(&window, &history, AGENT);
        assert!(
            sessions.iter().all(|s| s.session_id != "s1"),
            "a SessionEnd sitting only in history must never terminate a live session"
        );
    }

    #[test]
    fn history_dedup_by_event_id_no_double_count() {
        // The history query is session-scoped, not id-range-scoped, so it
        // naturally returns the window's own rows again (same ids) plus a
        // genuinely older event. The window copies must dedup by id, not
        // double-count.
        let window = vec![
            ev("s1", 100, EventKind::UserPrompt, "p"),
            ev("s1", 200, EventKind::SessionEnd, "p"),
        ];
        let history = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"), // genuinely older — not a dup
            window[0].clone(),
            window[1].clone(),
        ];
        let sessions = build_sessions(&window, &history, AGENT);
        let s = sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("session rolled up");
        assert_eq!(
            s.prompts,
            Some(2),
            "one window prompt + one distinct older history prompt — the id-\
             duplicated window copies in history must not be double-counted"
        );
    }

    #[test]
    fn chunked_end_chunk_gets_full_history_and_batch_ids_unchanged() {
        // A window that splits into two chunks: a packed CHUNK_EVENTS-sized burst
        // (carrying the session's start/branch/prompt), then a > idle gap, then a
        // short tail chunk that carries the SessionEnd.
        let mut window: Vec<RawEvent> = Vec::new();
        window.push(ev_b("s1", 0, EventKind::SessionStart, "p", "main"));
        window.push(ev_b("s1", 1, EventKind::UserPrompt, "p", "main"));
        for secs in 2..CHUNK_EVENTS as i64 {
            window.push(ev("s1", secs, EventKind::PreTool, "p"));
        }
        // > idle (5 min = 300s) gap from the burst's last event (@999).
        window.push(ev("s1", 1300, EventKind::PreTool, "p"));
        window.push(ev("s1", 1305, EventKind::SessionEnd, "p"));

        let with_history =
            build_chunked_batches(&window, &[], &[], &[], "d", IDLE, AGENT, NOW, &[], &window);
        let without_history =
            build_chunked_batches(&window, &[], &[], &[], "d", IDLE, AGENT, NOW, &[], &[]);

        assert_eq!(
            with_history.len(),
            2,
            "expected the burst+tail split into exactly two chunks"
        );
        assert_eq!(without_history.len(), 2);

        // Chunk 1 (the burst) carries no SessionEnd in its own slice, so it never
        // emits a rollup for s1 — history or not.
        assert!(with_history[0]
            .batch
            .sessions
            .iter()
            .all(|s| s.session_id != "s1"));
        assert!(without_history[0]
            .batch
            .sessions
            .iter()
            .all(|s| s.session_id != "s1"));

        // Chunk 2 (the tail) carries the SessionEnd. WITH history, the rollup
        // aggregates the FULL session: the burst's prompt and start branch.
        let s_with = with_history[1]
            .batch
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("chunk 2 rolls up s1 with history");
        assert_eq!(
            s_with.prompts,
            Some(1),
            "the single UserPrompt lives in the burst chunk's slice"
        );
        assert_eq!(s_with.branch.as_deref(), Some("main"));
        assert_eq!(s_with.started_at, fmt(OffsetDateTime::UNIX_EPOCH));

        // WITHOUT history, the same chunk only sees its own 2-event tail slice —
        // the pre-#40 tail-only behavior this fix supersedes.
        let s_without = without_history[1]
            .batch
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("chunk 2 still rolls up s1 without history (it has its own activity)");
        assert_eq!(s_without.prompts, Some(0));
        assert_eq!(s_without.branch, None);
        assert_ne!(
            s_without.started_at, s_with.started_at,
            "without history, started_at falls back to the tail chunk's own first event"
        );

        // Batch ids (and, transitively, intervals) must be BYTE-IDENTICAL whether
        // or not history is supplied — history feeds ONLY build_sessions, never
        // build_intervals_seeded or batch_id_for_chunk.
        for i in 0..2 {
            assert_eq!(
                with_history[i].batch.batch_id, without_history[i].batch.batch_id,
                "chunk {i}'s batch_id must be unaffected by history"
            );
        }
    }

    #[test]
    fn history_ended_at_is_latest_end() {
        // A compaction-triggered SessionEnd sits in history at t=100; the real
        // end arrives in the window at t=500. `ended_at` must be the LATEST end
        // (t=500), not the stale compaction one.
        let history = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 100, EventKind::SessionEnd, "p"), // stale compaction end
        ];
        let window = vec![ev("s1", 500, EventKind::SessionEnd, "p")]; // the real end
        let sessions = build_sessions(&window, &history, AGENT);
        let s = sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("session rolled up");
        assert_eq!(
            s.ended_at,
            Some(fmt(OffsetDateTime::UNIX_EPOCH + Duration::seconds(500))),
            "ended_at must be the temporally-latest end, not the stale history one"
        );
    }
}
