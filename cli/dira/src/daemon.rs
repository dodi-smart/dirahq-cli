//! Daemon lifecycle: start/stop/status plus OS-service install.
//!
//! For the dev/dogfood loop, `start` spawns `dirad` detached and tracks it with a
//! pidfile; `install` writes a launchd/systemd-user unit for persistence across
//! reboots. `restart` (below) builds on both: it works out *how* the daemon is
//! currently supervised, then restarts it the way that supervisor expects.

use crate::client;
use anyhow::{Context, Result};
use dira_core::protocol::{Request, Response};
use dira_core::Config;
use std::path::PathBuf;
use std::process::{Command, Output};

fn pidfile(config: &Config) -> PathBuf {
    config
        .socket_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"))
        .join("dirad.pid")
}

/// Locate the `dirad` binary: `DIRAD_BIN`, else a sibling of this exe, else PATH.
fn locate_dirad() -> PathBuf {
    if let Ok(p) = std::env::var("DIRAD_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("dirad");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("dirad")
}

/// Is the daemon answering on the socket?
pub async fn is_up(config: &Config) -> bool {
    matches!(
        client::send(&config.socket_path, &Request::Ping).await,
        Ok(Response::Pong)
    )
}

pub async fn start(config: &Config) -> Result<()> {
    if is_up(config).await {
        println!("dirad already running");
        return Ok(());
    }
    let bin = locate_dirad();
    println!("starting {} ...", bin.display());
    let mut child = Command::new(&bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", bin.display()))?;

    std::fs::write(pidfile(config), child.id().to_string()).ok();

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

pub async fn stop(config: &Config) -> Result<()> {
    let pf = pidfile(config);
    if let Ok(pid) = std::fs::read_to_string(&pf) {
        let pid = pid.trim();
        let _ = Command::new("kill").arg(pid).status();
        std::fs::remove_file(&pf).ok();
        std::fs::remove_file(&config.socket_path).ok();
        println!("stopped dirad (pid {pid})");
    } else {
        println!("no pidfile; daemon may be managed by launchd/systemd or not running");
    }
    Ok(())
}

pub async fn status(config: &Config) -> Result<()> {
    if is_up(config).await {
        println!("dirad: up  (socket {})", config.socket_path.display());
    } else {
        println!("dirad: down");
    }
    Ok(())
}

/// Write and load an OS service so the daemon survives reboots.
pub fn install(config: &Config) -> Result<()> {
    let bin = locate_dirad();
    let bin = std::fs::canonicalize(&bin).unwrap_or(bin);

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
    /// Neither service manager claims it, but `dirad.pid` next to the socket
    /// names a live process (`kill -0` succeeds).
    Pidfile(u32),
    /// No pidfile (or a stale one), but the daemon answers on the socket and
    /// self-reports its pid via `Response::DaemonInfo` — e.g. it was started
    /// by something other than `dira daemon start`, or the pidfile was lost.
    Socket(u32),
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
    Other,
}

fn current_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Macos
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        Os::Other
    }
}

/// A probe for an external command's presence/exit status/stdout —
/// `launchctl`, `systemctl`, `kill -0`. Behind a trait purely so
/// [`detect_supervision`]'s branches (and [`restart`]'s) are unit-testable
/// without actually shelling out or standing up a live daemon.
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

/// Is the pidfile's pid alive? `None` if there is no pidfile, it doesn't
/// parse, or the pid is dead.
fn alive_pidfile_pid(config: &Config, runner: &dyn Runner) -> Option<u32> {
    let contents = std::fs::read_to_string(pidfile(config)).ok()?;
    let pid: u32 = contents.trim().parse().ok()?;
    let out = runner.run("kill", &["-0", &pid.to_string()])?;
    out.status.success().then_some(pid)
}

/// Work out how the daemon is currently supervised. See [`Supervision`] for
/// the five possible outcomes and the order they're checked in.
pub async fn detect_supervision(config: &Config) -> Supervision {
    detect_supervision_with(config, &SystemRunner, current_os()).await
}

async fn detect_supervision_with(config: &Config, runner: &dyn Runner, os: Os) -> Supervision {
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

    if let Some(pid) = alive_pidfile_pid(config, runner) {
        return Supervision::Pidfile(pid);
    }

    if let Ok(Response::DaemonInfo { pid, .. }) =
        client::send(&config.socket_path, &Request::DaemonInfo).await
    {
        return Supervision::Socket(pid);
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

/// Kill a bare (non-service-managed) `dirad` we found by pidfile or by asking
/// the socket, then reuse [`start`] to bring it back — the plain dev/dogfood
/// path `install()` doesn't touch.
async fn restart_bare(config: &Config, pid: u32, runner: &dyn Runner) -> Result<()> {
    let _ = runner.run("kill", &[&pid.to_string()]);

    let mut still_up = true;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !is_up(config).await {
            still_up = false;
            break;
        }
    }
    if still_up {
        let _ = runner.run("kill", &["-9", &pid.to_string()]);
    }

    std::fs::remove_file(pidfile(config)).ok();
    std::fs::remove_file(&config.socket_path).ok();

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
    restart_with(config, &SystemRunner, current_os()).await
}

async fn restart_with(config: &Config, runner: &dyn Runner, os: Os) -> Result<()> {
    let supervision = detect_supervision_with(config, runner, os).await;
    println!("supervision: {supervision:?}");

    match &supervision {
        Supervision::Launchd => restart_launchd(runner)?,
        Supervision::SystemdUser => restart_systemd(runner)?,
        Supervision::Pidfile(pid) | Supervision::Socket(pid) => {
            restart_bare(config, *pid, runner).await?;
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
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// A scripted [`Runner`]: `key(prog, args) -> (exit_code, stdout)`. A
    /// missing key means the command wasn't stubbed and behaves like
    /// `Command::spawn` failing to find it — `run` returns `None`, exactly
    /// like a real `Runner::run` when `launchctl`/`systemctl` isn't installed.
    #[derive(Default)]
    struct FakeRunner {
        responses: HashMap<String, (i32, String)>,
    }

    impl FakeRunner {
        fn returning(mut self, prog: &str, args: &[&str], code: i32, stdout: &str) -> Self {
            self.responses
                .insert(key(prog, args), (code, stdout.to_string()));
            self
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
            let (code, stdout) = self.responses.get(&key(prog, args))?;
            Some(Output {
                status: ExitStatus::from_raw(*code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    /// A unique temp dir per test, mirroring `project.rs`'s `temp_repo_dir` —
    /// pid + a nanosecond suffix so parallel tests never collide.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "dira-sup-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn test_config(tag: &str) -> Config {
        Config {
            socket_path: temp_dir(tag).join("d.sock"),
            ..Config::default()
        }
    }

    /// Answer one `DaemonInfo` request on an accepted connection with a
    /// canned response carrying `pid` — a minimal stand-in for a running
    /// `dirad`, speaking the same length-prefixed-JSON framing as
    /// `client::send`, so `detect_supervision`'s socket fallback can be
    /// exercised without a real daemon.
    async fn respond_daemon_info(mut stream: tokio::net::UnixStream, pid: u32) {
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        if stream.read_exact(&mut buf).await.is_err() {
            return;
        }

        let resp = Response::DaemonInfo {
            version: "9.9.9-test".to_string(),
            schema_version: "1".to_string(),
            pid,
            uptime_seconds: 1,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let _ = stream.write_all(&(bytes.len() as u32).to_be_bytes()).await;
        let _ = stream.write_all(&bytes).await;
    }

    // -- the five `Supervision` branches -----------------------------------

    #[tokio::test]
    async fn detect_supervision_launchd_when_launchctl_list_succeeds() {
        let config = test_config("launchd");
        let runner =
            FakeRunner::default().returning("launchctl", &["list", "sh.dirahq.dirad"], 0, "");
        let got = detect_supervision_with(&config, &runner, Os::Macos).await;
        assert_eq!(got, Supervision::Launchd);
    }

    #[tokio::test]
    async fn detect_supervision_ignores_launchd_probe_off_macos() {
        // Same stub, but the host isn't macOS — the launchd probe must not
        // even run, so this falls through to NotRunning rather than lying.
        let config = test_config("launchd-off-platform");
        let runner =
            FakeRunner::default().returning("launchctl", &["list", "sh.dirahq.dirad"], 0, "");
        let got = detect_supervision_with(&config, &runner, Os::Linux).await;
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
        let got = detect_supervision_with(&config, &runner, Os::Linux).await;
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
        let got = detect_supervision_with(&config, &runner, Os::Linux).await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[tokio::test]
    async fn detect_supervision_pidfile_when_process_alive() {
        let config = test_config("pidfile");
        let pid = std::process::id();
        std::fs::write(pidfile(&config), pid.to_string()).unwrap();
        let runner = FakeRunner::default().returning("kill", &["-0", &pid.to_string()], 0, "");
        let got = detect_supervision_with(&config, &runner, Os::Other).await;
        assert_eq!(got, Supervision::Pidfile(pid));
    }

    #[tokio::test]
    async fn detect_supervision_pidfile_ignored_when_process_dead() {
        let config = test_config("pidfile-dead");
        std::fs::write(pidfile(&config), "999999").unwrap();
        let runner = FakeRunner::default().returning("kill", &["-0", "999999"], 1, "");
        let got = detect_supervision_with(&config, &runner, Os::Other).await;
        assert_eq!(got, Supervision::NotRunning);
    }

    #[tokio::test]
    async fn detect_supervision_socket_when_daemon_answers_without_pidfile() {
        let config = test_config("socket");
        let fake_pid = 424_242u32;
        let listener = UnixListener::bind(&config.socket_path).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                respond_daemon_info(stream, fake_pid).await;
            }
        });

        let runner = FakeRunner::default();
        let got = detect_supervision_with(&config, &runner, Os::Other).await;
        assert_eq!(got, Supervision::Socket(fake_pid));
    }

    #[tokio::test]
    async fn detect_supervision_not_running_when_nothing_answers() {
        let config = test_config("notrunning");
        let runner = FakeRunner::default();
        let got = detect_supervision_with(&config, &runner, Os::Other).await;
        assert_eq!(got, Supervision::NotRunning);
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
}
