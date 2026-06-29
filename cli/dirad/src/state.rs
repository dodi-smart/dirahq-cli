//! Shared daemon state and the live session registry.
//!
//! The hot path never touches this beyond a non-blocking channel send. All
//! enrichment (git resolution) and DB writes happen in the single writer task
//! that drains the channel, which also keeps the live registry up to date.

use crate::sync::SyncHandle;
use dira_contract::{Harness, SessionKind};
use dira_core::model::{EventKind, RawEvent};
use dira_core::signing::DeviceKey;
use dira_core::{Config, Store};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};
use tokio::sync::{mpsc, OnceCell};

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
    /// When this daemon process started, for `dira version` uptime reporting.
    pub started_at: std::time::Instant,
    /// Bearer token required on the loopback HTTP ingress.
    pub bearer: Arc<String>,
    /// Handle to the background cloud-sync task (trigger channel).
    pub sync: SyncHandle,
    /// This device's signing key, used to sign attestation batches. Loaded
    /// **lazily** off the startup critical path: the key is only needed for
    /// sync/signing, never to answer a control request, and loading it can block
    /// on a keychain unlock prompt. The control socket binds and answers
    /// `Ping`/`Status` long before this resolves. Access via
    /// [`AppState::device_key`].
    pub device_key: Arc<OnceCell<DeviceKey>>,
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
}

impl AppState {
    /// The device signing key, loaded on first use and cached. Returns `None`
    /// only if loading the key fails (a logged, non-fatal condition — the device
    /// simply can't sign/sync yet). Kept off the startup critical path so a
    /// keychain prompt never delays control-socket readiness (Commit 2).
    pub async fn device_key(&self) -> Option<&DeviceKey> {
        self.device_key
            .get_or_try_init(|| async {
                dira_core::identity::load_or_create_unlinked(&self.store).await
            })
            .await
            .map_err(|e| tracing::warn!("device identity load failed: {e}"))
            .ok()
    }
}

/// Liveness timestamps the watchdog reads to tell a stalled task from a quiet one.
///
/// Each is the unix-epoch second of the last forward progress for that task,
/// stored as an atomic so the supervisor reads it without a lock. `0` means
/// "never progressed yet" (just-started). The writer marks progress on every
/// drained message; the idle ticker marks on every tick.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    writer_at: AtomicI64,
    ticker_at: AtomicI64,
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
    /// Seconds since the writer last made progress, or `None` if it never has.
    pub fn writer_idle_secs(&self) -> Option<i64> {
        Self::elapsed(self.writer_at.load(Ordering::Relaxed))
    }
    /// Seconds since the idle ticker last ran, or `None` if it never has.
    pub fn ticker_idle_secs(&self) -> Option<i64> {
        Self::elapsed(self.ticker_at.load(Ordering::Relaxed))
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
/// - `engaged_seconds` — de-duplicated, idle-trimmed human time for this session,
///   the per-session analogue of [`dira_core::accounting::total_human_seconds`]
///   over the session's own human signals.
/// - `active_seconds` — idle-trimmed active wall time over *all* of the session's
///   event timestamps, the per-session analogue of
///   [`dira_core::accounting::active_seconds`].
///
/// Both are computed by the exact same gap rule the batch scan uses: when an
/// event arrives, add `gap = at - <previous timestamp of the same class>` to the
/// running counter iff `0 < gap <= idle`. Accumulating consecutive in-window gaps
/// telescopes to the same sum a one-shot sort-and-sum over the whole sequence
/// produces, so the incremental value equals the accounting-core value (events
/// arrive in non-decreasing `at` order on the writer path; the registry tracks the
/// last-seen timestamp regardless). The cold-start hydrate replays the recent log
/// through `observe`, so a daemon bounce reconstructs the same totals.
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub session_id: String,
    pub harness: Harness,
    pub kind: SessionKind,
    pub project: Option<String>,
    pub identity_email: Option<String>,
    pub label: Option<String>,
    pub started_at: OffsetDateTime,
    pub last_event_at: OffsetDateTime,
    pub last_signal_at: Option<OffsetDateTime>,
    pub ended: bool,
    /// Rolling de-duplicated human-engaged seconds for this session (6b).
    pub engaged_seconds: u64,
    /// Rolling idle-trimmed active wall seconds over all this session's events (6b).
    pub active_seconds: u64,
    /// The `at` of the last human-signal event folded in, for the engaged gap math.
    pub last_human_signal_at: Option<OffsetDateTime>,
    /// The `at` of the last event of *any* kind folded in, for the active gap math.
    /// Distinct from `last_event_at` only in intent; kept separate so the gap math
    /// reads from a field the partial-rollup / observe logic never repurposes.
    pub last_active_at: Option<OffsetDateTime>,
    /// `active_seconds` as of the last partial rollup we emitted for this session,
    /// or `None` if none yet. Used to decide "new activity since the last partial"
    /// (6c) without re-scanning.
    pub last_partial_active_seconds: Option<u64>,
}

impl LiveSession {
    /// A session is idle if no human signal has arrived within the idle window.
    pub fn is_idle(&self, now: OffsetDateTime, idle: time::Duration) -> bool {
        match self.last_signal_at {
            Some(t) => now - t > idle,
            None => true,
        }
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
}

impl SessionRegistry {
    /// Fold an appended event into the live registry, maintaining the rolling
    /// `engaged_seconds` / `active_seconds` counters with `idle` as the gap
    /// threshold (Phase 6b). Pass the same `idle` the accounting core uses
    /// (`Config::idle()`); see [`LiveSession`] for why the incremental sum equals
    /// the one-shot accounting scan.
    pub fn observe(&mut self, ev: &RawEvent, idle: Duration) {
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
                started_at: ev.at,
                last_event_at: ev.at,
                last_signal_at: None,
                ended: false,
                engaged_seconds: 0,
                active_seconds: 0,
                last_human_signal_at: None,
                last_active_at: None,
                last_partial_active_seconds: None,
            });

        // Active gap (all events): add the gap from the previous event of any kind
        // when it is within (0, idle], exactly the active_seconds rule.
        if let Some(prev) = entry.last_active_at {
            let gap = ev.at - prev;
            if gap > Duration::ZERO && gap <= idle {
                entry.active_seconds += gap.whole_seconds() as u64;
            }
        }
        entry.last_active_at = Some(ev.at);

        entry.last_event_at = ev.at;
        if entry.project.is_none() && ev.project.is_some() {
            entry.project = ev.project.clone();
        }
        if entry.label.is_none() && ev.label.is_some() {
            entry.label = ev.label.clone();
        }
        if ev.kind.is_human_signal() {
            // Engaged gap (human signals only): same rule over consecutive signals.
            if let Some(prev) = entry.last_human_signal_at {
                let gap = ev.at - prev;
                if gap > Duration::ZERO && gap <= idle {
                    entry.engaged_seconds += gap.whole_seconds() as u64;
                }
            }
            entry.last_human_signal_at = Some(ev.at);
            entry.last_signal_at = Some(ev.at);
        }
        // `ended` follows the latest lifecycle signal, not a one-way latch: a
        // terminal event ends the session, but any later event (a SessionStart on
        // resume, or a tool/prompt) reactivates it. Claude Code emits SessionEnd
        // on compaction mid-conversation, so a long session would otherwise vanish
        // from `active` even while it keeps working.
        entry.ended = matches!(ev.kind, EventKind::SessionEnd | EventKind::ManualStop);
    }

    /// Drop all live sessions. Called on `nuke` so the registry doesn't keep
    /// showing "active" sessions whose backing events were just wiped.
    pub fn clear(&mut self) {
        self.sessions.clear();
    }

    /// All sessions that have not ended.
    pub fn active(&self) -> Vec<LiveSession> {
        let mut v: Vec<LiveSession> = self
            .sessions
            .values()
            .filter(|s| !s.ended)
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
    pub fn active_manual(&self) -> Vec<LiveSession> {
        self.active()
            .into_iter()
            .filter(|s| s.kind == SessionKind::Manual)
            .collect()
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
        }
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

    #[test]
    fn session_reactivates_after_end_when_activity_resumes() {
        let repo = "github.com/acme/api";
        let mut reg = SessionRegistry::default();

        reg.observe(&ev("s1", EventKind::SessionStart, Some(repo)), IDLE);
        // SessionEnd (e.g. Claude Code compaction) ends it.
        reg.observe(&ev("s1", EventKind::SessionEnd, Some(repo)), IDLE);
        assert!(reg.active().is_empty(), "ended session is not active");

        // Activity resumes on the same id — it must come back as active.
        reg.observe(&ev("s1", EventKind::SessionStart, Some(repo)), IDLE);
        assert_eq!(reg.active().len(), 1, "SessionStart reactivates");

        reg.observe(&ev("s1", EventKind::SessionEnd, Some(repo)), IDLE);
        reg.observe(&ev("s1", EventKind::PostTool, Some(repo)), IDLE);
        assert_eq!(reg.active().len(), 1, "a tool event after end reactivates");
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

        // active_seconds over *all* event timestamps.
        let all_times: Vec<OffsetDateTime> = seq
            .iter()
            .map(|(_, secs)| OffsetDateTime::UNIX_EPOCH + Duration::seconds(*secs))
            .collect();
        let expect_active = accounting::active_seconds(&all_times, IDLE).max(0) as u64;
        assert_eq!(
            s.active_seconds, expect_active,
            "incremental active_seconds must equal accounting::active_seconds for {seq:?}"
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
}
