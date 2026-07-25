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
    let ev = match msg {
        EventMsg::Raw(ev) => *ev,
        EventMsg::Hook { norm, harness, at } => enrich(norm, harness, at, cache),
    };

    // Capture-time coalescing (Phase 2a): drop a tool-activity event when the
    // session's last *stored* activity is younger than the coalesce window.
    // Only the high-volume PreTool/PostTool pair is eligible — human signals,
    // lifecycle, Stop, and CwdChanged are always stored. `coalesce < idle`
    // (clamped in Config::coalesce) guarantees every surviving gap stays under
    // the idle threshold, so accounting::active_seconds is preserved.
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

    // Periodic ingest heartbeat (6d): one info line per 256 stored events keeps
    // the steady-state log quiet while still surfacing volume + the live
    // coalescing ratio and an approximate queue depth.
    if *events_ingested % 256 == 0 {
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
/// Deliberately narrow: only `PreTool`/`PostTool`, which `dira init` hooks on
/// *every* tool call. Human signals (UserPrompt, PermissionDecision, ManualTick),
/// lifecycle (SessionStart/End, ManualStart/Stop), the agent-pause `Stop`, and
/// `CwdChanged` are never coalesced — they're low-volume and load-bearing for
/// accounting and project resolution.
fn coalesces(kind: EventKind) -> bool {
    matches!(kind, EventKind::PreTool | EventKind::PostTool)
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
async fn capture_tokens(
    state: &AppState,
    transcript_path: &str,
    harness: Harness,
    session_id: &str,
    project: Option<&str>,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

    let offset_key = format!("token_offset:{session_id}");
    let stored_offset: u64 = match state.store.meta_get(&offset_key).await {
        Ok(Some(s)) => s.parse().unwrap_or(0),
        Ok(None) => 0,
        Err(e) => {
            tracing::debug!("token offset read failed ({session_id}): {e}");
            0
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

    // Truncated/rotated transcript (compaction): the file is shorter than where we
    // last read, so the old offset is meaningless — start over from the top.
    let start = if len < stored_offset {
        0
    } else {
        stored_offset
    };
    if len == start {
        return; // nothing appended since the last capture
    }
    if start > 0 {
        if let Err(e) = file.seek(SeekFrom::Start(start)).await {
            tracing::debug!("transcript seek failed ({transcript_path}): {e}");
            return;
        }
    }
    let mut tail = String::new();
    if let Err(e) = file.read_to_string(&mut tail).await {
        tracing::debug!("transcript read failed ({transcript_path}): {e}");
        return;
    }

    let turns = match harness {
        Harness::Grok => dira_core::tokens::parse_grok_updates_usage(&tail),
        _ => dira_core::tokens::parse_transcript_usage(&tail),
    };
    let mut captured = 0usize;
    for t in &turns {
        match state.store.upsert_token_usage(t, session_id, project).await {
            Ok(()) => captured += 1,
            Err(e) => tracing::warn!("token upsert failed: {e}"),
        }
    }
    // Advance the watermark to the byte after the final newline in the tail, so
    // the next read starts on a clean line boundary (any partial trailing line is
    // re-read next time — uuid dedup makes that overlap harmless). If the tail has
    // no newline at all, leave the offset where it was and re-read it next call.
    let new_offset = tail
        .rfind('\n')
        .map(|i| start + i as u64 + 1)
        .unwrap_or(start);
    if new_offset > start {
        if let Err(e) = state
            .store
            .meta_set(&offset_key, &new_offset.to_string())
            .await
        {
            tracing::debug!("token offset write failed ({session_id}): {e}");
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
