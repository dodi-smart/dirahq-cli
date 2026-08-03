//! The control server: handles `dira` CLI commands over the platform IPC
//! channel (a Unix domain socket on unix, a named pipe on windows — see
//! `dira_ipc`).
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
    AnalyticsBreakdown, AnalyticsBucket, AnalyticsGrouping, BillingView, ComputeView, ReportScope,
    Request, Response, SessionView, StatusView, StopSelector, SyncHealthView, WriterHealthView,
};
use dira_core::report;
use dira_core::timeline;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::{Duration, OffsetDateTime};
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

/// Serve a single CLI connection.
///
/// `Request::Shutdown` gets one piece of special handling here (everything
/// else is a plain dispatch-then-reply): the daemon's shutdown notify is
/// fired *after* the response is written, not inside `dispatch` itself, so
/// the CLI's `Ok` is durably queued on the wire before `run()`'s shutdown
/// select can wake up and start tearing the process down. `dispatch` staying
/// pure (compute a `Response`, no side channel to the transport) also keeps
/// it trivially callable by tests without a real connection — see
/// `tests/daemon_stability.rs`'s direct `dispatch(&state, Request::Status)`
/// calls.
pub async fn handle_conn(state: AppState, mut stream: dira_ipc::Stream) {
    let parsed = match dira_ipc::read_frame(&mut stream).await {
        Ok(buf) => serde_json::from_slice::<Request>(&buf).map_err(|e| format!("bad request: {e}")),
        Err(e) => Err(format!("read error: {e}")),
    };
    let (resp, is_shutdown) = match parsed {
        Ok(req) => {
            let is_shutdown = matches!(req, Request::Shutdown);
            (dispatch(&state, req).await, is_shutdown)
        }
        Err(message) => (Response::Error { message }, false),
    };
    let bytes = serde_json::to_vec(&resp).unwrap_or_default();
    let _ = dira_ipc::write_frame(&mut stream, &bytes).await;
    if is_shutdown {
        // See the field doc on `AppState::shutdown`: `notify_one` stores a
        // permit even if `run()`'s select! hasn't started waiting yet, so
        // this can never race a startup window and drop the request.
        state.shutdown.notify_one();
    }
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
        Request::Timeline { before, days } => timeline_cmd(state, before, days).await,
        Request::Analytics { from, to, group_by } => analytics_cmd(state, from, to, group_by).await,
        Request::Projects { from, to } => projects_cmd(state, from, to).await,
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
        // The actual `state.shutdown.notify_one()` happens in `handle_conn`,
        // AFTER this `Ok` is written to the client — see its doc comment for
        // why the ordering matters and `dispatch` doesn't touch `shutdown`
        // itself.
        Request::Shutdown => Response::Ok,
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
        control_channel_warning: lock_recover(&state.control_channel_warning).clone(),
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
    let today = report::build(&events, state.config.idle(), state.config.agent_policy());
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
        state.config.agent_policy(),
        &rollups,
        rollup_sessions,
    ))
}

/// RFC3339, the one format every timestamp on this protocol uses.
fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn parse_rfc3339(s: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|e| format!("not an RFC3339 timestamp: {s} ({e})"))
}

/// One page of the Sessions timeline (see [`timeline`] and the cloud's D-0019).
///
/// `before` is the previous page's cursor; without it the page ends at now.
/// Consecutive pages tile with no gap (`ceiling(N+1) == floor(N)`), which is what
/// makes the head-in-`[floor, ceiling)` filter claim every unit exactly once.
async fn timeline_cmd(state: &AppState, before: Option<String>, days: Option<i64>) -> Response {
    let ceiling = match before.as_deref() {
        Some(s) => match parse_rfc3339(s) {
            Ok(t) => t,
            Err(message) => return Response::Error { message },
        },
        None => OffsetDateTime::now_utc(),
    };
    let days = days.unwrap_or(timeline::PAGE_DAYS).max(1);
    let floor = ceiling - Duration::days(days);

    // Padded on BOTH ends: a unit straddling either boundary must be visible
    // whole, so the page that owns its head assembles it complete and the
    // neighbouring page can defer instead of re-clustering a fragment.
    let events = state
        .store
        .events_in_window(
            &rfc3339(floor - timeline::SESSION_LOOKBACK),
            &rfc3339(ceiling + timeline::SESSION_LOOKBACK),
        )
        .await
        .unwrap_or_default();

    let sessions =
        timeline::summarize_sessions(&events, state.config.idle(), state.config.agent_policy());
    let units = timeline::page(
        timeline::assemble_work_units(&sessions),
        timeline::epoch_ms(floor),
        timeline::epoch_ms(ceiling),
    );

    let earlier_sessions = state
        .store
        .count_sessions_before(&rfc3339(floor))
        .await
        .unwrap_or(0);

    Response::Timeline {
        units,
        // Advance on the COUNT, never on whether this page happened to be empty:
        // a quiet week is still a page, and stopping there would silently truncate
        // history at the first gap.
        cursor: (earlier_sessions > 0).then(|| rfc3339(floor)),
        earlier_sessions,
    }
}

/// Time + token-cost rollups over an explicit window.
async fn analytics_cmd(
    state: &AppState,
    from: String,
    to: String,
    group_by: AnalyticsGrouping,
) -> Response {
    let (from_t, to_t) = match (parse_rfc3339(&from), parse_rfc3339(&to)) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(message), _) | (_, Err(message)) => return Response::Error { message },
    };
    if to_t <= from_t {
        return Response::Error {
            message: format!("empty window: `to` ({to}) is not after `from` ({from})"),
        };
    }
    let (from_s, to_s) = (rfc3339(from_t), rfc3339(to_t));

    let events = state
        .store
        .events_in_window(&from_s, &to_s)
        .await
        .unwrap_or_default();
    let tokens = state
        .store
        .token_usage_in_window(&from_s, &to_s)
        .await
        .unwrap_or_default();

    let sessions =
        timeline::summarize_sessions(&events, state.config.idle(), state.config.agent_policy());

    // Session id → its bucket key, so a token row (which knows only its session)
    // lands in the same bucket as the time that produced it.
    let day_offset = if state.config.report_local_day {
        crate::local_offset().unwrap_or(time::UtcOffset::UTC)
    } else {
        time::UtcOffset::UTC
    };

    let mut buckets: std::collections::BTreeMap<Option<String>, AnalyticsBucket> =
        std::collections::BTreeMap::new();
    let mut key_of_session: std::collections::HashMap<&str, Option<String>> =
        std::collections::HashMap::new();

    for s in &sessions {
        let key = match group_by {
            AnalyticsGrouping::Day => Some(day_key(&s.started_at, day_offset)),
            AnalyticsGrouping::Project => s.project.clone(),
            AnalyticsGrouping::Harness => Some(harness_key(s.harness)),
            // A model does not do human time — only token turns carry a model, so
            // model buckets are populated entirely from the token side below.
            AnalyticsGrouping::Model => None,
        };
        key_of_session.insert(s.session_id.as_str(), key.clone());
        if group_by == AnalyticsGrouping::Model {
            continue;
        }
        let b = buckets.entry(key.clone()).or_insert_with(|| new_bucket(key));
        b.human_seconds += s.human_seconds;
        b.agent_wall_seconds += s.agent_seconds;
    }

    for t in &tokens {
        let key = match group_by {
            AnalyticsGrouping::Day => Some(day_key(&t.at, day_offset)),
            AnalyticsGrouping::Model => Some(t.model.clone()),
            // Prefer the token row's own project, but fall back to its session's:
            // a turn captured before the repo resolved still belongs to the work
            // that produced it.
            AnalyticsGrouping::Project => t.project.clone().or_else(|| {
                key_of_session
                    .get(t.session_id.as_str())
                    .cloned()
                    .flatten()
            }),
            AnalyticsGrouping::Harness => key_of_session
                .get(t.session_id.as_str())
                .cloned()
                .flatten(),
        };
        let b = buckets.entry(key.clone()).or_insert_with(|| new_bucket(key));
        b.input_tokens += t.input;
        b.output_tokens += t.output;
        b.cache_read_tokens += t.cache_read;
        b.cache_create_tokens += t.cache_create;
        b.est_cost_usd += t.est_cost_usd.unwrap_or(0.0);
    }

    let buckets: Vec<AnalyticsBucket> = buckets.into_values().collect();
    Response::Analytics(AnalyticsBreakdown {
        group_by,
        from: from_s,
        to: to_s,
        total_human_seconds: buckets.iter().map(|b| b.human_seconds).sum(),
        total_agent_wall_seconds: buckets.iter().map(|b| b.agent_wall_seconds).sum(),
        total_est_cost_usd: buckets.iter().map(|b| b.est_cost_usd).sum(),
        buckets,
        // The bundled per-family table is an approximation, not the cloud's
        // maintained one. Say so on the wire so no renderer can imply otherwise.
        cost_is_estimated: true,
    })
}

fn new_bucket(key: Option<String>) -> AnalyticsBucket {
    AnalyticsBucket {
        key,
        human_seconds: 0,
        agent_wall_seconds: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_create_tokens: 0,
        est_cost_usd: 0.0,
    }
}

/// `YYYY-MM-DD` in the reporting timezone — the same day boundary `report` and
/// `status` use, so a day bucket and "today" can never disagree.
fn day_key(rfc: &str, offset: time::UtcOffset) -> String {
    match parse_rfc3339(rfc) {
        Ok(t) => {
            let local = t.to_offset(offset);
            format!(
                "{:04}-{:02}-{:02}",
                local.year(),
                local.month() as u8,
                local.day()
            )
        }
        Err(_) => "unknown".into(),
    }
}

fn harness_key(h: dira_contract::Harness) -> String {
    serde_json::to_value(h)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// Per-project time rollups over an explicit window.
async fn projects_cmd(state: &AppState, from: String, to: String) -> Response {
    let (from_t, to_t) = match (parse_rfc3339(&from), parse_rfc3339(&to)) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(message), _) | (_, Err(message)) => return Response::Error { message },
    };
    if to_t <= from_t {
        return Response::Error {
            message: format!("empty window: `to` ({to}) is not after `from` ({from})"),
        };
    }

    let events = state
        .store
        .events_in_window(&rfc3339(from_t), &rfc3339(to_t))
        .await
        .unwrap_or_default();

    // Same builder `Report` uses, so a windowed project rollup and a scoped one
    // can never disagree about how time is counted.
    let built = report::build(&events, state.config.idle(), state.config.agent_policy());
    Response::Projects {
        projects: built.projects,
    }
}

/// Build session views, optionally only active ones.
fn build_session_views(
    state: &AppState,
    events: &[RawEvent],
    active_only: bool,
) -> Vec<SessionView> {
    let now = OffsetDateTime::now_utc();
    let reg = lock_recover(&state.sessions);
    let live = if active_only {
        reg.active(now, state.config.session_stale_after())
    } else {
        reg.all()
    };
    let idle = state.config.idle();
    let agent_policy = state.config.agent_policy();

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
            let (has_agent_activity, agent) =
                session_agent_evidence(events, &s.session_id, agent_policy);
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
                harness: s.harness,
                kind: s.kind,
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
                has_agent_activity,
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

/// Whether a session has any agent-activity evidence, and its agent wall-clock
/// seconds — computed in **one** pass so the two can never disagree.
///
/// The pair matters: `SessionView.has_agent_activity` is what gates the `watch`
/// dashboard's live tail, and if it could ever say "yes" while the seconds say
/// "this session has no agent events", a manual session would grow phantom agent
/// time again (the 30-second sawtooth).
///
/// The seconds use [`accounting::agent_active_seconds`] — the same measure
/// [`report::build`], the sync/rollup path, and the cloud use, so a session left
/// open for hours between bursts of work reads as time actually worked, not its
/// raw lifetime. Human time is deliberately *not* computed here: it is a global,
/// de-duplicated measure attributed across all sessions (see
/// [`accounting::per_key_seconds`] in [`build_session_views`]), which a single
/// session viewed in isolation cannot reproduce.
pub(crate) fn session_agent_evidence(
    events: &[RawEvent],
    session_id: &str,
    agent: accounting::AgentPolicy,
) -> (bool, i64) {
    let mut samples = Vec::new();
    let mut had_activity = false;
    for e in events.iter().filter(|e| e.session_id == session_id) {
        samples.push(accounting::AgentSample {
            at: e.at,
            opens_span: e.kind.opens_agent_span(),
        });
        if e.kind.is_agent_activity() {
            had_activity = true;
        }
    }
    if had_activity {
        (true, accounting::agent_active_seconds(&samples, agent))
    } else {
        (false, 0)
    }
}

/// The start of "today" for report windows, as a UTC instant.
///
/// By default (`config.report_local_day == true`) this is **local** midnight, so
/// "today" means the user's today rather than a boundary that lands mid-morning
/// for anyone east of UTC. Set the knob `false` for UTC boundaries.
///
/// When local, the boundary is computed in the system local timezone via
/// [`dira_core::config::start_of_day`], using the offset captured **once at daemon
/// startup** ([`crate::local_offset`]) — `UtcOffset::current_local_offset` refuses
/// to run once the runtime has spawned threads, so resolving it per request would
/// always fail and silently fall back to UTC. If no offset was captured we fall
/// back to UTC so reporting never breaks.
///
/// Known limitation: a daemon running across a DST transition (or a laptop that
/// changes timezone) keeps its startup offset until restarted, so the boundary can
/// be an hour off until then.
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
