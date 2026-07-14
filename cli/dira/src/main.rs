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
#[cfg(test)]
mod test_support;
mod theme;
mod tui;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use dira_core::protocol::{ReportScope, Request, Response, StopSelector};
use dira_core::Config;
use std::io::Read;

/// Help styling — ANSI-16 only, deliberately: clap renders help before we can
/// probe `COLORTERM`, and theme.rs's own ANSI fallback maps Engaged→cyan and
/// Agent/Accent→magenta, so cyan literals + magenta headers ARE the sanctioned
/// degraded palette. clap (via anstream) auto-disables styling for non-TTY
/// output and honors NO_COLOR, so piped `--help` stays plain.
const HELP_STYLES: clap::builder::Styles = clap::builder::Styles::styled()
    .header(
        clap::builder::styling::AnsiColor::Magenta
            .on_default()
            .bold(),
    )
    .usage(
        clap::builder::styling::AnsiColor::Magenta
            .on_default()
            .bold(),
    )
    .literal(clap::builder::styling::AnsiColor::Cyan.on_default())
    .placeholder(clap::builder::styling::AnsiColor::BrightBlack.on_default());

/// `--version` output including the wire-schema version, so a bug report shows
/// which contract this build speaks without a second command. Returns
/// `&'static str` because clap's `Str` only accepts owned strings behind its
/// `string` feature; a process-lifetime `OnceLock` is the cheaper contract.
fn long_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        format!(
            "{}\nwire schema: {}",
            env!("CARGO_PKG_VERSION"),
            dira_contract::SCHEMA_VERSION
        )
    })
}

#[derive(Parser)]
#[command(
    name = "dira",
    version,
    long_version = long_version(),
    about = "AI-first time tracker — if you can clone it, you can bill it.",
    styles = HELP_STYLES,
    after_help = "\
Getting started:
  dira init             wire Claude Code hooks (also: codex, gemini, cursor, opencode)
  dira daemon start     start the resident tracker daemon
  dira status           today's summary — engaged, agent, compute, unbilled
  dira device link      link this device to the cloud for sync + billables

Run `dira help <command>` for details and examples of each command."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Today's summary: engaged / agent / compute + the unbilled value.
    #[command(
        long_about = "\
Today's summary, mirroring the cloud dashboard:

  ● engaged   de-duplicated human time — the billable base
  ◆ agent     agent wall-clock across sessions (evidence, never billed)
  ◇ compute   tokens through the pipe + a local ~$ cost estimate

The footer (`10.4h billable → €1,064 unbilled, this week`) is priced by the
cloud's billing policy and appears once this device is linked (`dira device
link`) and the daemon has fetched it; the compute estimate is always local.
Piped output is plain (no color) and 80-column stable.",
        after_help = "\
Examples:
  dira status               the summary block
  dira status --detailed    + parallel lanes, active sessions, today's rollup
  dira status | cat         plain, parseable output for scripts"
    )]
    Status {
        /// Also show the PARALLEL lanes, ACTIVE SESSIONS table, and TODAY report.
        #[arg(long, alias = "full")]
        detailed: bool,
    },
    /// Live auto-refreshing dashboard of the "Right Now" view (q/Esc to quit).
    #[command(
        alias = "top",
        long_about = "\
A full-screen live dashboard of the \"Right Now\" view: active sessions,
parallel lanes, today's rollup, compute, and the unbilled value. Timers
interpolate client-side between daemon polls, so seconds tick smoothly.
Quit with q, Esc, or Ctrl-C.",
        after_help = "\
Examples:
  dira watch                 refresh once a second
  dira watch --interval 250  smoother, chattier polling
  dira top                   same command, shorter to type"
    )]
    Watch {
        /// Refresh interval in milliseconds.
        #[arg(long, value_name = "MS", default_value_t = 1000)]
        interval: u64,
    },
    /// Open a manual session (meeting, manual testing, …). Several may run at once.
    #[command(
        long_about = "\
Open a manual session for work no agent harness observes — meetings, manual
testing, pairing, review calls. The session accrues engaged time until you
`dira stop` it. Several manual sessions may run at once; the accounting
de-duplicates overlapping human time, so parallel sessions never double-bill.
When --project is omitted, the project resolves from the current directory's
git repo.",
        after_help = "\
Examples:
  dira start --activity meeting --note \"sprint planning\"
  dira start --project github.com/acme/api --label deploy
  dira stop                  close it when you're done"
    )]
    Start {
        /// Repo/project to attribute the time to (default: resolved from cwd).
        #[arg(long, value_name = "REPO")]
        project: Option<String>,
        /// Operational tag, for selecting the session later (`dira stop --label`).
        #[arg(long, value_name = "TAG")]
        label: Option<String>,
        /// Activity classification, e.g. "meeting", "manual testing".
        #[arg(long, value_name = "KIND")]
        activity: Option<String>,
        /// Free-text description for the session.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
    /// Stop a manual session: by handle, by --label, or --all. Bare = the only one open.
    #[command(
        long_about = "\
Stop one or more manual sessions. Selector precedence:

  1. a handle argument       stop that session (`dira stop k3v9`)
  2. --label <TAG>           stop every session carrying the label
  3. --all                   stop every open manual session
  4. bare `dira stop`        the single open session; errors if several are open",
        after_help = "\
Examples:
  dira stop                  the only open manual session
  dira stop k3v9             by handle (shown by `dira start` / `dira sessions`)
  dira stop --label deploy   everything tagged #deploy
  dira stop --all            close every open manual session"
    )]
    Stop {
        /// Session handle from `start` (e.g. `k3v9`).
        #[arg(value_name = "HANDLE")]
        handle: Option<String>,
        /// Stop every manual session carrying this label.
        #[arg(long, value_name = "TAG", conflicts_with = "handle")]
        label: Option<String>,
        /// Stop every open manual session.
        #[arg(long, conflicts_with_all = ["handle", "label"])]
        all: bool,
    },
    /// List active + recent sessions.
    #[command(after_help = "\
Examples:
  dira sessions              handles, projects, human/agent time, state")]
    Sessions,
    /// Retroactive manual entry, e.g. `dira invoice 1h Meeting with Fol` (bare = minutes).
    #[command(
        alias = "invoice",
        long_about = "\
Record work after the fact — the invoice line you forgot to track live. The
duration comes first; any trailing words become the note (`--note` wins when
both are given).

Duration grammar: a bare integer is MINUTES (`dira log 45`); otherwise combine
h/m/s units — `1h30m`, `90m`, `2h`, `45s`.",
        after_help = "\
Examples:
  dira invoice 1h Meeting with Fol
  dira log 90 --project github.com/acme/api --activity \"code review\"
  dira log 2h30m --label onsite --note \"customer workshop\""
    )]
    Log {
        /// How long, e.g. `45` (minutes), `1h30m`, `90m`, `2h`.
        #[arg(value_name = "DURATION")]
        duration: String,
        /// Free-text comment — the trailing words after the duration.
        #[arg(value_name = "COMMENT")]
        comment: Vec<String>,
        /// Repo/project to attribute the time to (default: resolved from cwd).
        #[arg(long, value_name = "REPO")]
        project: Option<String>,
        /// Free-text description; overrides the trailing comment words.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Activity classification, e.g. "meeting", "code review".
        #[arg(long, value_name = "KIND")]
        activity: Option<String>,
        /// Operational tag for the entry.
        #[arg(long, value_name = "TAG")]
        label: Option<String>,
    },
    /// Local report straight from the on-device store.
    #[command(
        long_about = "\
A per-project report computed from the on-device event log — no cloud, no
network. Scopes are mutually exclusive; the default is --today. History past
the raw-event retention window survives as daily rollups, so --all stays
accurate after compaction.",
        after_help = "\
Examples:
  dira report                today (default)
  dira report --week         the last 7 days
  dira report --all          everything on this device
  dira report --project github.com/acme/api"
    )]
    Report {
        /// Today only (the default).
        #[arg(long, group = "scope")]
        today: bool,
        /// The last 7 days.
        #[arg(long, group = "scope")]
        week: bool,
        /// Everything on this device.
        #[arg(long, group = "scope")]
        all: bool,
        /// Only this repo/project (any scope).
        #[arg(long, value_name = "REPO")]
        project: Option<String>,
    },
    /// Wire a harness's hooks to report to the daemon (default: claude).
    #[command(
        long_about = "\
Wire an AI coding harness so its lifecycle hooks report to the daemon. Writes
the harness's settings file (or prints the snippet with --print). Run it once
per harness you use; `dira hook <harness>` is what the written hooks invoke.",
        after_help = "\
Examples:
  dira init                  Claude Code, current project
  dira init --global         Claude Code, all projects
  dira init codex --print    show what would be written, write nothing
  dira init opencode         also: gemini, cursor"
    )]
    Init {
        /// Harness to wire: `claude` (default), `codex`, `gemini`, `cursor`, or `opencode`.
        #[arg(value_name = "HARNESS")]
        harness: Option<String>,
        /// Write to the user-level settings instead of the project's.
        #[arg(long)]
        global: bool,
        /// Print the resulting settings/snippet without writing.
        #[arg(long)]
        print: bool,
    },
    /// Manage the resident daemon.
    #[command(
        long_about = "\
Control the resident `dirad` daemon — the single process that ingests hook
events, keeps the live registry, and syncs to the cloud. `install` registers
an OS service (launchd on macOS, systemd --user on Linux) so it survives
reboots; `start` is the ad-hoc alternative, tracked by a pidfile.",
        after_help = "\
Examples:
  dira daemon start          run it now
  dira daemon status         is it up?
  dira daemon install        start on login, restart on crash"
    )]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Inspect the effective config or persist overrides to the XDG config.toml.
    #[command(
        long_about = "\
Inspect the effective configuration (defaults → config.toml → DIRA_* env) or
persist an override. Only user-tunable knobs are settable — transport and
identity values (socket path, db path, http port) are derived and read-only.",
        after_help = config_cmd::knobs_after_help()
    )]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Link this device to the cloud, or show its link status.
    #[command(
        long_about = "\
Manage this device's cloud link. Linking claims a one-time code from the cloud
Connections page and binds this device's Ed25519 signing key to your account;
from then on the daemon signs and ships attestations, presence, and fetches
the billable summary `dira status` shows.",
        after_help = "\
Examples:
  dira device link --code ABCD-1234
  dira device status         linked? cloud URL? sync backlog?
  dira device rotate-key     new keypair, signed over by the old one
  dira device resync         re-send everything; dedup-safe on the cloud"
    )]
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Hook shim: read a harness hook on stdin and forward it to the daemon.
    #[command(long_about = "\
The shim `dira init` wires into each harness: it reads one hook payload from
stdin, forwards it to the daemon, and always exits 0 — a tracker must never
break an agent loop, so failures (daemon down, bad JSON) are silently dropped.
Not intended for interactive use.")]
    Hook {
        /// Harness name, e.g. `claude`.
        #[arg(value_name = "HARNESS")]
        harness: String,
    },
    /// Wipe ALL local events + token usage for a fresh start (keeps the device link).
    #[command(
        long_about = "\
Delete every locally-stored event and token row — the full statistics history
on this device. The device identity, signing key, cloud link, and config are
kept. Asks for confirmation unless --yes. Does NOT touch anything already
synced to the cloud.",
        after_help = "\
Examples:
  dira nuke                  asks before deleting
  dira nuke --yes            no prompt (scripts)"
    )]
    Nuke {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Print shell completions for the given shell.
    #[command(
        long_about = "\
Generate a completion script for your shell. Completions are derived from the
CLI definition itself, so they're always in sync with this binary — re-run
after upgrading dira to pick up new commands and flags.",
        after_help = "\
Install:
  bash        dira completions bash > ~/.local/share/bash-completion/completions/dira
  zsh         dira completions zsh > \"${fpath[1]}/_dira\"   # then restart or compinit
  fish        dira completions fish > ~/.config/fish/completions/dira.fish
  powershell  dira completions powershell | Out-String | Invoke-Expression
              # persist: add that line to $PROFILE
  elvish      dira completions elvish > ~/.config/elvish/lib/dira.elv
              # then `use dira` in rc.elv"
    )]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Show the CLI and running-daemon versions (and flag any skew).
    #[command(after_help = "\
Examples:
  dira version               CLI + daemon build, wire schema, uptime — and a
                             warning when the CLI and daemon builds differ")]
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
    /// Rewind the sync cursor and re-send events to the cloud (manual recovery).
    /// Safe — the cloud dedups, so a re-send never double-counts.
    Resync {
        /// Rewind to this event id instead of the beginning (full re-send default).
        #[arg(long)]
        from: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the effective resolved config, or just one key.
    #[command(after_help = "\
Examples:
  dira config get            every resolved key (defaults + file + env)
  dira config get idle_seconds")]
    Get {
        /// A single config key (e.g. `idle_seconds`); omit to print all.
        #[arg(value_name = "KEY")]
        key: Option<String>,
    },
    /// Persist a key to the XDG config.toml (created if absent; comments kept).
    #[command(after_help = config_cmd::knobs_after_help())]
    Set {
        /// The config key, e.g. `cloud_url` or `idle_seconds`.
        #[arg(value_name = "KEY")]
        key: String,
        /// The new value.
        #[arg(value_name = "VALUE")]
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
            let id = harness.as_deref().unwrap_or("claude");
            // Alias spelling lives in the sources crate so it can't drift from
            // what the hook dispatch accepts.
            return match dira_sources::canonical_harness_id(id) {
                Some("claude") => init::run(*global, *print),
                Some("codex") => init::run_codex(*print),
                Some("gemini") => init::run_gemini(*global, *print),
                Some("cursor") => init::run_cursor(*global, *print),
                Some("opencode") => init::run_opencode(&config, *print).await,
                _ => Err(anyhow::anyhow!(
                    "unknown harness '{id}' (expected: claude, codex, gemini, cursor, opencode)"
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
                DeviceAction::Resync { from } => device::resync(&config, from.clone()).await,
            };
        }
        _ => {}
    }

    // `status` renders with a client-side flag (summary vs detailed), so it
    // sends + renders here instead of the generic print path below.
    if let Command::Status { detailed } = &cli.command {
        let resp = client::send(&config.socket_path, &Request::Status).await?;
        match resp {
            Response::Status(s) => render::print_status(&s, *detailed),
            other => {
                if !render::print(&other) {
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }

    // Commands that talk to the daemon.
    let req = match cli.command {
        Command::Sessions => Request::Sessions,
        Command::Start {
            project,
            label,
            activity,
            note,
        } => Request::Start {
            project,
            label,
            activity,
            note,
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
            comment,
            project,
            note,
            activity,
            label,
        } => {
            let secs = duration::parse(&duration).map_err(|e| anyhow::anyhow!(e))?;
            // `--note` wins; otherwise the trailing positional words form the note.
            let note = note.or_else(|| {
                let joined = comment.join(" ");
                (!joined.trim().is_empty()).then_some(joined)
            });
            Request::Log {
                duration_secs: secs,
                project,
                note,
                activity,
                label,
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
        Command::Status { .. }
        | Command::Init { .. }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// clap's built-in consistency audit: conflicting args, duplicate names,
    /// broken groups — anything that would panic at runtime panics here instead.
    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Every subcommand ships a long help; the flagship ones ship examples.
    /// Rendering the help exercises the runtime-built attributes
    /// (`knobs_after_help`, `long_version`) so a drift panics a test, not a user.
    #[test]
    fn help_carries_examples_for_flagship_commands() {
        let mut cmd = Cli::command();
        for name in ["status", "log", "completions", "stop", "init", "device"] {
            let sub = cmd
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("subcommand {name} exists"))
                .clone();
            let help = sub.clone().render_long_help().to_string();
            assert!(
                help.contains("Examples:") || help.contains("Install:"),
                "{name} --help must carry an Examples/Install block:\n{help}"
            );
        }
    }

    /// `dira config set --help` lists every settable knob — generated from the
    /// same KNOBS table `set()` validates against, so they cannot drift.
    #[test]
    fn config_set_help_lists_every_knob() {
        let mut cmd = Cli::command();
        let config = cmd
            .find_subcommand_mut("config")
            .expect("config subcommand")
            .clone();
        let mut set = config
            .find_subcommand("set")
            .expect("config set subcommand")
            .clone();
        let help = set.render_long_help().to_string();
        for key in [
            "cloud_url",
            "idle_seconds",
            "heartbeat_active_secs",
            "heartbeat_idle_secs",
            "coalesce_seconds",
            "retention_days",
            "partial_rollup_after_secs",
            "report_local_day",
        ] {
            assert!(help.contains(key), "config set --help must list `{key}`");
        }
    }

    /// The long version mentions the wire schema, so `dira --version` identifies
    /// the contract a build speaks.
    #[test]
    fn long_version_carries_the_wire_schema() {
        assert!(long_version().contains(dira_contract::SCHEMA_VERSION));
    }
}
