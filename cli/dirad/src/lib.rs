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
pub mod jitter;
pub mod knowledge_sync;
pub mod state;
pub mod supervisor;
pub mod sync;
#[cfg(test)]
pub(crate) mod test_support;
pub mod writer;
pub mod zavet;

use crate::state::{AppState, EventMsg, ProgressTracker, SessionRegistry};
use crate::writer::QUEUE_CAPACITY;
use anyhow::Context;
use dira_core::{Config, Store};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use time::{Duration, OffsetDateTime, UtcOffset};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
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
) -> anyhow::Result<(
    AppState,
    mpsc::Receiver<EventMsg>,
    mpsc::Receiver<()>,
    mpsc::Receiver<()>,
)> {
    let bearer = resolve_bearer(&store).await?;
    let (tx, rx) = mpsc::channel::<EventMsg>(QUEUE_CAPACITY);
    let sessions = Arc::new(Mutex::new(SessionRegistry::with_agent_policy(
        config.agent_policy(),
    )));
    let (sync_handle, sync_rx) = sync::channel();
    let (knowledge_handle, knowledge_rx) = knowledge_sync::channel();

    // One pooled HTTP client for every device→cloud task. Keep-alive so repeat
    // POSTs (heartbeat/sync/billing) to the same cloud host reuse the connection
    // instead of a fresh TCP/TLS handshake per tick. No default timeout — callers
    // set a per-request timeout sized to that call. A build failure here now fails
    // daemon startup outright rather than silently disabling whichever task used
    // to build its own client.
    let http = reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(120))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build shared http client: {e}"))?;

    let state = AppState {
        store,
        tx,
        sessions,
        config,
        http,
        started_at: std::time::Instant::now(),
        bearer: Arc::new(bearer),
        sync: sync_handle,
        knowledge_sync: knowledge_handle,
        device_key: Arc::new(tokio::sync::RwLock::new(None)),
        progress: Arc::new(ProgressTracker::default()),
        hydrated: Arc::new(AtomicBool::new(false)),
        repo_dirs: Arc::new(Mutex::new(HashMap::new())),
        presence_hints: Arc::new(crate::state::PresenceHints::default()),
        billing: Arc::new(Mutex::new(None)),
        billing_refresh: Arc::new(tokio::sync::Notify::new()),
        presence_wake: Arc::new(tokio::sync::Notify::new()),
        shutdown: Arc::new(tokio::sync::Notify::new()),
        http_ingress_error: Arc::new(Mutex::new(None)),
        control_channel_warning: Arc::new(Mutex::new(None)),
    };
    Ok((state, rx, sync_rx, knowledge_rx))
}

/// Holds the daemon's single-instance guard for as long as the listener bound
/// beside it is in use.
///
/// unix: an exclusive `flock` on `<sock>.lock`. Dropping it releases the lock,
/// and the kernel releases it if the process dies — so it can never go stale
/// the way a leftover socket file (or a pidfile) can.
///
/// windows: nothing to hold — the named pipe's `first_pipe_instance` bind is
/// itself the kernel-held guard (a second `create` on a live pipe name fails,
/// and a pipe name cannot exist stale: the namespace entry dies with its last
/// handle). The type still exists so [`bind_control_socket`]'s contract —
/// "keep this alive as long as the listener serves" — reads the same on both
/// platforms.
#[derive(Debug)]
pub struct SocketLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

/// Bind the control channel at `sock`, refusing to take it from a daemon that
/// is already using it. Hold the returned [`SocketLock`] as long as the
/// listener is in use.
///
/// unix: the probe is a plain `connect`: a live listener accepts, while a
/// socket file orphaned by a dead daemon gives `ECONNREFUSED`. Only the latter
/// is unlinked. This matters because binding a UDS *requires* the path to be
/// free, so the pre-D-0009 code unlinked unconditionally — which meant a
/// second daemon took the path away from a healthy first one. The first
/// daemon's listener stayed alive on an unlinked inode, reachable by nobody,
/// and no error was logged on either side.
///
/// The probe→unlink→bind sequence is not atomic on its own: two daemons racing
/// onto a *stale* socket file can both see "nothing answers" before either
/// unlinks, and the loser's unlink then steals the path from the winner's
/// freshly bound listener — the same orphaned-listener failure via a second
/// door. An exclusive `flock` on `<sock>.lock`, taken before the probe and held
/// for the daemon's lifetime, makes the sequence mutually exclusive (the
/// stale-path unlink inside `dira_ipc::Listener::bind` runs under it). Unlike
/// the pidfile lock D-0009 rejected, an `flock` cannot go stale: the kernel
/// drops it on any exit, SIGKILL included. The probe stays: a pre-lock daemon
/// holds the socket without holding any lock.
///
/// This is also the daemon's only single-instance guard. It used to be provided
/// by accident: the HTTP port was bound first, so a duplicate died on
/// `EADDRINUSE` before it could reach the unlink. Now that an unavailable port
/// is survivable (see [`run`]), the guard has to be explicit.
///
/// windows: none of the filesystem failure modes exist — a pipe name is a
/// kernel-namespace entry, never a stale file — and
/// `first_pipe_instance(true)` inside `dira_ipc::Listener::bind` makes the
/// bind itself refuse a name another daemon holds live, atomically. So the
/// whole guard is the bind, and the returned [`SocketLock`] holds nothing.
pub async fn bind_control_socket(
    sock: &std::path::Path,
) -> anyhow::Result<(dira_ipc::Listener, SocketLock)> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        if let Some(parent) = sock.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let lock_path = sock.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| {
                format!("failed to open the socket lock at {}", lock_path.display())
            })?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::bail!(
                    "another dirad is already running (or starting) on {} — stop it first, or \
                     use `dira daemon restart` to replace it",
                    sock.display(),
                );
            }
            return Err(err).with_context(|| {
                format!("failed to lock the socket lock at {}", lock_path.display())
            });
        }

        if sock.exists() && dira_ipc::connect(sock).await.is_ok() {
            anyhow::bail!(
                "another dirad is already running on {} — stop it first, or use \
                 `dira daemon restart` to replace it",
                sock.display(),
            );
        }
        // Either nothing at the path, or a leftover file nothing answers on;
        // `dira_ipc::Listener::bind` clears the stale file (safe: we hold the
        // flock and just proved nothing live is behind it), creates the parent
        // dir, binds, and restricts the socket to the owner (0600).
        let listener = dira_ipc::Listener::bind(sock)
            .await
            .with_context(|| format!("failed to bind the control socket at {}", sock.display()))?;
        Ok((listener, SocketLock { _file: lock }))
    }
    #[cfg(windows)]
    {
        let listener = dira_ipc::Listener::bind(sock).await.with_context(|| {
            format!(
                "failed to bind the control pipe at {} — another dirad may already be \
                 running (stop it first, or use `dira daemon restart` to replace it)",
                sock.display()
            )
        })?;
        Ok((listener, SocketLock {}))
    }
}

/// Serve the loopback hook ingress on `addr`, treating an unavailable port as
/// a degradation rather than a fatal error.
///
/// A port conflict used to abort startup — and because the port was bound
/// before the control socket, the daemon died without ever becoming
/// introspectable, so every client reported "down" while `KeepAlive` respawned
/// it on a 10s loop. The control socket is now bound first and survives this,
/// so the daemon stays answerable and *reports* why capture is not flowing.
///
/// On failure a background task retries with exponential backoff from
/// `retry_base` (capped at [`HTTP_RETRY_MAX`]) until the port frees, then
/// clears the degradation. That self-healing is what removes the respawn loop:
/// there is no longer anything for the supervisor to restart. Returns once the
/// outcome is known; serving and retrying continue detached.
pub async fn serve_http_ingress(state: AppState, addr: String, retry_base: std::time::Duration) {
    match TcpListener::bind(&addr).await {
        Ok(listener) => {
            *state.http_ingress_error.lock().unwrap() = None;
            tracing::info!("hook ingress on http://{addr}/hooks/claude");
            spawn_http_server(state, listener);
        }
        Err(e) => {
            let reason = format!(
                "could not bind the hook ingress on {addr}: {e} — another dirad may already \
                 be running (check `dira daemon status`), or set a different `http_port`"
            );
            tracing::error!(
                "{reason}; the daemon is running DEGRADED: control socket is up, but harness \
                 hooks cannot reach it, so no capture will flow until the port frees"
            );
            *state.http_ingress_error.lock().unwrap() = Some(reason);
            spawn_http_retry(state, addr, retry_base);
        }
    }
}

/// First delay before retrying an unavailable hook-ingress port.
pub const HTTP_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(5);

/// Ceiling for the hook-ingress rebind backoff.
pub const HTTP_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

fn spawn_http_server(state: AppState, listener: TcpListener) {
    let app = http::router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server error: {e}");
        }
    });
}

/// Retry the ingress bind until it succeeds, then clear the degradation.
fn spawn_http_retry(state: AppState, addr: String, retry_base: std::time::Duration) {
    tokio::spawn(async move {
        let mut delay = retry_base;
        loop {
            tokio::time::sleep(delay).await;
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    *state.http_ingress_error.lock().unwrap() = None;
                    tracing::info!("hook ingress recovered on http://{addr}/hooks/claude");
                    spawn_http_server(state, listener);
                    return;
                }
                // Still held. Back off so a permanently-occupied port costs
                // one bind attempt a minute, not a spin.
                Err(_) => delay = (delay * 2).min(HTTP_RETRY_MAX),
            }
        }
    });
}

/// Spawn the accept loop for a bound control listener (UDS on unix, a named
/// pipe on windows — see `dira_ipc`) on `state`. Bound *before* hydration so
/// the daemon answers `Ping`/`Status` immediately (Commit 2). Used by both
/// [`run`] and the integration tests; production binds via
/// [`bind_control_socket`] so the single-instance guard applies.
pub fn serve_control(state: AppState, mut listener: dira_ipc::Listener) {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let st = state.clone();
                    tokio::spawn(control::handle_conn(st, stream));
                }
                Err(e) => tracing::warn!("control accept error: {e}"),
            }
        }
    });
}

/// Run the daemon to completion (until Ctrl-C or the accept loop ends).
pub async fn run() -> anyhow::Result<()> {
    let config = Config::load().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    tracing::info!(db = %config.db_path.display(), sock = %config.socket_path.display(), port = config.http_port, "starting dirad");

    let store = Store::open(&config.db_path).await?;
    let (state, rx, sync_rx, knowledge_rx) = build_state(store, config).await?;

    // --- Bind the ingress surfaces FIRST (Commit 2) ---------------------------
    // Bind the control socket and HTTP *before* hydration so the daemon answers
    // `Ping`/`Status` the instant it's up. Hydration then runs on a background
    // task; a status during warm-up reports `hydrating: true` rather than hanging
    // on a multi-second log replay or returning a connection error.
    //
    // The control socket goes first (D-0009). It is the surface every client and
    // every supervision probe depends on, so it must survive an unavailable HTTP
    // port — the reverse order meant a port conflict killed the daemon before it
    // was ever introspectable. Ordering the UDS first is only safe because
    // `bind_control_socket` refuses to take the path from a live daemon; it used
    // to unlink unconditionally, and the HTTP bind was the accidental guard.

    // Control channel (UDS on unix, a named pipe on windows — see `dira_ipc`).
    // The lock lives until `run` returns — i.e. for the daemon's whole life —
    // so no duplicate can race the endpoint meanwhile. `bind_control_socket`
    // owns the guard + bind on both platforms (flock+probe on unix, the pipe's
    // first-instance bind on windows), including the owner-only socket perms.
    let sock = state.config.socket_path.clone();
    let (listener, _socket_lock) = bind_control_socket(&sock).await?;
    tracing::info!(sock = %sock.display(), "control socket ready");

    // Report anything that makes this control channel less reachable than
    // intended. Precedence is deliberate: a failed security descriptor *plus*
    // elevation is the genuinely-broken combination, so the descriptor message
    // wins — it is the one that explains why hooks will be refused.
    //
    // Elevation alone is a warning, never a refusal (see
    // `dira_ipc::elevation::daemon_elevation_warning`): with the descriptor
    // applied an elevated daemon works, and exiting here would recreate D-0009's
    // respawn-loop shape with every client reporting "down".
    let control_warning = listener.security_degradation().or_else(|| {
        dira_ipc::elevation::daemon_elevation_warning(dira_ipc::elevation::is_elevated())
    });
    if let Some(w) = &control_warning {
        tracing::warn!("{w}");
    }
    *control::lock_recover(&state.control_channel_warning) = control_warning;

    // HTTP ingress (loopback only). Survivable: a conflict leaves the daemon up
    // and degraded, retrying in the background.
    let http_addr = format!("127.0.0.1:{}", state.config.http_port);
    serve_http_ingress(state.clone(), http_addr, HTTP_RETRY_BASE).await;

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
    // Knowledge sync (M2): consent-gated, no-ops until [sync] knowledge is
    // enabled AND the device is linked.
    knowledge_sync::spawn(state.clone(), knowledge_rx);
    // Live-presence heartbeat (ephemeral; no-ops until linked + cloud_url set).
    heartbeat::spawn(state.clone());
    // Cloud billing-summary fetch (best-effort; no-ops until linked + cloud_url set).
    billing::spawn(state.clone());
    // Best-effort schema-version handshake: warn if our contract version is
    // outside the cloud's supported range. Non-fatal; skipped when cloud_url is
    // unset OR the device is unlinked — cloud_url now DEFAULTS to the hosted
    // cloud, and an unlinked daemon must stay fully offline (no phone-home,
    // not even an anonymous GET /meta). Run detached so it never delays
    // startup.
    {
        let handshake_state = state.clone();
        tokio::spawn(async move {
            if let Some((cloud_url, _device_id)) = handshake_state.cloud_link("handshake").await {
                sync::check_schema_handshake(&handshake_state.http, Some(&cloud_url)).await;
            }
        });
    }

    serve_control(state.clone(), listener);

    // Block until shutdown. The accept loop runs detached in `serve_control`.
    wait_for_shutdown_signal(&state).await;
    tracing::info!("shutting down");
    // Graceful offline: tell the cloud this device is going offline with one
    // best-effort empty-sessions beat (short timeout, errors ignored) so it
    // doesn't wait out the presence TTL.
    heartbeat::send_offline_beat(&state).await;

    // Nothing to clean up on windows — a named pipe isn't a filesystem object,
    // so there's no stale file for the next `run()` to trip over the way a
    // leftover UDS path would.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Block until an orderly-shutdown trigger arrives, then return so the caller
/// (`run`) performs the SAME shutdown sequence regardless of which trigger
/// fired: Ctrl-C, a platform termination signal/event, or an in-band
/// `Request::Shutdown` over the control channel (`control::handle_conn`'s
/// post-write `state.shutdown.notify_one()` — the windows-required,
/// platform-neutral SIGTERM equivalent; see `Request::Shutdown`'s doc for why
/// windows needs this at all).
///
/// unix: `kill <pid>` (what `dira daemon stop` sends), `launchctl kickstart
/// -k`, and `systemctl restart` all deliver **SIGTERM**, not SIGINT — so
/// `ctrl_c()` alone left every daemon restart (including the ones `dira
/// update` performs) killing the process via the default signal disposition:
/// no log line, no offline beat, no chance to flush.
#[cfg(unix)]
async fn wait_for_shutdown_signal(state: &AppState) {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(e) => {
            // Registration only fails on resource exhaustion; fall back to
            // Ctrl-C + the in-band notify rather than panicking a running
            // daemon over it.
            tracing::warn!(
                "failed to install SIGTERM handler: {e}; SIGTERM will use the default disposition"
            );
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = state.shutdown.notified() => {}
            }
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
        _ = state.shutdown.notified() => {}
    }
}

/// windows has no SIGTERM at all, so `dira daemon stop` and daemon
/// self-restarts (`dira update`) rely entirely on `Request::Shutdown` over the
/// control pipe to ask this loop to wind down — `state.shutdown.notified()` is
/// therefore not just a nicety here the way it is on unix (which also has real
/// signals), it's windows' only orderly-shutdown path.
///
/// The three Win32 console-control events tokio exposes are watched too, so a
/// console close/logoff/shutdown still gets the orderly path when nothing
/// sent an explicit `Shutdown` request first: Ctrl-C (interactive), Ctrl-Close
/// (console window closed), and Ctrl-Shutdown (system shutdown/logoff).
/// Ctrl-Close in particular gives the process only ~5s before Windows hard-
/// kills it — the shutdown sequence in `run()` (the offline beat) is already a
/// single short-timeout best-effort HTTP call with no new blocking work added
/// here, so it comfortably fits inside that budget.
#[cfg(windows)]
async fn wait_for_shutdown_signal(state: &AppState) {
    // Each registration only fails on resource exhaustion; on failure that
    // branch below simply never fires (`std::future::pending`) instead of
    // panicking a running daemon over it — the remaining triggers still cover
    // shutdown.
    let mut ctrl_c = tokio::signal::windows::ctrl_c()
        .inspect_err(|e| tracing::warn!("failed to install Ctrl-C handler: {e}"))
        .ok();
    let mut ctrl_close = tokio::signal::windows::ctrl_close()
        .inspect_err(|e| tracing::warn!("failed to install Ctrl-Close handler: {e}"))
        .ok();
    let mut ctrl_shutdown = tokio::signal::windows::ctrl_shutdown()
        .inspect_err(|e| tracing::warn!("failed to install Ctrl-Shutdown handler: {e}"))
        .ok();

    tokio::select! {
        _ = async {
            match ctrl_c.as_mut() {
                Some(s) => { s.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
        _ = async {
            match ctrl_close.as_mut() {
                Some(s) => { s.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
        _ = async {
            match ctrl_shutdown.as_mut() {
                Some(s) => { s.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
        _ = state.shutdown.notified() => {}
    }
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

/// Deep-idle decimation for the repo-commit sweep: with zero active sessions
/// and no recent activity, sweep only every Nth tick (10 × 30s ⇒ every ~5 min)
/// instead of forking `git rev-parse` per known repo every 30s forever. Commits
/// are never lost to the slower cadence — `capture::git_walk` walks the full
/// `baseline..HEAD` range whenever it does run, and the moment events flow
/// again the writer's own capture path fires immediately (and the daemon is no
/// longer deep-idle, so the next tick sweeps too).
const DEEP_IDLE_SWEEP_EVERY_N_TICKS: u32 = 10;

/// Whether this tick runs the repo sweep. Pure so the decimation policy is
/// unit-testable: an active daemon sweeps every tick; a deep-idle one only
/// when enough ticks have accumulated since the last sweep.
fn sweep_this_tick(deep_idle: bool, ticks_since_sweep: u32) -> bool {
    !deep_idle || ticks_since_sweep >= DEEP_IDLE_SWEEP_EVERY_N_TICKS
}

/// Periodically emit a tick for each open manual session so it accrues
/// continuously, and sweep every known repo for new commits (caught even when no
/// agent events are flowing). Marks ticker progress for the watchdog each tick.
///
/// The tick itself stays at the fixed 30s cadence — an empty tick is just a
/// timer fire, and both manual-session accrual and the watchdog's stall
/// threshold depend on it — but the git sweep (the only part that forks
/// subprocesses) is decimated while deep-idle; see [`sweep_this_tick`].
///
/// Loops forever; the supervisor treats any *panic* as a fault and re-spawns it.
pub async fn idle_ticker(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(TICK_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks_since_sweep: u32 = 0;
    loop {
        interval.tick().await;
        state.progress.mark_ticker();

        let now = OffsetDateTime::now_utc();
        let (manual, active_count, last_activity) = {
            let reg = control::lock_recover(&state.sessions);
            (
                reg.active_manual(),
                reg.active(now, state.config.session_stale_after()).len(),
                reg.last_activity_at(),
            )
        };
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

        // Same deep-idle rule as the heartbeat (WP-A3). `None` last-activity
        // (nothing observed since daemon start) counts as idle-forever: the
        // only thing the slower sweep can delay is a commit made with no
        // events flowing at all, by a few minutes, with git timestamps intact.
        let idle_for = last_activity
            .map(|at| (now - at).max(Duration::ZERO))
            .unwrap_or(Duration::MAX);
        let deep_idle =
            heartbeat::is_deep_idle(active_count, idle_for, state.config.deep_idle_after());
        if !sweep_this_tick(deep_idle, ticks_since_sweep) {
            ticks_since_sweep += 1;
            continue;
        }
        ticks_since_sweep = 0;

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
    let base = std::time::Duration::from_secs(MAINTENANCE_INTERVAL_SECS);
    let mut sweeps: u64 = 0;
    loop {
        // Jittered ±10% so many daemons don't all compact on the same wall-clock
        // cadence; purely cosmetic here (this task never hits the network) but
        // kept consistent with the other background timers.
        tokio::time::sleep(jitter::jittered(base, jitter::DEFAULT_FRAC)).await;
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
        if sweeps.is_multiple_of(VACUUM_EVERY_N_SWEEPS) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep decimation policy: an active daemon sweeps every tick; a
    /// deep-idle one only once `DEEP_IDLE_SWEEP_EVERY_N_TICKS` ticks have
    /// accumulated — so a fully idle daemon stops forking `git rev-parse`
    /// per repo every 30s, without ever stopping the sweep entirely.
    #[test]
    fn deep_idle_decimates_the_repo_sweep_but_never_stops_it() {
        // Active: every tick sweeps, regardless of the counter.
        assert!(sweep_this_tick(false, 0));
        assert!(sweep_this_tick(false, DEEP_IDLE_SWEEP_EVERY_N_TICKS));
        // Deep idle: skipped until the counter reaches the decimation factor…
        assert!(!sweep_this_tick(true, 0));
        assert!(!sweep_this_tick(true, DEEP_IDLE_SWEEP_EVERY_N_TICKS - 1));
        // …then it MUST run (the sweep slows down, it never stops).
        assert!(sweep_this_tick(true, DEEP_IDLE_SWEEP_EVERY_N_TICKS));
    }
}
