//! `dirad` — the resident Dira daemon (library crate).
//!
//! Owns all state: ingress (loopback HTTP + UDS), the session/idle accounting,
//! the append-only store, and signing + sync. Designed so the ingress hot path
//! only ever does a non-blocking channel send; everything heavy runs in the
//! single writer task or background timers.
//!
//! [`run`] is mostly *wiring*: the drain loop lives in [`writer`], git commit
//! capture in [`capture`] (off the hot path via `spawn_blocking` + `timeout`),
//! and a [`supervisor`] watchdog tracks writer/ticker liveness and self-heals.
//!
//! **Startup ordering (Commit 2):** the control socket (+ `/healthz`) binds
//! *before* hydration so `dira status`/`Ping` answer immediately; [`hydrate`]
//! then runs on a background task and the device signing key loads lazily off the
//! critical path (a keychain prompt never delays socket readiness).
//!
//! The library form exists so integration tests under `tests/` can stand up a
//! real daemon (bind a socket, drive `Ping`/`Status`) against the same code the
//! binary runs.

pub mod billing;
pub mod capture;
pub mod control;
pub mod events;
pub mod heartbeat;
pub mod http;
pub mod state;
pub mod supervisor;
pub mod sync;
pub mod writer;

use crate::state::{AppState, EventMsg, ProgressTracker, SessionRegistry};
use crate::writer::QUEUE_CAPACITY;
use dira_core::{Config, Store};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use time::{Duration, OffsetDateTime, UtcOffset};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{mpsc, OnceCell};
use ulid::Ulid;

/// Idle-ticker cadence; must be below the idle threshold so manual sessions accrue.
pub const TICK_INTERVAL_SECS: u64 = 30;

/// Process-wide local UTC offset, captured once at startup by [`init_local_offset`].
static LOCAL_OFFSET: OnceLock<Option<UtcOffset>> = OnceLock::new();

/// Resolve and cache the system local UTC offset for `report_local_day`.
///
/// **Must be called while the process is still single-threaded** — before the tokio
/// runtime spawns worker threads. `time::UtcOffset::current_local_offset()` refuses
/// to run once other threads exist (a `localtime_r`/`setenv` soundness guard), so
/// calling it per-request inside the multithreaded daemon *always* errors and falls
/// back to UTC. Capturing the offset at boot, when only the main thread exists, lets
/// the report day boundary actually land on local midnight. Idempotent; later calls
/// are ignored.
pub fn init_local_offset() {
    let _ = LOCAL_OFFSET.set(UtcOffset::current_local_offset().ok());
}

/// The local UTC offset captured at startup, or `None` when it was never resolved
/// (never initialised, or resolution failed). Callers fall back to a UTC day
/// boundary on `None`.
pub fn local_offset() -> Option<UtcOffset> {
    LOCAL_OFFSET.get().copied().flatten()
}

/// Assemble the shared [`AppState`] from an opened store + config.
///
/// Returns the state plus the writer's ingest receiver and the sync trigger
/// receiver, which the caller hands to the supervisor and the sync task. The
/// device signing key is **not** loaded here — it loads lazily on first use (see
/// [`AppState::device_key`]) so a keychain prompt never blocks startup.
pub async fn build_state(
    store: Store,
    config: Config,
) -> anyhow::Result<(AppState, mpsc::Receiver<EventMsg>, mpsc::Receiver<()>)> {
    let bearer = resolve_bearer(&store).await?;
    let (tx, rx) = mpsc::channel::<EventMsg>(QUEUE_CAPACITY);
    let sessions = Arc::new(Mutex::new(SessionRegistry::default()));
    let (sync_handle, sync_rx) = sync::channel();

    let state = AppState {
        store,
        tx,
        sessions,
        config,
        started_at: std::time::Instant::now(),
        bearer: Arc::new(bearer),
        sync: sync_handle,
        device_key: Arc::new(OnceCell::new()),
        progress: Arc::new(ProgressTracker::default()),
        hydrated: Arc::new(AtomicBool::new(false)),
        repo_dirs: Arc::new(Mutex::new(HashMap::new())),
        presence_hints: Arc::new(crate::state::PresenceHints::default()),
        billing: Arc::new(Mutex::new(None)),
        billing_refresh: Arc::new(tokio::sync::Notify::new()),
    };
    Ok((state, rx, sync_rx))
}

/// Bind the UDS control socket and spawn its accept loop on `state`. Bound
/// *before* hydration so the daemon answers `Ping`/`Status` immediately
/// (Commit 2). Used by both [`run`] and the integration tests.
pub fn serve_control(state: AppState, uds: UnixListener) {
    tokio::spawn(async move {
        loop {
            match uds.accept().await {
                Ok((stream, _)) => {
                    let st = state.clone();
                    tokio::spawn(control::handle_conn(st, stream));
                }
                Err(e) => tracing::warn!("uds accept error: {e}"),
            }
        }
    });
}

/// Run the daemon to completion (until Ctrl-C or the accept loop ends).
pub async fn run() -> anyhow::Result<()> {
    let config = Config::load().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    tracing::info!(db = %config.db_path.display(), sock = %config.socket_path.display(), port = config.http_port, "starting dirad");

    let store = Store::open(&config.db_path).await?;
    let (state, rx, sync_rx) = build_state(store, config).await?;

    // --- Bind the ingress surfaces FIRST (Commit 2) ---------------------------
    // Bind the control socket and HTTP *before* hydration so the daemon answers
    // `Ping`/`Status` the instant it's up. Hydration then runs on a background
    // task; a status during warm-up reports `hydrating: true` rather than hanging
    // on a multi-second log replay or returning a connection error.

    // HTTP ingress (loopback only).
    let http_addr = format!("127.0.0.1:{}", state.config.http_port);
    let http_listener = TcpListener::bind(&http_addr).await?;
    let http_app = http::router(state.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(http_listener, http_app).await {
            tracing::error!("http server error: {e}");
        }
    });
    tracing::info!("hook ingress on http://{http_addr}/hooks/claude");

    // UDS control channel.
    let sock = state.config.socket_path.clone();
    let _ = std::fs::remove_file(&sock); // clear stale socket
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let uds = UnixListener::bind(&sock)?;
    set_socket_perms(&sock);
    tracing::info!(sock = %sock.display(), "control socket ready");

    // --- Background work (after the sockets are live) -------------------------
    // Rebuild live-session state from the log so a daemon bounce loses nothing.
    // Off the critical path so it never delays socket readiness; flips
    // `hydrated` when done so `status` can stop reporting `hydrating`.
    spawn_hydrate(state.clone());

    // The single writer + the idle ticker run under a supervisor that tracks their
    // liveness and self-heals a one-off panic, so a stall can never silently end
    // timer accrual (Commit 1).
    supervisor::spawn(state.clone(), rx);

    // Retention/compaction: roll up & prune old, already-synced events.
    tokio::spawn(maintenance(state.clone()));
    // Background cloud sync (off the hot path; no-ops until linked + cloud_url set).
    sync::spawn(state.clone(), sync_rx);
    // Live-presence heartbeat (ephemeral; no-ops until linked + cloud_url set).
    heartbeat::spawn(state.clone());
    // Cloud billing-summary fetch (best-effort; no-ops until linked + cloud_url set).
    billing::spawn(state.clone());
    // Best-effort schema-version handshake: warn if our contract version is
    // outside the cloud's supported range. Non-fatal; skipped when cloud_url is
    // unset. Run detached so it never delays startup.
    {
        let cloud_url = state.config.cloud_url.clone();
        tokio::spawn(async move { sync::check_schema_handshake(cloud_url.as_deref()).await });
    }

    serve_control(state.clone(), uds);

    // Block until shutdown. The accept loop runs detached in `serve_control`.
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutting down");
    // Graceful offline: tell the cloud this device is going offline with one
    // best-effort empty-sessions beat (short timeout, errors ignored) so it
    // doesn't wait out the presence TTL.
    heartbeat::send_offline_beat(&state).await;

    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Best-effort restrict the control socket to the owner (0600). No-op on non-unix.
pub fn set_socket_perms(sock: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = sock;
}

/// Spawn the background hydrate, flipping `hydrated` true when it completes.
pub fn spawn_hydrate(state: AppState) {
    tokio::spawn(async move {
        hydrate(&state).await;
        state
            .hydrated
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Periodically emit a tick for each open manual session so it accrues
/// continuously, and sweep every known repo for new commits (caught even when no
/// agent events are flowing). Marks ticker progress for the watchdog each tick.
///
/// Loops forever; the supervisor treats any *panic* as a fault and re-spawns it.
pub async fn idle_ticker(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(TICK_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        state.progress.mark_ticker();

        let manual = control::lock_recover(&state.sessions).active_manual();
        let now = OffsetDateTime::now_utc();
        for s in manual {
            let ev = events::manual_event(
                &s.session_id,
                dira_core::model::EventKind::ManualTick,
                now,
                s.project.clone(),
                s.identity_email.clone(),
                s.label.clone(),
                s.activity.clone(),
                s.note.clone(),
            );
            // If the writer is gone the channel send errors; nothing else to do.
            let _ = state.tx.send(EventMsg::Raw(Box::new(ev))).await;
        }

        // Sweep every known repo for new commits. The per-repo baseline check makes
        // an unchanged HEAD a cheap no-op, so this catches manual commits even when
        // no agent events are flowing (e.g. a pure manual session). Each capture is
        // spawned detached + time-boxed, so a wedged git never stalls the ticker.
        let dirs: Vec<(String, String)> = control::lock_recover_map(&state.repo_dirs)
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (canonical, cwd) in dirs {
            capture::spawn_capture(&state, &cwd, &canonical);
        }
    }
}

/// Maintenance cadence: how often the retention sweep runs. Low frequency —
/// compaction is cheap when there's nothing to do (a bounded query) and we never
/// want it competing with the capture path.
const MAINTENANCE_INTERVAL_SECS: u64 = 3600;
/// Run a full `VACUUM` at most once every this-many sweeps (≈ daily at hourly
/// cadence). A WAL-truncate checkpoint runs every sweep; VACUUM is heavier.
const VACUUM_EVERY_N_SWEEPS: u64 = 24;

/// Periodically roll up & prune old, already-synced events, then compact the
/// on-disk file. Best-effort and crash-safe: [`Store::compact`] does the
/// summarize+delete in one transaction and only touches rows that are both past
/// the sync cursor's confirmation *and* older than the retention window, so
/// un-synced or recent data is never lost.
pub async fn maintenance(state: AppState) {
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(MAINTENANCE_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sweeps: u64 = 0;
    loop {
        interval.tick().await;
        sweeps += 1;

        let cursor = state.store.sync_cursor().await.ok().flatten();
        let cutoff = OffsetDateTime::now_utc() - state.config.retention();
        match state
            .store
            .compact(cursor.as_deref(), cutoff, state.config.idle())
            .await
        {
            Ok(0) => continue, // nothing eligible — skip the heavier compaction
            Ok(deleted) => {
                tracing::info!(deleted, "compacted old synced events into daily rollup");
            }
            Err(e) => {
                tracing::warn!("compaction failed: {e}");
                continue;
            }
        }

        // Fold the deletes' WAL back into the main file every sweep; VACUUM only
        // periodically (it rewrites the whole file).
        if let Err(e) = state.store.wal_checkpoint_truncate().await {
            tracing::debug!("wal checkpoint failed: {e}");
        }
        if sweeps % VACUUM_EVERY_N_SWEEPS == 0 {
            if let Err(e) = state.store.vacuum().await {
                tracing::debug!("vacuum failed: {e}");
            }
        }
    }
}

/// Rebuild the in-memory registry from today's event log.
///
/// This is also the cold-start reconstruction for the rolling counters:
/// replaying the recent log through `observe` rebuilds each session's
/// `engaged_seconds` / `active_seconds` exactly as they were before a bounce, so a
/// daemon restart doesn't reset the live totals the heartbeat reports. The 1-day
/// lookback matches the heartbeat's old scan window; older idle gaps never
/// contribute (idle-trim already excludes any gap wider than `idle`).
pub async fn hydrate(state: &AppState) {
    let since = OffsetDateTime::now_utc() - Duration::days(1);
    let idle = state.config.idle();
    if let Ok(events) = state.store.events_since(Some(since)).await {
        let mut reg = control::lock_recover(&state.sessions);
        for ev in &events {
            reg.observe(ev, idle);
        }
        tracing::info!(count = events.len(), "hydrated registry from event log");
    }
}

/// Resolve the HTTP bearer token: `DIRA_BEARER` env, else stored, else generate.
pub async fn resolve_bearer(store: &Store) -> anyhow::Result<String> {
    if let Ok(env) = std::env::var("DIRA_BEARER") {
        if !env.is_empty() {
            return Ok(env);
        }
    }
    if let Some(existing) = store.meta_get("bearer").await? {
        return Ok(existing);
    }
    let token = Ulid::new().to_string();
    store.meta_set("bearer", &token).await?;
    Ok(token)
}
