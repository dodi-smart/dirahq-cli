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
    now: OffsetDateTime,
) -> AttestationBatch {
    build_batch_with_partials(events, token_rows, artifact_rows, &[], device_id, idle, now)
}

/// Like [`build_batch`], but also emits a *partial* [`SessionRollup`]
/// (`ended_at: None`) for each [`PartialSession`] — a long-running session that
/// has not ended yet (Phase 6c). See [`PartialSession`] for the cloud UPSERT
/// contract this relies on.
///
/// A partial is suppressed when an *ended* rollup for the same `session_id` is
/// already in this batch (the session ended in this very window): the final
/// rollup supersedes the partial, so shipping both would be redundant.
pub fn build_batch_with_partials(
    events: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    partials: &[PartialSession],
    device_id: &str,
    idle: Duration,
    now: OffsetDateTime,
) -> AttestationBatch {
    let batch_id = batch_id_for(events, artifact_rows);
    assemble_batch(
        events,
        token_rows,
        artifact_rows,
        partials,
        device_id,
        idle,
        now,
        batch_id,
    )
}

/// Assemble one [`AttestationBatch`] from a window of events/tokens/artifacts/
/// partials with a caller-supplied `batch_id`. Shared by the single-window builder
/// ([`build_batch_with_partials`]) and the chunked builder
/// ([`build_chunked_batches`]) so the fact-derivation is identical either way.
#[allow(clippy::too_many_arguments)]
fn assemble_batch(
    events: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    partials: &[PartialSession],
    device_id: &str,
    idle: Duration,
    now: OffsetDateTime,
    batch_id: String,
) -> AttestationBatch {
    let intervals = build_intervals(events, idle);
    let mut sessions = build_sessions(events, idle);
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
/// final chunk additionally carries the token rows, artifacts, and partial rollups
/// (the cloud dedups tokens by id, artifacts by sha, sessions by id), so their
/// cursors advance only once the whole window has drained.
#[allow(clippy::too_many_arguments)]
pub fn build_chunked_batches(
    events: &[RawEvent],
    token_rows: &[TokenRow],
    artifact_rows: &[ArtifactRow],
    partials: &[PartialSession],
    device_id: &str,
    idle: Duration,
    now: OffsetDateTime,
) -> Vec<ChunkBatch> {
    if events.is_empty() {
        // Artifact/partial-only flush — one batch, no event cursor to advance.
        let batch = assemble_batch(
            &[],
            token_rows,
            artifact_rows,
            partials,
            device_id,
            idle,
            now,
            batch_id_for_chunk(&[], artifact_rows),
        );
        return vec![ChunkBatch {
            batch,
            cursor_event_id: None,
            is_last: true,
        }];
    }

    let ranges = chunk_ranges(events, idle, CHUNK_EVENTS);
    let n = ranges.len();
    ranges
        .into_iter()
        .enumerate()
        .map(|(i, (s, e))| {
            let is_last = i == n - 1;
            let chunk = &events[s..=e];
            let (toks, arts, parts): (&[TokenRow], &[ArtifactRow], &[PartialSession]) =
                if is_last {
                    (token_rows, artifact_rows, partials)
                } else {
                    (&[], &[], &[])
                };
            let batch = assemble_batch(
                chunk,
                toks,
                arts,
                parts,
                device_id,
                idle,
                now,
                batch_id_for_chunk(chunk, arts),
            );
            ChunkBatch {
                batch,
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

/// A human signal carrying the session it came from, so a counted gap can be
/// attributed back to a `source_session` (which `accounting::Signal` omits).
struct TaggedSignal {
    at: OffsetDateTime,
    project: Option<String>,
    session_id: String,
}

/// De-duplicated, idle-trimmed intervals, coalescing adjacent counted gaps that
/// share the same `(project, source_session)`.
fn build_intervals(events: &[RawEvent], idle: Duration) -> Vec<Interval> {
    // Collect human signals tagged with their session, then sort exactly as
    // `accounting::counted_gaps` does (by time, stable on ties) so our gap
    // stream is identical to the reporting one — same de-duplicated seconds.
    let mut tagged: Vec<TaggedSignal> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| TaggedSignal {
            at: e.at,
            project: e.project.clone(),
            session_id: e.session_id.clone(),
        })
        .collect();
    tagged.sort_by_key(|s| s.at);

    // A resolved fallback email per session, used to satisfy the contract's
    // non-empty `identity_email` requirement on each interval.
    let emails = session_emails(events);
    // First non-null activity per session (manual sessions classify their time —
    // "meeting", "qa", … — which the cloud uses for billing assurance).
    let activities = session_activities(events);

    let mut out: Vec<Interval> = Vec::new();
    for pair in tagged.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let delta = b.at - a.at;
        if delta <= Duration::ZERO || delta > idle {
            continue;
        }
        let human = (b.at - a.at).whole_seconds().max(0) as u64;

        // Coalesce into the previous interval when it abuts and shares the same
        // (project, source_session) — the same attribution key the gap opens on.
        if let Some(last) = out.last_mut() {
            if last.ended_at == fmt(a.at)
                && last.repo_canonical == a.project
                && last.source_session == a.session_id
            {
                last.ended_at = fmt(b.at);
                last.human_seconds += human;
                continue;
            }
        }

        out.push(Interval {
            // Placeholder — the id is content-derived in a final pass below, AFTER
            // coalescing settles each interval's started_at/ended_at.
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
    // no recovery path can ever double-count human_seconds. Computed last so the
    // value reflects each interval's coalesced started_at/ended_at, not a pre-merge
    // snapshot.
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
fn build_sessions(events: &[RawEvent], idle: Duration) -> Vec<SessionRollup> {
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
        event_times: Vec<OffsetDateTime>,
        /// First non-null free-text note / operational label across the session's
        /// events (manual sessions; agent sessions leave these None).
        note: Option<String>,
        label: Option<String>,
    }

    let mut sessions: BTreeMap<&str, Acc> = BTreeMap::new();
    for e in events {
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
        entry.event_times.push(e.at);
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
                crate::accounting::active_seconds(&a.event_times, idle).max(0) as u64
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

/// Deterministic batch id over the window's event-id range, so a crash-retry of
/// the *same* window produces the *same* `batchId` and the cloud no-ops on it
/// (idempotent ingest) instead of creating a duplicate attestation.
///
/// Events are ULIDs, so the (min, max) id pair plus the count pins the window.
/// We hash that into a 128-bit value and render it as a ULID for a wire id that
/// is the right shape and collision-resistant across distinct windows.
///
/// Artifact shas are folded in too, so an *artifact-only* flush (no new events,
/// e.g. a commit landed between sessions) still gets a distinct id rather than
/// colliding with the empty-event hash and being dedup'd away by the cloud.
fn batch_id_for(events: &[RawEvent], artifacts: &[ArtifactRow]) -> String {
    // Two independent FNV-1a passes give us 128 bits without an extra dep.
    let mut ids: Vec<&str> = events
        .iter()
        .map(|e| e.id.as_str())
        .chain(artifacts.iter().map(|a| a.sha.as_str()))
        .collect();
    ids.sort_unstable();
    let lo = fnv1a(&ids, 0xcbf29ce484222325);
    let hi = fnv1a(&ids, 0x100000001b3);
    let value = ((hi as u128) << 64) | lo as u128;
    Ulid::from(value).to_string()
}

/// Like [`batch_id_for`], but stamps the ULID's 48-bit timestamp with the chunk's
/// **maximum covered event-id time** (the rest of the id stays the content hash).
///
/// This makes `max(batchId)` — the value the cloud keeps as a device's persisted
/// watermark — monotonic in covered event time, so it becomes comparable to the
/// daemon's event cursor and `dira device status` can say "in sync / cloud behind"
/// honestly. Determinism is preserved: the same chunk event-set yields the same
/// timestamp AND the same content hash. Falls back to the pure content hash for an
/// artifact-only chunk (no events ⇒ no covered time).
fn batch_id_for_chunk(events: &[RawEvent], artifacts: &[ArtifactRow]) -> String {
    let mut ids: Vec<&str> = events
        .iter()
        .map(|e| e.id.as_str())
        .chain(artifacts.iter().map(|a| a.sha.as_str()))
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
        Some(ts) => Ulid::from(((ts as u128) << 80) | (content & ((1u128 << 80) - 1)))
            .to_string(),
        None => Ulid::from(content).to_string(),
    }
}

fn fnv1a(ids: &[&str], seed: u64) -> u64 {
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
            identity_email: Some("dev@acme.com".to_string()),
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    const IDLE: Duration = Duration::minutes(5);
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
        let batch = build_batch(&events, &[], &[], "d", IDLE, NOW);
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

        let report = report::build(&events, IDLE);
        let batch = build_batch(&events, &[], &[], "01DEVICE", IDLE, NOW);

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
    fn coalesces_adjacent_gaps_for_one_session() {
        // One session, prompts at 0/30/60 — three signals, two abutting 30s gaps
        // that share (project, session) and collapse into a single 60s interval.
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, NOW);
        assert_eq!(batch.intervals.len(), 1);
        assert_eq!(batch.intervals[0].human_seconds, 60);
        assert_eq!(batch.intervals[0].source_session, "s1");
        assert_eq!(batch.intervals[0].repo_canonical.as_deref(), Some("p"));
        assert_eq!(batch.intervals[0].identity_email, "dev@acme.com");
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
        let batch = build_batch(&events, &[], &[], "d", IDLE, NOW);
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
        let batch = build_batch(&events, &[], &[], "d", IDLE, NOW);
        assert_eq!(batch.sessions.len(), 1);
        let s = &batch.sessions[0];
        assert_eq!(s.session_id, "s1");
        assert_eq!(s.agent_wall_seconds, 20); // span 0..20 with activity
        assert!(s.ended_at.is_some());
        assert_eq!(s.identity_email, "dev@acme.com");
    }

    #[test]
    fn agent_wall_is_idle_trimmed_not_raw_span() {
        // A session opened, a short burst of activity (0..30s), then a long idle
        // gap (~1h), then a final end event. The raw span is ~3690s, but the
        // idle-trimmed active time is only the 30s burst — the dead hour is not
        // counted toward agent wall-clock.
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::PreTool, "p"),
            ev("s1", 30, EventKind::PostTool, "p"),
            ev("s1", 30 + 3600, EventKind::SessionEnd, "p"),
        ];
        let batch = build_batch(&events, &[], &[], "d", IDLE, NOW);
        assert_eq!(batch.sessions.len(), 1);
        let s = &batch.sessions[0];
        // started_at / ended_at remain first / last as before.
        assert_eq!(s.started_at, fmt(OffsetDateTime::UNIX_EPOCH));
        assert!(s.ended_at.is_some());
        // active = 0..10 + 10..30 = 30s; the >5min gap to the end is trimmed.
        assert_eq!(s.agent_wall_seconds, 30);
    }

    #[test]
    fn manual_session_kind_is_manual() {
        let mut start = ev("m1", 0, EventKind::ManualStart, "p");
        start.harness = Harness::Manual;
        let mut stop = ev("m1", 60, EventKind::ManualStop, "p");
        stop.harness = Harness::Manual;
        let batch = build_batch(&[start, stop], &[], &[], "d", IDLE, NOW);
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
        let batch = build_batch(&[], &rows, &[], "d", IDLE, NOW);
        assert_eq!(batch.token_usage.len(), 1);
        let t = &batch.token_usage[0];
        assert_eq!(t.id, "t1");
        assert_eq!(t.repo_canonical.as_deref(), Some("p"));
        assert_eq!(t.input, 10);
        assert_eq!(t.est_cost_usd, Some(0.5));
    }

    #[test]
    fn artifacts_map_to_contract_with_sha_as_id() {
        let rows = vec![ArtifactRow {
            sha: "abc123".into(),
            repo: Some("github.com/acme/api".into()),
            git_ref: Some("main".into()),
            kind: "commit".into(),
            authored_at: Some("2026-06-27T10:00:00Z".into()),
            author_email: Some("dev@acme.com".into()),
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
        let batch = build_batch(&[], &[], &rows, "d", IDLE, NOW);
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
        assert_eq!(a.author_email.as_deref(), Some("dev@acme.com"));
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
        let batch = build_batch(&[start, work, end], &[], &[], "d", IDLE, NOW);
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
        let batch = build_batch(&[start, w1, w2, end], &[], &[], "d", IDLE, NOW);
        assert_eq!(batch.sessions.len(), 1);
        assert_eq!(batch.sessions[0].branch.as_deref(), Some("feat/x"));
    }

    #[test]
    fn batch_id_is_deterministic_for_same_window() {
        let events = vec![
            ev("s1", 0, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::UserPrompt, "p"),
        ];
        let a = build_batch(&events, &[], &[], "d", IDLE, NOW);
        let b = build_batch(&events, &[], &[], "d", IDLE, NOW);
        assert_eq!(a.batch_id, b.batch_id, "same window ⇒ same batch id");

        // A different window ⇒ a different id.
        let mut more = events.clone();
        more.push(ev("s1", 60, EventKind::UserPrompt, "p"));
        let c = build_batch(&more, &[], &[], "d", IDLE, NOW);
        assert_ne!(a.batch_id, c.batch_id);
    }

    #[test]
    fn empty_window_is_an_empty_batch() {
        let batch = build_batch(&[], &[], &[], "d", IDLE, NOW);
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
            identity_email: Some("dev@acme.com".into()),
            started_at: OffsetDateTime::UNIX_EPOCH,
            active_seconds: 4242,
            prompts: Some(7),
            branch: Some("feat/x".into()),
            note: None,
            label: None,
        }];
        let batch = build_batch_with_partials(&window, &[], &[], &partials, "d", IDLE, NOW);
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
        assert_eq!(s.identity_email, "dev@acme.com");
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
            identity_email: Some("dev@acme.com".into()),
            started_at: OffsetDateTime::UNIX_EPOCH,
            active_seconds: 999,
            prompts: None,
            branch: None,
            note: None,
            label: None,
        }];
        let batch = build_batch_with_partials(&window, &[], &[], &partials, "d", IDLE, NOW);
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
            identity_email: Some("dev@acme.com".into()),
            started_at: OffsetDateTime::UNIX_EPOCH,
            active_seconds: 0,
            prompts: None,
            branch: None,
            note: None,
            label: None,
        }];
        let batch = build_batch_with_partials(&[], &[], &[], &partials, "d", IDLE, NOW);
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
        let mut single: Vec<String> =
            build_intervals(&events, IDLE).into_iter().map(|i| i.id).collect();
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
        let batch = build_batch(&[start, stop], &[], &[], "d", IDLE, NOW);

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
        let abatch = build_batch(&[a, b], &[], &[], "d", IDLE, NOW);
        if let Some(asess) = abatch.sessions.iter().find(|s| s.session_id == "a") {
            assert!(asess.note.is_none() && asess.label.is_none());
        }
    }
}
