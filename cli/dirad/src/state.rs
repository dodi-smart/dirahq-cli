//! Shared daemon state and the live session registry.
//!
//! The hot path never touches this beyond a non-blocking channel send. All
//! enrichment (git resolution) and DB writes happen in the single writer task
//! that drains the channel, which also keeps the live registry up to date.

use crate::sync::SyncHandle;
use dira_contract::{Harness, SessionKind};
use dira_core::accounting;
use dira_core::model::{EventKind, RawEvent};
use dira_core::signing::DeviceKey;
use dira_core::sync::CachedBillingSummary;
use dira_core::{Config, Store};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};
use tokio::sync::{mpsc, Notify};

/// A message bound for the writer task.
#[derive(Debug)]
pub enum EventMsg {
    /// A harness hook needing enrichment (project/identity/id) before it's logged.
    Hook {
        norm: dira_sources::Normalized,
        harness: Harness,
        at: OffsetDateTime,
    },
    /// A fully-formed event (manual commands resolve their own project off the hot path).
    Raw(Box<RawEvent>),
}

/// Cloneable daemon handle shared by the HTTP and UDS servers.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub tx: mpsc::Sender<EventMsg>,
    pub sessions: Arc<Mutex<SessionRegistry>>,
    pub config: Config,
    /// Shared `reqwest::Client` for every device→cloud task (heartbeat, sync,
    /// billing, the schema handshake). Built ONCE in `build_state` with a pooled
    /// keep-alive connection so repeat POSTs to the same cloud host reuse the
    /// TCP/TLS connection instead of paying a fresh handshake every tick. Carries
    /// no default timeout — each call site sets its own `RequestBuilder::timeout`
    /// sized to that request (heartbeat/billing/handshake short, sync longer).
    pub http: reqwest::Client,
    /// When this daemon process started, for `dira version` uptime reporting.
    pub started_at: std::time::Instant,
    /// Bearer token required on the loopback HTTP ingress.
    pub bearer: Arc<String>,
    /// Handle to the background cloud-sync task (trigger channel).
    pub sync: SyncHandle,
    /// Handle to the knowledge sync task (M2; consent-gated, own cursors).
    pub knowledge_sync: crate::knowledge_sync::KnowledgeSyncHandle,
    /// Handle to the telemetry sync task (WP2; consent-gated, unsigned
    /// batches). See [`crate::telemetry_sync::TelemetrySyncHandle`] for why
    /// this holds its receiver internally rather than threading it out of
    /// [`crate::build_state`] alongside `sync`/`knowledge_sync`'s.
    pub telemetry_sync: crate::telemetry_sync::TelemetrySyncHandle,
    /// This device's signing key, used to sign attestation batches. Loaded
    /// **lazily** off the startup critical path: the key is only needed for
    /// sync/signing, never to answer a control request, and loading it can block
    /// on a keychain unlock prompt. The control socket binds and answers
    /// `Ping`/`Status` long before this resolves. Access via
    /// [`AppState::device_key`].
    ///
    /// An `RwLock<Option<..>>` rather than the former `OnceCell` (WP-B1b): a
    /// completed key rotation must invalidate the cache so the very next
    /// sync/heartbeat/billing tick signs with the newly-promoted key instead
    /// of the dead old one — a `OnceCell` can only ever be set once. See
    /// [`AppState::invalidate_device_key`].
    pub device_key: Arc<tokio::sync::RwLock<Option<DeviceKey>>>,
    /// Writer/ticker liveness, for the watchdog supervisor (Commit 1).
    pub progress: Arc<ProgressTracker>,
    /// `false` until the background hydrate finishes replaying the recent log into
    /// the registry (Commit 2). The control socket binds and answers before
    /// hydrate runs, so a `status` issued during warm-up reports `hydrating: true`
    /// and an initially-sparse view that fills in within a frame or two.
    pub hydrated: Arc<AtomicBool>,
    /// Last-seen working dir per canonical repo, so the commit-capture poller has
    /// a filesystem path to run `git` in (the idle ticker re-polls these to catch
    /// manual commits even when no agent events are flowing).
    pub repo_dirs: Arc<Mutex<HashMap<String, String>>>,
    /// Latest presence-ack hints stashed by the heartbeat task from the cloud's
    /// [`dira_contract::PresenceAck`]. Wiring only for the future adaptive
    /// heartbeat (Phase 6): the heartbeat task writes these on each 2xx; nothing
    /// reads them yet. Held as atomics so a reader needs no lock.
    pub presence_hints: Arc<PresenceHints>,
    /// The last successfully-fetched cloud billing summary, served on `status`.
    /// Written by the billing task (which also persists it to the store's meta
    /// table); `None` until the first fetch/hydrate — the renderers omit the
    /// billable footer then. Never hold this lock across an await.
    pub billing: Arc<Mutex<Option<CachedBillingSummary>>>,
    /// Poked by the sync task after a successful flush so the billing task
    /// refreshes shortly after new facts land on the cloud.
    pub billing_refresh: Arc<Notify>,
    /// Fired at the same sites that already fire the sync trigger (writer,
    /// control's manual resync, capture's commit-recorded path) so the heartbeat
    /// loop can wake instantly out of a (potentially long, deep-idle) sleep
    /// instead of waiting out its cadence (WP-A3). Uses `notify_waiters` — the
    /// heartbeat loop spends nearly all its time parked in the
    /// `select! { sleep, wake.notified() }`, so an edge fired while it's briefly
    /// off doing a beat is the acceptable, rare miss (the sleep still bounds the
    /// wait to at most one cadence, and jitter already keeps that bounded).
    pub presence_wake: Arc<Notify>,
    /// Fired (at most once, ever) by `control::handle_conn` after it has
    /// written the response to a `Request::Shutdown` — the in-band,
    /// platform-neutral SIGTERM equivalent `dirad::wait_for_shutdown_signal`
    /// also selects on, required on windows where no SIGTERM exists to send.
    /// Deliberately `notify_one`, not `notify_waiters`, at the call site: a
    /// `Notify` only stores a permit for `notify_one`, so a `Shutdown` request
    /// that lands before `run()` reaches its `select!` (e.g. mid-startup)
    /// still wakes it the instant it starts waiting, instead of the
    /// notification being silently dropped for lack of a parked waiter.
    pub shutdown: Arc<Notify>,
    /// Why the loopback hook ingress is not serving, or `None` when it is
    /// healthy. Set when its port cannot be bound — a conflict is survivable
    /// (the control socket is bound first and stays live), so the daemon runs
    /// **degraded** rather than exiting, and a background task retries until
    /// the port frees and clears this. Surfaced over `DaemonInfo` so
    /// `dira daemon status` can say so instead of the daemon silently
    /// capturing nothing. Never hold this lock across an await. See D-0009.
    pub http_ingress_error: Arc<Mutex<Option<String>>>,
    /// Why the control channel is not in its intended state (windows: the pipe's
    /// security descriptor fell back a rung, and/or dirad is running elevated),
    /// or `None`. Reported on `DaemonInfo` for the same reason as
    /// `http_ingress_error` — D-0009: a daemon that cannot do its job must never
    /// look plainly healthy.
    pub control_channel_warning: Arc<Mutex<Option<String>>>,
    /// The single in-flight `dira doctor --probe` capture probe, or `None`.
    ///
    /// A `std::sync::Mutex` like `sessions`/`repo_dirs`: every hold is a field
    /// read or a swap with no `await` inside, and it is taken via
    /// `control::lock_recover` so a poisoned lock degrades to "stale but
    /// serving" rather than killing the control surface.
    pub probe: Arc<Mutex<Option<crate::probe::ProbeSlot>>>,
}

impl AppState {
    /// The device signing key, loaded on first use and cached. Returns `None`
    /// only if loading the key fails (a logged, non-fatal condition — the device
    /// simply can't sign/sync yet). Kept off the startup critical path so a
    /// keychain prompt never delays control-socket readiness (Commit 2).
    ///
    /// Returns an owned clone (WP-B1b; `DeviceKey` is cheap to clone — see its
    /// doc comment) rather than a borrow tied to the lock guard, so callers
    /// don't hold the `RwLock` across their own signing/HTTP awaits.
    pub async fn device_key(&self) -> Option<DeviceKey> {
        // Fast path: already loaded.
        {
            let guard = self.device_key.read().await;
            if let Some(k) = guard.as_ref() {
                return Some(k.clone());
            }
        }
        // Not loaded (or just invalidated by a promoted rotation) — load under
        // the write lock, double-checking in case another caller raced us here.
        let mut guard = self.device_key.write().await;
        if let Some(k) = guard.as_ref() {
            return Some(k.clone());
        }
        match dira_core::identity::load_or_create_unlinked(&self.store).await {
            Ok(k) => {
                let out = k.clone();
                *guard = Some(k);
                Some(out)
            }
            Err(e) => {
                tracing::warn!("device identity load failed: {e}");
                None
            }
        }
    }

    /// Discard the cached device key so the NEXT [`AppState::device_key`] call
    /// reloads it from the store (WP-B1b). Call this immediately after
    /// promoting a pending rotation key (`dira_core::identity::promote_pending_key`)
    /// so every subsequent sync/heartbeat/billing tick picks up the newly-active
    /// key without requiring a daemon restart.
    pub async fn invalidate_device_key(&self) {
        *self.device_key.write().await = None;
    }

    /// The cloud-link gate every device→cloud task re-checks per tick:
    /// `Some((cloud_url, device_id))` only when a cloud URL is configured AND
    /// the device is linked. Re-reading both each call is the point — `dira
    /// device link` / a config change takes effect without a daemon restart.
    /// `task` names the caller in the read-failure log line.
    pub async fn cloud_link(&self, task: &str) -> Option<(String, String)> {
        let cloud_url = self.config.cloud_url.clone()?;
        match dira_core::identity::device_id(&self.store).await {
            Ok(Some(id)) => Some((cloud_url, id)),
            Ok(None) => None, // not linked yet
            Err(e) => {
                tracing::warn!("{task}: read device_id failed: {e}");
                None
            }
        }
    }
}

/// Liveness timestamps the watchdog reads to tell a stalled task from a quiet one.
///
/// Each is the unix-epoch second of the last forward progress for that task,
/// stored as an atomic so the supervisor reads it without a lock. `0` means
/// "never progressed yet" (just-started). The writer marks progress on every
/// drained message; the idle ticker marks on every tick.
///
/// `started_at` (set at construction) is the watchdog's fallback baseline for
/// the never-progressed case: a task that has made NO progress since daemon
/// start is measured from `started_at`, so a hang inside a task's very first
/// unit of work is just as detectable as one after years of uptime.
#[derive(Debug)]
pub struct ProgressTracker {
    started_at: AtomicI64,
    writer_at: AtomicI64,
    ticker_at: AtomicI64,
    /// Messages whose processing panicked and were caught + dropped by the
    /// writer's per-message `catch_unwind` (WP-B7). Surfaced on `dira status`:
    /// a nonzero count means the writer is silently shedding bad events even
    /// though accrual for everything else kept running — worth investigating,
    /// not itself an outage.
    writer_panics: AtomicU64,
    /// How many times the watchdog has observed the writer stalled (queue
    /// backed up with no progress past the stall threshold). Distinct from
    /// `writer_panics`: a stall means the writer isn't making progress at
    /// all, not that it caught and recovered from a bad message.
    writer_stalls: AtomicU64,
    /// Sync flush attempts/successes/failures since daemon start (WP-B9),
    /// mirroring the writer-health counters above. An "attempt" is every wake
    /// that reaches `sync::flush` (whether or not it finds anything to send);
    /// `successes + failures <= attempts` (a `Skipped` outcome — not
    /// configured/linked yet — counts as neither).
    flush_attempts: AtomicU64,
    flush_successes: AtomicU64,
    flush_failures: AtomicU64,
    /// Token turns stored with `project = NULL` since daemon start (issue #93).
    /// Under D-0026, repo-less compute is neither counted nor shown, so a
    /// nonzero value is usage that has gone invisible — an operator signal, not
    /// itself an outage (mirrors `writer_panics`).
    unattributed_token_rows: AtomicU64,
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self {
            started_at: AtomicI64::new(Self::now()),
            writer_at: AtomicI64::new(0),
            ticker_at: AtomicI64::new(0),
            writer_panics: AtomicU64::new(0),
            writer_stalls: AtomicU64::new(0),
            flush_attempts: AtomicU64::new(0),
            flush_successes: AtomicU64::new(0),
            flush_failures: AtomicU64::new(0),
            unattributed_token_rows: AtomicU64::new(0),
        }
    }
}

impl ProgressTracker {
    fn now() -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }
    /// Record that the writer just drained a message.
    pub fn mark_writer(&self) {
        self.writer_at.store(Self::now(), Ordering::Relaxed);
    }
    /// Record that the idle ticker just ran a tick.
    pub fn mark_ticker(&self) {
        self.ticker_at.store(Self::now(), Ordering::Relaxed);
    }
    /// Record that one message's processing panicked and was dropped.
    pub fn mark_writer_panic(&self) {
        self.writer_panics.fetch_add(1, Ordering::Relaxed);
    }
    /// Record that the watchdog observed the writer stalled.
    pub fn mark_writer_stall(&self) {
        self.writer_stalls.fetch_add(1, Ordering::Relaxed);
    }
    /// Seconds since the writer last made progress, or `None` if it never has.
    pub fn writer_idle_secs(&self) -> Option<i64> {
        Self::elapsed(self.writer_at.load(Ordering::Relaxed))
    }
    /// Seconds since the idle ticker last ran, or `None` if it never has.
    pub fn ticker_idle_secs(&self) -> Option<i64> {
        Self::elapsed(self.ticker_at.load(Ordering::Relaxed))
    }
    /// Watchdog baseline: seconds since the writer last made progress, falling
    /// back to seconds since daemon start when it never has — a hang on the
    /// very first message must be as detectable as any later one.
    pub fn writer_idle_or_start_secs(&self) -> i64 {
        self.writer_idle_secs()
            .unwrap_or_else(|| self.since_start_secs())
    }
    /// Watchdog baseline for the idle ticker; see [`Self::writer_idle_or_start_secs`].
    pub fn ticker_idle_or_start_secs(&self) -> i64 {
        self.ticker_idle_secs()
            .unwrap_or_else(|| self.since_start_secs())
    }
    fn since_start_secs(&self) -> i64 {
        (Self::now() - self.started_at.load(Ordering::Relaxed)).max(0)
    }
    /// Test-only: pretend the daemon started `secs` seconds earlier, so stall
    /// thresholds can be crossed without real sleeps.
    #[cfg(test)]
    pub fn backdate_start_for_test(&self, secs: i64) {
        self.started_at.store(Self::now() - secs, Ordering::Relaxed);
    }
    /// Total messages dropped to a caught per-message panic since daemon start.
    pub fn writer_panics(&self) -> u64 {
        self.writer_panics.load(Ordering::Relaxed)
    }
    /// Total times the watchdog has observed the writer stalled since daemon start.
    pub fn writer_stalls(&self) -> u64 {
        self.writer_stalls.load(Ordering::Relaxed)
    }
    /// Record that `n` token turns were stored with no project this capture pass.
    pub fn mark_unattributed_token_rows(&self, n: u64) {
        self.unattributed_token_rows.fetch_add(n, Ordering::Relaxed);
    }
    /// Total token turns stored with no project since daemon start.
    pub fn unattributed_token_rows(&self) -> u64 {
        self.unattributed_token_rows.load(Ordering::Relaxed)
    }
    /// Record that the sync task attempted a flush (WP-B9).
    pub fn mark_flush_attempt(&self) {
        self.flush_attempts.fetch_add(1, Ordering::Relaxed);
    }
    /// Record that a flush attempt fully succeeded (accepted or a no-op tick).
    pub fn mark_flush_success(&self) {
        self.flush_successes.fetch_add(1, Ordering::Relaxed);
    }
    /// Record that a flush attempt failed (any `SyncError` variant).
    pub fn mark_flush_failure(&self) {
        self.flush_failures.fetch_add(1, Ordering::Relaxed);
    }
    /// Total flush attempts since daemon start.
    pub fn flush_attempts(&self) -> u64 {
        self.flush_attempts.load(Ordering::Relaxed)
    }
    /// Total flush attempts that fully succeeded since daemon start.
    pub fn flush_successes(&self) -> u64 {
        self.flush_successes.load(Ordering::Relaxed)
    }
    /// Total flush attempts that failed since daemon start.
    pub fn flush_failures(&self) -> u64 {
        self.flush_failures.load(Ordering::Relaxed)
    }
    fn elapsed(at: i64) -> Option<i64> {
        if at == 0 {
            None
        } else {
            Some((Self::now() - at).max(0))
        }
    }
}

/// Cloud-advertised presence pacing hints, updated on every successful heartbeat.
///
/// `0` is the sentinel for "unset / no hint" on both fields, so a future adaptive
/// heartbeat reads `Relaxed` and falls back to its configured cadence when zero.
/// This is deliberately lock-free and best-effort — a torn read across the two
/// fields is harmless because each is independently advisory.
#[derive(Debug, Default)]
pub struct PresenceHints {
    /// The TTL the cloud last said it will honor for this device, in seconds.
    pub ttl_secs: AtomicU64,
    /// The cloud's last "wait this many seconds before the next beat" hint, or 0.
    pub next_beat_hint_secs: AtomicU64,
}

/// A session currently or recently live, derived as events are observed.
///
/// Phase 6b adds two rolling counters maintained *incrementally* by [`observe`]
/// as each event is folded in, so the heartbeat reads them straight off the
/// registry instead of re-scanning SQLite every tick:
///
/// - `engaged_seconds` — de-duplicated, idle-trimmed human time attributed to this
///   session by the opening-signal policy: each counted gap on the *merged*
///   all-session signal timeline is credited to the session that opened it. So the
///   per-session values sum to the deduped grand total
///   ([`dira_core::accounting::total_human_seconds`]) and match
///   [`dira_core::accounting::per_key_seconds`] keyed by session — a session with a
///   single prompt is still credited for the gap it opens rather than reading 0.
///   Attribution uses registry-level state, so it lives in [`SessionRegistry`].
/// - `active_seconds` — idle-trimmed active wall time over *all* of the session's
///   event timestamps, the per-session analogue of
///   [`dira_core::accounting::active_seconds`].
///
/// Both are computed by the same gap rule the batch scan uses: add `gap = at -
/// <previous timestamp>` to the running counter iff `0 < gap <= idle` — `active`
/// against the session's own previous event, `engaged` against the previous human
/// signal on the merged timeline. Accumulating consecutive in-window gaps telescopes
/// to the same sum a one-shot sort-and-sum produces (events arrive in non-decreasing
/// `at` order on the writer path). The cold-start hydrate replays the recent log
/// through `observe`, so a daemon bounce reconstructs the same totals.
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub session_id: String,
    pub harness: Harness,
    pub kind: SessionKind,
    pub project: Option<String>,
    pub identity_email: Option<String>,
    pub label: Option<String>,
    /// Activity classification + free-text note for manual sessions (surfaced live
    /// in `dira status`/`watch` and on the synced rollup).
    pub activity: Option<String>,
    pub note: Option<String>,
    pub started_at: OffsetDateTime,
    pub last_event_at: OffsetDateTime,
    pub last_signal_at: Option<OffsetDateTime>,
    pub ended: bool,
    /// Rolling de-duplicated human-engaged seconds attributed to this session by the
    /// opening-signal policy (the gaps this session *opened* on the merged, all-session
    /// timeline). Maintained at the registry level so these per-session values sum to
    /// the deduped grand total — see [`SessionRegistry::observe`].
    pub engaged_seconds: u64,
    /// Rolling idle-trimmed active wall seconds over all this session's events (6b).
    pub active_seconds: u64,
    /// The `at` of the last human-signal event folded in, for the engaged gap math.
    pub last_human_signal_at: Option<OffsetDateTime>,
    /// The `at` of the last event of *any* kind folded in, for the active gap math.
    /// Distinct from `last_event_at` only in intent; kept separate so the gap math
    /// reads from a field the partial-rollup / observe logic never repurposes.
    pub last_active_at: Option<OffsetDateTime>,
    /// Did the event at `last_active_at` open an agent span (a `PreTool`)? The
    /// registry sums gaps incrementally, so it has to remember what opened the
    /// currently-open one to know whether the next gap is a tool call in flight
    /// (credited in full) or an ordinary quiet stretch (clamped).
    pub last_opens_span: bool,
    /// `active_seconds` as of the last partial rollup we emitted for this session,
    /// or `None` if none yet. Used to decide "new activity since the last partial"
    /// (6c) without re-scanning.
    pub last_partial_active_seconds: Option<u64>,
    /// Human prompts (`UserPrompt` events) observed so far.
    pub prompts: u64,
    /// Last resolved (non-null) branch across the session's events. Deliberately
    /// last-wins — a *live* session's current branch is the honest answer for a
    /// partial rollup — unlike `build_sessions`' start-branch-with-frequency-
    /// fallback policy for ended rollups, whose terminal write overwrites this
    /// anyway (issue #40).
    pub branch: Option<String>,
    /// Has this session ever shown a *real* signal — a human signal or agent
    /// activity — as opposed to nothing but lifecycle noise (issue #74)?
    ///
    /// Sticky for the entry's lifetime: once a session has done something it stays
    /// real, so Claude Code's mid-conversation compaction `SessionEnd` can never
    /// make a working session look degenerate. False means the session is made of
    /// `SessionStart`/`SessionEnd`/`CwdChanged` and nothing else — no observable
    /// work by any measure the daemon has.
    ///
    /// Deliberately the UNION of `is_human_signal` and `is_agent_activity`, so an
    /// agent-only session (tool calls, no prompt — e.g. a headless run) counts as
    /// real on its very first tool call.
    pub had_signal: bool,
}

impl LiveSession {
    /// A session is idle if no human signal has arrived within the idle window.
    pub fn is_idle(&self, now: OffsetDateTime, idle: time::Duration) -> bool {
        match self.last_signal_at {
            Some(t) => now - t > idle,
            None => true,
        }
    }

    /// A session is *stale* once no event of any kind has arrived for `after`.
    ///
    /// Distinct from [`Self::is_idle`], which is about human attention within a live
    /// session. Staleness is about the harness process being gone: not every session
    /// gets a `SessionEnd` (a crashed or force-quit harness never sends one), and
    /// `ended` is not a latch, so without this bound such a session is reported as
    /// live forever — observed in production as sessions idle for 10–31 hours still
    /// listed under "Right now" (issue #74).
    ///
    /// This only gates *reporting*: the entry stays in the registry, so if events
    /// resume, [`SessionRegistry::observe`] finds it again with `started_at` and every
    /// counter intact.
    pub fn is_stale(&self, now: OffsetDateTime, after: time::Duration) -> bool {
        now - self.last_event_at > after
    }

    /// Short, user-facing handle (the ULID tail for manual sessions).
    pub fn handle(&self) -> String {
        let s = &self.session_id;
        if s.len() > 6 {
            s[s.len() - 6..].to_string()
        } else {
            s.clone()
        }
    }
}

/// The set of concurrently-tracked sessions.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<String, LiveSession>,
    /// `at` of the most recent human signal across *all* sessions, and the session
    /// that fired it. Together they drive the de-duplicated, opening-signal
    /// attribution of `engaged_seconds` (below): each counted gap is credited to the
    /// session that *opened* it, so the per-session counters sum to the deduped grand
    /// total instead of each counting its own signals in isolation.
    last_human_signal_at: Option<OffsetDateTime>,
    last_human_signal_session: Option<String>,
    /// Ceilings for agent wall-clock. Held on the registry rather than passed per
    /// event because it is configuration, not a property of an event — and
    /// because `observe` is called from a dozen places that would otherwise all
    /// have to thread it through. `Default` matches `Config`'s defaults, so a
    /// registry built without one still behaves exactly like a shipped daemon.
    agent: accounting::AgentPolicy,
}

impl SessionRegistry {
    /// A registry using `agent` for wall-clock ceilings. `build_state` passes
    /// `Config::agent_policy()`; `Default` is for tests.
    pub fn with_agent_policy(agent: accounting::AgentPolicy) -> Self {
        Self {
            agent,
            ..Self::default()
        }
    }

    /// Fold an appended event into the live registry, maintaining the rolling
    /// `engaged_seconds` / `active_seconds` counters with `idle` as the gap
    /// threshold (Phase 6b). Pass the same `idle` the accounting core uses
    /// (`Config::idle()`); see [`LiveSession`] for why the incremental sum equals
    /// the one-shot accounting scan.
    pub fn observe(&mut self, ev: &RawEvent, idle: Duration) {
        // A `dira doctor --probe` row is a transport test, not a session, and
        // the registry is not backed by SQL — `partial_rollups` ships straight
        // from here and `build_session_views` renders from here, so the store's
        // prefix filters cannot reach either. Refusing it at the single entry
        // point covers both the writer's live fold and hydrate's replay.
        //
        // The writer already returns early for probe rows; this is the guard
        // that makes the property hold rather than depend on that.
        if dira_core::model::is_probe_session(&ev.session_id) {
            return;
        }
        let kind = match ev.kind {
            EventKind::ManualStart | EventKind::ManualStop | EventKind::ManualTick => {
                SessionKind::Manual
            }
            _ => SessionKind::Agent,
        };
        let entry = self
            .sessions
            .entry(ev.session_id.clone())
            .or_insert_with(|| LiveSession {
                session_id: ev.session_id.clone(),
                harness: ev.harness,
                kind,
                project: ev.project.clone(),
                identity_email: ev.identity_email.clone(),
                label: ev.label.clone(),
                activity: ev.activity.clone(),
                note: ev.note.clone(),
                started_at: ev.at,
                last_event_at: ev.at,
                last_signal_at: None,
                ended: false,
                engaged_seconds: 0,
                active_seconds: 0,
                last_human_signal_at: None,
                last_active_at: None,
                last_opens_span: false,
                last_partial_active_seconds: None,
                prompts: 0,
                branch: ev.branch.clone(),
                had_signal: false,
            });

        // Sticky, and set before anything else can early-return: the first real
        // signal promotes the session out of "lifecycle noise" for good (issue #74).
        if ev.kind.is_human_signal() || ev.kind.is_agent_activity() {
            entry.had_signal = true;
        }

        // Agent wall-clock gap (all events): credit the gap from the previous
        // event under exactly the rule `accounting::agent_active_seconds` applies
        // in one shot, so the incremental sum here and the batch scan agree.
        // A gap opened by a `PreTool` is a tool call in flight and is credited in
        // full; any other gap is clamped. Neither is discarded — dropping
        // over-idle gaps is what used to zero a whole two-hour build.
        if let Some(prev) = entry.last_active_at {
            entry.active_seconds +=
                accounting::agent_gap_seconds(ev.at - prev, entry.last_opens_span, self.agent)
                    as u64;
        }
        entry.last_active_at = Some(ev.at);
        entry.last_opens_span = ev.kind.opens_agent_span();

        entry.last_event_at = ev.at;
        if entry.project.is_none() && ev.project.is_some() {
            entry.project = ev.project.clone();
        }
        if entry.label.is_none() && ev.label.is_some() {
            entry.label = ev.label.clone();
        }
        if entry.activity.is_none() && ev.activity.is_some() {
            entry.activity = ev.activity.clone();
        }
        if entry.note.is_none() && ev.note.is_some() {
            entry.note = ev.note.clone();
        }
        if ev.branch.is_some() {
            entry.branch = ev.branch.clone();
        }
        if matches!(ev.kind, EventKind::UserPrompt) {
            entry.prompts += 1;
        }
        if ev.kind.is_human_signal() {
            entry.last_human_signal_at = Some(ev.at);
            entry.last_signal_at = Some(ev.at);
        }
        // `ended` follows the latest lifecycle signal, not a one-way latch: a
        // terminal event ends the session, but any later event (a SessionStart on
        // resume, or a tool/prompt) reactivates it. Claude Code emits SessionEnd
        // on compaction mid-conversation, so a long session would otherwise vanish
        // from `active` even while it keeps working.
        entry.ended = matches!(ev.kind, EventKind::SessionEnd | EventKind::ManualStop);

        // Engaged gap — de-duplicated across ALL sessions and attributed to the
        // session that *opens* the gap (the v1 opening-signal policy, identical to
        // `accounting::per_key_seconds` keyed by session). Maintained at the registry
        // level (not per session) so the per-session `engaged_seconds` counters sum to
        // the deduped grand total: a session with a single prompt still gets credited
        // for the gap it opens, instead of reading 0 because it has no *second* signal
        // of its own. Done in a fresh borrow so we can credit a different session than
        // the one this event belongs to.
        if ev.kind.is_human_signal() {
            let prev = self.last_human_signal_at;
            let opener = self.last_human_signal_session.clone();
            if let (Some(prev_at), Some(opener)) = (prev, opener) {
                let gap = ev.at - prev_at;
                if gap > Duration::ZERO && gap <= idle {
                    if let Some(op) = self.sessions.get_mut(&opener) {
                        op.engaged_seconds += gap.whole_seconds() as u64;
                    }
                }
            }
            self.last_human_signal_at = Some(ev.at);
            self.last_human_signal_session = Some(ev.session_id.clone());
        }
    }

    /// Drop all live sessions. Called on `nuke` so the registry doesn't keep
    /// showing "active" sessions whose backing events were just wiped.
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.last_human_signal_at = None;
        self.last_human_signal_session = None;
    }

    /// The newest event timestamp observed across ALL known sessions (active or
    /// ended), or `None` if the registry has never observed a single event.
    /// Used by the heartbeat's deep-idle predicate (WP-A3) as "the newest
    /// activity" — a session that just ended still counts, so the daemon doesn't
    /// snap to deep idle the instant the last session's final event lands.
    pub fn last_activity_at(&self) -> Option<OffsetDateTime> {
        self.sessions.values().map(|s| s.last_event_at).max()
    }

    /// The sessions that are genuinely live right now: not ended, not lifecycle
    /// noise, and not stale.
    ///
    /// `!ended` alone is not enough (issue #74). Two things masquerade as live:
    ///
    /// - **Lifecycle noise** — a bare `SessionStart` with nothing after it. The
    ///   desktop app spawns short-lived Claude Code processes that each get a fresh
    ///   session id; on one observed day 113 of 126 sessions carried no signal at
    ///   all. `had_signal` drops them. Agent-only sessions are unaffected: a tool
    ///   call is a signal.
    /// - **Stale sessions** — a session that did real work and never received a
    ///   `SessionEnd` (crashed or force-quit harness). `ended` is not a latch, so
    ///   these are otherwise reported as live indefinitely; four such sessions were
    ///   found idle for 10–31 hours while still being broadcast to the cloud's
    ///   "Right now" panel. [`LiveSession::is_stale`] bounds them.
    ///
    /// Neither filter removes anything: the entries stay in the registry, so a
    /// session that resumes is reported again with its full history.
    pub fn active(&self, now: OffsetDateTime, stale_after: time::Duration) -> Vec<LiveSession> {
        self.snapshot(|s| !s.ended && s.had_signal && !s.is_stale(now, stale_after))
    }

    /// Sessions matching `keep`, oldest-started first — the shape every
    /// live-session view returns. Private so the predicate stays the only thing
    /// that differs between them.
    fn snapshot(&self, keep: impl Fn(&LiveSession) -> bool) -> Vec<LiveSession> {
        let mut v: Vec<LiveSession> = self
            .sessions
            .values()
            .filter(|s| keep(s))
            .cloned()
            .collect();
        v.sort_by_key(|s| s.started_at);
        v
    }

    /// All sessions, active first then recent, newest-started first.
    pub fn all(&self) -> Vec<LiveSession> {
        let mut v: Vec<LiveSession> = self.sessions.values().cloned().collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        v
    }

    /// Active manual sessions (for the idle ticker and `stop --auto`).
    ///
    /// Deliberately NOT routed through [`Self::active`]: a manual session is an
    /// explicit `dira start` and must keep accruing while the user is away from the
    /// keyboard, so neither the signal nor the staleness filter may touch it. (In
    /// practice `ManualTick` keeps it fresh every 30s and `ManualStart` is itself a
    /// human signal, so both filters would pass anyway — but the ticker is what
    /// *produces* those ticks, and gating it on them would be a self-starving loop.)
    pub fn active_manual(&self) -> Vec<LiveSession> {
        self.snapshot(|s| !s.ended && s.kind == SessionKind::Manual)
    }

    /// Whether this session is *known* to the registry and has never shown a real
    /// signal — i.e. its whole life so far is lifecycle noise (issue #74).
    ///
    /// `false` for an unknown session on purpose: the registry hydrates from only
    /// the last day of the log, so "not in the registry" means "no opinion", never
    /// "degenerate". A caller acting on this must treat absence as a reason to leave
    /// the session alone.
    pub fn is_degenerate(&self, session_id: &str) -> bool {
        self.sessions.get(session_id).is_some_and(|s| !s.had_signal)
    }

    /// Forget a session entirely — used when the writer prunes a degenerate
    /// session's events from the log (issue #74), so the registry doesn't keep
    /// reporting a session whose backing rows no longer exist.
    pub fn forget(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        if self.last_human_signal_session.as_deref() == Some(session_id) {
            // The pruned session can't have opened a counted gap (it had no human
            // signal, or it wouldn't be degenerate), but clear the attribution
            // anyway so a later signal can't be credited to a session that's gone.
            self.last_human_signal_session = None;
            self.last_human_signal_at = None;
        }
    }

    /// Snapshot of long-running, not-yet-ended sessions eligible for a *partial*
    /// rollup (Phase 6c): older than `now - older_than`, with positive active
    /// time, and with *new* active time since the last partial we shipped for
    /// them. Read-only — the caller marks them sent via [`mark_partials_sent`]
    /// only after the cloud accepts the batch, so a failed flush re-offers the
    /// same candidates next time.
    pub fn partial_rollup_candidates(
        &self,
        now: OffsetDateTime,
        older_than: Duration,
    ) -> Vec<LiveSession> {
        if older_than <= Duration::ZERO {
            return Vec::new(); // partial rollups disabled
        }
        let cutoff = now - older_than;
        self.sessions
            .values()
            .filter(|s| !s.ended)
            .filter(|s| s.started_at <= cutoff)
            .filter(|s| s.active_seconds > 0)
            // New activity since the last partial (or never shipped one).
            .filter(|s| s.last_partial_active_seconds != Some(s.active_seconds))
            .cloned()
            .collect()
    }

    /// Record that a partial rollup was shipped for each of `session_ids`, pinning
    /// its `last_partial_active_seconds` watermark to the session's *current*
    /// `active_seconds`. The next partial is then only offered once the counter
    /// grows again, so an idle long session doesn't re-ship an identical rollup
    /// every flush.
    pub fn mark_partials_sent(&mut self, session_ids: &[String]) {
        for id in session_ids {
            if let Some(s) = self.sessions.get_mut(id) {
                s.last_partial_active_seconds = Some(s.active_seconds);
            }
        }
    }

    /// The single non-ended session observed for `repo`, or `None` when zero or
    /// more than one match. Used to attribute an observed commit to the session
    /// that produced it — we never guess, so an ambiguous (concurrent) or absent
    /// match yields `None` and the cloud falls back to author + time.
    pub fn session_for_repo(&self, repo: &str) -> Option<String> {
        let mut matches = self
            .sessions
            .values()
            .filter(|s| !s.ended && s.project.as_deref() == Some(repo));
        let first = matches.next()?;
        match matches.next() {
            Some(_) => None, // more than one active session for this repo
            None => Some(first.session_id.clone()),
        }
    }

    /// This session's sticky project, if the registry has seen one. The
    /// last-known-good fallback for token attribution (issue #93): `observe`
    /// sets `project` first-non-null and never clears it, so this is the
    /// registry's best answer even when the session's LATEST event carries no
    /// project (or none at all, as in a session the registry hasn't hydrated).
    pub fn project_for(&self, session_id: &str) -> Option<String> {
        self.sessions.get(session_id)?.project.clone()
    }

    /// Resolve a stop selector to the session ids to close.
    pub fn select_manual(
        &self,
        by_handle: Option<&str>,
        by_label: Option<&str>,
        all: bool,
    ) -> Vec<String> {
        self.active_manual()
            .into_iter()
            .filter(|s| {
                if all {
                    true
                } else if let Some(h) = by_handle {
                    s.handle() == h || s.session_id == h
                } else if let Some(l) = by_label {
                    s.label.as_deref() == Some(l)
                } else {
                    false
                }
            })
            .map(|s| s.session_id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::accounting::{self, Signal};

    /// WP-B1b: `device_key()` caches on first load, and
    /// `invalidate_device_key()` forces the NEXT call to reload rather than
    /// keep serving the stale cached key — the mechanism `sync.rs`'s
    /// `try_pending_key_flush` relies on after promoting a rotation.
    ///
    /// Uses `DIRA_DEVICE_SECRET` (never touches the OS keychain) so this is
    /// safe to run in any environment; this is the only test in this binary
    /// that calls `device_key()` without an explicit override, so the
    /// process-global env var can't race a concurrent test.
    #[tokio::test]
    async fn device_key_reloads_after_invalidation() {
        struct ClearEnv;
        impl Drop for ClearEnv {
            fn drop(&mut self) {
                std::env::remove_var(dira_core::identity::ENV_DEVICE_SECRET);
            }
        }
        let _clear = ClearEnv;

        let first = dira_core::signing::DeviceKey::generate();
        std::env::set_var(
            dira_core::identity::ENV_DEVICE_SECRET,
            first.secret_base64(),
        );

        let store = dira_core::Store::open_in_memory().await.unwrap();
        let config = dira_core::Config::default();
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();

        let loaded = state.device_key().await.expect("loads via env seed");
        assert_eq!(loaded.public_base64(), first.public_base64());
        // Cached: a second call returns the same key without re-reading env
        // (env doesn't change between calls here, so this mainly proves the
        // fast path doesn't error).
        let cached = state.device_key().await.expect("cached read");
        assert_eq!(cached.public_base64(), first.public_base64());

        // Swap the env seed and invalidate — the NEXT call must pick up the
        // NEW key, proving the cache doesn't silently keep serving the old one.
        let second = dira_core::signing::DeviceKey::generate();
        assert_ne!(first.public_base64(), second.public_base64());
        std::env::set_var(
            dira_core::identity::ENV_DEVICE_SECRET,
            second.secret_base64(),
        );
        state.invalidate_device_key().await;

        let reloaded = state
            .device_key()
            .await
            .expect("reloads after invalidation");
        assert_eq!(reloaded.public_base64(), second.public_base64());
    }

    const IDLE: Duration = Duration::minutes(5);

    fn ev(session: &str, kind: EventKind, project: Option<&str>) -> RawEvent {
        ev_at(session, kind, project, 0)
    }

    /// An event at `OffsetDateTime::UNIX_EPOCH + secs`.
    fn ev_at(session: &str, kind: EventKind, project: Option<&str>, secs: i64) -> RawEvent {
        RawEvent {
            id: format!("{session}-{:?}-{secs:08}", kind),
            at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs),
            session_id: session.to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: project.map(str::to_string),
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    /// Like `ev_at`, but carries a `branch` — for the last-wins branch tracking
    /// tests, which need events that actually set it.
    fn ev_at_branch(
        session: &str,
        kind: EventKind,
        project: Option<&str>,
        secs: i64,
        branch: Option<&str>,
    ) -> RawEvent {
        RawEvent {
            branch: branch.map(str::to_string),
            ..ev_at(session, kind, project, secs)
        }
    }

    #[test]
    fn last_activity_at_tracks_the_newest_event_including_ended_sessions() {
        let repo = "github.com/acme/api";
        let mut reg = SessionRegistry::default();
        assert_eq!(
            reg.last_activity_at(),
            None,
            "empty registry ⇒ no activity yet"
        );

        reg.observe(&ev_at("s1", EventKind::SessionStart, Some(repo), 0), IDLE);
        assert_eq!(reg.last_activity_at(), Some(OffsetDateTime::UNIX_EPOCH));

        // A later event on a second session moves the newest-activity mark.
        reg.observe(&ev_at("s2", EventKind::SessionStart, Some(repo), 100), IDLE);
        assert_eq!(
            reg.last_activity_at(),
            Some(OffsetDateTime::UNIX_EPOCH + Duration::seconds(100))
        );

        // Ending s2 still counts as the newest activity — a just-ended session
        // must not make the registry look older than it is.
        reg.observe(&ev_at("s2", EventKind::SessionEnd, Some(repo), 150), IDLE);
        assert_eq!(
            reg.last_activity_at(),
            Some(OffsetDateTime::UNIX_EPOCH + Duration::seconds(150))
        );
    }

    #[test]
    fn session_for_repo_resolves_only_a_unique_active_match() {
        let repo = "github.com/acme/api";
        let mut reg = SessionRegistry::default();

        // Zero active sessions for the repo ⇒ None.
        assert_eq!(reg.session_for_repo(repo), None);

        // Exactly one active session ⇒ Some(its id).
        reg.observe(&ev("s1", EventKind::SessionStart, Some(repo)), IDLE);
        assert_eq!(reg.session_for_repo(repo).as_deref(), Some("s1"));

        // A second active session on the same repo ⇒ ambiguous ⇒ None.
        reg.observe(&ev("s2", EventKind::SessionStart, Some(repo)), IDLE);
        assert_eq!(reg.session_for_repo(repo), None);

        // End s2; s1 is again the unique active match.
        reg.observe(&ev("s2", EventKind::SessionEnd, Some(repo)), IDLE);
        assert_eq!(reg.session_for_repo(repo).as_deref(), Some("s1"));

        // A different repo with no active session ⇒ None.
        assert_eq!(reg.session_for_repo("github.com/acme/other"), None);
    }

    /// Test `now`, far enough past the fixture events that nothing is stale.
    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;
    /// Test staleness bound — generous, so only tests that *mean* to go stale do.
    const STALE: Duration = Duration::hours(4);

    /// `active()` at the fixture clock. Every test that predates the signal +
    /// staleness filters (issue #74) reads through here.
    fn live(reg: &SessionRegistry) -> Vec<LiveSession> {
        reg.active(NOW, STALE)
    }

    #[test]
    fn session_reactivates_after_end_when_activity_resumes() {
        let repo = "github.com/acme/api";
        let mut reg = SessionRegistry::default();

        // A tool call makes the session real; without one it is lifecycle noise
        // and `active` would (correctly, per issue #74) never report it at all.
        reg.observe(&ev("s1", EventKind::SessionStart, Some(repo)), IDLE);
        reg.observe(&ev("s1", EventKind::PostTool, Some(repo)), IDLE);
        // SessionEnd (e.g. Claude Code compaction) ends it.
        reg.observe(&ev("s1", EventKind::SessionEnd, Some(repo)), IDLE);
        assert!(live(&reg).is_empty(), "ended session is not active");

        // Activity resumes on the same id — it must come back as active.
        reg.observe(&ev("s1", EventKind::SessionStart, Some(repo)), IDLE);
        assert_eq!(live(&reg).len(), 1, "SessionStart reactivates");

        reg.observe(&ev("s1", EventKind::SessionEnd, Some(repo)), IDLE);
        reg.observe(&ev("s1", EventKind::PostTool, Some(repo)), IDLE);
        assert_eq!(live(&reg).len(), 1, "a tool event after end reactivates");
    }

    /// Issue #74's core claim: a session whose whole life is lifecycle events did
    /// no observable work and must not be reported as live.
    #[test]
    fn a_bare_session_start_is_never_reported_as_live() {
        let mut reg = SessionRegistry::default();
        reg.observe(&ev("phantom", EventKind::SessionStart, None), IDLE);

        assert!(live(&reg).is_empty(), "a bare SessionStart is not live");
        assert!(
            reg.is_degenerate("phantom"),
            "and the writer can recognise it as prunable"
        );
    }

    /// The signal predicate is the UNION of human signal and agent activity, so a
    /// headless run that never submits a prompt is still fully real. This is the
    /// regression that would silently delete legitimate agent-only work.
    #[test]
    fn an_agent_only_session_is_live_from_its_first_tool_call() {
        let mut reg = SessionRegistry::default();
        reg.observe(&ev("agent", EventKind::SessionStart, None), IDLE);
        reg.observe(&ev_at("agent", EventKind::PreTool, None, 1), IDLE);

        assert_eq!(live(&reg).len(), 1, "a tool call alone makes it live");
        assert!(
            !reg.is_degenerate("agent"),
            "and it must never be prunable — no prompt is not the same as no work"
        );
    }

    /// An unknown session is "no opinion", not "degenerate": the hydrate replay
    /// only covers the last day, so absence must never authorise a prune.
    #[test]
    fn an_unknown_session_is_not_degenerate() {
        let reg = SessionRegistry::default();
        assert!(!reg.is_degenerate("never-seen"));
    }

    /// A session that did real work but never received a `SessionEnd` (crashed or
    /// force-quit harness) must stop being reported as live once it goes quiet —
    /// four such sessions were found in production still broadcast as "Right now"
    /// after 10–31 hours idle.
    #[test]
    fn a_real_session_that_never_ended_goes_quiet_once_stale() {
        let mut reg = SessionRegistry::default();
        reg.observe(&ev("zombie", EventKind::SessionStart, None), IDLE);
        reg.observe(&ev_at("zombie", EventKind::Stop, None, 60), IDLE);

        let last_event = OffsetDateTime::UNIX_EPOCH + Duration::seconds(60);
        assert_eq!(
            reg.active(last_event + Duration::hours(1), STALE).len(),
            1,
            "an hour of quiet is an ordinary pause, not a dead session"
        );
        assert!(
            reg.active(last_event + Duration::hours(5), STALE)
                .is_empty(),
            "past the staleness bound it is no longer 'right now'"
        );

        // Nothing was evicted: if the harness comes back, so does the session,
        // with its history intact.
        reg.observe(&ev_at("zombie", EventKind::PostTool, None, 40_000), IDLE);
        let resumed = reg.active(last_event + Duration::hours(12), STALE);
        assert_eq!(resumed.len(), 1, "a resumed session is reported again");
        assert_eq!(
            resumed[0].started_at,
            OffsetDateTime::UNIX_EPOCH,
            "and keeps its original start, so accounting is unaffected"
        );
    }

    /// A manual `dira start` must keep accruing while the user is away from the
    /// keyboard, so the idle ticker's view is deliberately unfiltered.
    #[test]
    fn a_manual_session_is_never_hidden_by_the_staleness_bound() {
        let mut reg = SessionRegistry::default();
        reg.observe(&ev("manual", EventKind::ManualStart, None), IDLE);

        assert_eq!(
            reg.active_manual().len(),
            1,
            "the ticker still sees it — it is what keeps the session fresh"
        );
    }

    /// Phase 6b: the incrementally-maintained `active_seconds` /
    /// `engaged_seconds` on a `LiveSession` must equal the one-shot
    /// accounting-core scan over the identical event sequence. This is the
    /// equivalence the heartbeat relies on to read the registry instead of
    /// re-scanning SQLite each tick.
    fn registry_counter_equals_accounting(seq: &[(EventKind, i64)]) {
        let p = Some("github.com/acme/api");
        let mut reg = SessionRegistry::default();
        for (kind, secs) in seq {
            reg.observe(&ev_at("s1", *kind, p, *secs), IDLE);
        }
        let s = reg.sessions.get("s1").expect("session present in registry");

        // Agent wall-clock over *all* events, under the same policy the registry
        // used. This is the guard that keeps the registry's incremental sum and
        // the one-shot batch scan from ever disagreeing — the two numbers a user
        // sees as "live" and "reported".
        let all_samples: Vec<accounting::AgentSample> = seq
            .iter()
            .map(|(kind, secs)| accounting::AgentSample {
                at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(*secs),
                opens_span: kind.opens_agent_span(),
            })
            .collect();
        let expect_active = accounting::agent_active_seconds(&all_samples, reg.agent).max(0) as u64;
        assert_eq!(
            s.active_seconds, expect_active,
            "incremental active_seconds must equal accounting::agent_active_seconds for {seq:?}"
        );

        // engaged_seconds over human signals only.
        let signals: Vec<Signal> = seq
            .iter()
            .filter(|(k, _)| k.is_human_signal())
            .map(|(_, secs)| Signal {
                at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(*secs),
                project: p.map(str::to_string),
            })
            .collect();
        let expect_engaged = accounting::total_human_seconds(&signals, IDLE).max(0) as u64;
        assert_eq!(
            s.engaged_seconds, expect_engaged,
            "incremental engaged_seconds must equal accounting::total_human_seconds for {seq:?}"
        );
    }

    #[test]
    fn partial_rollup_candidates_respect_age_growth_and_watermark() {
        let p = Some("github.com/acme/api");
        let mut reg = SessionRegistry::default();
        // A session that started "now" with some activity.
        reg.observe(&ev_at("s1", EventKind::SessionStart, p, 0), IDLE);
        reg.observe(&ev_at("s1", EventKind::PreTool, p, 30), IDLE);

        let start = OffsetDateTime::UNIX_EPOCH;
        // Too young (threshold 1h, only 30s old) ⇒ not a candidate.
        let young =
            reg.partial_rollup_candidates(start + Duration::seconds(30), Duration::hours(1));
        assert!(young.is_empty(), "a fresh session is not eligible");

        // Old enough now ⇒ eligible (positive active time, never shipped a partial).
        let now = start + Duration::hours(2);
        let cands = reg.partial_rollup_candidates(now, Duration::hours(1));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].session_id, "s1");
        assert_eq!(cands[0].active_seconds, 30);

        // Mark it sent: the same active_seconds must no longer be offered.
        reg.mark_partials_sent(&["s1".to_string()]);
        let after = reg.partial_rollup_candidates(now, Duration::hours(1));
        assert!(after.is_empty(), "no new activity since last partial");

        // New activity within idle of the prior event grows active_seconds (gap
        // 30s→60s is within the 5min idle) ⇒ eligible again.
        reg.observe(&ev_at("s1", EventKind::PostTool, p, 60), IDLE);
        let grown = reg
            .sessions
            .get("s1")
            .map(|s| s.active_seconds)
            .unwrap_or(0);
        assert_eq!(grown, 60, "active grew from 30 to 60");
        let grown = reg.partial_rollup_candidates(now, Duration::hours(1));
        assert_eq!(grown.len(), 1, "growth re-offers the partial");

        // A disabled threshold (0) never offers candidates.
        assert!(reg
            .partial_rollup_candidates(now, Duration::ZERO)
            .is_empty());
    }

    #[test]
    fn ended_session_is_not_a_partial_candidate() {
        let p = Some("github.com/acme/api");
        let mut reg = SessionRegistry::default();
        reg.observe(&ev_at("s1", EventKind::SessionStart, p, 0), IDLE);
        reg.observe(&ev_at("s1", EventKind::PreTool, p, 30), IDLE);
        reg.observe(&ev_at("s1", EventKind::SessionEnd, p, 40), IDLE);
        let now = OffsetDateTime::UNIX_EPOCH + Duration::hours(2);
        assert!(
            reg.partial_rollup_candidates(now, Duration::hours(1))
                .is_empty(),
            "an ended session ships via the normal ended rollup, not a partial"
        );
    }

    #[test]
    fn incremental_counters_equal_accounting_core() {
        use EventKind::*;
        // Dense activity, all gaps within idle.
        registry_counter_equals_accounting(&[
            (SessionStart, 0),
            (UserPrompt, 10),
            (PreTool, 30),
            (PostTool, 60),
            (UserPrompt, 90),
            (Stop, 120),
            (SessionEnd, 150),
        ]);
        // A wide idle gap in the middle must be trimmed from both counters.
        registry_counter_equals_accounting(&[
            (SessionStart, 0),
            (UserPrompt, 30),
            // > 5min idle gap here, excluded:
            (UserPrompt, 30 + 600),
            (PreTool, 30 + 600 + 20),
            (UserPrompt, 30 + 600 + 50),
        ]);
        // Activity-only session (no human signals) ⇒ engaged stays 0.
        registry_counter_equals_accounting(&[
            (SessionStart, 0),
            (PreTool, 5),
            (PostTool, 25),
            (PreTool, 40),
        ]);
        // A single event ⇒ both zero.
        registry_counter_equals_accounting(&[(SessionStart, 0)]);
    }

    #[test]
    fn engaged_seconds_are_deduped_and_attributed_across_sessions() {
        // The parallel-supervision case: two sessions, ONE prompt each, interleaved.
        // In isolation each session has a single signal, so the old per-session
        // counter read 0 for both. With registry-level opening-signal attribution,
        // each counted gap is credited to the session that opened it, so the
        // per-session counters are non-zero and sum to the deduped grand total.
        let p = Some("github.com/acme/api");
        let mut reg = SessionRegistry::default();
        reg.observe(&ev_at("s1", EventKind::UserPrompt, p, 0), IDLE); // opens 0..60
        reg.observe(&ev_at("s2", EventKind::UserPrompt, p, 60), IDLE); // opens 60..120
        reg.observe(&ev_at("s1", EventKind::UserPrompt, p, 120), IDLE); // last signal

        let s1 = reg.sessions.get("s1").unwrap().engaged_seconds;
        let s2 = reg.sessions.get("s2").unwrap().engaged_seconds;

        // Cross-check against the one-shot accounting core: per-session values must
        // equal per_key_seconds keyed by session, and sum to total_human_seconds.
        let keyed: Vec<(OffsetDateTime, &str)> = [(0, "s1"), (60, "s2"), (120, "s1")]
            .into_iter()
            .map(|(secs, sid)| (OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs), sid))
            .collect();
        let by = accounting::per_key_seconds(&keyed, IDLE);
        assert_eq!(s1 as i64, by[&"s1"]); // gap 0..60
        assert_eq!(s2 as i64, by[&"s2"]); // gap 60..120
        assert_eq!(s1, 60);
        assert_eq!(s2, 60);

        let signals: Vec<Signal> = [0i64, 60, 120]
            .into_iter()
            .map(|secs| Signal {
                at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs),
                project: p.map(str::to_string),
            })
            .collect();
        assert_eq!(
            s1 + s2,
            accounting::total_human_seconds(&signals, IDLE) as u64,
            "per-session engaged must sum to the deduped grand total",
        );
    }

    /// Issue #40: a partial rollup for a still-live session needs `prompts` and
    /// `branch` filled in instead of `None`/0. `prompts` counts `UserPrompt`
    /// events; `branch` is last-wins over any event that carries one, and a
    /// branchless event must never clear a previously-resolved branch.
    #[test]
    fn observe_counts_prompts_and_tracks_last_branch() {
        let p = Some("github.com/acme/api");
        let mut reg = SessionRegistry::default();

        reg.observe(
            &ev_at_branch("s1", EventKind::SessionStart, p, 0, Some("main")),
            IDLE,
        );
        reg.observe(
            &ev_at_branch("s1", EventKind::UserPrompt, p, 10, Some("main")),
            IDLE,
        );
        reg.observe(&ev_at("s1", EventKind::PreTool, p, 20), IDLE);
        reg.observe(&ev_at("s1", EventKind::PostTool, p, 30), IDLE);
        // Branch flips mid-session.
        reg.observe(
            &ev_at_branch("s1", EventKind::UserPrompt, p, 40, Some("feat/x")),
            IDLE,
        );
        // A branchless event must NOT clear the resolved branch.
        reg.observe(&ev_at("s1", EventKind::PostTool, p, 50), IDLE);

        let s1 = reg.sessions.get("s1").expect("session present");
        assert_eq!(s1.prompts, 2, "two UserPrompt events observed");
        assert_eq!(
            s1.branch.as_deref(),
            Some("feat/x"),
            "last non-null branch wins, and a later branchless event doesn't clear it"
        );

        // Replay-equivalence: a fresh registry fed the identical sequence
        // reproduces the same (prompts, branch) — the guarantee `hydrate` relies
        // on to reconstruct live state after a daemon restart.
        let seq = [
            ev_at_branch("s1", EventKind::SessionStart, p, 0, Some("main")),
            ev_at_branch("s1", EventKind::UserPrompt, p, 10, Some("main")),
            ev_at("s1", EventKind::PreTool, p, 20),
            ev_at("s1", EventKind::PostTool, p, 30),
            ev_at_branch("s1", EventKind::UserPrompt, p, 40, Some("feat/x")),
            ev_at("s1", EventKind::PostTool, p, 50),
        ];
        let mut replay = SessionRegistry::default();
        for ev in &seq {
            replay.observe(ev, IDLE);
        }
        let replayed = replay.sessions.get("s1").expect("session present");
        assert_eq!(replayed.prompts, s1.prompts);
        assert_eq!(replayed.branch, s1.branch);
    }
}
