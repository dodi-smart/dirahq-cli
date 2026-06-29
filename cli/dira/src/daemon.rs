//! Daemon lifecycle: start/stop/status plus OS-service install.
//!
//! For the dev/dogfood loop, `start` spawns `dirad` detached and tracks it with a
//! pidfile; `install` writes a launchd/systemd-user unit for persistence across
//! reboots.

use crate::client;
use anyhow::{Context, Result};
use dira_core::protocol::{Request, Response};
use dira_core::Config;
use std::path::PathBuf;
use std::process::Command;

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
