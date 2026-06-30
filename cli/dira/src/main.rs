//! `dira` — the thin CLI client. Talks to the resident `dirad` daemon over a
//! Unix domain socket; holds no state of its own.

mod client;
mod config_cmd;
mod daemon;
mod device;
mod duration;
mod format;
mod init;
mod render;
mod theme;
mod tui;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use dira_core::protocol::{ReportScope, Request, Response, StopSelector};
use dira_core::Config;
use std::io::Read;

#[derive(Parser)]
#[command(
    name = "dira",
    version,
    about = "AI-first time tracker — if you can clone it, you can bill it."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Active sessions + today's per-project human vs agent time.
    Status,
    /// Live auto-refreshing dashboard of the "Right Now" view (q/Esc to quit).
    #[command(alias = "top")]
    Watch {
        /// Refresh interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval: u64,
    },
    /// Open a manual session (meeting, manual testing, …). Several may run at once.
    Start {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        activity: Option<String>,
    },
    /// Stop a manual session: by handle, by --label, or --all. Bare = the only one open.
    Stop {
        /// Session handle from `start`.
        handle: Option<String>,
        #[arg(long, conflicts_with = "handle")]
        label: Option<String>,
        #[arg(long, conflicts_with_all = ["handle", "label"])]
        all: bool,
    },
    /// List active + recent sessions.
    Sessions,
    /// Retroactive manual entry, e.g. `dira log 45 --note "review"` (bare = minutes).
    Log {
        duration: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Local report straight from the on-device store.
    Report {
        #[arg(long, group = "scope")]
        today: bool,
        #[arg(long, group = "scope")]
        week: bool,
        #[arg(long, group = "scope")]
        all: bool,
        #[arg(long)]
        project: Option<String>,
    },
    /// Wire a harness's hooks to report to the daemon (default: claude).
    Init {
        /// Harness to wire: `claude` (default), `codex`, or `opencode`.
        harness: Option<String>,
        #[arg(long)]
        global: bool,
        /// Print the resulting settings/snippet without writing.
        #[arg(long)]
        print: bool,
    },
    /// Manage the resident daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Inspect the effective config or persist overrides to the XDG config.toml.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Link this device to the cloud, or show its link status.
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Hook shim: read a harness hook on stdin and forward it to the daemon.
    Hook {
        /// Harness name, e.g. `claude`.
        harness: String,
    },
    /// Wipe ALL local events + token usage for a fresh start (keeps the device link).
    Nuke {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Print shell completions for the given shell.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Show the CLI and running-daemon versions (and flag any skew).
    Version,
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Claim a link code and bind this device to the cloud.
    Link {
        /// The one-time code from the cloud Connections page (prompted if omitted).
        #[arg(long)]
        code: Option<String>,
        /// A human label for this device (defaults to the hostname).
        #[arg(long)]
        label: Option<String>,
    },
    /// Show whether this device is linked, the cloud URL, and the sync backlog.
    Status,
    /// Rotate this device's signing key (new keypair, signed by the old key).
    RotateKey,
    /// Locally unlink this device (clears the device id; keeps the signing key).
    Unlink {
        /// Skip the confirmation prompt even when events are unsynced.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the effective resolved config, or just one key.
    Get {
        /// A single config key (e.g. `idle_seconds`); omit to print all.
        key: Option<String>,
    },
    /// Persist a key to the XDG config.toml (created if absent; comments kept).
    Set {
        /// The config key, e.g. `cloud_url` or `idle_seconds`.
        key: String,
        /// The new value.
        value: String,
    },
    /// Print the path of the config.toml `set` writes to.
    Path,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (spawns dirad, tracked by a pidfile).
    Start,
    /// Stop the daemon.
    Stop,
    /// Show whether the daemon is up.
    Status,
    /// Install an OS service (launchd/systemd-user) so it survives reboots.
    Install,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());

    // Commands handled entirely client-side.
    match &cli.command {
        Command::Init {
            harness,
            global,
            print,
        } => {
            return match harness.as_deref().unwrap_or("claude") {
                "claude" | "claude_code" | "claudecode" => init::run(*global, *print),
                "codex" => init::run_codex(*print),
                "opencode" => init::run_opencode(&config, *print).await,
                other => Err(anyhow::anyhow!(
                    "unknown harness '{other}' (expected: claude, codex, opencode)"
                )),
            };
        }
        Command::Watch { interval } => {
            return tui::run(&config, std::time::Duration::from_millis(*interval)).await;
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, "dira", &mut std::io::stdout());
            return Ok(());
        }
        Command::Daemon { action } => {
            return match action {
                DaemonAction::Start => daemon::start(&config).await,
                DaemonAction::Stop => daemon::stop(&config).await,
                DaemonAction::Status => daemon::status(&config).await,
                DaemonAction::Install => daemon::install(&config),
            };
        }
        Command::Config { action } => {
            return match action {
                ConfigAction::Get { key } => config_cmd::get(&config, key.as_deref()),
                ConfigAction::Set { key, value } => config_cmd::set(&config, key, value),
                ConfigAction::Path => config_cmd::path(),
            };
        }
        Command::Hook { harness } => return forward_hook(&config, harness).await,
        Command::Nuke { yes } => return nuke(&config, *yes).await,
        Command::Version => return print_version(&config).await,
        Command::Device { action } => {
            return match action {
                DeviceAction::Link { code, label } => {
                    device::link(&config, code.clone(), label.clone()).await
                }
                DeviceAction::Status => device::status(&config).await,
                DeviceAction::RotateKey => device::rotate_key(&config).await,
                DeviceAction::Unlink { yes } => device::unlink(&config, *yes).await,
            };
        }
        _ => {}
    }

    // Commands that talk to the daemon.
    let req = match cli.command {
        Command::Status => Request::Status,
        Command::Sessions => Request::Sessions,
        Command::Start {
            project,
            label,
            activity,
        } => Request::Start {
            project,
            label,
            activity,
            cwd,
        },
        Command::Stop { handle, label, all } => Request::Stop {
            selector: if all {
                StopSelector::All
            } else if let Some(label) = label {
                StopSelector::Label { label }
            } else if let Some(handle) = handle {
                StopSelector::Handle { handle }
            } else {
                StopSelector::Auto
            },
        },
        Command::Log {
            duration,
            project,
            note,
        } => {
            let secs = duration::parse(&duration).map_err(|e| anyhow::anyhow!(e))?;
            Request::Log {
                duration_secs: secs,
                project,
                note,
                cwd,
            }
        }
        Command::Report {
            today,
            week,
            all,
            project,
        } => {
            let scope = if let Some(p) = project {
                ReportScope::Project { project: p }
            } else if week {
                ReportScope::Week
            } else if all {
                ReportScope::All
            } else {
                let _ = today;
                ReportScope::Today
            };
            Request::Report { scope }
        }
        // already handled above
        Command::Init { .. }
        | Command::Watch { .. }
        | Command::Daemon { .. }
        | Command::Device { .. }
        | Command::Config { .. }
        | Command::Hook { .. }
        | Command::Nuke { .. }
        | Command::Completions { .. }
        | Command::Version => unreachable!(),
    };

    let resp = client::send(&config.socket_path, &req).await?;
    let ok = render::print(&resp);
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Forward a hook payload from stdin to the daemon. Must never break the agent
/// loop: any failure (daemon down, bad JSON) exits 0 silently.
async fn forward_hook(config: &Config, harness: &str) -> Result<()> {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return Ok(());
    }
    let payload: serde_json::Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let req = Request::IngestHook {
        harness: harness.to_string(),
        payload,
    };
    // Bounded so a wedged daemon can't stall the agent.
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        client::send(&config.socket_path, &req),
    )
    .await;
    Ok(())
}

/// Print the CLI version (and wire schema), then best-effort query the running
/// daemon for its own version. Flags a CLI/daemon skew — common after upgrading
/// the binaries without restarting a long-lived daemon.
async fn print_version(config: &Config) -> Result<()> {
    let cli = env!("CARGO_PKG_VERSION");
    println!("dira    {cli}  (schema {})", dira_contract::SCHEMA_VERSION);

    match client::send(&config.socket_path, &Request::DaemonInfo).await {
        Ok(Response::DaemonInfo {
            version,
            schema_version,
            pid,
            uptime_seconds,
        }) => {
            println!(
                "dirad   {version}  (schema {schema_version}, pid {pid}, up {})",
                format::hms(uptime_seconds as i64)
            );
            if version != cli {
                println!(
                    "warning: CLI ({cli}) and daemon ({version}) differ — restart the daemon \
                     (`dira daemon stop && dira daemon start`) so they match"
                );
            }
        }
        Ok(_) => println!("dirad   (unexpected daemon response)"),
        Err(_) => println!("dirad   not running"),
    }
    Ok(())
}

/// Wipe all local statistics via the daemon (so its live-session registry is
/// cleared too). Confirms first unless `--yes`. Routing through the daemon — not
/// deleting the db file directly — is deliberate: an emptied db with a running
/// daemon would still show stale "active" sessions from in-memory state.
async fn nuke(config: &Config, yes: bool) -> Result<()> {
    let db = config.db_path.display();
    if !yes {
        println!("This permanently deletes ALL local events and token usage from {db}.");
        println!("Your device link is kept.");
        print!("Continue? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("aborted; nothing was deleted");
            return Ok(());
        }
    }

    match client::send(&config.socket_path, &Request::Nuke).await {
        Ok(resp) => {
            if !render::print(&resp) {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(_) => {
            // The daemon isn't reachable. Don't touch the db ourselves — point the
            // user at the two ways forward.
            eprintln!(
                "daemon not running — start it with `dira daemon start`, \
                 or delete the db file at {db}"
            );
            std::process::exit(1);
        }
    }
}
