//! The single writer task: drains the ingest queue, enriches hooks, appends to
//! the store, and folds each event into the live registry.
//!
//! **Hot-path rule:** this loop must never block. Enrichment (git project
//! resolution) is cheap and cached; the store append is local SQLite. The one
//! historically dangerous operation — shelling out to `git` for commit capture —
//! is now spawned detached and time-boxed in [`crate::capture`], so a wedged git
//! can never stall the drain loop (and with it every session's `active_seconds`
//! accrual + the `ManualTick` queue). See [`crate::capture`] for the why.

use crate::capture::{self, Throttle};
use crate::state::{AppState, EventMsg};
use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use dira_core::project::{self, Resolved};
use futures::FutureExt as _;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use time::{Duration, OffsetDateTime};
use tokio::sync::mpsc;
use ulid::Ulid;

/// Bounded ingest queue. Full = drop (we never stall the agent loop). Generous so
/// bursts of subagent tool calls don't trip it.
pub const QUEUE_CAPACITY: usize = 4096;

/// How a commit-bearing event triggers a capture. Production uses
/// [`capture::spawn_capture`] (detached, time-boxed). Tests inject a fake to
/// simulate a slow/hung git and prove it can't stall the drain loop.
pub type CaptureFn = fn(&AppState, &str, &str);

/// Drain the ingest queue with the production capture (detached + time-boxed).
pub async fn writer(rx: mpsc::Receiver<EventMsg>, state: AppState) {
    writer_with(rx, state, capture::spawn_capture).await
}

/// Drain the ingest queue: enrich hook events with a resolved project (cached),
/// append to the log, fold into the live registry, and trigger commit capture on
/// commit-bearing events. `capture_fn` is the seam tests use to inject a slow git.
///
/// **Panic isolation (WP-B7).** This is the sole ingest receiver — it can't be
/// re-spawned with a fresh channel (see `supervisor.rs`), so a bug tripped by
/// one malformed/unusual message must never take down accrual for every other
/// message. Each iteration wraps the entire per-message body — compute, the
/// store append, and the coalescing/registry bookkeeping, all in
/// [`process_message`] — in `catch_unwind`. A caught panic drops that one
/// message, logs loudly, bumps `writer_panics`, and the loop moves straight on
/// to the next message. See [`process_message`]'s doc comment for the
/// accounting-ordering invariant this preserves between the store, the
/// coalescing watermark, and the live registry.
pub async fn writer_with(mut rx: mpsc::Receiver<EventMsg>, state: AppState, capture_fn: CaptureFn) {
    let mut cache: HashMap<String, Resolved> = HashMap::new();
    // Per-repo throttle so a burst of tool calls doesn't shell out to git on every
    // event. The baseline check inside capture is already cheap, but this caps it.
    let mut throttle = Throttle::default();
    // Per-session timestamp of the last *stored* tool-activity event. Used to
    // coalesce the high-volume PreTool/PostTool stream at capture time (the
    // biggest lever on event volume). Lives in the single-threaded writer, so no
    // lock is needed; a daemon bounce just restarts coalescing cleanly.
    let mut last_stored_activity: HashMap<String, OffsetDateTime> = HashMap::new();
    let coalesce = state.config.coalesce();
    let idle = state.config.idle();
    // Lightweight ingest observability (6d): how many events we've stored vs
    // dropped to capture-time coalescing, logged periodically rather than per
    // event so the log stays quiet under a tool-call burst.
    let mut events_ingested: u64 = 0;
    let mut events_coalesced: u64 = 0;
    while let Some(msg) = rx.recv().await {
        // Cheap context captured BEFORE `msg` is consumed, so a caught panic
        // below can still log which kind of event / session it was processing
        // even though `process_message` took the message by value.
        let (kind_hint, session_hint) = match &msg {
            EventMsg::Raw(ev) => (ev.kind, ev.session_id.clone()),
            EventMsg::Hook { norm, .. } => (norm.kind, norm.session_id.clone()),
        };

        let outcome = AssertUnwindSafe(process_message(
            &state,
            msg,
            &mut cache,
            &mut last_stored_activity,
            coalesce,
            idle,
            capture_fn,
            &mut throttle,
            &mut events_ingested,
            &mut events_coalesced,
        ))
        .catch_unwind()
        .await;

        if let Err(panic) = outcome {
            state.progress.mark_writer_panic();
            tracing::error!(
                kind = ?kind_hint,
                session = %session_hint,
                panic = %panic_message(panic),
                "writer: message processing panicked — event dropped, writer continues"
            );
        }
    }
}

/// Process one drained message end-to-end: enrich (if a hook), decide
/// capture-time coalescing, append to the store, fold into the live registry,
/// and best-effort trigger sync / commit-capture / token-capture. Called from
/// [`writer_with`] wrapped in `catch_unwind`.
///
/// **Accounting-ordering invariant (WP-B7).** The store append is the one
/// durable, irreversible thing this function does, and everything before it —
/// enrichment, project resolution, the coalescing *decision* — is pure
/// computation over already-received data. The coalescing *watermark*
/// (`last_stored_activity`) and the live registry (`observe`) are only ever
/// mutated AFTER that append has already returned `Ok`, and with nothing
/// fallible in between the append and those two in-memory updates. So a caught
/// panic can only land in one of two places:
///   - before the append: nothing persisted, the watermark/registry are
///     untouched, and the message is cleanly dropped (no double-store, no
///     stuck watermark); or
///   - inside the trivial map-insert/registry-fold pair right after the
///     append: not expected in practice (no fallible I/O, no `unwrap` on
///     external input, just arithmetic + `HashMap` ops), and even if it did
///     happen, it self-heals — the registry is a pure fold over the store's
///     log, fully reconstructed by the hydrate replay on the next daemon
///     restart (see `state.rs`'s `SessionRegistry::observe` docs).
///
/// Either way, a panic here can never cause a double-store or a silently-stuck
/// coalescing watermark. Sync-trigger / commit-capture / token-capture run
/// last, deliberately after the accounting-critical section: they're already
/// best-effort and idempotent (a missed nudge or a dropped capture just
/// retries on the next qualifying event), so they carry no ordering constraint.
#[allow(clippy::too_many_arguments)]
async fn process_message(
    state: &AppState,
    msg: EventMsg,
    cache: &mut HashMap<String, Resolved>,
    last_stored_activity: &mut HashMap<String, OffsetDateTime>,
    coalesce: Duration,
    idle: Duration,
    capture_fn: CaptureFn,
    throttle: &mut Throttle,
    events_ingested: &mut u64,
    events_coalesced: &mut u64,
) {
    // For hooks at a turn boundary, remember the transcript (and which harness
    // wrote it) so we can capture token usage after the event is logged (off
    // the response path).
    let transcript = match &msg {
        EventMsg::Hook { norm, harness, .. } if captures_tokens(norm.kind) => {
            norm.transcript_path.clone().map(|p| (p, *harness))
        }
        _ => None,
    };
    // Why the harness says this session began or ended, when it says at all:
    // Claude Code's `SessionStart.source` and `SessionEnd.reason`. They are
    // mutually exclusive — one per lifecycle kind — so they collapse to a single
    // value. Nothing accounts on it; it exists because without it a launcher
    // spawn and a real session are indistinguishable at the ingress, which is
    // what kept issue #74 invisible.
    //
    // Extracted only for lifecycle events, so the high-volume tool-call path
    // pays nothing for a field it can never carry.
    let lifecycle_why = match &msg {
        EventMsg::Hook { norm, .. }
            if matches!(norm.kind, EventKind::SessionStart | EventKind::SessionEnd) =>
        {
            norm.source.clone().or_else(|| norm.reason.clone())
        }
        _ => None,
    };
    let ev = match msg {
        EventMsg::Raw(ev) => *ev,
        EventMsg::Hook { norm, harness, at } => enrich(norm, harness, at, cache),
    };

    // Capture-time coalescing (Phase 2a): drop a tool-activity event when the
    // session's last *stored* activity is younger than the coalesce window.
    // Only the high-volume `PostTool` is eligible — human signals, lifecycle,
    // Stop, CwdChanged and `PreTool` are always stored. `PreTool` is excluded
    // because it is the sole opener of an agent span; see `coalesces`.
    //
    // Note this no longer keeps every surviving gap under the idle threshold —
    // a coalesced-away `PostTool` can leave a wider one. That is fine and
    // deliberate: human accounting is unaffected (it counts only human signals),
    // and agent accounting now clamps rather than discards, so a wide gap is
    // credited up to its ceiling instead of being thrown away.
    //
    // This is a READ-ONLY decision over `last_stored_activity` — see the
    // ordering invariant above for why the watermark itself may only be
    // mutated once the store append below has actually succeeded.
    let watermark = match coalesce_decision(last_stored_activity, &ev, coalesce) {
        Watermark::Drop => {
            // Dropped to capture-time coalescing. Approx queue depth = capacity
            // minus the free slots reported by the sender.
            *events_coalesced += 1;
            let queue_depth = QUEUE_CAPACITY.saturating_sub(state.tx.capacity());
            tracing::debug!(
                events_coalesced = *events_coalesced,
                queue_depth,
                kind = ?ev.kind,
                "ingest: coalesced tool-activity event"
            );
            return; // too soon since the last stored activity — drop it
        }
        watermark => watermark,
    };

    // Issue #74: the desktop app spawns short-lived harness processes that each get
    // a fresh session id, so a `SessionStart`/`SessionEnd` pair with nothing between
    // it is the most common thing in the log — 113 of 126 sessions on one observed
    // day, making the events table ~90% noise. The terminal event is the first
    // moment we can be sure the session showed nothing, and the registry (a pure
    // fold over this same log) is the cheapest place to ask.
    //
    // Guarded on the registry KNOWING the session: hydrate replays only the last
    // day, so an unknown session is "no opinion", not "degenerate", and takes the
    // normal path. `ManualStop` is deliberately not a trigger — a manual session is
    // an explicit `dira start`, and `ManualStop` is itself a human signal, so it
    // could never be degenerate anyway.
    if ev.kind == EventKind::SessionEnd
        && crate::control::lock_recover(&state.sessions).is_degenerate(&ev.session_id)
    {
        match state.store.delete_session_events(&ev.session_id).await {
            Ok(deleted) => {
                crate::control::lock_recover(&state.sessions).forget(&ev.session_id);
                tracing::debug!(
                    session_id = %ev.session_id,
                    deleted,
                    reason = lifecycle_why.as_deref().unwrap_or("-"),
                    "ingest: pruned a session that never showed an activity signal"
                );
                return; // the pair is gone; storing this event would recreate it
            }
            // Best-effort: a failed delete must not lose the event, so fall through
            // and store it the ordinary way.
            Err(e) => tracing::warn!("prune of degenerate session failed: {e}"),
        }
    }

    if let Err(e) = state.store.append(&ev).await {
        tracing::warn!("append failed: {e}");
        return; // not stored — the watermark/registry stay untouched too
    }
    *events_ingested += 1;
    // Watchdog progress: record that the writer drained a message just now, so
    // the supervisor can tell a stalled writer from a quiet one.
    state.progress.mark_writer();

    // Apply the watermark and fold into the registry immediately after the
    // append succeeds — nothing fallible runs between them. See the
    // accounting-ordering invariant in this function's doc comment.
    apply_watermark(last_stored_activity, watermark);
    crate::control::lock_recover(&state.sessions).observe(&ev, idle);

    // Surface the harness's own account of a lifecycle transition. This is the
    // only place `SessionStart.source` is observable — the prune below runs on
    // `SessionEnd`, where `source` is by definition absent — and telling a
    // `startup` apart from a `resume`/`compact` is what makes a flood of
    // short-lived launcher sessions legible in the log (issue #74).
    if let Some(why) = lifecycle_why.as_deref() {
        tracing::debug!(
            session_id = %ev.session_id,
            kind = ?ev.kind,
            why,
            "ingest: harness reported why a session started or ended"
        );
    }

    // Periodic ingest heartbeat (6d): one info line per 256 stored events keeps
    // the steady-state log quiet while still surfacing volume + the live
    // coalescing ratio and an approximate queue depth.
    if events_ingested.is_multiple_of(256) {
        let queue_depth = QUEUE_CAPACITY.saturating_sub(state.tx.capacity());
        tracing::info!(
            events_ingested = *events_ingested,
            events_coalesced = *events_coalesced,
            queue_depth,
            queue_capacity = QUEUE_CAPACITY,
            "ingest: progress"
        );
    }
    // Nudge the sync task on lifecycle boundaries — the points where a window
    // becomes worth shipping (a session just ended, or an agent paused). The
    // send is non-blocking; a full channel just means a flush is already
    // pending, and the periodic backstop covers any missed nudge.
    if triggers_sync(ev.kind) {
        let _ = state.sync.trigger.try_send(());
        // Wake the heartbeat instantly out of a (possibly deep-idle) sleep so
        // this lifecycle boundary is reflected in presence without waiting out
        // the cadence (WP-A3).
        state.presence_wake.notify_waiters();
    }
    // Remember a working dir for this repo so the idle ticker can re-poll it,
    // then capture commits at the points one likely just landed (a tool call
    // returned, an agent paused, or a session/manual session closed). The
    // capture is spawned detached + time-boxed, so it never blocks this loop.
    if let (Some(cwd), Some(proj)) = (ev.cwd.as_deref(), ev.project.as_deref()) {
        crate::control::lock_recover_map(&state.repo_dirs)
            .insert(proj.to_string(), cwd.to_string());
        if capture::captures_commits(ev.kind) && throttle.ready(proj) {
            capture_fn(state, cwd, proj);
        }
    }
    if let Some((path, harness)) = transcript {
        capture_tokens(state, &path, harness, &ev.session_id, ev.project.as_deref()).await;
        // Claude Code writes `Task`-tool subagent turns to sibling
        // `agent-*.jsonl` files that no hook ever names, so the main transcript
        // alone misses them entirely. Attributed to the parent session — the id
        // dedup keeps the sweep idempotent, and each sidecar carries its own
        // watermark so an unchanged one costs a single `metadata` call.
        //
        // Claude only: the sidecar convention is Claude Code's, and grok's
        // transcript is a single `updates.jsonl` with a different layout.
        if harness == Harness::ClaudeCode {
            for sidecar in subagent_transcripts(&path) {
                capture_tokens(
                    state,
                    &sidecar,
                    harness,
                    &ev.session_id,
                    ev.project.as_deref(),
                )
                .await;
            }
        }
    }
}

/// What (if anything) the coalescing watermark map needs once an event's fate
/// is known. Computed by [`coalesce_decision`] (read-only) and applied by
/// [`apply_watermark`] — split in two so [`process_message`] can run the store
/// append between "decide" and "mutate" (the ordering invariant above).
#[derive(Debug, Clone)]
enum Watermark {
    /// Too soon since this session's last stored activity — drop the event
    /// before it ever reaches the store.
    Drop,
    /// Record `at` as this session's newest stored tool-activity timestamp.
    Set(String, OffsetDateTime),
    /// The session closed — forget its watermark so the map can't grow
    /// unbounded over a long-lived daemon.
    Clear(String),
    /// Not a coalescing-eligible or lifecycle event — nothing to update.
    None,
}

/// Read-only coalescing decision over `last` (the per-session last-*stored*-
/// activity watermark). Mirrors the pre-WP-B7 `keep_event`'s logic exactly, but
/// without the mutation — see [`Watermark`] for why that's split out.
fn coalesce_decision(
    last: &HashMap<String, OffsetDateTime>,
    ev: &RawEvent,
    coalesce: Duration,
) -> Watermark {
    if coalesces(ev.kind) && coalesce > Duration::ZERO {
        if let Some(prev) = last.get(&ev.session_id) {
            if ev.at - *prev < coalesce {
                return Watermark::Drop;
            }
        }
        Watermark::Set(ev.session_id.clone(), ev.at)
    } else if matches!(ev.kind, EventKind::SessionEnd | EventKind::ManualStop) {
        Watermark::Clear(ev.session_id.clone())
    } else {
        Watermark::None
    }
}

/// Apply a [`Watermark`] decided by [`coalesce_decision`] to the map. Only ever
/// called once the corresponding event is durably stored (or wasn't eligible
/// for coalescing in the first place).
fn apply_watermark(last: &mut HashMap<String, OffsetDateTime>, watermark: Watermark) {
    match watermark {
        Watermark::Set(session, at) => {
            last.insert(session, at);
        }
        Watermark::Clear(session) => {
            last.remove(&session);
        }
        Watermark::None | Watermark::Drop => {}
    }
}

/// Best-effort human-readable panic payload, for the loud `tracing::error!` on
/// a caught per-message panic.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The high-volume tool-activity events eligible for capture-time coalescing.
///
/// Only `PostTool`. Human signals (UserPrompt, PermissionDecision, ManualTick),
/// lifecycle (SessionStart/End, ManualStart/Stop), the agent-pause `Stop`, and
/// `CwdChanged` are never coalesced — they're low-volume and load-bearing for
/// accounting and project resolution.
///
/// `PreTool` used to be eligible too, and dropping it silently destroyed the
/// agent time it was opening. `PreTool` is the *only* event that sets
/// `EventKind::opens_agent_span`, which is what lets a long tool call be
/// credited in full rather than clamped to `agent_idle_seconds`. Coalescing it
/// away left the preceding `PostTool` as the gap's opener, so:
///
/// ```text
/// T=0      PostTool stored            (opens_span = false)
/// T=30s    PreTool for a 2h build     DROPPED — within coalesce (45s)
/// T=2h30s  closing PostTool stored
///          → gap keyed on a non-opener → clamped to 300s, not 2 hours
/// ```
///
/// Two hours of real work banked five minutes — the exact failure mode the
/// clamp-not-discard fix set out to end, reachable whenever a tool call starts
/// within `coalesce_seconds` of the previous activity, i.e. the common case in a
/// burst. `opens_agent_span` is deliberately keyed on the opener so a *lost*
/// `PostToolUse` cannot zero the work; the coalescer was dropping the opener
/// itself, through the same door.
///
/// The volume argument still holds for `PostTool`, which fires at the same rate,
/// so the cap on stored rows is halved rather than removed.
fn coalesces(kind: EventKind) -> bool {
    matches!(kind, EventKind::PostTool)
}

/// Hooks that mark the end of an agent turn — good points to (re-)read the
/// transcript and capture token usage. Idempotent upserts make re-reads cheap.
fn captures_tokens(kind: EventKind) -> bool {
    matches!(kind, EventKind::Stop | EventKind::SessionEnd)
}

/// Lifecycle events that should nudge a cloud flush: a session bookend or an
/// agent pause. Frequent enough that the cloud stays fresh, sparse enough that
/// we don't flush on every tool call (the debounce + backstop handle the rest).
fn triggers_sync(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::SessionStart
            | EventKind::SessionEnd
            | EventKind::ManualStart
            | EventKind::ManualStop
            | EventKind::Stop
    )
}

/// Turn a normalized hook into a full event, resolving its project off the hot path.
fn enrich(
    norm: dira_sources::Normalized,
    harness: Harness,
    at: OffsetDateTime,
    cache: &mut HashMap<String, Resolved>,
) -> RawEvent {
    let resolved = norm.cwd.as_ref().map(|cwd| {
        cache
            .entry(cwd.clone())
            .or_insert_with(|| project::resolve(std::path::Path::new(cwd)))
            .clone()
    });

    // The session branch is volatile (a checkout can change it within a session),
    // so resolve it live per event rather than caching it with the project.
    let branch = norm
        .cwd
        .as_ref()
        .and_then(|cwd| project::current_branch(std::path::Path::new(cwd)));

    RawEvent {
        id: Ulid::new().to_string(),
        at,
        session_id: norm.session_id,
        harness,
        kind: norm.kind,
        cwd: norm.cwd,
        project: resolved.as_ref().and_then(|r| r.project.clone()),
        identity_email: resolved.as_ref().and_then(|r| r.identity_email.clone()),
        branch,
        tool: norm.tool,
        label: None,
        activity: None,
        note: None,
    }
}

/// Tail-read whichever per-session file the harness's hook reported —
/// Claude's JSONL transcript, grok's `updates.jsonl` — and upsert each turn's
/// token usage.
///
/// Reads only the *appended tail* since the last capture: a per-session byte
/// offset is kept in `meta` (`token_offset:<session_id>`) so a long-running
/// transcript isn't re-parsed whole on every turn (which is O(n²) over a
/// session). On each call we seek to the stored offset, parse from there to EOF,
/// upsert, then advance the offset to the start of the last fully-written line
/// (the byte after the final `\n`). Stopping at a line boundary means the next
/// read never begins mid-record, so a turn written across two captures is never
/// dropped. If the file shrank below the stored offset (truncation/rotation, e.g.
/// a compaction), we reset to 0 and re-read. This offset/dedup machinery is
/// shared across harnesses; only the per-line parser below is harness-specific.
///
/// `harness` selects the parser: grok turns come from
/// `dira_core::tokens::parse_grok_updates_usage` and are keyed by the update's
/// `eventId`, everything else uses `dira_core::tokens::parse_transcript_usage`
/// (Claude's transcript uuid).
///
/// id dedup (`upsert ON CONFLICT DO NOTHING`) is the correctness backstop, so a
/// small overlap at the offset boundary is harmless. Best-effort throughout: any
/// IO error logs at debug and leaves the offset untouched (the next capture
/// retries from the same point).
/// Is this transcript a subagent sidecar rather than the session's own file?
///
/// Claude Code writes `Task`-tool subagent turns to `agent-<id>.jsonl` beside the
/// main `<session-uuid>.jsonl`. The name is the only signal available — the hook
/// payload never distinguishes them.
fn is_subagent_transcript(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("agent-"))
}

/// The `meta` key holding the byte watermark for one transcript FILE.
///
/// The watermark is a byte offset into a specific file, but the key used to be
/// keyed on `session_id` alone. That was safe only while exactly one file was
/// ever read per session. A subagent sidecar carries the *parent's* session id,
/// so sharing the key would make each capture seek into one file using the
/// other's offset: the `len < stored_offset` guard resets to 0, a full re-read
/// follows, and the smaller file's offset is then written back under the shared
/// key — leaving the main transcript to restart far too early, forever.
///
/// The session's own transcript keeps the legacy key so existing watermarks stay
/// valid and no install re-reads a multi-megabyte transcript on upgrade; only
/// sidecars get the qualified form.
/// The `meta` key holding the prologue fingerprint that says WHICH file
/// [`offset_key_for`]'s byte offset belongs to. Sibling of the offset key, same
/// suffix, so `nuke` clears both with one `LIKE` each and a sidecar keeps its
/// own.
fn fp_key_for(session_id: &str, transcript_path: &str) -> String {
    let offset = offset_key_for(session_id, transcript_path);
    format!(
        "token_fp:{}",
        offset.strip_prefix("token_offset:").unwrap_or(session_id)
    )
}

/// Hash of the file's first `<=FP_PROLOGUE` bytes.
///
/// A transcript is append-only in practice, so its prologue is stable for the
/// life of the file and changes exactly when the file is replaced — which is the
/// one thing a length comparison cannot see. Returns `None` when the prologue
/// cannot be read, and a `None` never invalidates a stored offset: an unreadable
/// prologue must not trigger a multi-megabyte re-read.
async fn transcript_fingerprint(file: &mut tokio::fs::File, len: u64) -> Option<u64> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    const FP_PROLOGUE: usize = 4096;
    if len == 0 {
        return None;
    }
    file.seek(SeekFrom::Start(0)).await.ok()?;
    let mut buf = vec![0u8; FP_PROLOGUE.min(len as usize)];
    file.read_exact(&mut buf).await.ok()?;
    Some(dira_core::sync::fnv1a_bytes(&buf, 0xcbf29ce484222325))
}

fn offset_key_for(session_id: &str, transcript_path: &str) -> String {
    let path = std::path::Path::new(transcript_path);
    if is_subagent_transcript(path) {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("subagent");
        format!("token_offset:{session_id}:{stem}")
    } else {
        format!("token_offset:{session_id}")
    }
}

/// Sibling `agent-*.jsonl` transcripts beside the session's own, oldest-first.
///
/// Claude Code writes subagent turns to separate files that no hook ever names,
/// so without this sweep their usage never enters the store on any platform —
/// measured at 840 turns / 85M tokens / ~$158 in a single week on one machine.
/// Directory reads are cheap and happen only at a turn boundary; each file
/// carries its own watermark, so an unchanged sidecar costs one `metadata` call.
fn subagent_transcripts(main: &str) -> Vec<String> {
    let Some(dir) = std::path::Path::new(main).parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            is_subagent_transcript(p) && p.extension().and_then(|x| x.to_str()) == Some("jsonl")
        })
        .filter_map(|p| p.to_str().map(str::to_string))
        .collect();
    // Deterministic order so a capture sweep is reproducible.
    found.sort();
    found
}

async fn capture_tokens(
    state: &AppState,
    transcript_path: &str,
    harness: Harness,
    session_id: &str,
    project: Option<&str>,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

    let offset_key = offset_key_for(session_id, transcript_path);
    let fp_key = fp_key_for(session_id, transcript_path);
    let stored_offset: u64 = match state.store.meta_get(&offset_key).await {
        Ok(Some(s)) => s.parse().unwrap_or(0),
        Ok(None) => 0,
        Err(e) => {
            // `warn`, not `debug`: a watermark we cannot read means this session
            // re-imports its whole transcript on every turn boundary, and the
            // default filter (`dirad=info,warn`) hides `debug` entirely.
            tracing::warn!("token offset read failed ({session_id}): {e}");
            0
        }
    };
    let stored_fp: Option<u64> = match state.store.meta_get(&fp_key).await {
        Ok(v) => v.and_then(|s| s.parse().ok()),
        Err(e) => {
            tracing::warn!("token fingerprint read failed ({session_id}): {e}");
            None
        }
    };

    let mut file = match tokio::fs::File::open(transcript_path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("transcript unreadable ({transcript_path}): {e}");
            return;
        }
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::debug!("transcript metadata failed ({transcript_path}): {e}");
            return;
        }
    };

    // The file's prologue identifies WHICH file this offset belongs to. Length
    // alone cannot: a transcript replaced by a different file of equal-or-greater
    // length passes a `len < stored_offset` check, and the seek then lands
    // mid-record in unrelated content — silently, and for good.
    let fresh_fp = transcript_fingerprint(&mut file, len).await;

    // Truncated/rotated transcript (compaction): the file is shorter than where we
    // last read, so the old offset is meaningless — start over from the top. Same
    // for a file whose prologue changed under a still-valid offset. An ABSENT
    // fingerprint means an install upgrading into this check: accept the offset
    // and record the fingerprint below, rather than re-reading megabytes.
    let swapped = matches!((stored_fp, fresh_fp), (Some(a), Some(b)) if a != b);
    let start = if len < stored_offset || swapped {
        0
    } else {
        stored_offset
    };
    if len == start && !swapped {
        // Nothing appended since the last capture. Still record the fingerprint
        // if this is the first pass that computed one, so the NEXT swap is caught.
        if stored_fp.is_none() {
            if let Some(fp) = fresh_fp {
                if let Err(e) = state.store.meta_set(&fp_key, &fp.to_string()).await {
                    tracing::warn!("token fingerprint write failed ({session_id}): {e}");
                }
            }
        }
        return;
    }
    if let Err(e) = file.seek(SeekFrom::Start(start)).await {
        tracing::warn!("transcript seek failed ({transcript_path}): {e}");
        return;
    }
    // Bytes, not `read_to_string`. That call is all-or-nothing on UTF-8
    // validity, so a single invalid byte anywhere in the tail aborted the whole
    // capture and returned BEFORE advancing the watermark — leaving the session
    // to re-read the identical bad tail on every later turn boundary, forever.
    // A lossy decode costs the one corrupt line instead (the per-line JSON parse
    // skips it) and keeps everything after it reachable.
    let mut raw = Vec::new();
    if let Err(e) = file.read_to_end(&mut raw).await {
        tracing::warn!("transcript read failed ({transcript_path}): {e}");
        return;
    }
    let tail = String::from_utf8_lossy(&raw);

    let turns = match harness {
        Harness::Grok => dira_core::tokens::parse_grok_updates_usage(&tail),
        _ => dira_core::tokens::parse_transcript_usage(&tail),
    };
    let mut captured = 0usize;
    for t in &turns {
        // Leave a trace when a model family has no bundled price, so a sonnet-rate
        // estimate for an unrecognised model is noticeable rather than silent.
        dira_core::tokens::warn_if_unpriced(&t.model);
        match state.store.upsert_token_usage(t, session_id, project).await {
            Ok(()) => captured += 1,
            Err(e) => tracing::warn!("token upsert failed: {e}"),
        }
    }
    // Advance the watermark to the byte after the final newline in the tail, so
    // the next read starts on a clean line boundary (any partial trailing line is
    // re-read next time — uuid dedup makes that overlap harmless). If the tail has
    // no newline at all, leave the offset where it was and re-read it next call.
    //
    // Searched over the RAW bytes, never the decoded string: `from_utf8_lossy`
    // substitutes a 3-byte U+FFFD for each 1–3 invalid bytes, so an index into
    // `tail` is not a byte offset into the file once anything was replaced.
    let new_offset = raw
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| start + i as u64 + 1)
        .unwrap_or(start);
    if new_offset > start {
        if let Err(e) = state
            .store
            .meta_set(&offset_key, &new_offset.to_string())
            .await
        {
            // `warn`, not `debug`: this is the write whose silent failure makes a
            // watermark stop advancing with nothing to show for it.
            tracing::warn!("token offset write failed ({session_id}): {e}");
        }
        if let Some(fp) = fresh_fp {
            if let Err(e) = state.store.meta_set(&fp_key, &fp.to_string()).await {
                tracing::warn!("token fingerprint write failed ({session_id}): {e}");
            }
        }
    }
    if captured > 0 {
        tracing::debug!(turns = captured, session = %session_id, "captured token usage");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::accounting::active_seconds;

    fn tool_ev(session: &str, at: OffsetDateTime, kind: EventKind) -> RawEvent {
        RawEvent {
            id: Ulid::new().to_string(),
            at,
            session_id: session.to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some("github.com/acme/api".into()),
            identity_email: None,
            branch: None,
            tool: Some("Bash".into()),
            label: None,
            activity: None,
            note: None,
        }
    }

    /// A `PreTool` opening a long tool call must survive coalescing, even when it
    /// lands well inside the coalesce window.
    ///
    /// `PreTool` is the only event that sets `opens_agent_span`. Dropping it left
    /// the previous `PostTool` as the gap's opener, so the tool call was clamped
    /// to `agent_idle_seconds` instead of credited in full: a 2-hour build banked
    /// 5 minutes. This asserts both the survival and the resulting credit.
    #[test]
    fn a_pretool_opening_a_long_build_survives_coalescing_and_is_credited() {
        use dira_core::accounting::{agent_active_seconds, AgentPolicy, AgentSample};

        let coalesce = Duration::seconds(45);
        let t0 = OffsetDateTime::now_utc();
        let mut last = HashMap::new();

        // A fast tool call lands first, so the session's watermark is fresh.
        assert!(keep_event(
            &mut last,
            &tool_ev("s1", t0, EventKind::PostTool),
            coalesce
        ));
        // 30s later — inside the 45s window — a PreTool opens a 2-hour build.
        let pre_at = t0 + Duration::seconds(30);
        assert!(
            keep_event(
                &mut last,
                &tool_ev("s1", pre_at, EventKind::PreTool),
                coalesce
            ),
            "a span-opening PreTool must never be coalesced away"
        );
        // The closing PostTool, 2 hours on.
        let post_at = pre_at + Duration::hours(2);
        assert!(keep_event(
            &mut last,
            &tool_ev("s1", post_at, EventKind::PostTool),
            coalesce
        ));

        // With the PreTool stored, the 2h gap is keyed on an opener and credited
        // in full. Without it the same span clamps to agent_idle (300s).
        let policy = AgentPolicy::default();
        let stored = [
            AgentSample {
                at: t0,
                opens_span: false,
            },
            AgentSample {
                at: pre_at,
                opens_span: true,
            },
            AgentSample {
                at: post_at,
                opens_span: false,
            },
        ];
        assert_eq!(
            agent_active_seconds(&stored, policy),
            30 + 7200,
            "the full build must be credited"
        );

        let coalesced_away = [
            AgentSample {
                at: t0,
                opens_span: false,
            },
            AgentSample {
                at: post_at,
                opens_span: false,
            },
        ];
        assert_eq!(
            agent_active_seconds(&coalesced_away, policy),
            policy.idle.whole_seconds(),
            "sanity: dropping the opener is what used to clamp 2h to the idle ceiling"
        );
    }

    /// The watermark is a byte offset into a FILE, so two files read for the same
    /// session must never share a key. A sidecar carries the parent's session id,
    /// so a shared key would make each capture seek into one file using the
    /// other's offset and thrash both forever.
    #[test]
    fn a_sidecar_never_shares_a_watermark_with_the_session_transcript() {
        let main = offset_key_for("sess-1", "/p/sess-1.jsonl");
        let side = offset_key_for("sess-1", "/p/agent-abc.jsonl");
        assert_ne!(main, side);
        // The session's own transcript keeps the legacy key, so upgrading does not
        // orphan existing watermarks and re-read multi-megabyte transcripts.
        assert_eq!(main, "token_offset:sess-1");
        assert_eq!(side, "token_offset:sess-1:agent-abc");
        // Two different sidecars are also distinct from each other.
        assert_ne!(side, offset_key_for("sess-1", "/p/agent-def.jsonl"));
        // `nuke` clears these with `LIKE 'token_offset:%'` — both forms match.
        assert!(main.starts_with("token_offset:") && side.starts_with("token_offset:"));
    }

    #[test]
    fn subagent_discovery_finds_siblings_and_ignores_the_main_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("11111111-2222.jsonl");
        for name in [
            "11111111-2222.jsonl",
            "agent-aaa.jsonl",
            "agent-bbb.jsonl",
            "agent-notes.md",
            "other.jsonl",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let found = subagent_transcripts(main.to_str().unwrap());
        let names: Vec<&str> = found
            .iter()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            names,
            vec!["agent-aaa.jsonl", "agent-bbb.jsonl"],
            "only agent-*.jsonl siblings, sorted, never the main transcript"
        );
    }

    #[test]
    fn subagent_discovery_is_quiet_on_a_missing_directory() {
        assert!(subagent_transcripts("/definitely/not/here/s.jsonl").is_empty());
    }

    fn turn_line(uuid: &str, model: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","timestamp":"2026-06-27T15:18:07.732Z","message":{{"model":"{model}","usage":{{"input_tokens":2,"output_tokens":{output},"cache_read_input_tokens":1000,"cache_creation_input_tokens":10}}}}}}"#
        )
    }

    async fn state_in_memory() -> AppState {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, Default::default()).await.unwrap();
        state
    }

    /// Subagent turns live in files no hook ever names. Capturing them must add
    /// their usage without moving the main transcript's watermark — the two are
    /// byte offsets into different files.
    #[tokio::test]
    async fn a_sidecar_is_captured_without_disturbing_the_main_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("sess-1.jsonl");
        let side = dir.path().join("agent-aaa.jsonl");
        // The main transcript is deliberately the LONGER file: under the old
        // shared key the sidecar's much smaller offset was written back over it,
        // which is the thrash this separation prevents.
        std::fs::write(
            &main,
            format!(
                "{}\n{}\n",
                turn_line("main-1", "claude-opus-4-8", 100),
                turn_line("main-2", "claude-opus-4-8", 200)
            ),
        )
        .unwrap();
        std::fs::write(
            &side,
            format!("{}\n", turn_line("sub-1", "claude-haiku", 5)),
        )
        .unwrap();

        let state = state_in_memory().await;
        let (m, s) = (main.to_str().unwrap(), side.to_str().unwrap());

        capture_tokens(&state, m, Harness::ClaudeCode, "sess-1", None).await;
        let main_offset = state
            .store
            .meta_get("token_offset:sess-1")
            .await
            .unwrap()
            .unwrap();

        for sidecar in subagent_transcripts(m) {
            capture_tokens(&state, &sidecar, Harness::ClaudeCode, "sess-1", None).await;
        }

        // All three turns are stored — the subagent's usage is no longer invisible.
        assert_eq!(state.store.count_token_usage_after(None).await.unwrap(), 3);

        // The main watermark is untouched by the sidecar capture, and the sidecar
        // has its own.
        assert_eq!(
            state
                .store
                .meta_get("token_offset:sess-1")
                .await
                .unwrap()
                .unwrap(),
            main_offset,
            "capturing a sidecar must not rewind the main transcript"
        );
        assert!(state
            .store
            .meta_get("token_offset:sess-1:agent-aaa")
            .await
            .unwrap()
            .is_some());

        // Re-running captures nothing new: both watermarks hold.
        capture_tokens(&state, m, Harness::ClaudeCode, "sess-1", None).await;
        capture_tokens(&state, s, Harness::ClaudeCode, "sess-1", None).await;
        assert_eq!(state.store.count_token_usage_after(None).await.unwrap(), 3);
    }

    /// One invalid byte used to stall a session's token capture FOREVER.
    ///
    /// `read_to_string` is all-or-nothing on UTF-8: it returned `InvalidData`,
    /// `capture_tokens` returned early, and the watermark never advanced — so
    /// every later `Stop` re-read the identical bad tail and failed identically.
    /// Everything after the bad byte, including turns appended days later, was
    /// unreachable. It logged at `debug`, which is off in production.
    #[tokio::test]
    async fn a_corrupt_line_does_not_stall_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess-bad.jsonl");

        // Two good turns, a line carrying a lone 0xFF (invalid UTF-8), then a
        // third good turn after it.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(turn_line("ok-1", "claude-opus-4-8", 10).as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(turn_line("ok-2", "claude-opus-4-8", 20).as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"type":"assistant","uuid":"bad"#);
        bytes.push(0xFF);
        bytes.extend_from_slice(br#""}"#);
        bytes.push(b'\n');
        bytes.extend_from_slice(turn_line("ok-3", "claude-opus-4-8", 30).as_bytes());
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();

        let state = state_in_memory().await;
        let p = path.to_str().unwrap();
        capture_tokens(&state, p, Harness::ClaudeCode, "sess-bad", None).await;

        // The corrupt line is dropped by the per-line JSON parse; the three good
        // turns around it are captured.
        assert_eq!(
            state.store.count_token_usage_after(None).await.unwrap(),
            3,
            "a bad byte must cost one line, not the whole file"
        );

        // And the watermark reached the end of the file, so the session is not
        // wedged. This is the assertion that fails on the old code.
        let offset: u64 = state
            .store
            .meta_get("token_offset:sess-bad")
            .await
            .unwrap()
            .expect("watermark must exist")
            .parse()
            .unwrap();
        assert_eq!(
            offset,
            bytes.len() as u64,
            "the watermark must clear the corrupt line, not stall before it"
        );

        // A turn appended afterwards still arrives — the real cost of the stall.
        let mut appended = bytes.clone();
        appended.extend_from_slice(turn_line("ok-4", "claude-opus-4-8", 40).as_bytes());
        appended.push(b'\n');
        std::fs::write(&path, &appended).unwrap();
        capture_tokens(&state, p, Harness::ClaudeCode, "sess-bad", None).await;
        assert_eq!(state.store.count_token_usage_after(None).await.unwrap(), 4);
    }

    /// Truncation detection was length-only (`len < stored_offset`), so a
    /// transcript REPLACED by a different file of equal-or-greater length passed
    /// the guard and the seek landed mid-record in unrelated content — silently,
    /// and for good. A prologue fingerprint catches the swap.
    #[tokio::test]
    async fn a_replaced_transcript_of_equal_length_is_re_read_from_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess-swap.jsonl");

        let first = format!(
            "{}\n{}\n",
            turn_line("aaa-1", "claude-opus-4-8", 100),
            turn_line("aaa-2", "claude-opus-4-8", 200)
        );
        std::fs::write(&path, &first).unwrap();

        let state = state_in_memory().await;
        let p = path.to_str().unwrap();
        capture_tokens(&state, p, Harness::ClaudeCode, "sess-swap", None).await;
        assert_eq!(state.store.count_token_usage_after(None).await.unwrap(), 2);

        // A DIFFERENT transcript, byte-length identical (same uuid widths, same
        // model, same output widths — only the uuids differ).
        let second = format!(
            "{}\n{}\n",
            turn_line("bbb-1", "claude-opus-4-8", 100),
            turn_line("bbb-2", "claude-opus-4-8", 200)
        );
        assert_eq!(first.len(), second.len(), "the swap must be same-length");
        std::fs::write(&path, &second).unwrap();

        capture_tokens(&state, p, Harness::ClaudeCode, "sess-swap", None).await;
        assert_eq!(
            state.store.count_token_usage_after(None).await.unwrap(),
            4,
            "a same-length replacement must be re-read from byte 0, not skipped"
        );
    }

    /// An install upgrading into the fingerprint must not re-read its
    /// multi-megabyte transcripts: an absent `token_fp:` key means "accept the
    /// offset and record the fingerprint", never "start over".
    #[tokio::test]
    async fn an_upgraded_install_keeps_its_offset_without_a_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess-up.jsonl");
        let body = format!(
            "{}\n{}\n",
            turn_line("old-1", "claude-opus-4-8", 100),
            turn_line("old-2", "claude-opus-4-8", 200)
        );
        std::fs::write(&path, &body).unwrap();

        let state = state_in_memory().await;
        // Pre-seed only the legacy watermark, as an upgrade would leave it: the
        // whole file already consumed, no fingerprint recorded.
        state
            .store
            .meta_set("token_offset:sess-up", &body.len().to_string())
            .await
            .unwrap();

        let p = path.to_str().unwrap();
        capture_tokens(&state, p, Harness::ClaudeCode, "sess-up", None).await;

        assert_eq!(
            state.store.count_token_usage_after(None).await.unwrap(),
            0,
            "an upgrade must not re-import a transcript it already consumed"
        );
        assert!(
            state
                .store
                .meta_get("token_fp:sess-up")
                .await
                .unwrap()
                .is_some(),
            "the fingerprint is recorded on the first pass that sees none"
        );
    }

    /// Test-only compatibility shim reconstructing the pre-WP-B7 `keep_event`
    /// signature/behavior (decide-and-mutate in one call) by composing
    /// [`coalesce_decision`] + [`apply_watermark`], so the coalescing-policy
    /// tests below exercise the exact same decision logic the production path
    /// now runs, unmodified.
    fn keep_event(
        last: &mut HashMap<String, OffsetDateTime>,
        ev: &RawEvent,
        coalesce: Duration,
    ) -> bool {
        match coalesce_decision(last, ev, coalesce) {
            Watermark::Drop => false,
            watermark => {
                apply_watermark(last, watermark);
                true
            }
        }
    }

    /// Tool-spam coalescing: 600 PostTool events 1s apart collapse to a handful
    /// of stored rows, yet `active_seconds` over the *stored* timestamps stays
    /// within the coalesce tolerance of the dense value. This is the Phase 2a
    /// invariant: drop volume, preserve accounting.
    #[test]
    fn coalescing_slashes_rows_but_preserves_active_seconds() {
        let idle = Duration::minutes(5);
        let coalesce = Duration::seconds(45); // < idle, as Config clamps
        let base = OffsetDateTime::UNIX_EPOCH;

        // Dense stream: 600 PostTool, 1s apart -> 600 events, span 599s.
        let dense: Vec<RawEvent> = (0..600)
            .map(|i| tool_ev("spam", base + Duration::seconds(i), EventKind::PostTool))
            .collect();
        let dense_active = active_seconds(&dense.iter().map(|e| e.at).collect::<Vec<_>>(), idle);
        assert_eq!(dense_active, 599);

        // Apply the writer's coalescing decision over the same stream.
        let mut last: HashMap<String, OffsetDateTime> = HashMap::new();
        let stored: Vec<RawEvent> = dense
            .into_iter()
            .filter(|ev| keep_event(&mut last, ev, coalesce))
            .collect();

        // Row count drops dramatically: ~one per 45s window (≈14), not 600.
        assert!(
            stored.len() <= 16,
            "expected a big reduction, got {} rows",
            stored.len()
        );
        let reduction = 1.0 - (stored.len() as f64 / 600.0);
        assert!(reduction > 0.95, "reduction was only {reduction:.3}");

        // active_seconds over the STORED timestamps is within `coalesce` of dense.
        let stored_active = active_seconds(&stored.iter().map(|e| e.at).collect::<Vec<_>>(), idle);
        assert!(stored_active <= dense_active);
        assert!(
            dense_active - stored_active <= 45,
            "active drifted by {}s (> coalesce tolerance)",
            dense_active - stored_active
        );
    }

    #[test]
    fn human_signals_and_lifecycle_are_never_coalesced() {
        let coalesce = Duration::seconds(45);
        let base = OffsetDateTime::UNIX_EPOCH;
        let mut last: HashMap<String, OffsetDateTime> = HashMap::new();
        // Three human signals 1s apart — all kept (never coalesced).
        for i in 0..3 {
            let ev = tool_ev("s", base + Duration::seconds(i), EventKind::UserPrompt);
            assert!(keep_event(&mut last, &ev, coalesce));
        }
        // A Stop right after is also kept (agent pause is load-bearing).
        let stop = tool_ev("s", base + Duration::seconds(3), EventKind::Stop);
        assert!(keep_event(&mut last, &stop, coalesce));
    }

    /// Every session bookend must nudge sync AND wake the heartbeat out of a
    /// (possibly deep-idle) sleep. `ManualStart` was missing: `dira start`
    /// during deep idle left the heartbeat parked for up to the full deep-idle
    /// cadence (~10 min) before presence reflected the new active session.
    #[test]
    fn all_session_bookends_trigger_sync_and_presence_wake() {
        for kind in [
            EventKind::SessionStart,
            EventKind::SessionEnd,
            EventKind::ManualStart,
            EventKind::ManualStop,
            EventKind::Stop,
        ] {
            assert!(
                triggers_sync(kind),
                "{kind:?} is a lifecycle boundary — it must nudge sync and wake presence"
            );
        }
    }

    /// Drive `process_message` over a whole session and report what survived in
    /// the log — the end-to-end shape issue #74 is actually about.
    async fn stored_after(events: &[RawEvent]) -> usize {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let config = dira_core::Config::default();
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        let mut cache = HashMap::new();
        let mut last_stored = HashMap::new();
        let (mut ingested, mut coalesced) = (0u64, 0u64);
        let mut throttle = Throttle::default();
        for ev in events {
            process_message(
                &state,
                EventMsg::Raw(Box::new(ev.clone())),
                &mut cache,
                &mut last_stored,
                Duration::ZERO,
                Duration::minutes(5),
                |_, _, _| {},
                &mut throttle,
                &mut ingested,
                &mut coalesced,
            )
            .await;
        }
        let session = &events[0].session_id;
        state
            .store
            .events_for_sessions(std::slice::from_ref(session), "\u{10FFFF}")
            .await
            .unwrap()
            .len()
    }

    fn lifecycle(session: &str, at: OffsetDateTime, kind: EventKind) -> RawEvent {
        RawEvent {
            tool: None,
            ..tool_ev(session, at, kind)
        }
    }

    /// The reported case: a desktop-app spawn that starts and immediately quits.
    /// Both rows must go — storing either one recreates the phantom session.
    #[tokio::test]
    async fn a_session_that_only_starts_and_ends_leaves_nothing_behind() {
        let t = OffsetDateTime::UNIX_EPOCH;
        let stored = stored_after(&[
            lifecycle("phantom", t, EventKind::SessionStart),
            lifecycle("phantom", t + Duration::seconds(1), EventKind::SessionEnd),
        ])
        .await;
        assert_eq!(stored, 0, "a session with no signal leaves no rows");
    }

    /// The guard against over-deletion: one real signal anywhere in the session
    /// keeps the WHOLE session, bookends included.
    #[tokio::test]
    async fn one_signal_anywhere_keeps_the_whole_session() {
        let t = OffsetDateTime::UNIX_EPOCH;
        let stored = stored_after(&[
            lifecycle("real", t, EventKind::SessionStart),
            tool_ev("real", t + Duration::seconds(1), EventKind::PreTool),
            lifecycle("real", t + Duration::seconds(2), EventKind::SessionEnd),
        ])
        .await;
        assert_eq!(stored, 3, "start + tool call + end all survive");
    }

    /// An agent-only session never submits a prompt. It must be retained in full:
    /// "no human signal" is not "no work".
    #[tokio::test]
    async fn an_agent_only_session_is_retained_in_full() {
        let t = OffsetDateTime::UNIX_EPOCH;
        let stored = stored_after(&[
            lifecycle("agent", t, EventKind::SessionStart),
            tool_ev("agent", t + Duration::seconds(1), EventKind::PreTool),
            tool_ev("agent", t + Duration::seconds(2), EventKind::PostTool),
            lifecycle("agent", t + Duration::seconds(3), EventKind::SessionEnd),
        ])
        .await;
        assert_eq!(stored, 4, "tool calls with no prompt are still real work");
    }

    /// A manual `dira start`/`dira stop` pair is an explicit user action and is
    /// never a prune candidate, even though it is two lifecycle events.
    #[tokio::test]
    async fn a_manual_session_is_never_pruned() {
        let t = OffsetDateTime::UNIX_EPOCH;
        let stored = stored_after(&[
            lifecycle("manual", t, EventKind::ManualStart),
            lifecycle("manual", t + Duration::seconds(1), EventKind::ManualStop),
        ])
        .await;
        assert_eq!(stored, 2, "manual sessions are deliberate, not noise");
    }

    #[test]
    fn zero_coalesce_disables_dropping() {
        let base = OffsetDateTime::UNIX_EPOCH;
        let mut last: HashMap<String, OffsetDateTime> = HashMap::new();
        for i in 0..5 {
            let ev = tool_ev("s", base + Duration::seconds(i), EventKind::PostTool);
            assert!(keep_event(&mut last, &ev, Duration::ZERO));
        }
    }
}
