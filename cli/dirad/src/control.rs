//! The Unix domain socket control server: handles `dira` CLI commands.
//!
//! Unlike the HTTP hot path, these are human-initiated and may resolve git
//! synchronously — millisecond latency is fine. Each connection carries exactly
//! one length-prefixed JSON request and gets one length-prefixed JSON response.

use crate::events::{handle_of, manual_event, materialize_interval};
use crate::state::{AppState, EventMsg};
use dira_core::accounting;
use dira_core::model::{EventKind, RawEvent};
use dira_core::project;
use dira_core::protocol::{
    BillingView, ComputeView, ReportScope, Request, Response, SessionView, StatusView,
    StopSelector, SyncHealthView, WriterHealthView,
};
use dira_core::report;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::{Duration, OffsetDateTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use ulid::Ulid;

/// Lock a mutex, recovering the guard even if it was poisoned.
///
/// A poisoned mutex means some other holder panicked mid-update, so the data may
/// be slightly inconsistent — but for the live-session registry and the repo-dir
/// map that's "stale but serving", which is far better than crashing the
/// CLI-facing control surface. [`PoisonError::into_inner`] hands back the guard
/// regardless, so a one-off panic anywhere degrades gracefully instead of
/// permanently breaking `status`/`sessions`/`stop` with a re-panic. This replaces
/// the former `.lock().unwrap()` sites, which would propagate the poison.
pub fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`lock_recover`] for the repo-dir map; a thin alias kept for call-site clarity.
pub fn lock_recover_map<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    lock_recover(m)
}

/// Read one framed JSON value (4-byte BE length prefix + payload).
pub async fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write one framed JSON value.
pub async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

/// Serve a single CLI connection.
pub async fn handle_conn(state: AppState, mut stream: UnixStream) {
    let resp = match read_frame(&mut stream).await {
        Ok(buf) => match serde_json::from_slice::<Request>(&buf) {
            Ok(req) => dispatch(&state, req).await,
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        },
        Err(e) => Response::Error {
            message: format!("read error: {e}"),
        },
    };
    let bytes = serde_json::to_vec(&resp).unwrap_or_default();
    let _ = write_frame(&mut stream, &bytes).await;
}

/// Dispatch a single control request to its handler. Public so integration tests
/// can drive the control surface without framing a socket round-trip.
pub async fn dispatch(state: &AppState, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Status => status(state).await,
        Request::Sessions => sessions(state).await,
        Request::Start {
            project,
            label,
            activity,
            note,
            cwd,
        } => start(state, project, label, activity, note, cwd).await,
        Request::Stop { selector } => stop(state, selector).await,
        Request::Log {
            duration_secs,
            project,
            note,
            activity,
            label,
            cwd,
        } => log(state, duration_secs, project, note, activity, label, cwd).await,
        Request::Report { scope } => report_cmd(state, scope).await,
        Request::IngestHook { harness, payload } => ingest_hook(state, harness, payload).await,
        Request::Nuke => nuke(state).await,
        Request::DaemonInfo => daemon_info(state),
        Request::ResyncCursor { from } => resync_cursor(state, from).await,
        Request::IngestZavet { payload } => crate::zavet::ingest(state, payload).await,
        Request::ZavetStatus { cwd, repo } => crate::zavet::status(state, cwd, repo).await,
        Request::ZavetWhy { query, cwd, repo } => crate::zavet::why(state, query, cwd, repo).await,
        Request::ZavetWiki { topic, cwd, repo } => {
            crate::zavet::wiki(state, topic, cwd, repo).await
        }
        Request::ZavetDecisions { cwd, repo } => crate::zavet::decisions(state, cwd, repo).await,
        Request::ZavetSetMode { cwd, repo, mode } => {
            crate::zavet::set_mode(state, cwd, repo, mode).await
        }
    }
}

/// Rewind the sync cursor and trigger a flush so the daemon re-sends events to the
/// cloud (`dira device resync`). `from = None` rewinds to the beginning (full
/// re-send); `Some(id)` to that event id. Safe — the cloud dedups attestations by
/// id and intervals by content, so a re-send never double-counts.
async fn resync_cursor(state: &AppState, from: Option<String>) -> Response {
    use dira_core::sync::{META_ARTIFACTS_CURSOR, META_SYNC_CURSOR};
    let new_cursor = from.clone().unwrap_or_default();
    if let Err(e) = state.store.meta_set(META_SYNC_CURSOR, &new_cursor).await {
        return Response::Error {
            message: format!("resync failed (set cursor): {e}"),
        };
    }
    // A full rewind also re-sends artifacts (they ride their own rowid cursor).
    if from.is_none() {
        if let Err(e) = state.store.meta_set(META_ARTIFACTS_CURSOR, "").await {
            return Response::Error {
                message: format!("resync failed (reset artifacts cursor): {e}"),
            };
        }
    }
    let cursor_ref = Some(new_cursor.as_str()).filter(|s| !s.is_empty());
    let pending = state
        .store
        .count_events_after(cursor_ref)
        .await
        .unwrap_or(0);
    // Nudge the sync task to drain now rather than waiting for the backstop.
    let _ = state.sync.trigger.try_send(());
    // A manual resync is user-initiated activity — wake the heartbeat too (WP-A3).
    state.presence_wake.notify_waiters();
    tracing::info!(pending, from = ?from, "resync: cursor rewound, flush triggered");
    Response::ResyncQueued { pending, from }
}

/// Build + runtime info for the running daemon (`dira version`). The version is
/// the daemon binary's own `CARGO_PKG_VERSION`, so the CLI can flag a skew when
/// it differs from the CLI build.
fn daemon_info(state: &AppState) -> Response {
    Response::DaemonInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: dira_contract::SCHEMA_VERSION.to_string(),
        pid: std::process::id(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        http_ingress_error: state.http_ingress_error.lock().unwrap().clone(),
    }
}

/// Wipe all local statistics and clear the live-session registry, so the daemon
/// doesn't keep reporting sessions whose events were just deleted. Device
/// identity is kept (the store preserves it).
async fn nuke(state: &AppState) -> Response {
    let (events, tokens) = match state.store.nuke().await {
        Ok(counts) => counts,
        Err(e) => {
            return Response::Error {
                message: format!("nuke failed: {e}"),
            }
        }
    };
    lock_recover(&state.sessions).clear();
    tracing::info!(events, tokens, "nuked local statistics");
    Response::Nuked { events, tokens }
}

/// Normalize a forwarded harness hook and enqueue it on the same path as HTTP
/// ingress. Acks immediately; enrichment + append happen in the writer task.
async fn ingest_hook(state: &AppState, harness: String, payload: serde_json::Value) -> Response {
    let (norm, harness_kind) = match dira_sources::normalize_for(&harness, payload) {
        Some(pair) => pair,
        None => {
            // Distinguish a genuinely unknown harness from a known harness whose
            // hook we don't account for (the latter is a normal, silent ignore).
            if dira_sources::is_known_harness(&harness) {
                return Response::Ok; // known harness, ignored/unknown hook
            }
            return Response::Error {
                message: format!("unknown harness: {harness}"),
            };
        }
    };

    let msg = EventMsg::Hook {
        norm,
        harness: harness_kind,
        at: OffsetDateTime::now_utc(),
    };
    match state.tx.try_send(msg) {
        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Response::Ok,
        Err(_) => Response::Error {
            message: "daemon shutting down".into(),
        },
    }
}

/// Resolve a project: explicit value wins, else resolve from cwd, else daemon cwd.
fn resolve(project: Option<String>, cwd: Option<String>) -> (Option<String>, Option<String>) {
    if let Some(p) = project {
        return (Some(p), None);
    }
    let dir = cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf())
    });
    let r = project::resolve(&dir);
    (r.project, r.identity_email)
}

async fn start(
    state: &AppState,
    project: Option<String>,
    label: Option<String>,
    activity: Option<String>,
    note: Option<String>,
    cwd: Option<String>,
) -> Response {
    let (project, identity) = resolve(project, cwd.clone());
    // Register the dira's working dir against its repo so the idle-ticker commit
    // poller can pick up commits made during a pure manual dira (no agent events).
    if let (Some(p), Some(dir)) = (project.as_deref(), cwd.as_deref()) {
        lock_recover_map(&state.repo_dirs).insert(p.to_string(), dir.to_string());
    }
    let session_id = Ulid::new().to_string();
    let handle = handle_of(&session_id);
    let ev = manual_event(
        &session_id,
        EventKind::ManualStart,
        OffsetDateTime::now_utc(),
        project.clone(),
        identity,
        label,
        activity,
        note,
    );
    if state.tx.send(EventMsg::Raw(Box::new(ev))).await.is_err() {
        return Response::Error {
            message: "daemon shutting down".into(),
        };
    }
    Response::Started { handle, project }
}

async fn stop(state: &AppState, selector: StopSelector) -> Response {
    let (by_handle, by_label, all, auto) = match selector {
        StopSelector::Handle { ref handle } => (Some(handle.as_str()), None, false, false),
        StopSelector::Label { ref label } => (None, Some(label.as_str()), false, false),
        StopSelector::All => (None, None, true, false),
        StopSelector::Auto => (None, None, false, true),
    };

    let targets =
        {
            let reg = lock_recover(&state.sessions);
            if auto {
                let manual = reg.active_manual();
                match manual.len() {
                    1 => vec![manual[0].session_id.clone()],
                    0 => {
                        return Response::Error {
                            message: "no active manual session".into(),
                        }
                    }
                    _ => return Response::Error {
                        message:
                            "several manual sessions active; specify a handle, --label, or --all"
                                .into(),
                    },
                }
            } else {
                reg.select_manual(by_handle, by_label, all)
            }
        };

    if targets.is_empty() {
        return Response::Error {
            message: "no matching active manual session".into(),
        };
    }

    let mut count = 0usize;
    for sid in targets {
        // Pull the session's project so the stop event is attributed correctly.
        let (project, identity, label) = {
            let reg = lock_recover(&state.sessions);
            reg.active_manual()
                .into_iter()
                .find(|s| s.session_id == sid)
                .map(|s| (s.project, s.identity_email, s.label))
                .unwrap_or((None, None, None))
        };
        let ev = manual_event(
            &sid,
            EventKind::ManualStop,
            OffsetDateTime::now_utc(),
            project,
            identity,
            label,
            None,
            // Note rides the ManualStart event; build_sessions takes first non-null.
            None,
        );
        if state.tx.send(EventMsg::Raw(Box::new(ev))).await.is_ok() {
            count += 1;
        }
    }
    Response::Stopped { count }
}

#[allow(clippy::too_many_arguments)]
async fn log(
    state: &AppState,
    duration_secs: u64,
    project: Option<String>,
    note: Option<String>,
    activity: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
) -> Response {
    let (project, identity) = resolve(project, cwd);
    let end = OffsetDateTime::now_utc();
    let start = end - Duration::seconds(duration_secs as i64);
    let session_id = Ulid::new().to_string();
    let handle = handle_of(&session_id);
    let events = materialize_interval(
        &session_id,
        start,
        end,
        project,
        identity,
        label,
        activity,
        note,
    );
    for ev in events {
        if state.tx.send(EventMsg::Raw(Box::new(ev))).await.is_err() {
            return Response::Error {
                message: "daemon shutting down".into(),
            };
        }
    }
    Response::Logged { handle }
}

async fn status(state: &AppState) -> Response {
    let since = start_of_today(state);
    let events = state
        .store
        .events_since(Some(since))
        .await
        .unwrap_or_default();
    let today = report::build(&events, state.config.idle());
    let active = build_session_views(state, &events, true);
    // Un-synced backlog: events past the confirmed sync cursor.
    let cursor = state
        .store
        .meta_get(crate::sync::META_SYNC_CURSOR)
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let sync_pending = state
        .store
        .count_events_after(cursor.as_deref())
        .await
        .unwrap_or(0);
    // Today's compute totals for the summary row. Best-effort: a read failure
    // (or an all-zero day) renders as "no compute row", never a failed status.
    let tokens = state
        .store
        .token_totals_since(Some(since))
        .await
        .ok()
        .map(|t| ComputeView {
            total_tokens: t.input + t.output + t.cache_read + t.cache_create,
            est_cost_usd: t.est_cost_usd,
        });
    // Last-known cloud billing summary (fetched + hydrated by the billing task).
    let billing = state
        .billing
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .map(|c| BillingView {
            billable_hours: c.summary.billable_hours,
            unbilled_amount: c.summary.unbilled_amount,
            currency: c.summary.currency,
            period: c.summary.period,
            fetched_at: c.fetched_at,
        });
    // Writer self-report (WP-B7): panics caught + dropped, watchdog stalls, and
    // whether it looks wedged right now, using the same definition the
    // supervisor's watchdog uses (`supervisor::writer_wedged`).
    let writer_health = Some(WriterHealthView {
        panics: state.progress.writer_panics(),
        stalls: state.progress.writer_stalls(),
        idle_secs: state.progress.writer_idle_secs(),
        wedged: crate::supervisor::writer_wedged(state),
    });
    // Sync self-report (WP-B9): the persisted per-flush snapshot `sync.rs`
    // writes after every attempt, plus the process-wide flush counters —
    // mirrors the writer-health pattern above. Always `Some` (unlike
    // `billing`), same as `writer_health`: a fresh daemon that has never
    // flushed still reports a well-defined all-zero/all-`None` snapshot.
    let sync_health = {
        let persisted = state
            .store
            .meta_get(dira_core::sync::META_SYNC_HEALTH)
            .await
            .ok()
            .flatten()
            .and_then(|j| dira_core::sync::parse_sync_health(&j))
            .unwrap_or_default();
        Some(SyncHealthView {
            last_attempt_at: persisted.last_attempt_at,
            last_success_at: persisted.last_success_at,
            last_error_kind: persisted.last_error_kind,
            consecutive_failures: persisted.consecutive_failures,
            backoff_secs: persisted.backoff_secs,
            cursor: persisted.cursor,
            cloud_watermark: persisted.cloud_watermark,
            flush_attempts: state.progress.flush_attempts(),
            flush_successes: state.progress.flush_successes(),
            flush_failures: state.progress.flush_failures(),
        })
    };
    Response::Status(Box::new(StatusView {
        active,
        today,
        sync_pending,
        hydrating: !state.hydrated.load(std::sync::atomic::Ordering::Relaxed),
        tokens,
        billing,
        writer_health,
        sync_health,
    }))
}

async fn sessions(state: &AppState) -> Response {
    let events = state
        .store
        .events_since(Some(start_of_today(state)))
        .await
        .unwrap_or_default();
    Response::Sessions {
        sessions: build_session_views(state, &events, false),
    }
}

async fn report_cmd(state: &AppState, scope: ReportScope) -> Response {
    let (since, project_filter) = match scope {
        ReportScope::Today => (Some(start_of_today(state)), None),
        ReportScope::Week => (Some(start_of_today(state) - Duration::days(7)), None),
        ReportScope::All => (None, None),
        ReportScope::Project { project } => (None, Some(project)),
    };
    let mut events = state.store.events_since(since).await.unwrap_or_default();
    if let Some(p) = &project_filter {
        events.retain(|e| e.project.as_deref() == Some(p.as_str()));
    }

    // Fold in the compacted historical rollup so totals survive retention. The
    // rollup window matches the report's lower bound: a `since` inside the
    // retention window means no rollups match (they only hold older data), so
    // `--today/--week` stay exactly raw.
    let since_day = since.and_then(|s| {
        s.format(&time::format_description::well_known::Iso8601::DATE)
            .ok()
    });
    let mut rollups = state
        .store
        .rollup_totals_since(since_day.as_deref())
        .await
        .unwrap_or_default();
    if let Some(p) = &project_filter {
        rollups.retain(|l| l.project.as_deref() == Some(p.as_str()));
    }
    let rollup_sessions = state
        .store
        .rollup_session_count(since_day.as_deref())
        .await
        .unwrap_or(0);

    Response::Report(report::build_merged(
        &events,
        state.config.idle(),
        &rollups,
        rollup_sessions,
    ))
}

/// Build session views, optionally only active ones.
fn build_session_views(
    state: &AppState,
    events: &[RawEvent],
    active_only: bool,
) -> Vec<SessionView> {
    let reg = lock_recover(&state.sessions);
    let live = if active_only { reg.active() } else { reg.all() };
    let now = OffsetDateTime::now_utc();
    let idle = state.config.idle();

    // De-duplicated human time attributed **per session** by the opening-signal
    // policy — the exact global attribution `report::build` uses per project. The
    // old path recomputed each session's human time from only *its own* signals, so
    // a session holding fewer than two prompts within the idle window read 0; in
    // parallel supervision (one prompt per session, then move on) that meant an
    // "engaged" session routinely showed HUMAN 0s. Attributing across the merged
    // timeline reconciles the per-session column to the TODAY total instead.
    let human_signals: Vec<(OffsetDateTime, String)> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| (e.at, e.session_id.clone()))
        .collect();
    let human_by_session = accounting::per_key_seconds(&human_signals, idle);

    live.into_iter()
        .map(|s| {
            let human = human_by_session.get(&s.session_id).copied().unwrap_or(0);
            let agent = session_agent_seconds(events, &s.session_id, idle);
            // Two independent notions of "recent": `idle` tracks the last *human*
            // signal (are you driving it?), `agent_active` tracks the last event of
            // any kind (is its agent working?). The activity basis mirrors the
            // presence heartbeat's "Right Now" idle, so a busy agent reads `active`
            // rather than `idle` even when you last prompted it long ago.
            let human_idle = s.is_idle(now, idle);
            let agent_active = human_idle && (now - s.last_event_at) <= idle;
            SessionView {
                handle: s.handle(),
                session_id: s.session_id.clone(),
                harness: format!("{:?}", s.harness),
                kind: format!("{:?}", s.kind),
                project: s.project.clone(),
                label: s.label.clone(),
                activity: s.activity.clone(),
                note: s.note.clone(),
                started_at: s
                    .started_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                human_seconds: human,
                agent_seconds: agent,
                idle: human_idle,
                agent_active,
                // Live tails for the `watch` dashboard: it grows the timers by
                // `now - these` (clamped to idle) between polls.
                last_activity_at: Some(fmt_ts(s.last_active_at.unwrap_or(s.last_event_at))),
                last_human_at: s.last_human_signal_at.or(s.last_signal_at).map(fmt_ts),
            }
        })
        .collect()
}

/// Format an instant as RFC3339 for the wire (empty string on the format error,
/// which never happens for a valid `OffsetDateTime`).
fn fmt_ts(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Per-session agent wall-clock seconds for display: the idle-trimmed active span
/// over the session's own event timestamps ([`accounting::active_seconds`]) when it
/// has any agent activity, else 0 — the same measure [`report::build`], the
/// sync/rollup path, and the cloud use, so a session left open for hours between
/// bursts of work reads as time actually worked, not its raw lifetime. Human time is
/// deliberately *not* computed here: it is a global, de-duplicated measure
/// attributed across all sessions (see [`accounting::per_key_seconds`] in
/// [`build_session_views`]), which a single session viewed in isolation cannot
/// reproduce.
pub(crate) fn session_agent_seconds(events: &[RawEvent], session_id: &str, idle: Duration) -> i64 {
    let mut times = Vec::new();
    let mut had_activity = false;
    for e in events.iter().filter(|e| e.session_id == session_id) {
        times.push(e.at);
        if e.kind.is_agent_activity() {
            had_activity = true;
        }
    }
    if had_activity {
        accounting::active_seconds(&times, idle)
    } else {
        0
    }
}

/// The start of "today" for report windows, as a UTC instant.
///
/// By default this is UTC midnight (`config.report_local_day == false`), which
/// preserves the original Phase 1 behavior. When `report_local_day` is enabled the
/// boundary is computed in the system local timezone via
/// [`dira_core::config::start_of_day`], using the offset captured at daemon startup
/// ([`crate::local_offset`]) — resolving it per-request would fail in the
/// multithreaded runtime. If no offset was captured we fall back to UTC so reporting
/// never breaks.
fn start_of_today(state: &AppState) -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    if !state.config.report_local_day {
        return now.replace_time(time::Time::MIDNIGHT);
    }
    match crate::local_offset() {
        Some(offset) => dira_core::config::start_of_day(now, offset),
        None => {
            tracing::debug!("local offset unavailable; using UTC day boundary");
            now.replace_time(time::Time::MIDNIGHT)
        }
    }
}
