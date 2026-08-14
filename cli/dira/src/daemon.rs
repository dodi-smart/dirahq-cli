//! Daemon lifecycle: start/stop/status plus OS-service install.
//!
//! For the dev/dogfood loop, `start` spawns `dirad` detached and tracks it with a
//! pidfile; `install` writes a launchd/systemd-user unit (or, on windows, a
//! scheduled task) for persistence across reboots. `restart` (below) builds on
//! both: it works out *how* the daemon is currently supervised, then restarts it
//! the way that supervisor expects.

use crate::client;
use anyhow::{Context, Result};
use dira_core::protocol::{Request, Response};
use dira_core::Config;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The pidfile that sits beside a given socket. Generalized from the
/// configured-socket-only version so the legacy-migration path (W1) can ask
/// the same question about the pre-D-0008 socket. Unix shape only — a windows
/// pipe endpoint has no filesystem parent to sit beside (see [`pidfile`]).
fn pidfile_beside(sock: &Path) -> PathBuf {
    sock.parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("dirad.pid")
}

/// The pidfile for the configured control endpoint.
///
/// unix: beside the socket, via [`pidfile_beside`].
///
/// windows: `socket_path` is a `\\.\pipe\...` device-namespace path whose
/// "parent" (`\\.\pipe`) isn't a writable filesystem directory, so the pidfile
/// lives under [`Config::runtime_dir`] (the per-user data dir) instead. That
/// directory is shared — there is no per-socket parent for isolation — so the
/// file name carries the endpoint identity: two daemons (or two tests) with
/// distinct pipe endpoints must not fight over a single dirad.pid.
fn pidfile(config: &Config) -> PathBuf {
    #[cfg(unix)]
    {
        pidfile_beside(&config.socket_path)
    }
    #[cfg(windows)]
    {
        let stem = config
            .socket_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("dira");
        let sanitized = dira_core::config::sanitize_ident(stem);
        config.runtime_dir().join(format!("dirad-{sanitized}.pid"))
    }
}

/// Locate the `dirad` binary: `DIRAD_BIN`, else a sibling of this exe, else PATH.
fn locate_dirad() -> PathBuf {
    if let Ok(p) = std::env::var("DIRAD_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // The sibling lookup needs the platform file extension explicitly
            // (`dirad.exe` on windows) — unlike the bare-name PATH fallback
            // below, this builds an exact path rather than something `Command`
            // resolves itself, so there's no implicit `.exe` appending here.
            let sibling = dir.join(dira_ipc::DIRAD_BIN);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from(dira_ipc::DIRAD_BIN)
}

/// A truncating file for the spawned daemon's stdout/stderr, or `None` when no
/// log location resolves (in which case the caller nulls them, as before).
///
/// Deliberately truncating and separate from the daemon's own rolling log: this
/// only ever holds one process's pre-`tracing` output, so it cannot grow, and a
/// crash loop leaves the *latest* failure rather than an unbounded pile.
fn spawn_log(config: &Config) -> Option<std::fs::File> {
    let dir = match std::env::var("DIRA_LOG_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ if cfg!(windows) => config.runtime_dir().join("logs"),
        _ => return None,
    };
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::File::create(dir.join("dirad.spawn.log")).ok()
}

/// Record a started daemon's pid, warning rather than failing.
///
/// A running daemon with a missing pidfile is strictly better than aborting an
/// otherwise-successful start: the pidfile is a convenience for `stop`/`restart`,
/// not a correctness requirement (`detect_supervision` also probes the socket).
/// But the failure is no longer swallowed with `.ok()` — silence here is how a
/// stale pid survives unnoticed.
fn write_pidfile(pf: &Path, pid: u32) {
    if let Some(dir) = pf.parent() {
        // `runtime_dir()` isn't guaranteed to exist yet — on unix it's the
        // socket's own parent (already created when the daemon binds), but on
        // windows it's the project data dir, which may never have been touched
        // before this first `dira daemon start`.
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("warning: could not create {} ({e})", dir.display());
            return;
        }
    }
    if let Err(e) = std::fs::write(pf, pid.to_string()) {
        eprintln!(
            "warning: could not write the pidfile at {} ({e}) — `dira daemon stop` \
             will fall back to a process-name kill",
            pf.display()
        );
    }
}

/// Is a daemon answering `Ping` on this exact socket?
pub async fn answers(sock: &Path) -> bool {
    matches!(client::send(sock, &Request::Ping).await, Ok(Response::Pong))
}

/// Is the daemon answering on the socket?
pub async fn is_up(config: &Config) -> bool {
    answers(&config.socket_path).await
}

/// Tri-state liveness: `Up`, `Down`, or *refusing us*.
///
/// [`is_up`] answers "can I talk to it", which is the right question for most
/// callers and is left alone. This answers "is one there at all", which is a
/// different question whenever the endpoint is permission-gated — and collapsing
/// the two is what let a running-but-unreachable daemon be reported as `down`,
/// with advice that made it worse.
pub async fn reach(config: &Config) -> client::Reach {
    match dira_ipc::connect(&config.socket_path).await {
        Ok(_) => client::Reach::Up,
        Err(e) => client::classify(&e),
    }
}

pub async fn start(config: &Config) -> Result<()> {
    if is_up(config).await {
        println!("dirad already running");
        return Ok(());
    }
    // A daemon that refuses us is still a daemon. Spawning a second one cannot
    // help: it fails `first_pipe_instance` against the live one and exits — after
    // this function has already overwritten the real daemon's pidfile, leaving
    // `stop`/`restart`/`status` pointing at a dead pid. Refuse before spawning.
    if reach(config).await == client::Reach::Denied {
        anyhow::bail!(
            "{}",
            dira_ipc::elevation::access_denied_advice(dira_ipc::elevation::is_elevated())
        );
    }
    let bin = locate_dirad();
    println!("starting {} ...", bin.display());
    let mut cmd = Command::new(&bin);
    cmd.stdin(std::process::Stdio::null());
    // A daemon that dies *before* `tracing` initialises — a config parse error, a
    // missing DLL, a panic in startup — writes to stderr and nothing else. Nulling
    // it meant those failures left no trace on any platform. Truncate per start so
    // this stays bounded to one process's output.
    match spawn_log(config) {
        Some(f) => {
            let dup = f.try_clone().ok();
            cmd.stderr(std::process::Stdio::from(f));
            match dup {
                Some(d) => cmd.stdout(std::process::Stdio::from(d)),
                None => cmd.stdout(std::process::Stdio::null()),
            };
        }
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x0800_0000): suppresses the console-flash that would
        // otherwise briefly appear for this stdio-nulled child. NOT
        // DETACHED_PROCESS (0x0000_0008) — the Win32 docs call the two mutually
        // exclusive, and DETACHED_PROCESS is the one that actually conflicts here
        // since it also detaches stdio, which this spawn already does explicitly.
        // CREATE_NEW_PROCESS_GROUP (0x0000_0200): puts dirad in its own process
        // group so a Ctrl-C/Ctrl-Break delivered to the parent console (e.g. the
        // terminal `dira daemon start` was run from) doesn't propagate to it.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {}", bin.display()))?;

    // The pidfile is written only once the child has proven itself (below), never
    // here. Writing it up front meant a start that failed — the common case when
    // an already-running daemon refuses us — clobbered the live daemon's pidfile
    // on its way out, so `stop`/`restart`/`status` all lost track of the real
    // process. Cross-platform.
    let pf = pidfile(config);

    // Poll for readiness. Startup can be slow on first run — a keychain unlock
    // prompt (for the device key) or a large event-log hydration both delay the
    // socket bind — so poll generously (~10s) and distinguish a real crash from
    // a slow start by checking whether the child is still alive, instead of
    // reporting a false "did not come up".
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if is_up(config).await {
            write_pidfile(&pf, child.id());
            println!("dirad up (pid {})", child.id());
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "dirad exited during startup ({status}); run `dirad` in the foreground to see why"
            );
        }
    }

    // Still alive but not answering yet — almost certainly waiting on a keychain
    // prompt. It will come up on its own, so record the pid and report that
    // rather than fail; this branch DID produce a live daemon.
    write_pidfile(&pf, child.id());
    println!(
        "dirad starting (pid {}) but not answering yet — it may be waiting on a \
         keychain prompt. Check `dira daemon status`.",
        child.id()
    );
    Ok(())
}

/// Attempts × interval for "did dirad actually go down after we asked?" polling,
/// shared by `stop`'s and `restart_bare`'s windows branches and by
/// `restart_scheduled_task` — ~5s total, generous enough to cover a slow
/// shutdown (event-log flush, keychain teardown) without hanging the CLI
/// indefinitely on a wedged daemon.
/// 15 s. Must exceed `dirad`'s worst-case teardown, which is the 3 s shutdown
/// offline beat (`heartbeat::SHUTDOWN_BEAT_TIMEOUT`) plus the 5 s WAL checkpoint
/// budget, plus slack. The old 5 s sat *below* that, so an orderly shutdown was
/// routinely force-killed partway through its checkpoint.
const STOP_GRACE_ATTEMPTS: u32 = 150;
const STOP_GRACE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// 5 s. After `taskkill /F` the kernel should reap almost immediately; this only
/// has to cover handle release, and a long wait here would just delay the error
/// path for a process that is never coming back.
const KILL_CONFIRM_ATTEMPTS: u32 = 50;

/// Poll [`is_up`] every `interval`, up to `attempts` times; `true` if the daemon
/// is still answering after all of them. `attempts`/`interval` are parameters —
/// rather than the grace window being hardcoded inline — purely so tests can
/// exercise the "still up → escalate" branch on a short fake budget instead of
/// a real multi-second sleep; every production call site passes
/// [`STOP_GRACE_ATTEMPTS`]/[`STOP_GRACE_INTERVAL`].
async fn still_up_after(config: &Config, attempts: u32, interval: std::time::Duration) -> bool {
    for _ in 0..attempts {
        tokio::time::sleep(interval).await;
        if !is_up(config).await {
            return false;
        }
    }
    true
}

/// Ask a bare (non-service-managed) dirad to shut down, wait for the PROCESS to
/// exit, then escalate to a forced kill and wait again.
///
/// Returns whether the process is confirmed gone. Callers that are about to
/// start a replacement MUST NOT proceed on `false` — the single-instance guard
/// is the socket/pipe bind, and the name stays taken until the old process's
/// handles are released, so spawning early races a guard that cannot yet refuse
/// (D-0019).
///
/// One function with the platform seam inside, matching how `pid_is_alive`,
/// `wait_for_exit` and `alive_pid_at` already take `os`. Only the ask and the
/// force differ; the sequence — parse, wait, escalate, re-wait on the capped
/// budget — is the invariant, and two copies of it would be two copies to keep
/// identical by hand. That is the same reasoning DIRASH-0031 applied to the
/// backoff ladder.
///
/// **Windows** has no unix-style signal a process can trap, so it asks nicely
/// via the in-band `Request::Shutdown` that `dirad` answers with `Response::Ok`
/// before winding down, and escalates straight to `taskkill /F` — a plain
/// `taskkill` cannot reliably stop a windowless background process. **Unix**
/// sends SIGTERM and escalates to SIGKILL.
async fn graceful_then_force(
    config: &Config,
    pid: &str,
    runner: &dyn Runner,
    os: Os,
    attempts: u32,
    interval: std::time::Duration,
) -> bool {
    let windows = os == Os::Windows;
    if windows {
        let _ = client::send(&config.socket_path, &Request::Shutdown).await;
    } else {
        let _ = runner.run("kill", &[pid]);
    }

    // Without a parseable pid there is no process to wait on. Windows falls back
    // to the channel-based probe first — it is the platform where a lingering
    // pipe handle is the whole hazard — but both report honestly that exit was
    // not confirmed.
    let Ok(parsed) = pid.trim().parse::<u32>() else {
        if windows {
            let _ = still_up_after(config, attempts, interval).await;
        }
        return false;
    };

    if wait_for_exit(parsed, runner, os, attempts, interval).await {
        return true;
    }
    if windows {
        let _ = runner.run("taskkill", &["/PID", pid, "/F"]);
    } else {
        let _ = runner.run("kill", &["-9", pid]);
    }
    // Cap the post-kill confirmation at the production budget, but never exceed
    // the caller's own — a test injecting a few milliseconds must not then block
    // on a five-second reap.
    wait_for_exit(
        parsed,
        runner,
        os,
        attempts.min(KILL_CONFIRM_ATTEMPTS),
        interval,
    )
    .await
}

pub async fn stop(config: &Config) -> Result<()> {
    stop_with(
        config,
        &SystemRunner,
        current_os(),
        STOP_GRACE_ATTEMPTS,
        STOP_GRACE_INTERVAL,
    )
    .await
}

/// `attempts`/`interval` are parameters for the same reason `restart_*` take
/// them: so the "will not die" branch runs on a millisecond budget in tests
/// instead of spending the real 15s grace window. Every production call site
/// passes [`STOP_GRACE_ATTEMPTS`]/[`STOP_GRACE_INTERVAL`].
async fn stop_with(
    config: &Config,
    runner: &dyn Runner,
    os: Os,
    attempts: u32,
    interval: std::time::Duration,
) -> Result<()> {
    let pf = pidfile(config);
    let pid = match std::fs::read_to_string(&pf) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            println!("no pidfile; daemon may be managed by launchd/systemd or not running");
            return Ok(());
        }
    };

    // One escalation sequence, one call. The pidfile is released only once exit
    // is CONFIRMED: it is the last handle on a process we could not stop, and
    // printing "stopped" over it would be a plain lie.
    if !graceful_then_force(config, &pid, runner, os, attempts, interval).await {
        anyhow::bail!("{}", unconfirmed_exit_advice(&pid, os));
    }
    std::fs::remove_file(&pf).ok();
    if os != Os::Windows {
        // A named pipe isn't a filesystem object, so there is nothing to unlink
        // on windows — `run()`'s own teardown already gates this the same way.
        std::fs::remove_file(&config.socket_path).ok();
    }

    println!("stopped dirad (pid {pid})");
    Ok(())
}

/// What to tell a user whose daemon would not die. The cause is almost always a
/// privilege mismatch, and the command that fixes it is per-OS — which is why
/// this stays split while the escalation itself does not.
fn unconfirmed_exit_advice(pid: &str, os: Os) -> String {
    if os == Os::Windows {
        format!(
            "could not confirm dirad (pid {pid}) exited — it may be running with \
             different privileges (e.g. started from an Administrator terminal). \
             Stop it from a terminal at the same privilege level:\n  \
             taskkill /PID {pid} /F"
        )
    } else {
        format!(
            "could not confirm dirad (pid {pid}) exited — it may be running as another \
             user. Stop it from a terminal with the same privileges:\n  kill -9 {pid}"
        )
    }
}

/// The `dirad: down` line, plus — when a daemon is still answering on the
/// pre-D-0008 `$TMPDIR` socket — the reason and the one command that fixes it.
/// Pure so both branches are testable without a live daemon.
fn down_message(legacy: Option<&std::path::Path>) -> String {
    match legacy {
        None => "dirad: down".to_string(),
        Some(sock) => format!(
            "dirad: down\n  \
             note: a daemon IS answering on the legacy socket {}\n  \
             it predates the move to a fixed per-user socket path, so this \
             client can no longer reach it.\n  \
             restart it to move it over: dira daemon restart",
            sock.display(),
        ),
    }
}

/// `dira version`'s `Err(_)` line, plus — same legacy-daemon nudge as
/// [`down_message`] — the reason the CLI/daemon skew check below it couldn't
/// even run: it depends on reaching the daemon over the socket, and a
/// legacy daemon defeats that silently otherwise. Pure for the same reason
/// as `down_message`.
pub(crate) fn version_not_running_message(legacy: Option<&Path>) -> String {
    match legacy {
        None => "dirad   not running".to_string(),
        Some(sock) => format!(
            "dirad   not running\n\
             note: a pre-upgrade daemon is still answering on {} — run `dira daemon restart`",
            sock.display(),
        ),
    }
}

/// Is a pre-D-0008 daemon still listening on `legacy`? Only ever called once
/// the configured socket has already failed, and only to build the "restart
/// it" hint or (via [`status`]) decide whether a daemon is running at all —
/// never to actually talk to it. `legacy` is injected, mirroring
/// `detect_supervision_with`'s `legacy_sock` param, so tests never probe the
/// real `$TMPDIR/dira.sock` on this dogfooding machine.
async fn legacy_daemon_socket(config: &Config, legacy: &Path) -> Option<PathBuf> {
    // On an exotic platform (or with the socket pinned via config/env) the
    // legacy path can BE the configured one — we already know it is down.
    if legacy == config.socket_path {
        return None;
    }
    answers(legacy).await.then(|| legacy.to_path_buf())
}

/// The `dirad: up` line. A daemon whose hook ingress failed to bind answers
/// every control request but captures nothing, so it is reported as **degraded**
/// with the reason rather than as a healthy "up" (D-0009).
/// A warning when `dira` and `dirad` resolved DIFFERENT capture stores.
///
/// Neither process can see this alone. `project_dirs()` succeeds on both sides;
/// they just land in two profiles — a daemon started from an elevated shell, a
/// service account, or `runas`. The daemon reports itself healthy, `dira status`
/// reads an empty database, and the user concludes their history is gone. The
/// only signal is the comparison, which is why the daemon publishes `db_path`.
///
/// `None` when they agree, and `None` when the daemon is too old to say — an
/// unknown is not a divergence, and inventing a warning from a missing field
/// would be worse than silence.
pub(crate) fn store_divergence_line(cli_db: &Path, daemon_db: Option<&str>) -> Option<String> {
    let daemon_db = daemon_db?;
    if Path::new(daemon_db) == cli_db {
        return None;
    }
    Some(format!(
        "note: the daemon's capture store is {daemon_db}\n      \
         but this CLI resolves {}\n      \
         the daemon is most likely running as a different user or elevated, so \
         it is writing a history you are not reading",
        cli_db.display(),
    ))
}

fn up_message(sock: &std::path::Path, ingress_error: Option<&str>) -> String {
    match ingress_error {
        None => format!("dirad: up  (socket {})", sock.display()),
        Some(reason) => format!(
            "dirad: up, DEGRADED  (socket {})\n  {reason}\n  \
             capture will not flow until this resolves; the daemon retries in the background",
            sock.display(),
        ),
    }
}

/// What the daemon said about itself, in one `DaemonInfo` round-trip.
///
/// Mirrors `Response::DaemonInfo` field for field so callers never have to
/// re-send: `dira version`, `dira daemon status` and four `dira doctor` checks
/// all read from one probe.
#[derive(Debug, Clone)]
pub(crate) struct Info {
    pub version: String,
    pub schema_version: String,
    pub pid: u32,
    pub uptime_seconds: u64,
    pub http_ingress_error: Option<String>,
    pub control_channel_warning: Option<String>,
    pub db_path: Option<String>,
    pub storage_warning: Option<String>,
}

/// One round-trip's worth of daemon truth.
///
/// Exists because the three diagnostic surfaces used to each ask the daemon
/// their own question and print the answer inline, so nothing could compose
/// them. Gathering once also keeps [`client::Reach`] intact: [`client::send`]
/// collapses a typed `io::Error` into a string, and the `Denied` discriminant
/// is precisely the one a diagnostic must not lose.
///
/// Diagnostic only. Nothing here licenses a restart decision — that path asks
/// whether the *process* exited, not whether the channel answers (D-0019).
#[derive(Debug, Clone)]
pub(crate) struct DaemonProbe {
    /// How the control endpoint answered a connect attempt. Recorded here
    /// rather than re-derived, because `client::send` collapses the typed
    /// `io::Error` into a string and `Denied` — a daemon that is running and
    /// refusing us — is the one discriminant a diagnostic cannot afford to
    /// lose (D-0016).
    pub reach: client::Reach,
    /// `Some` when the daemon answered `DaemonInfo`.
    pub info: Option<Info>,
    /// The daemon answered *something* — either `DaemonInfo`, or `Ping` during
    /// a partial update where it is too old for `DaemonInfo`. Distinct from
    /// `info.is_some()`, and distinct again from no daemon at all.
    pub answered_ping: bool,
    /// The daemon answered the `DaemonInfo` request with something that was not
    /// a `DaemonInfo`. Kept separate from `answered_ping` because `dira version`
    /// says "unexpected daemon response" for this and "not running" for a
    /// transport failure, and collapsing the two would change its output.
    pub unexpected: bool,
    /// A pre-D-0008 daemon still answering on the legacy `$TMPDIR` socket.
    /// Only ever probed once the configured socket has already failed.
    pub legacy: Option<PathBuf>,
}

impl DaemonProbe {
    /// The `dira daemon status` line, unchanged from when `status_with` built
    /// it inline.
    pub(crate) fn status_line(&self, sock: &Path) -> String {
        match (&self.info, self.answered_ping) {
            (Some(i), _) => up_message(sock, i.http_ingress_error.as_deref()),
            (None, true) => up_message(sock, None),
            (None, false) => down_message(self.legacy.as_deref()),
        }
    }

    /// `true` iff a daemon is running somewhere — see [`status`] for why
    /// "legacy" counts.
    pub(crate) fn running(&self) -> bool {
        self.info.is_some() || self.answered_ping || self.legacy.is_some()
    }
}

/// Probe the daemon once, against the real legacy socket path.
pub(crate) async fn probe(config: &Config) -> DaemonProbe {
    probe_with(config, &dira_core::config::legacy_socket_path()).await
}

/// [`probe`] with the legacy socket injected, mirroring `detect_supervision_with`
/// so tests never poke the real `$TMPDIR/dira.sock` on a dogfooding machine.
///
/// The order is load-bearing and reproduces what `status_with` did: `DaemonInfo`
/// first (only it carries the degradation), then `Ping` (a daemon too old for
/// `DaemonInfo` still answers it, so a partial update must not be called down),
/// and only then the legacy socket — probing that eagerly would make every
/// healthy `dira version` stat `$TMPDIR`.
pub(crate) async fn probe_with(config: &Config, legacy_sock: &Path) -> DaemonProbe {
    match client::send(&config.socket_path, &Request::DaemonInfo).await {
        Ok(Response::DaemonInfo {
            version,
            schema_version,
            pid,
            uptime_seconds,
            http_ingress_error,
            control_channel_warning,
            db_path,
            storage_warning,
        }) => {
            return DaemonProbe {
                reach: client::Reach::Up,
                info: Some(Info {
                    version,
                    schema_version,
                    pid,
                    uptime_seconds,
                    http_ingress_error,
                    control_channel_warning,
                    db_path,
                    storage_warning,
                }),
                answered_ping: true,
                unexpected: false,
                legacy: None,
            };
        }
        // The daemon answered, just not with what we asked for. It is up.
        Ok(_) => {
            return DaemonProbe {
                reach: client::Reach::Up,
                info: None,
                answered_ping: true,
                unexpected: true,
                legacy: None,
            };
        }
        Err(_) => {}
    }

    // `DaemonInfo` did not get through. The legacy socket is probed from here
    // on regardless of `Ping`, because `dira version`'s not-running line needs
    // the hint and the healthy path (`info: Some`) already returned above.
    let legacy = legacy_daemon_socket(config, legacy_sock).await;

    if is_up(config).await {
        return DaemonProbe {
            reach: client::Reach::Up,
            info: None,
            answered_ping: true,
            unexpected: false,
            legacy,
        };
    }

    DaemonProbe {
        reach: reach(config).await,
        info: None,
        answered_ping: false,
        unexpected: false,
        legacy,
    }
}

/// The CLI/daemon skew warning, or `None` when they match. Pure so the wording
/// is testable without a live daemon.
pub(crate) fn version_skew_line(cli: &str, daemon: &str) -> Option<String> {
    (cli != daemon).then(|| {
        format!(
            "warning: CLI ({cli}) and daemon ({daemon}) differ — restart the daemon \
             (`dira daemon stop && dira daemon start`) so they match"
        )
    })
}

/// The human label for a supervision state, or `None` when there is nothing to
/// say. Pure so `dira daemon status` and `dira doctor` speak with one voice.
pub(crate) fn supervision_label(s: &Supervision) -> Option<String> {
    Some(match s {
        Supervision::Launchd => "launchd".to_string(),
        Supervision::SystemdUser => "systemd --user".to_string(),
        Supervision::ScheduledTask => "scheduled task".to_string(),
        Supervision::Pidfile(pid) => format!("pidfile (pid {pid})"),
        Supervision::Socket(pid) => format!("unmanaged (pid {pid}, no pidfile)"),
        Supervision::LegacySocket { pid, sock } => format!(
            "pre-upgrade daemon on legacy socket {} (pid {})",
            sock.display(),
            pid.map_or("unknown".into(), |p| p.to_string())
        ),
        Supervision::NotRunning => return None,
    })
}

/// `true` iff a daemon is running somewhere — healthy, degraded, or still
/// answering on the legacy socket. install.sh (and any other scripted
/// caller) keys a restart-after-upgrade decision off this exit code, so
/// "legacy" must count as running: once `restart` migrates it (W1),
/// answering "no" here would make install.sh skip the restart and strand
/// the old daemon — the exact failure W1 fixes.
pub async fn status(config: &Config) -> Result<bool> {
    status_with(config, &dira_core::config::legacy_socket_path()).await
}

async fn status_with(config: &Config, legacy_sock: &Path) -> Result<bool> {
    let probe = probe_with(config, legacy_sock).await;
    println!("{}", probe.status_line(&config.socket_path));
    Ok(probe.running())
}

/// Write and load an OS service so the daemon survives reboots.
///
/// Stops an unmanaged daemon first, and waits for it to actually exit.
///
/// This is not a convenience — it is the only ordering that works. D-0009 makes
/// the control socket the single-instance guard, so a service installed while a
/// bare-started daemon holds it cannot bind: launchd (`KeepAlive=true`), systemd
/// (`Restart=always`) and the windows logon task all then restart the loser, in
/// a loop, for as long as the old process lives.
///
/// It used to live in the callers, which meant it lived in *three* of them —
/// the onboard step, `install.sh` and `install.ps1` — and not in the fourth. A
/// bare `dira daemon install` on a machine with a hand-started `dirad` walked
/// straight into the flap, and that is the documented path a user following the
/// old "run `dira daemon start`" advice was on. Centralising it here is what
/// makes every caller correct by construction; the installers keep their own
/// best-effort `dira daemon stop` only because they may be driving an older
/// binary that lacks this.
///
/// A daemon under a service manager already is left alone: stopping it would
/// just make its supervisor restart it mid-install.
pub async fn install(config: &Config) -> Result<()> {
    install_with_supervision(config, detect_supervision(config).await).await
}

/// [`install`] for a caller that has already probed supervision.
///
/// `detect_supervision` is not cheap — a `launchctl list` / `schtasks /Query`
/// plus `reg query` subprocess, a pidfile probe and a socket round-trip — and
/// `onboard` holds the answer already. Same guarantee either way: the pre-stop
/// is inside, not at the call site.
pub async fn install_with_supervision(config: &Config, supervision: Supervision) -> Result<()> {
    stop_unmanaged_before_install(
        config,
        supervision,
        &SystemRunner,
        current_os(),
        STOP_GRACE_ATTEMPTS,
        STOP_GRACE_INTERVAL,
    )
    .await?;
    install_service(config)
}

/// Stop a bare-started daemon so [`install`] can bind the control socket.
///
/// Silent and inert when there is nothing to do, which is the common case: no
/// pidfile, or the daemon already supervised. A stop that cannot confirm the
/// process exited is a hard error — installing on top of it would produce
/// exactly the flap this exists to prevent, and D-0019 forbids proceeding to
/// start a replacement without that confirmation.
async fn stop_unmanaged_before_install(
    config: &Config,
    supervision: Supervision,
    runner: &dyn Runner,
    os: Os,
    attempts: u32,
    interval: std::time::Duration,
) -> Result<()> {
    // Only the states that mean "running, and nothing is supervising it". A
    // daemon a service manager already owns is left alone — stopping it would
    // just make its supervisor restart it mid-install — and `NotRunning` has
    // nothing to stop.
    //
    // `supervision` is a parameter rather than probed here so the three arms are
    // unit-testable without a live daemon, the same seam `restart_*` already use
    // for their grace budgets.
    let running_unmanaged = matches!(
        supervision,
        Supervision::Pidfile(_) | Supervision::Socket(_) | Supervision::LegacySocket { .. }
    );
    if !running_unmanaged {
        return Ok(());
    }
    println!("stopping the unsupervised daemon first (it holds the control socket)");
    stop_with(config, runner, os, attempts, interval)
        .await
        .context("could not stop the running daemon, so the service cannot bind the socket")
}

/// The service-manager half of [`install`], with no daemon handling of its own.
fn install_service(config: &Config) -> Result<()> {
    let bin = locate_dirad();
    // `dunce::canonicalize` behaves exactly like `std::fs::canonicalize` on
    // unix, but on windows it strips the `\\?\` verbatim-prefix windows'
    // canonicalize adds — a prefix neither `schtasks /TR` nor the registry's
    // Run key resolves, so the plain (non-`\\?\`) form is what must be written
    // out below.
    let bin = dunce::canonicalize(&bin).unwrap_or(bin);

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        let dir = PathBuf::from(&home).join("Library/LaunchAgents");
        std::fs::create_dir_all(&dir)?;
        let plist_path = dir.join("sh.dirahq.dirad.plist");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>sh.dirahq.dirad</string>
  <key>ProgramArguments</key>
  <array><string>{bin}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{home}/Library/Logs/dirad.log</string>
  <key>StandardErrorPath</key><string>{home}/Library/Logs/dirad.err.log</string>
</dict>
</plist>
"#,
            bin = bin.display(),
        );
        std::fs::write(&plist_path, plist)?;
        let _ = Command::new("launchctl")
            .args(["unload", plist_path.to_str().unwrap()])
            .status();
        Command::new("launchctl")
            .args(["load", plist_path.to_str().unwrap()])
            .status()?;
        println!("installed launchd agent at {}", plist_path.display());
        let _ = config; // socket path is environment-resolved by the agent
    }

    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").context("HOME not set")?;
        let dir = PathBuf::from(&home).join(".config/systemd/user");
        std::fs::create_dir_all(&dir)?;
        let unit_path = dir.join("dirad.service");
        let unit = format!(
            "[Unit]\nDescription=Dira capture daemon\nAfter=default.target\n\n[Service]\nType=simple\nExecStart={bin}\nRestart=always\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n",
            bin = bin.display(),
        );
        std::fs::write(&unit_path, unit)?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        Command::new("systemctl")
            .args(["--user", "enable", "--now", "dirad.service"])
            .status()?;
        println!("installed systemd-user unit at {}", unit_path.display());
        let _ = config;
    }

    #[cfg(target_os = "windows")]
    {
        let bin_str = bin
            .to_str()
            .context("dirad's install path must be valid UTF-8")?;
        // `schtasks /TR` parses its value as a single command-line string and
        // splits it on the first unquoted space — so a path containing spaces
        // must carry its OWN embedded quotes as part of the value, not just be
        // shell-quoted at this process's argv boundary (that only protects the
        // value from *our* argv splitting, not schtasks' own parsing of the
        // string it receives). Hence wrapping the value itself in literal `"`s:
        // the /TR argument's *content* becomes `"C:\path with space\dirad.exe"`.
        let quoted = format!("\"{bin_str}\"");

        let created = Command::new("schtasks")
            .args([
                "/Create",
                "/F",
                "/TN",
                "DiraDaemon",
                "/SC",
                "ONLOGON",
                "/RL",
                "LIMITED",
                "/TR",
                quoted.as_str(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if created {
            // Start-now parity with the launchd/systemd blocks above (both
            // `launchctl load` and `systemctl enable --now` bring the daemon up
            // immediately, not just at the next login).
            let _ = Command::new("schtasks")
                .args(["/Run", "/TN", "DiraDaemon"])
                .status();
            println!("installed scheduled task DiraDaemon");
        } else {
            // Known gap: an ONLOGON task can require elevation on hardened or
            // managed machines (Group Policy restricting non-admin task
            // registration). Any user can write their own HKCU hive without
            // elevation, so fall back to a Run key with the same "launch dirad
            // at logon" effect. UNVALIDATED ON REAL WINDOWS — this fallback must
            // be exercised on an actual machine before public launch; the
            // windows CI job that gates this code has no way to simulate a
            // restricted-elevation account.
            println!(
                "schtasks /Create failed (may require elevation) — falling back to an HKCU Run key"
            );
            let reg_ok = Command::new("reg")
                .args([
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "DiraDaemon",
                    "/t",
                    "REG_SZ",
                    "/d",
                    quoted.as_str(),
                    "/f",
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if reg_ok {
                println!("installed HKCU Run key DiraDaemon");
            } else {
                anyhow::bail!(
                    "could not install DiraDaemon via schtasks or the HKCU Run key fallback"
                );
            }
        }

        // Known gap: neither a logon task nor a Run key resupervises a crashed
        // daemon the way launchd's KeepAlive or systemd's Restart=always do —
        // both only launch dirad once, at logon. Follow-up: a task-XML
        // definition with <RestartOnFailure> (the flat schtasks CLI form can't
        // express it).
        let _ = config;
    }

    Ok(())
}

/// Remove whatever [`install`] set up: the launchd agent, the systemd-user
/// unit, or (windows) BOTH the scheduled task and the HKCU Run key — install
/// writes exactly one of the two depending on whether `schtasks /Create`
/// succeeded, and a caller can't know which, so uninstall sweeps both,
/// best-effort. Does NOT stop a bare (unsupervised) daemon — that's [`stop`]'s
/// job, and installers call it separately before this.
pub fn uninstall(config: &Config) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path =
            dira_core::config::home_dir()?.join("Library/LaunchAgents/sh.dirahq.dirad.plist");
        if plist_path.exists() {
            let _ = Command::new("launchctl")
                .args(["unload", plist_path.to_str().unwrap()])
                .status();
            std::fs::remove_file(&plist_path).ok();
            println!("removed launchd agent {}", plist_path.display());
        } else {
            println!("no launchd agent installed");
        }
        let _ = config;
    }

    #[cfg(target_os = "linux")]
    {
        let unit_path = dira_core::config::home_dir()?.join(".config/systemd/user/dirad.service");
        if unit_path.exists() {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", "dirad.service"])
                .status();
            std::fs::remove_file(&unit_path).ok();
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            println!("removed systemd-user unit {}", unit_path.display());
        } else {
            println!("no systemd-user unit installed");
        }
        let _ = config;
    }

    #[cfg(target_os = "windows")]
    {
        // `/End` first so a task-launched dirad isn't left running headless
        // after its task disappears; both steps best-effort — an absent task
        // or key is success, not an error.
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", "DiraDaemon"])
            .status();
        let task_gone = Command::new("schtasks")
            .args(["/Delete", "/F", "/TN", "DiraDaemon"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let key_gone = Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "DiraDaemon",
                "/f",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        match (task_gone, key_gone) {
            (true, _) => println!("removed scheduled task DiraDaemon"),
            (false, true) => println!("removed HKCU Run key DiraDaemon"),
            (false, false) => println!("no DiraDaemon scheduled task or Run key installed"),
        }
        let _ = config;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Supervision detection + restart
// ---------------------------------------------------------------------------

/// How the currently-running daemon (if any) is being kept alive. Determines
/// how [`restart`] brings it back: a service-managed daemon must be restarted
/// through its service manager (a bare `kill` would just leave it dead, or
/// launchd would relaunch it from a plist that may itself be stale), while a
/// bare `dirad` process started by [`start`] can simply be killed and
/// re-spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervision {
    /// A launchd agent (`sh.dirahq.dirad`) is loaded (`launchctl list` exits 0).
    Launchd,
    /// A systemd --user unit (`dirad.service`) is active.
    SystemdUser,
    /// A windows scheduled task (`DiraDaemon`, `ONLOGON`) is registered
    /// (`schtasks /Query` exits 0), or — if `install` fell back to it because
    /// task creation needed elevation it didn't have — the HKCU Run-key
    /// equivalent is present (`reg query` exits 0). Both are restarted the same
    /// way (see [`restart_scheduled_task`]), so one variant covers either.
    ScheduledTask,
    /// Neither service manager claims it, but `dirad.pid` next to the socket
    /// names a live process (`kill -0` succeeds, or on windows, `tasklist`
    /// lists it).
    Pidfile(u32),
    /// No pidfile (or a stale one), but the daemon answers on the socket and
    /// self-reports its pid via `Response::DaemonInfo` — e.g. it was started
    /// by something other than `dira daemon start`, or the pidfile was lost.
    Socket(u32),
    /// Nothing answers on the configured socket, but a bare pre-D-0008 daemon
    /// is still answering on the legacy `$TMPDIR` one — invisible to every
    /// check above because its pidfile and socket both live under the old
    /// anchor. `pid` is `None` for a pre-public dev build old enough to
    /// answer `Ping` but not `DaemonInfo`, with no pidfile to fall back to.
    LegacySocket { pid: Option<u32>, sock: PathBuf },
    /// Nothing answers and no service manager claims it.
    NotRunning,
}

/// Host OS, injected rather than read from `cfg!` inline so [`detect_supervision`]
/// and [`restart`]'s branch logic can be unit-tested for every platform from a
/// single dev machine, mirroring how [`dira_core::config::start_of_day`] injects
/// the UTC offset instead of reading the ambient machine timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Macos,
    Linux,
    Windows,
    Other,
}

fn current_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        Os::Other
    }
}

/// A probe for an external command's presence/exit status/stdout —
/// `launchctl`, `systemctl`, `kill -0`, `tasklist`, `schtasks`. Behind a trait
/// purely so [`detect_supervision`]'s branches (and [`restart`]'s) are
/// unit-testable without actually shelling out or standing up a live daemon.
pub trait Runner {
    /// Run `prog args…` and return its `Output`, or `None` if the command
    /// could not even be spawned (not installed, no permission, etc.) —
    /// mirrors `Command::spawn` failing with `NotFound`.
    fn run(&self, prog: &str, args: &[&str]) -> Option<Output>;
}

/// The real `Runner`, backed by `std::process::Command`.
struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, prog: &str, args: &[&str]) -> Option<Output> {
        Command::new(prog).args(args).output().ok()
    }
}

/// Is the pid named by `pidfile` alive? `None` if there is no pidfile, it
/// doesn't parse, or the pid is dead. Generalized from the configured-socket
/// case so the legacy-migration path (W1) can ask the same question about the
/// pidfile beside the pre-D-0008 socket. Liveness is decided by `os` (through
/// the [`Runner`] seam), not `cfg!`, mirroring how the launchd/systemd
/// detection below branches on `os` — so the windows probe is unit-testable
/// from any host.
fn alive_pid_at(pidfile: &Path, runner: &dyn Runner, os: Os) -> Option<u32> {
    let contents = std::fs::read_to_string(pidfile).ok()?;
    let pid: u32 = contents.trim().parse().ok()?;
    pid_is_alive(pid, runner, os).then_some(pid)
}

/// Does this PROCESS still exist? Distinct from [`is_up`], which asks whether the
/// control channel answers — a strictly weaker question, because the channel
/// keeps serving for the whole of `dirad`'s teardown (offline beat + WAL
/// checkpoint) and the process outlives it.
///
/// A probe failure reads as "not alive": the callers use this to decide whether
/// to escalate or give up, and both are safer than looping forever on a probe
/// that cannot answer.
fn pid_is_alive(pid: u32, runner: &dyn Runner, os: Os) -> bool {
    if os == Os::Windows {
        // `tasklist /FI "PID eq <pid>"` exits 0 whether or not anything
        // matched — a non-matching filter just prints an "INFO: No tasks are
        // running which match the specified criteria." line to stdout instead
        // of a nonzero exit code — so the exit status alone can't distinguish
        // alive from dead here, unlike the unix `kill -0` branch below. The
        // stdout content is the real signal: check the filtered listing
        // actually names `dirad` (its Image Name column) before calling it
        // alive.
        let filter = format!("PID eq {pid}");
        let Some(out) = runner.run("tasklist", &["/FI", &filter, "/NH"]) else {
            return false;
        };
        return out.status.success() && String::from_utf8_lossy(&out.stdout).contains("dirad");
    }

    let Some(out) = runner.run("kill", &["-0", &pid.to_string()]) else {
        return false;
    };
    out.status.success()
}

/// Poll until `pid` is gone, up to `attempts × interval`. Returns whether exit
/// was actually observed.
///
/// This is the question the restart path has to ask and never did. It polled
/// [`is_up`] instead — but `dirad`'s control listener is a detached accept loop
/// that nothing cancels, and the shutdown notify fires only *after* the response
/// is written, so the pipe answers `Ping` throughout teardown. "Still answering"
/// therefore meant "still tearing down", not "wedged", and the old 5 s budget
/// sat below the ~8 s worst case (3 s offline beat + 5 s WAL checkpoint) — so a
/// daemon honouring the request exactly as designed got `taskkill /F` mid
/// checkpoint, and its replacement was spawned into a pipe whose handles had not
/// been released.
async fn wait_for_exit(
    pid: u32,
    runner: &dyn Runner,
    os: Os,
    attempts: u32,
    interval: std::time::Duration,
) -> bool {
    for _ in 0..attempts {
        if !pid_is_alive(pid, runner, os) {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    !pid_is_alive(pid, runner, os)
}

fn alive_pidfile_pid(config: &Config, runner: &dyn Runner, os: Os) -> Option<u32> {
    alive_pid_at(&pidfile(config), runner, os)
}

/// Work out how the daemon is currently supervised. See [`Supervision`] for
/// the possible outcomes and the order they're checked in.
pub async fn detect_supervision(config: &Config) -> Supervision {
    detect_supervision_with(
        config,
        &SystemRunner,
        current_os(),
        &dira_core::config::legacy_socket_path(),
    )
    .await
}

async fn detect_supervision_with(
    config: &Config,
    runner: &dyn Runner,
    os: Os,
    legacy_sock: &Path,
) -> Supervision {
    if os == Os::Macos {
        if let Some(out) = runner.run("launchctl", &["list", "sh.dirahq.dirad"]) {
            if out.status.success() {
                return Supervision::Launchd;
            }
        }
    }

    if os == Os::Linux {
        if let Some(out) = runner.run("systemctl", &["--user", "is-active", "dirad.service"]) {
            if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "active" {
                return Supervision::SystemdUser;
            }
        }
    }

    if os == Os::Windows {
        if let Some(out) = runner.run("schtasks", &["/Query", "/TN", "DiraDaemon"]) {
            if out.status.success() {
                return Supervision::ScheduledTask;
            }
        }
        // Covers the HKCU Run-key fallback `install` uses when `schtasks
        // /Create` needed elevation it didn't have — see
        // [`Supervision::ScheduledTask`]'s doc.
        if let Some(out) = runner.run(
            "reg",
            &[
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "DiraDaemon",
            ],
        ) {
            if out.status.success() {
                return Supervision::ScheduledTask;
            }
        }
    }

    if let Some(pid) = alive_pidfile_pid(config, runner, os) {
        return Supervision::Pidfile(pid);
    }

    if let Ok(Response::DaemonInfo { pid, .. }) =
        client::send(&config.socket_path, &Request::DaemonInfo).await
    {
        return Supervision::Socket(pid);
    }

    // A bare pre-D-0008 daemon predates the fixed socket path (D-0008) and is
    // otherwise invisible above — its pidfile and socket both live under the
    // old $TMPDIR anchor. Probed only to find and kill it on restart, never
    // as a working rendezvous point (see D-0008's amendment).
    if legacy_sock != config.socket_path {
        if let Ok(Response::DaemonInfo { pid, .. }) =
            client::send(legacy_sock, &Request::DaemonInfo).await
        {
            return Supervision::LegacySocket {
                pid: Some(pid),
                sock: legacy_sock.to_path_buf(),
            };
        }

        if answers(legacy_sock).await {
            // Too old to answer `DaemonInfo` — a pre-public dev build. Fall
            // back to the pidfile beside the legacy socket; if that's also
            // gone, the pid is unrecoverable and `restart` must say so
            // rather than guess.
            let pid = alive_pid_at(&pidfile_beside(legacy_sock), runner, os);
            return Supervision::LegacySocket {
                pid,
                sock: legacy_sock.to_path_buf(),
            };
        }
    }

    Supervision::NotRunning
}

/// `launchctl kickstart -k gui/<uid>/sh.dirahq.dirad`, falling back to
/// `launchctl stop sh.dirahq.dirad` on older macOS (the plist's `KeepAlive`
/// respawns it — from the *current* `ProgramArguments` path, so this still
/// picks up a swapped inode from an update). Errors with the exact manual
/// command on total failure, rather than reporting success silently.
fn restart_launchd(runner: &dyn Runner) -> Result<()> {
    let uid = runner
        .run("id", &["-u"])
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(uid) = &uid {
        let target = format!("gui/{uid}/sh.dirahq.dirad");
        if let Some(out) = runner.run("launchctl", &["kickstart", "-k", &target]) {
            if out.status.success() {
                println!("launchctl kickstart -k {target}");
                return Ok(());
            }
        }
    }

    if let Some(out) = runner.run("launchctl", &["stop", "sh.dirahq.dirad"]) {
        if out.status.success() {
            println!("launchctl stop sh.dirahq.dirad (KeepAlive will respawn it)");
            return Ok(());
        }
    }

    anyhow::bail!(
        "could not restart the launchd agent automatically — run this yourself:\n  \
         launchctl kickstart -k gui/$(id -u)/sh.dirahq.dirad"
    )
}

/// `systemctl --user restart dirad.service`. Errors with the exact manual
/// command on failure — the common real-world case is a headless box without
/// `loginctl enable-linger`, where `systemctl --user` may not even be
/// reachable, and that failure must be visible, not swallowed.
fn restart_systemd(runner: &dyn Runner) -> Result<()> {
    if let Some(out) = runner.run("systemctl", &["--user", "restart", "dirad.service"]) {
        if out.status.success() {
            println!("systemctl --user restart dirad.service");
            return Ok(());
        }
    }

    anyhow::bail!(
        "could not restart the systemd-user unit automatically (headless box without \
         `loginctl enable-linger`? systemd --user unreachable?) — run this yourself:\n  \
         systemctl --user restart dirad.service"
    )
}

/// Restart a windows daemon supervised by the scheduled task or Run-key
/// fallback (see [`Supervision::ScheduledTask`]): stop the running daemon and
/// CONFIRM IT EXITED, then relaunch via `schtasks /Run`, the same start-now step
/// `install` uses.
///
/// This used to send `Shutdown`, wait 5 s, **discard the result**, and run
/// `schtasks /Run` regardless — no force-kill on this path at all. That is a
/// guaranteed second daemon rather than a race whenever the old one is slow,
/// wedged, or unreachable, and it is the branch every user who ran
/// `dira daemon install` takes. `detect_supervision` returns `ScheduledTask` on
/// mere *registration*, returning before the pidfile probe, so the pid is
/// resolved here explicitly rather than inherited from detection.
///
/// With no resolvable pid and something still answering, this refuses and prints
/// manual instructions — the standard the legacy-socket path already holds
/// itself to, rather than starting a second daemon beside the first.
async fn restart_scheduled_task(config: &Config, runner: &dyn Runner) -> Result<()> {
    restart_scheduled_task_with(config, runner, STOP_GRACE_ATTEMPTS, STOP_GRACE_INTERVAL).await
}

/// [`restart_scheduled_task`] with an injectable grace budget — see
/// [`restart_bare_with`].
async fn restart_scheduled_task_with(
    config: &Config,
    runner: &dyn Runner,
    attempts: u32,
    interval: std::time::Duration,
) -> Result<()> {
    let pid = match alive_pidfile_pid(config, runner, Os::Windows) {
        Some(p) => Some(p),
        None => match client::send(&config.socket_path, &Request::DaemonInfo).await {
            Ok(Response::DaemonInfo { pid, .. }) => Some(pid),
            _ => None,
        },
    };

    match pid {
        Some(pid) => {
            if !graceful_then_force(
                config,
                &pid.to_string(),
                runner,
                Os::Windows,
                attempts,
                interval,
            )
            .await
            {
                anyhow::bail!(
                    "dirad (pid {pid}) did not exit — refusing to start a second daemon \
                     beside it. Stop it yourself, then start it again:\n  \
                     taskkill /PID {pid} /F\n  schtasks /Run /TN DiraDaemon"
                );
            }
        }
        // Nothing there and no pid: nothing to stop, so relaunching is safe.
        // Anything else with no resolvable pid is refused.
        //
        // The tri-state `reach` rather than `is_up` (D-0016): `is_up` collapses
        // ERROR_ACCESS_DENIED into "down", so an elevated daemon refusing this
        // non-elevated caller would read as "nothing running" — and this is
        // precisely the function that would then launch a second one beside it.
        None => match reach(config).await {
            client::Reach::Down => {}
            client::Reach::Up => anyhow::bail!(
                "a dirad is answering but its pid could not be determined — refusing \
                 to start a second daemon beside it. Stop it via Task Manager, then:\n  \
                 schtasks /Run /TN DiraDaemon"
            ),
            client::Reach::Denied => anyhow::bail!(
                "a dirad is running but refuses this connection — it was very likely \
                 started with different privileges (e.g. from an Administrator \
                 terminal). Refusing to start a second daemon beside it. Stop it from \
                 a terminal at the same privilege level, then:\n  \
                 schtasks /Run /TN DiraDaemon"
            ),
            // Busy means a daemon IS listening, every instance just happened to
            // be taken. That is emphatically not "nothing running".
            client::Reach::Busy => anyhow::bail!(
                "a dirad is listening but every pipe instance was busy — refusing to \
                 start a second daemon beside it. Retry in a moment:\n  \
                 dira daemon restart"
            ),
            // An unclassified connect error is not evidence of absence either.
            client::Reach::Other => anyhow::bail!(
                "could not determine whether a dirad is running — refusing to start a \
                 second daemon beside it. Check `dira daemon status` first."
            ),
        },
    }

    if let Some(out) = runner.run("schtasks", &["/Run", "/TN", "DiraDaemon"]) {
        if out.status.success() {
            println!("schtasks /Run /TN DiraDaemon");
            return Ok(());
        }
    }

    anyhow::bail!(
        "could not restart the scheduled task automatically — run this yourself:\n  \
         schtasks /Run /TN DiraDaemon"
    )
}

/// Kill the `dirad` at `pid` answering on `sock` (escalating to `-9` if it
/// doesn't let go within 5s), then remove its pidfile and socket file so
/// nothing stumbles over a stale rendezvous point afterward. Split out from
/// [`restart_bare`] so the cleanup half is unit-testable without spawning a
/// real daemon — starting one is still the part no test here attempts.
///
/// Unix shape (`kill`, files beside the socket): windows bare daemons go
/// through [`graceful_then_force`] in [`restart_bare`] instead — a
/// pipe endpoint has no files to clean, and windows has no `kill`.
async fn reap(pid: u32, sock: &Path, runner: &dyn Runner) {
    let _ = runner.run("kill", &[&pid.to_string()]);

    let mut still_up = true;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !answers(sock).await {
            still_up = false;
            break;
        }
    }
    if still_up {
        let _ = runner.run("kill", &["-9", &pid.to_string()]);
    }

    std::fs::remove_file(pidfile_beside(sock)).ok();
    std::fs::remove_file(sock).ok();
}

/// Kill a bare (non-service-managed) `dirad` we found by pidfile or by asking
/// the socket, then reuse [`start`] to bring it back — the plain dev/dogfood
/// path `install()` doesn't touch. `sock` is the socket the dying daemon
/// answers on — the configured one for [`Supervision::Pidfile`]/[`Supervision::Socket`],
/// or the legacy one when migrating a pre-D-0008 daemon off it (a unix-only
/// situation: no pre-D-0008 windows daemon can exist, windows support shipped
/// after the socket moved).
async fn restart_bare(
    config: &Config,
    pid: u32,
    sock: &Path,
    runner: &dyn Runner,
    os: Os,
) -> Result<()> {
    restart_bare_with(
        config,
        pid,
        sock,
        runner,
        os,
        STOP_GRACE_ATTEMPTS,
        STOP_GRACE_INTERVAL,
    )
    .await
}

/// [`restart_bare`] with an injectable grace budget, so tests can exercise the
/// refuse-to-spawn branch on a few milliseconds instead of the real 15 s window
/// — the same seam, and for the same reason, as
/// [`graceful_then_force`]'s parameters.
async fn restart_bare_with(
    config: &Config,
    pid: u32,
    sock: &Path,
    runner: &dyn Runner,
    os: Os,
    attempts: u32,
    interval: std::time::Duration,
) -> Result<()> {
    if os == Os::Windows {
        // Confirm the PROCESS is gone before spawning a replacement. On windows
        // the single-instance guard is the pipe bind, and the pipe name stays
        // taken until the old process's handles are released — so starting while
        // it lingers races a guard that cannot yet refuse, and the replacement
        // either dies on `first_pipe_instance` (leaving zero daemons) or, on the
        // scheduled-task path, lands beside a live one.
        if !graceful_then_force(
            config,
            &pid.to_string(),
            runner,
            Os::Windows,
            attempts,
            interval,
        )
        .await
        {
            // Deliberately leave the pidfile: it is the only remaining handle on
            // a process we could not stop, and deleting it would make the
            // survivor untrackable by `daemon status`/`stop`.
            anyhow::bail!(
                "dirad (pid {pid}) did not exit after a graceful shutdown and a forced kill — \
                 refusing to start a second daemon beside it. Stop it yourself, then \
                 `dira daemon start`:\n  taskkill /PID {pid} /F"
            );
        }
        std::fs::remove_file(pidfile(config)).ok();
    } else {
        reap(pid, sock, runner).await;
    }
    start(config).await
}

/// Poll `DaemonInfo` for up to 10s and report the version the daemon that
/// answers is running — the final check that a restart actually landed a new,
/// live process, regardless of which supervision mode restarted it.
async fn wait_up_and_report(config: &Config) -> Result<()> {
    for _ in 0..100 {
        if let Ok(Response::DaemonInfo { version, pid, .. }) =
            client::send(&config.socket_path, &Request::DaemonInfo).await
        {
            println!("dirad restarted (pid {pid}, version {version})");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("dirad did not come back up within 10s after restart")
}

/// Restart the daemon, however it is currently supervised.
///
/// Detects [`Supervision`], restarts the way that supervisor expects, then
/// polls until the daemon answers again and reports its version. A
/// service-managed restart that fails prints the exact command to run by
/// hand rather than pretending it succeeded; a daemon that wasn't running is
/// a no-op, not an error.
pub async fn restart(config: &Config) -> Result<()> {
    restart_with(
        config,
        &SystemRunner,
        current_os(),
        &dira_core::config::legacy_socket_path(),
    )
    .await
}

async fn restart_with(
    config: &Config,
    runner: &dyn Runner,
    os: Os,
    legacy_sock: &Path,
) -> Result<()> {
    let supervision = detect_supervision_with(config, runner, os, legacy_sock).await;
    println!("supervision: {supervision:?}");

    match &supervision {
        Supervision::Launchd => restart_launchd(runner)?,
        Supervision::SystemdUser => restart_systemd(runner)?,
        Supervision::ScheduledTask => restart_scheduled_task(config, runner).await?,
        Supervision::Pidfile(pid) | Supervision::Socket(pid) => {
            restart_bare(config, *pid, &config.socket_path, runner, os).await?;
        }
        Supervision::LegacySocket {
            pid: Some(pid),
            sock,
        } => {
            restart_bare(config, *pid, sock, runner, os).await?;
        }
        Supervision::LegacySocket { pid: None, sock } => {
            anyhow::bail!(
                "a pre-upgrade dirad is still running on {} but its pid could not be \
                 determined — stop it yourself (pkill -x dirad), then run `dira daemon start`",
                sock.display()
            );
        }
        Supervision::NotRunning => {
            println!("daemon was not running");
            return Ok(());
        }
    }

    wait_up_and_report(config).await
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    fn info(version: &str) -> Info {
        Info {
            version: version.to_string(),
            schema_version: "3".into(),
            pid: 1,
            uptime_seconds: 0,
            http_ingress_error: None,
            control_channel_warning: None,
            db_path: None,
            storage_warning: None,
        }
    }

    fn probe_of(info: Option<Info>, answered_ping: bool, legacy: Option<PathBuf>) -> DaemonProbe {
        DaemonProbe {
            reach: if answered_ping {
                client::Reach::Up
            } else {
                client::Reach::Down
            },
            info,
            answered_ping,
            unexpected: false,
            legacy,
        }
    }

    /// install.sh keys a restart-after-upgrade decision off `dira daemon
    /// status`'s exit code, and "legacy" must count as running (W1). This is
    /// that contract, pinned away from the printing.
    #[test]
    fn running_matches_the_install_sh_contract() {
        assert!(probe_of(Some(info("0.3.0")), true, None).running());
        assert!(probe_of(None, true, None).running());
        assert!(probe_of(None, false, Some(PathBuf::from("/tmp/dira.sock"))).running());
        assert!(!probe_of(None, false, None).running());
    }

    #[test]
    fn status_line_reports_degradation_only_from_daemon_info() {
        let sock = Path::new("/run/dira.sock");
        let mut degraded = info("0.3.0");
        degraded.http_ingress_error = Some("port busy".into());
        assert!(probe_of(Some(degraded), true, None)
            .status_line(sock)
            .contains("DEGRADED"));
        // A daemon too old for `DaemonInfo` is up, not degraded.
        assert_eq!(
            probe_of(None, true, None).status_line(sock),
            up_message(sock, None)
        );
        assert_eq!(
            probe_of(None, false, None).status_line(sock),
            down_message(None)
        );
    }

    #[test]
    fn version_skew_line_is_silent_when_they_match() {
        assert!(version_skew_line("0.3.0", "0.3.0").is_none());
        let line = version_skew_line("0.4.0", "0.3.0").expect("skew");
        assert!(line.contains("0.4.0") && line.contains("0.3.0"));
        assert!(line.contains("dira daemon stop && dira daemon start"));
    }

    #[test]
    fn supervision_label_covers_every_variant() {
        assert_eq!(
            supervision_label(&Supervision::Launchd).as_deref(),
            Some("launchd")
        );
        assert_eq!(
            supervision_label(&Supervision::SystemdUser).as_deref(),
            Some("systemd --user")
        );
        assert_eq!(
            supervision_label(&Supervision::ScheduledTask).as_deref(),
            Some("scheduled task")
        );
        assert_eq!(
            supervision_label(&Supervision::Pidfile(7)).as_deref(),
            Some("pidfile (pid 7)")
        );
        assert_eq!(
            supervision_label(&Supervision::Socket(7)).as_deref(),
            Some("unmanaged (pid 7, no pidfile)")
        );
        assert_eq!(
            supervision_label(&Supervision::LegacySocket {
                pid: None,
                sock: PathBuf::from("/tmp/dira.sock"),
            })
            .as_deref(),
            Some("pre-upgrade daemon on legacy socket /tmp/dira.sock (pid unknown)")
        );
        // Nothing running has nothing to say — this reproduces the early
        // `return` `print_supervision` used to do inline.
        assert!(supervision_label(&Supervision::NotRunning).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::ExitStatus;

    /// Build a fake `ExitStatus` from a process exit code. `ExitStatusExt` is
    /// split by platform (unix's `from_raw` takes the raw wait(2) status word,
    /// shifted; windows' takes a plain `u32` exit code) — this hides the split
    /// behind one signature so `FakeRunner` compiles and behaves the same way
    /// on every host `cargo test -p dira` runs on.
    #[cfg(unix)]
    fn fake_exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    fn fake_exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    /// A scripted [`Runner`]: `key(prog, args) -> (exit_code, stdout)`. A
    /// missing key means the command wasn't stubbed and behaves like
    /// `Command::spawn` failing to find it — `run` returns `None`, exactly
    /// like a real `Runner::run` when `launchctl`/`systemctl`/`schtasks` isn't
    /// installed. Every call (stubbed or not) is recorded in `calls` so tests
    /// can assert a command was (or wasn't) invoked, not just what it would
    /// have returned.
    #[derive(Default)]
    struct FakeRunner {
        responses: HashMap<String, (i32, String)>,
        calls: std::cell::RefCell<Vec<String>>,
        /// When set, `tasklist` reports the process alive until a `taskkill` has
        /// been issued, then reports it gone. Models the one transition the
        /// restart sequence turns on.
        dies_when_killed: bool,
    }

    impl FakeRunner {
        fn returning(mut self, prog: &str, args: &[&str], code: i32, stdout: &str) -> Self {
            self.responses
                .insert(key(prog, args), (code, stdout.to_string()));
            self
        }

        /// `tasklist` reports alive until `taskkill` runs, then reports gone.
        fn dying_when_killed(mut self) -> Self {
            self.dies_when_killed = true;
            self
        }

        /// `tasklist` always reports the process alive — a daemon that survives
        /// even a forced kill (the access-denied-against-elevated case).
        fn always_alive(self) -> Self {
            self.returning(
                "tasklist",
                &["/FI", "PID eq 4242", "/NH"],
                0,
                "dirad.exe                     4242 Console   1     12,345 K",
            )
        }

        /// The unix twin of [`Self::always_alive`]: `kill -0` keeps succeeding,
        /// so the process is never confirmed gone however hard it is signalled.
        fn always_alive_unix(self) -> Self {
            self.returning("kill", &["-0", "4242"], 0, "")
        }

        fn called(&self, prog: &str, args: &[&str]) -> bool {
            self.calls.borrow().contains(&key(prog, args))
        }
    }

    fn key(prog: &str, args: &[&str]) -> String {
        std::iter::once(prog)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }

    impl Runner for FakeRunner {
        fn run(&self, prog: &str, args: &[&str]) -> Option<Output> {
            self.calls.borrow_mut().push(key(prog, args));
            // A live process that stops being live once it is killed. Static
            // responses can't express that, and the whole point of the restart
            // sequence is the transition — "still listed" before the kill,
            // "gone" after it.
            // The unix liveness probe, with the same transition: `kill -0`
            // succeeds until the process has been signalled, then fails.
            if self.dies_when_killed && prog == "kill" && args.first() == Some(&"-0") {
                let signalled = self
                    .calls
                    .borrow()
                    .iter()
                    .any(|c| c.starts_with("kill ") && !c.starts_with("kill -0 "));
                return Some(Output {
                    status: fake_exit_status(i32::from(signalled)),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if self.dies_when_killed && prog == "tasklist" {
                let killed = self
                    .calls
                    .borrow()
                    .iter()
                    .any(|c| c.starts_with("taskkill "));
                return Some(Output {
                    status: fake_exit_status(0),
                    stdout: if killed {
                        b"INFO: No tasks are running which match the specified criteria.".to_vec()
                    } else {
                        b"dirad.exe                     4242 Console   1     12,345 K".to_vec()
                    },
                    stderr: Vec::new(),
                });
            }
            let (code, stdout) = self.responses.get(&key(prog, args))?;
            Some(Output {
                status: fake_exit_status(*code),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    /// A unique temp dir per test, mirroring `project.rs`'s `temp_repo_dir` —
    /// pid + a clock suffix so parallel tests never collide.
    ///
    /// Deliberately terse. Everything here ends up inside a `sun_path`, which
    /// caps at 104 bytes on macOS, and `$TMPDIR` alone eats ~49 of those on a
    /// stock mac. A descriptive directory name is what pushed five of these
    /// tests over the limit — they passed under a short `$TMPDIR` locally and
    /// failed in CI with a bare `InvalidInput`. `sock_in` re-checks the budget
    /// so a future long tag fails with an explanation instead of that.
    ///
    /// Unix-only: windows `test_config` builds a pipe name instead of a socket
    /// file, so nothing there needs a temp dir — ungated this is dead code
    /// under the windows CI job's `-D warnings`.
    #[cfg(unix)]
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        // Hex-truncated: the low 32 bits of the nanosecond clock still separate
        // parallel tests, in 8 characters instead of 19.
        let base = std::env::temp_dir().join(format!(
            "ds-{tag}-{}-{:x}",
            std::process::id(),
            uniq & 0xffff_ffff
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// The longest path `UnixListener::bind` accepts: `sun_path` is 104 bytes
    /// on macOS, 108 on Linux. Assert against the smaller so a path that would
    /// only fail on macOS fails everywhere.
    #[cfg(unix)]
    const SUN_MAX: usize = 104;

    /// A socket path inside a fresh temp dir, checked against [`SUN_MAX`] so an
    /// over-long path names itself instead of surfacing as `InvalidInput` from
    /// deep inside a bind.
    #[cfg(unix)]
    fn sock_in(tag: &str, name: &str) -> PathBuf {
        let path = temp_dir(tag).join(name);
        assert!(
            path.as_os_str().len() < SUN_MAX,
            "test socket path is {} bytes, over the {SUN_MAX}-byte sun_path limit: {} \
             — shorten the tag",
            path.as_os_str().len(),
            path.display(),
        );
        path
    }

    fn test_config(tag: &str) -> Config {
        // unix: a socket file in a fresh temp dir (budget-checked by
        // `sock_in`). windows: a filesystem path is NOT a valid pipe name, so
        // build a uniquely-named pipe endpoint instead — nothing is created on
        // disk for it, and `pidfile()` keys off the endpoint name for per-test
        // isolation.
        #[cfg(unix)]
        let socket_path = sock_in(tag, "d.sock");
        #[cfg(windows)]
        let socket_path = std::path::PathBuf::from(format!(
            r"\\.\pipe\dira-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Config {
            socket_path,
            ..Config::default()
        }
    }

    /// Write a pidfile for `config`, creating its parent first — on windows the
    /// pidfile lives under the per-user data dir, which need not exist yet on a
    /// fresh machine/CI runner (prod `start()` does the same `create_dir_all`).
    fn write_pidfile(config: &Config, contents: &str) {
        let pf = pidfile(config);
        if let Some(parent) = pf.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(pf, contents).unwrap();
    }

    /// A legacy-socket path guaranteed to be dead — nothing ever binds it.
    /// Every `detect_supervision_with`/`restart_with` call in this module must
    /// pass one of these rather than the real `legacy_socket_path()`, or the
    /// test probes the live `$TMPDIR/dira.sock` on this dogfooding machine
    /// and flakes. On windows the legacy concept doesn't exist (no pre-D-0008
    /// windows daemon was ever shipped), so any never-bound pipe name does.
    fn dead_legacy(tag: &str) -> PathBuf {
        #[cfg(unix)]
        {
            sock_in(tag, "l.sock")
        }
        #[cfg(windows)]
        {
            PathBuf::from(format!(
                r"\\.\pipe\dira-test-legacy-{tag}-{}",
                std::process::id()
            ))
        }
    }

    /// Answer one `DaemonInfo` request on an accepted connection with a
    /// canned response carrying `pid` — a minimal stand-in for a running
    /// `dirad`, speaking the same length-prefixed-JSON framing as
    /// `client::send` (via `dira_ipc`), so `detect_supervision`'s socket
    /// fallback can be exercised without a real daemon.
    async fn respond_daemon_info(mut stream: dira_ipc::Stream, pid: u32) {
        if dira_ipc::read_frame(&mut stream).await.is_err() {
            return;
        }

        let resp = Response::DaemonInfo {
            version: "9.9.9-test".to_string(),
            schema_version: "1".to_string(),
            pid,
            uptime_seconds: 1,
            http_ingress_error: None,
            control_channel_warning: None,
            db_path: None,
            storage_warning: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let _ = dira_ipc::write_frame(&mut stream, &bytes).await;
    }

    /// Answer one `Ping` with `Pong` — stands in for a pre-public dev build
    /// that predates `DaemonInfo` but still answers the oldest request. Framed
    /// via `dira_ipc` like [`respond_daemon_info`] — the wire shape is
    /// identical to what a raw pre-D-0008 daemon spoke. Unix-gated with its
    /// only callers, the legacy-migration tests.
    #[cfg(unix)]
    async fn respond_pong(mut stream: dira_ipc::Stream) {
        if dira_ipc::read_frame(&mut stream).await.is_err() {
            return;
        }
        let bytes = serde_json::to_vec(&Response::Pong).unwrap();
        let _ = dira_ipc::write_frame(&mut stream, &bytes).await;
    }

    // -- the "up" line, incl. the degraded-ingress flag ----------------------

    // -- the store-divergence line -------------------------------------------
    //
    // The elevated / service-account case: `project_dirs()` resolves on BOTH
    // sides, so neither process can detect it alone — the daemon is happily
    // writing into one profile's AppData while `dira` reads another's and finds
    // an empty database. Comparing the two answers is the only way to see it.

    #[test]
    fn store_divergence_is_quiet_when_the_paths_agree() {
        assert!(
            store_divergence_line(
                Path::new("/home/u/.local/share/dira/dira.db"),
                Some("/home/u/.local/share/dira/dira.db")
            )
            .is_none(),
            "the normal case must not nag"
        );
    }

    #[test]
    fn store_divergence_names_both_paths_when_they_differ() {
        let line = store_divergence_line(
            Path::new("/Users/me/Library/Application Support/dira/dira.db"),
            Some("C:\\Windows\\system32\\config\\systemprofile\\AppData\\dira.db"),
        )
        .expect("a divergence must be reported");
        assert!(
            line.contains("/Users/me/Library/Application Support/dira/dira.db"),
            "must name the CLI's store, got: {line}"
        );
        assert!(
            line.contains("systemprofile"),
            "must name the DAEMON's store, got: {line}"
        );
        assert!(
            line.to_lowercase().contains("elevated")
                || line.to_lowercase().contains("different user"),
            "must name the likely cause, got: {line}"
        );
    }

    #[test]
    fn store_divergence_is_quiet_against_an_older_daemon() {
        // A pre-upgrade daemon omits `db_path`. Unknown is not divergent, and
        // inventing a warning from a missing field would be worse than silence.
        assert!(store_divergence_line(Path::new("/x/dira.db"), None).is_none());
    }

    #[test]
    fn up_message_is_plain_when_the_daemon_is_healthy() {
        let msg = up_message(std::path::Path::new("/x/dira.sock"), None);
        assert_eq!(msg, "dirad: up  (socket /x/dira.sock)");
    }

    #[test]
    fn up_message_flags_a_degraded_hook_ingress() {
        // A daemon whose ingress port is taken answers every control request
        // but captures nothing — it must not read as plain "up".
        let msg = up_message(
            std::path::Path::new("/x/dira.sock"),
            Some("could not bind the hook ingress on 127.0.0.1:8722: Address already in use"),
        );
        assert!(
            msg.to_lowercase().contains("degraded"),
            "must be flagged as degraded, got: {msg}"
        );
        assert!(
            msg.contains("8722"),
            "must carry the underlying reason, got: {msg}"
        );
    }

    // -- the "down" line, incl. the legacy-socket nudge ----------------------

    #[test]
    fn down_message_is_bare_when_no_legacy_daemon_answers() {
        assert_eq!(down_message(None), "dirad: down");
    }

    #[test]
    fn down_message_points_at_a_daemon_still_on_the_legacy_socket() {
        let msg = down_message(Some(std::path::Path::new("/var/folders/x/T/dira.sock")));
        assert!(
            msg.contains("/var/folders/x/T/dira.sock"),
            "must name the legacy socket that answered, got: {msg}"
        );
        assert!(
            msg.contains("dira daemon restart"),
            "must tell the user how to move it onto the new socket, got: {msg}"
        );
    }

    // -- `dira version`'s legacy-skew note ------------------------------------

    #[test]
    fn version_not_running_message_is_bare_without_a_legacy_daemon() {
        assert_eq!(version_not_running_message(None), "dirad   not running");
    }

    #[test]
    fn version_not_running_message_points_at_the_legacy_daemon() {
        let msg =
            version_not_running_message(Some(std::path::Path::new("/var/folders/x/T/dira.sock")));
        assert!(
            msg.contains("/var/folders/x/T/dira.sock"),
            "must name the legacy socket, got: {msg}"
        );
        assert!(
            msg.contains("dira daemon restart"),
            "must tell the user how to migrate it, got: {msg}"
        );
    }

    // -- `status`'s exit-code contract ---------------------------------------

    #[tokio::test]
    async fn status_reports_running_when_the_daemon_answers() {
        let config = test_config("status-up");
        let mut listener = dira_ipc::Listener::bind(&config.socket_path).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                respond_daemon_info(stream, 111).await;
            }
        });

        let got = status_with(&config, &dead_legacy("status-up")).await;
        assert!(got.unwrap());
    }

    #[tokio::test]
    async fn status_reports_not_running_when_nothing_answers() {
        let config = test_config("status-down");
        let got = status_with(&config, &dead_legacy("status-down")).await;
        assert!(!got.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn status_reports_running_when_only_the_legacy_socket_answers() {
        // The configured socket is dead, but a pre-upgrade daemon still
        // answers on the legacy one — install.sh's restart-after-upgrade
        // decision must see this as "running", or it skips the restart that
        // W1 relies on to migrate it and strands the old daemon.
        let config = test_config("status-legacy");
        let legacy = sock_in("st-leg", "l.sock");
        let mut listener = dira_ipc::Listener::bind(&legacy).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                respond_pong(stream).await;
            }
        });

        let got = status_with(&config, &legacy).await;
        assert!(got.unwrap());
    }

    // -- the `Supervision` branches ------------------------------------------

    #[tokio::test]
    async fn detect_supervision_launchd_when_launchctl_list_succeeds() {
        let config = test_config("launchd");
        let runner =
            FakeRunner::default().returning("launchctl", &["list", "sh.dirahq.dirad"], 0, "");
        let got =
            detect_supervision_with(&config, &runner, Os::Macos, &dead_legacy("launchd")).await;
        assert_eq!(got, Supervision::Launchd);
    }

    #[tokio::test]
    async fn detect_supervision_ignores_launchd_probe_off_macos() {
        // Same stub, but the host isn't macOS — the launchd probe must not
        // even run, so this falls through to NotRunning rather than lying.
        let config = test_config("launchd-off-platform");
        let runner =
            FakeRunner::default().returning("launchctl", &["list", "sh.dirahq.dirad"], 0, "");
        let got = detect_supervision_with(
            &config,
            &runner,
            Os::Linux,
            &dead_legacy("launchd-off-platform"),
        )
        .await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[tokio::test]
    async fn detect_supervision_systemd_user_when_unit_is_active() {
        let config = test_config("systemd");
        let runner = FakeRunner::default().returning(
            "systemctl",
            &["--user", "is-active", "dirad.service"],
            0,
            "active\n",
        );
        let got =
            detect_supervision_with(&config, &runner, Os::Linux, &dead_legacy("systemd")).await;
        assert_eq!(got, Supervision::SystemdUser);
    }

    #[tokio::test]
    async fn detect_supervision_systemd_user_ignored_when_unit_inactive() {
        let config = test_config("systemd-inactive");
        let runner = FakeRunner::default().returning(
            "systemctl",
            &["--user", "is-active", "dirad.service"],
            3,
            "inactive\n",
        );
        let got = detect_supervision_with(
            &config,
            &runner,
            Os::Linux,
            &dead_legacy("systemd-inactive"),
        )
        .await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[tokio::test]
    async fn detect_supervision_scheduled_task_when_schtasks_query_succeeds() {
        let config = test_config("schtasks");
        let runner = FakeRunner::default().returning(
            "schtasks",
            &["/Query", "/TN", "DiraDaemon"],
            0,
            "TaskName: DiraDaemon\nStatus: Ready\n",
        );
        let got = detect_supervision_with(&config, &runner, Os::Windows, &dead_legacy("w")).await;
        assert_eq!(got, Supervision::ScheduledTask);
    }

    #[tokio::test]
    async fn detect_supervision_scheduled_task_via_run_key_when_schtasks_query_fails() {
        // `schtasks /Query` fails (task never registered, e.g. `install` took
        // the HKCU Run-key fallback), but `reg query` finds the fallback key —
        // still `ScheduledTask`, see its doc.
        let config = test_config("run-key");
        let runner = FakeRunner::default()
            .returning("schtasks", &["/Query", "/TN", "DiraDaemon"], 1, "ERROR:\n")
            .returning(
                "reg",
                &[
                    "query",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "DiraDaemon",
                ],
                0,
                "DiraDaemon    REG_SZ    \"C:\\bin\\dirad.exe\"\n",
            );
        let got = detect_supervision_with(&config, &runner, Os::Windows, &dead_legacy("w")).await;
        assert_eq!(got, Supervision::ScheduledTask);
    }

    #[tokio::test]
    async fn detect_supervision_ignores_scheduled_task_probes_off_windows() {
        let config = test_config("schtasks-off-platform");
        let runner =
            FakeRunner::default().returning("schtasks", &["/Query", "/TN", "DiraDaemon"], 0, "");
        let got = detect_supervision_with(&config, &runner, Os::Linux, &dead_legacy("l")).await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[tokio::test]
    async fn detect_supervision_falls_through_to_pidfile_when_windows_service_probes_fail() {
        let config = test_config("schtasks-missing");
        let pid = std::process::id();
        write_pidfile(&config, &pid.to_string());
        let filter = format!("PID eq {pid}");
        let runner = FakeRunner::default()
            .returning("schtasks", &["/Query", "/TN", "DiraDaemon"], 1, "ERROR:\n")
            .returning(
                "tasklist",
                &["/FI", &filter, "/NH"],
                0,
                "dirad.exe   1234 Console   1   12,345 K\n",
            );
        let got = detect_supervision_with(&config, &runner, Os::Windows, &dead_legacy("w")).await;
        assert_eq!(got, Supervision::Pidfile(pid));
    }

    #[tokio::test]
    async fn detect_supervision_pidfile_when_process_alive() {
        let config = test_config("pidfile");
        let pid = std::process::id();
        write_pidfile(&config, &pid.to_string());
        let runner = FakeRunner::default().returning("kill", &["-0", &pid.to_string()], 0, "");
        let got =
            detect_supervision_with(&config, &runner, Os::Other, &dead_legacy("pidfile")).await;
        assert_eq!(got, Supervision::Pidfile(pid));
    }

    #[tokio::test]
    async fn detect_supervision_pidfile_ignored_when_process_dead() {
        let config = test_config("pidfile-dead");
        write_pidfile(&config, "999999");
        let runner = FakeRunner::default().returning("kill", &["-0", "999999"], 1, "");
        let got =
            detect_supervision_with(&config, &runner, Os::Other, &dead_legacy("pidfile-dead"))
                .await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[tokio::test]
    async fn detect_supervision_socket_when_daemon_answers_without_pidfile() {
        let config = test_config("socket");
        let fake_pid = 424_242u32;
        let mut listener = dira_ipc::Listener::bind(&config.socket_path).await.unwrap();
        tokio::spawn(async move {
            if let Ok(stream) = listener.accept().await {
                respond_daemon_info(stream, fake_pid).await;
            }
        });

        let runner = FakeRunner::default();
        let got =
            detect_supervision_with(&config, &runner, Os::Other, &dead_legacy("socket")).await;
        assert_eq!(got, Supervision::Socket(fake_pid));
    }

    #[tokio::test]
    async fn detect_supervision_not_running_when_nothing_answers() {
        let config = test_config("notrunning");
        let runner = FakeRunner::default();
        let got =
            detect_supervision_with(&config, &runner, Os::Other, &dead_legacy("notrunning")).await;
        assert_eq!(got, Supervision::NotRunning);
    }

    // -- windows liveness probe (`alive_pidfile_pid`) ------------------------

    #[test]
    fn alive_pidfile_pid_windows_alive_when_tasklist_lists_dirad() {
        let config = test_config("win-alive");
        let pid = 4242u32;
        write_pidfile(&config, &pid.to_string());
        let filter = format!("PID eq {pid}");
        let runner = FakeRunner::default().returning(
            "tasklist",
            &["/FI", &filter, "/NH"],
            0,
            "dirad.exe   4242 Console   1   12,345 K\n",
        );
        assert_eq!(alive_pidfile_pid(&config, &runner, Os::Windows), Some(pid));
    }

    #[test]
    fn alive_pidfile_pid_windows_dead_when_tasklist_reports_no_tasks() {
        // `tasklist` exits 0 even with no match — it's an INFO line on stdout,
        // not a nonzero exit — so this must be treated as dead despite the
        // successful exit status.
        let config = test_config("win-dead");
        let pid = 4242u32;
        write_pidfile(&config, &pid.to_string());
        let filter = format!("PID eq {pid}");
        let runner = FakeRunner::default().returning(
            "tasklist",
            &["/FI", &filter, "/NH"],
            0,
            "INFO: No tasks are running which match the specified criteria.\n",
        );
        assert_eq!(alive_pidfile_pid(&config, &runner, Os::Windows), None);
    }

    // -- the legacy-socket migration (unix-only: no pre-D-0008 windows daemon
    // -- ever existed, and the fixtures are real socket files) ---------------

    #[cfg(unix)]
    #[tokio::test]
    async fn detect_supervision_finds_bare_daemon_on_legacy_socket() {
        // The configured socket is dead, but a pre-D-0008 daemon still
        // answers on the legacy $TMPDIR path — it must not read as NotRunning,
        // or `dira daemon restart` is a silent no-op against it.
        let config = test_config("legacy-daemon-info");
        let legacy = sock_in("leg-info", "l.sock");
        let mut listener = dira_ipc::Listener::bind(&legacy).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                respond_daemon_info(stream, 777).await;
            }
        });

        let runner = FakeRunner::default();
        let got = detect_supervision_with(&config, &runner, Os::Other, &legacy).await;
        assert_eq!(
            got,
            Supervision::LegacySocket {
                pid: Some(777),
                sock: legacy,
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detect_supervision_prefers_the_configured_socket_over_legacy() {
        // Both sockets answer, with different pids — the legacy probe must
        // never fire while the configured one is live, so the legacy path
        // stays non-load-bearing per D-0008.
        let config = test_config("both-sockets");
        let configured_pid = 111u32;
        let mut configured_listener = dira_ipc::Listener::bind(&config.socket_path).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = configured_listener.accept().await {
                respond_daemon_info(stream, configured_pid).await;
            }
        });

        let legacy = sock_in("both", "l.sock");
        let mut legacy_listener = dira_ipc::Listener::bind(&legacy).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = legacy_listener.accept().await {
                respond_daemon_info(stream, 222).await;
            }
        });

        let runner = FakeRunner::default();
        let got = detect_supervision_with(&config, &runner, Os::Other, &legacy).await;
        assert_eq!(got, Supervision::Socket(configured_pid));
    }

    #[tokio::test]
    async fn detect_supervision_skips_legacy_when_it_is_the_configured_path() {
        // Nothing listens anywhere, and the "legacy" path passed in is the
        // configured one — the guard must skip the (redundant) probe rather
        // than dialing the same dead path twice and returning something
        // other than NotRunning.
        let config = test_config("legacy-is-configured");
        let legacy = config.socket_path.clone();
        let runner = FakeRunner::default();
        let got = detect_supervision_with(&config, &runner, Os::Other, &legacy).await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detect_supervision_legacy_pid_from_pidfile_when_daemon_predates_daemon_info() {
        // A pre-public dev build answers `Ping` but not `DaemonInfo` — pid
        // recovery falls back to the pidfile beside the legacy socket.
        let config = test_config("legacy-pong-only");
        let legacy = sock_in("leg-pong", "l.sock");
        let mut listener = dira_ipc::Listener::bind(&legacy).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                respond_pong(stream).await;
            }
        });

        let pid = std::process::id();
        std::fs::write(pidfile_beside(&legacy), pid.to_string()).unwrap();
        let runner = FakeRunner::default().returning("kill", &["-0", &pid.to_string()], 0, "");

        let got = detect_supervision_with(&config, &runner, Os::Other, &legacy).await;
        assert_eq!(
            got,
            Supervision::LegacySocket {
                pid: Some(pid),
                sock: legacy,
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reap_removes_the_legacy_socket_and_pidfile() {
        // Nothing listens — the pid is already dead — but the socket file and
        // pidfile are still on disk, as they would be after a hard crash.
        // `reap` must clean both up so nothing trips on the stale rendezvous
        // point later.
        let legacy = sock_in("reap", "l.sock");
        std::fs::write(&legacy, b"").unwrap();
        std::fs::write(pidfile_beside(&legacy), "999999").unwrap();

        let runner = FakeRunner::default();
        reap(999_999, &legacy, &runner).await;

        assert!(!legacy.exists(), "legacy socket file must be removed");
        assert!(
            !pidfile_beside(&legacy).exists(),
            "legacy pidfile must be removed"
        );
    }

    // -- restart's per-mode command selection --------------------------------

    #[test]
    fn restart_launchd_uses_kickstart_when_id_and_launchctl_succeed() {
        let runner = FakeRunner::default()
            .returning("id", &["-u"], 0, "501\n")
            .returning(
                "launchctl",
                &["kickstart", "-k", "gui/501/sh.dirahq.dirad"],
                0,
                "",
            );
        assert!(restart_launchd(&runner).is_ok());
    }

    #[test]
    fn restart_launchd_falls_back_to_stop_when_kickstart_unavailable() {
        let runner =
            FakeRunner::default().returning("launchctl", &["stop", "sh.dirahq.dirad"], 0, "");
        assert!(restart_launchd(&runner).is_ok());
    }

    #[test]
    fn restart_launchd_errors_with_manual_command_when_both_fail() {
        let runner = FakeRunner::default();
        let err = restart_launchd(&runner).unwrap_err();
        assert!(err
            .to_string()
            .contains("launchctl kickstart -k gui/$(id -u)/sh.dirahq.dirad"));
    }

    #[test]
    fn restart_systemd_succeeds_when_unit_restarts() {
        let runner = FakeRunner::default().returning(
            "systemctl",
            &["--user", "restart", "dirad.service"],
            0,
            "",
        );
        assert!(restart_systemd(&runner).is_ok());
    }

    #[test]
    fn restart_systemd_errors_with_manual_command_when_unavailable() {
        let runner = FakeRunner::default();
        let err = restart_systemd(&runner).unwrap_err();
        assert!(err
            .to_string()
            .contains("systemctl --user restart dirad.service"));
    }

    #[tokio::test]
    async fn restart_scheduled_task_runs_schtasks_run_after_graceful_shutdown_attempt() {
        let config = test_config("win-restart-task");
        // Nothing is listening at `config.socket_path`, so the graceful
        // `Shutdown` send fails immediately and `still_up_after` reports "not
        // up" on its very first check — this exercises the happy path without
        // a real multi-second wait.
        let runner =
            FakeRunner::default().returning("schtasks", &["/Run", "/TN", "DiraDaemon"], 0, "");
        assert!(restart_scheduled_task(&config, &runner).await.is_ok());
    }

    #[tokio::test]
    async fn restart_scheduled_task_errors_with_manual_command_when_schtasks_run_fails() {
        let config = test_config("win-restart-task-fail");
        let runner = FakeRunner::default();
        let err = restart_scheduled_task(&config, &runner).await.unwrap_err();
        assert!(err.to_string().contains("schtasks /Run /TN DiraDaemon"));
    }

    // -- windows stop/restart escalation (`graceful_then_force`) ------------

    #[tokio::test]
    async fn graceful_then_force_skips_the_kill_when_the_process_exits() {
        let config = test_config("win-stop-graceful");
        // `tasklist` has no stub, so the probe reports the process gone on the
        // first poll — a dirad that honoured the graceful `Shutdown`.
        let runner = FakeRunner::default();
        let stopped = graceful_then_force(
            &config,
            "4242",
            &runner,
            Os::Windows,
            2,
            std::time::Duration::from_millis(5),
        )
        .await;
        assert!(stopped, "an exited process must be reported as stopped");
        assert!(!runner.called("taskkill", &["/PID", "4242", "/F"]));
    }

    // --- unix stop confirms exit, and install stops first (#123 / D-0019) ---

    /// A grace budget short enough that the "will not die" branch costs
    /// milliseconds instead of the real 15s window.
    const FAST: std::time::Duration = std::time::Duration::from_millis(5);

    /// A `kill` is a request, not an outcome. Unix used to signal, unlink the
    /// pidfile and the socket, and print "stopped" without ever asking whether
    /// the process had gone — so every caller that pre-stopped before
    /// `daemon install` was racing D-0009's socket guard, and unlinking the
    /// socket out from under a live daemon made it worse.
    #[tokio::test]
    async fn unix_stop_waits_for_the_process_to_actually_exit() {
        let config = test_config("unix-stop-graceful");
        let pf = pidfile(&config);
        std::fs::write(&pf, "4242").unwrap();
        let runner = FakeRunner::default().dying_when_killed();

        stop_with(&config, &runner, Os::Macos, 2, FAST)
            .await
            .expect("a process that exits on SIGTERM stops cleanly");

        assert!(
            runner.called("kill", &["-0", "4242"]),
            "exit must be confirmed by probing the process, not assumed"
        );
        assert!(
            !runner.called("kill", &["-9", "4242"]),
            "a daemon that honoured SIGTERM must never be force-killed"
        );
        assert!(
            !pf.exists(),
            "the pidfile is released once exit is confirmed"
        );
    }

    /// The unix twin of `escalation_keys_on_the_process_not_the_control_channel`.
    /// A daemon that will not die (another user's, say) must escalate and then
    /// report failure — never claim success, because the caller uses that answer
    /// to decide whether it may install a service over the top.
    #[tokio::test]
    async fn unix_stop_escalates_then_refuses_to_claim_an_unconfirmed_exit() {
        let config = test_config("unix-stop-stubborn");
        let pf = pidfile(&config);
        std::fs::write(&pf, "4242").unwrap();
        let runner = FakeRunner::default().always_alive_unix();

        let err = stop_with(&config, &runner, Os::Macos, 2, FAST)
            .await
            .expect_err("a surviving process must not report as stopped");

        assert!(
            runner.called("kill", &["-9", "4242"]),
            "SIGTERM being ignored must escalate to SIGKILL"
        );
        assert!(
            format!("{err:#}").contains("4242"),
            "the error must name the surviving pid: {err:#}"
        );
        assert!(
            pf.exists(),
            "the pidfile is the only handle left on a process we could not stop"
        );
    }

    /// #123: the pre-stop lived in three callers and not in the fourth. A bare
    /// `dira daemon install` walked into the flap — install now owns it.
    #[tokio::test]
    async fn install_stops_a_bare_daemon_before_registering_the_service() {
        let config = test_config("install-stops-bare");
        std::fs::write(pidfile(&config), "4242").unwrap();
        let runner = FakeRunner::default().dying_when_killed();

        stop_unmanaged_before_install(
            &config,
            Supervision::Pidfile(4242),
            &runner,
            Os::Macos,
            2,
            FAST,
        )
        .await
        .expect("a stoppable daemon is stopped");

        assert!(
            runner.called("kill", &["4242"]),
            "an unmanaged daemon holds the socket the service is about to bind"
        );
    }

    /// D-0019's directive applied to this path: never proceed to start a
    /// replacement — which `launchctl load` / `systemctl --now` / `schtasks
    /// /Run` all are — while the old pid is still alive. Installing anyway is
    /// precisely how the supervisor ends up restarting a loser in a loop.
    #[tokio::test]
    async fn install_refuses_when_the_old_daemon_cannot_be_confirmed_gone() {
        let config = test_config("install-stubborn");
        std::fs::write(pidfile(&config), "4242").unwrap();
        let runner = FakeRunner::default().always_alive_unix();

        stop_unmanaged_before_install(
            &config,
            Supervision::Pidfile(4242),
            &runner,
            Os::Macos,
            2,
            FAST,
        )
        .await
        .expect_err("installing over a live daemon is the flap this prevents");
    }

    /// A daemon a service manager already owns must be left alone: stopping it
    /// only makes its supervisor restart it in the middle of the install.
    #[tokio::test]
    async fn install_leaves_an_already_supervised_daemon_running() {
        let config = test_config("install-supervised");
        std::fs::write(pidfile(&config), "4242").unwrap();

        for supervision in [
            Supervision::Launchd,
            Supervision::SystemdUser,
            Supervision::ScheduledTask,
            Supervision::NotRunning,
        ] {
            let runner = FakeRunner::default().dying_when_killed();
            stop_unmanaged_before_install(
                &config,
                supervision.clone(),
                &runner,
                Os::Macos,
                2,
                FAST,
            )
            .await
            .expect("nothing to do");
            assert!(
                !runner.called("kill", &["4242"]),
                "{supervision:?} must not be stopped by install"
            );
        }
    }

    /// A daemon that stops ANSWERING is not a daemon that has exited.
    ///
    /// `dirad`'s control listener is a detached accept loop nothing cancels, and
    /// the shutdown notify fires only after the response is written, so the pipe
    /// answers `Ping` throughout teardown. The escalation must therefore key on
    /// the process, not the channel — this pins that it does, by leaving nothing
    /// listening (channel says "down") while `tasklist` still lists the process.
    #[tokio::test]
    async fn escalation_keys_on_the_process_not_the_control_channel() {
        let config = test_config("win-stop-escalate");
        let runner = FakeRunner::default().always_alive().returning(
            "taskkill",
            &["/PID", "4242", "/F"],
            0,
            "",
        );
        let stopped = graceful_then_force(
            &config,
            "4242",
            &runner,
            Os::Windows,
            2,
            std::time::Duration::from_millis(5),
        )
        .await;
        assert!(
            runner.called("taskkill", &["/PID", "4242", "/F"]),
            "a still-listed process must be force-killed even though nothing answers"
        );
        assert!(
            !stopped,
            "a process still listed after the kill must NOT report as stopped — \
             the caller uses this to refuse spawning a replacement"
        );
    }

    #[tokio::test]
    async fn a_forced_kill_that_works_reports_stopped() {
        let config = test_config("win-stop-escalate-ok");
        let runner = FakeRunner::default().dying_when_killed().returning(
            "taskkill",
            &["/PID", "4242", "/F"],
            0,
            "",
        );
        let stopped = graceful_then_force(
            &config,
            "4242",
            &runner,
            Os::Windows,
            2,
            std::time::Duration::from_millis(5),
        )
        .await;
        assert!(runner.called("taskkill", &["/PID", "4242", "/F"]));
        assert!(stopped, "exit observed after the kill must report stopped");
    }

    /// The reported bug, pinned without a live daemon: a replacement must never
    /// be spawned while the old process is still listed. `restart_bare` is
    /// otherwise untested on windows because its success path calls `start`,
    /// which spawns a real process — the failure path needs no such thing.
    #[tokio::test]
    async fn restart_refuses_to_spawn_a_replacement_while_the_old_pid_lives() {
        let config = test_config("win-restart-no-double");
        let runner = FakeRunner::default().always_alive().returning(
            "taskkill",
            &["/PID", "4242", "/F"],
            0,
            "",
        );
        let err = restart_bare_with(
            &config,
            4242,
            &config.socket_path,
            &runner,
            Os::Windows,
            2,
            std::time::Duration::from_millis(5),
        )
        .await
        .expect_err("a surviving daemon must fail the restart, not gain a sibling");
        let msg = err.to_string();
        assert!(
            msg.contains("did not exit") && msg.contains("4242"),
            "the error must name the surviving pid: {msg}"
        );
        assert!(
            msg.contains("taskkill /PID 4242 /F"),
            "and give the manual command: {msg}"
        );
    }

    /// The scheduled-task path used to discard its liveness result and run
    /// `schtasks /Run` unconditionally, with no force-kill at all — a guaranteed
    /// second daemon rather than a race, on the branch every user who ran
    /// `dira daemon install` takes.
    #[tokio::test]
    async fn scheduled_task_restart_never_launches_beside_a_live_daemon() {
        let config = test_config("win-task-no-double");
        write_pidfile(&config, "4242");
        let runner = FakeRunner::default()
            .always_alive()
            .returning("taskkill", &["/PID", "4242", "/F"], 0, "")
            .returning("schtasks", &["/Run", "/TN", "DiraDaemon"], 0, "");
        let err =
            restart_scheduled_task_with(&config, &runner, 2, std::time::Duration::from_millis(5))
                .await
                .expect_err("must refuse while the old daemon is alive");
        assert!(err.to_string().contains("did not exit"));
        assert!(
            !runner.called("schtasks", &["/Run", "/TN", "DiraDaemon"]),
            "schtasks /Run must not fire while the old process is still listed"
        );
    }

    #[tokio::test]
    async fn stop_with_windows_removes_pidfile_without_taskkill_when_shutdown_succeeds() {
        let config = test_config("win-stop-with");
        write_pidfile(&config, "4242");
        // Nothing listening → graceful attempt "succeeds" (there's nothing to
        // force-kill), so `taskkill` must not be invoked.
        let runner = FakeRunner::default();
        assert!(stop_with(&config, &runner, Os::Windows, 2, FAST)
            .await
            .is_ok());
        assert!(!pidfile(&config).exists());
        assert!(!runner.called("taskkill", &["/PID", "4242", "/F"]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_errors_with_manual_command_for_a_legacy_daemon_without_pid() {
        // A legacy daemon answers (so it's not NotRunning) but its pid can't
        // be determined (no pidfile, pre-public dev build) — restart must
        // refuse rather than start a second daemon beside an unkillable
        // zombie (the D-0009 two-daemon hazard).
        let config = test_config("legacy-no-pid-restart");
        let legacy = sock_in("leg-nopid", "l.sock");
        let mut listener = dira_ipc::Listener::bind(&legacy).await.unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                respond_pong(stream).await;
            }
        });

        let runner = FakeRunner::default();
        let err = restart_with(&config, &runner, Os::Other, &legacy)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("pkill -x dirad"), "message was: {msg}");
        assert!(
            msg.contains(&legacy.display().to_string()),
            "message was: {msg}"
        );
    }
}
