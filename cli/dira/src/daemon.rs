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

/// Is a daemon answering `Ping` on this exact socket?
pub async fn answers(sock: &Path) -> bool {
    matches!(client::send(sock, &Request::Ping).await, Ok(Response::Pong))
}

/// Is the daemon answering on the socket?
pub async fn is_up(config: &Config) -> bool {
    answers(&config.socket_path).await
}

pub async fn start(config: &Config) -> Result<()> {
    if is_up(config).await {
        println!("dirad already running");
        return Ok(());
    }
    let bin = locate_dirad();
    println!("starting {} ...", bin.display());
    let mut cmd = Command::new(&bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

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

    let pf = pidfile(config);
    if let Some(dir) = pf.parent() {
        // `runtime_dir()` isn't guaranteed to exist yet — on unix it's the
        // socket's own parent (already created when the daemon binds), but on
        // windows it's the project data dir, which may never have been touched
        // before this first `dira daemon start`.
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&pf, child.id().to_string()).ok();

    // Poll for readiness. Startup can be slow on first run — a keychain unlock
    // prompt (for the device key) or a large event-log hydration both delay the
    // socket bind — so poll generously (~10s) and distinguish a real crash from
    // a slow start by checking whether the child is still alive, instead of
    // reporting a false "did not come up".
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if is_up(config).await {
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
    // prompt. It will come up on its own, so report that rather than fail.
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
const STOP_GRACE_ATTEMPTS: u32 = 50;
const STOP_GRACE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

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

/// Ask a bare (non-service-managed) windows dirad to shut down gracefully over
/// the pipe, then escalate to `taskkill /F` if it's still up after the grace
/// window. Windows has no unix-style signal a process can trap for a clean
/// shutdown, so this asks nicely first via the same in-band `Request::Shutdown`
/// `dirad` already answers with `Response::Ok` before winding itself down. A
/// plain `taskkill` (no `/F`) can't reliably stop a windowless background
/// process, so the escalation goes straight to the force flag rather than
/// trying a graceful `taskkill` first. Shared by `stop`'s and `restart_bare`'s
/// windows branches — same escalation order both places.
async fn graceful_then_force_windows(
    config: &Config,
    pid: &str,
    runner: &dyn Runner,
    attempts: u32,
    interval: std::time::Duration,
) {
    let _ = client::send(&config.socket_path, &Request::Shutdown).await;
    if still_up_after(config, attempts, interval).await {
        let _ = runner.run("taskkill", &["/PID", pid, "/F"]);
    }
}

pub async fn stop(config: &Config) -> Result<()> {
    stop_with(config, &SystemRunner, current_os()).await
}

async fn stop_with(config: &Config, runner: &dyn Runner, os: Os) -> Result<()> {
    let pf = pidfile(config);
    let pid = match std::fs::read_to_string(&pf) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            println!("no pidfile; daemon may be managed by launchd/systemd or not running");
            return Ok(());
        }
    };

    if os == Os::Windows {
        graceful_then_force_windows(
            config,
            &pid,
            runner,
            STOP_GRACE_ATTEMPTS,
            STOP_GRACE_INTERVAL,
        )
        .await;
    } else {
        let _ = Command::new("kill").arg(&pid).status();
    }

    std::fs::remove_file(&pf).ok();
    std::fs::remove_file(&config.socket_path).ok();
    println!("stopped dirad (pid {pid})");
    Ok(())
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

/// `legacy_daemon_socket` against the real legacy path — for callers (like
/// `dira version`'s skew note) that have no reason to inject one.
pub(crate) async fn legacy_daemon_socket_default(config: &Config) -> Option<PathBuf> {
    legacy_daemon_socket(config, &dira_core::config::legacy_socket_path()).await
}

/// The `dirad: up` line. A daemon whose hook ingress failed to bind answers
/// every control request but captures nothing, so it is reported as **degraded**
/// with the reason rather than as a healthy "up" (D-0009).
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
    // `DaemonInfo` rather than `Ping`, because only it carries the degradation.
    // A daemon too old to answer it still answers `Ping`, so fall back rather
    // than calling a live daemon down during a partial update.
    let ingress_error = match client::send(&config.socket_path, &Request::DaemonInfo).await {
        Ok(Response::DaemonInfo {
            http_ingress_error, ..
        }) => Some(http_ingress_error),
        _ => None,
    };

    match ingress_error {
        Some(err) => {
            println!("{}", up_message(&config.socket_path, err.as_deref()));
            Ok(true)
        }
        None if is_up(config).await => {
            println!("{}", up_message(&config.socket_path, None));
            Ok(true)
        }
        None => {
            let legacy = legacy_daemon_socket(config, legacy_sock).await;
            let running = legacy.is_some();
            println!("{}", down_message(legacy.as_deref()));
            Ok(running)
        }
    }
}

/// Write and load an OS service so the daemon survives reboots.
pub fn install(config: &Config) -> Result<()> {
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
        let out = runner.run("tasklist", &["/FI", &filter, "/NH"])?;
        let alive = out.status.success() && String::from_utf8_lossy(&out.stdout).contains("dirad");
        return alive.then_some(pid);
    }

    let out = runner.run("kill", &["-0", &pid.to_string()])?;
    out.status.success().then_some(pid)
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
/// fallback (see [`Supervision::ScheduledTask`]): ask it to shut down
/// gracefully first — `Listener::bind`'s `first_pipe_instance` guard means a
/// still-live old instance would make a freshly launched one fail to bind the
/// pipe, the same way a live UDS bind already refuses a second `dirad` — then
/// relaunch via `schtasks /Run`, the same start-now step `install` uses.
async fn restart_scheduled_task(config: &Config, runner: &dyn Runner) -> Result<()> {
    let _ = client::send(&config.socket_path, &Request::Shutdown).await;
    still_up_after(config, STOP_GRACE_ATTEMPTS, STOP_GRACE_INTERVAL).await;

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
/// through [`graceful_then_force_windows`] in [`restart_bare`] instead — a
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
    if os == Os::Windows {
        graceful_then_force_windows(
            config,
            &pid.to_string(),
            runner,
            STOP_GRACE_ATTEMPTS,
            STOP_GRACE_INTERVAL,
        )
        .await;
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
    }

    impl FakeRunner {
        fn returning(mut self, prog: &str, args: &[&str], code: i32, stdout: &str) -> Self {
            self.responses
                .insert(key(prog, args), (code, stdout.to_string()));
            self
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

    // -- windows stop/restart escalation (`graceful_then_force_windows`) ----

    #[tokio::test]
    async fn graceful_then_force_windows_skips_taskkill_when_daemon_goes_down() {
        let config = test_config("win-stop-graceful");
        // Nothing is listening at `config.socket_path` — `is_up` reports down
        // on the very first poll, matching a dirad that honored the graceful
        // `Shutdown` request before this ever has to check a second time.
        let runner = FakeRunner::default();
        graceful_then_force_windows(
            &config,
            "4242",
            &runner,
            2,
            std::time::Duration::from_millis(5),
        )
        .await;
        assert!(!runner.called("taskkill", &["/PID", "4242", "/F"]));
    }

    #[tokio::test]
    async fn graceful_then_force_windows_escalates_to_taskkill_when_daemon_stays_up() {
        let config = test_config("win-stop-escalate");
        // A listener that answers every request with `Pong`, standing in for a
        // dirad that never acts on the graceful `Shutdown` request — the case
        // the taskkill escalation exists to handle. `attempts`/`interval` are
        // shrunk to a few milliseconds (rather than the real ~5s grace window)
        // so this test doesn't actually block on it.
        let mut listener = dira_ipc::Listener::bind(&config.socket_path).await.unwrap();
        tokio::spawn(async move {
            loop {
                let Ok(mut stream) = listener.accept().await else {
                    return;
                };
                if dira_ipc::read_frame(&mut stream).await.is_err() {
                    return;
                }
                let bytes = serde_json::to_vec(&Response::Pong).unwrap();
                let _ = dira_ipc::write_frame(&mut stream, &bytes).await;
            }
        });

        let runner = FakeRunner::default().returning("taskkill", &["/PID", "4242", "/F"], 0, "");
        graceful_then_force_windows(
            &config,
            "4242",
            &runner,
            2,
            std::time::Duration::from_millis(5),
        )
        .await;
        assert!(runner.called("taskkill", &["/PID", "4242", "/F"]));
    }

    #[tokio::test]
    async fn stop_with_windows_removes_pidfile_without_taskkill_when_shutdown_succeeds() {
        let config = test_config("win-stop-with");
        write_pidfile(&config, "4242");
        // Nothing listening → graceful attempt "succeeds" (there's nothing to
        // force-kill), so `taskkill` must not be invoked.
        let runner = FakeRunner::default();
        assert!(stop_with(&config, &runner, Os::Windows).await.is_ok());
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
