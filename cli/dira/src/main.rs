//! `dira` — the thin CLI client. Talks to the resident `dirad` daemon over a
//! Unix domain socket; holds no state of its own.

mod client;
mod config_cmd;
mod daemon;
mod device;
mod duration;
mod format;
mod hook_health;
mod init;
mod render;
#[cfg(test)]
mod test_support;
mod theme;
mod tui;
mod update;
mod zavet_install;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use dira_core::protocol::{AnalyticsGrouping, ReportScope, Request, Response, StopSelector};
use dira_core::Config;
use std::io::Read;
use std::path::PathBuf;

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
  dira init             wire Claude Code hooks (also: codex, gemini, cursor, opencode, grok)
  dira daemon start     start the resident tracker daemon
  dira status           today's summary — engaged, agent, compute, unbilled
  dira device link      link this device to the cloud for sync + billables

Run `dira help <command>` for details and examples of each command."
)]
struct Cli {
    /// Print the daemon's raw JSON response instead of the human table.
    ///
    /// This is the machine-readable surface: the daemon's `Response` verbatim,
    /// one JSON object on stdout. GUIs and scripts consume it by running `dira`
    /// rather than reimplementing the control protocol — so socket discovery,
    /// the busy budget, and version-skew handling all stay in one place instead
    /// of being duplicated (and drifting) in every client.
    #[arg(long, global = true)]
    json: bool,

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
    /// Walk the work timeline: sessions clustered into work-units, newest first.
    #[command(
        long_about = "\
One page of the work timeline. Sessions on the same repo, branch and identity
are clustered into work-units when they happen within 4h of each other, so a
burst of short sessions reads as one piece of work instead of twenty rows.

Pages are keyset-paginated on session start: each page reports how many earlier
sessions remain, and `--before` walks back using the previous page's floor. How
far back you can walk is bounded by local retention — an unlinked daemon never
compacts, so an offline-only install keeps its whole history.",
        after_help = "\
Examples:
  dira timeline                              the last 7 days
  dira timeline --days 1                     today
  dira timeline --before 2026-01-05T00:00:00Z   the week before that"
    )]
    Timeline {
        /// Page width in days (default 7).
        #[arg(long, value_name = "N")]
        days: Option<i64>,
        /// Walk back: the previous page's floor, as RFC3339.
        #[arg(long, value_name = "RFC3339")]
        before: Option<String>,
    },
    /// Time + token-cost rollups over a window.
    #[command(
        long_about = "\
Bucket a window of work by day, project, model, or harness. Cost is ESTIMATED
from a pricing table bundled with the daemon — it is a useful signal, not an
invoice, and never leaves this machine.

Note that a model bucket carries tokens and cost but no time: a model does not
spend human minutes, only token turns, so time is reported under the other
groupings.",
        after_help = "\
Examples:
  dira analytics --days 7                    the last week, by day
  dira analytics --days 30 --by project      last 30 days, per repo
  dira analytics --days 7 --by model         what each model cost"
    )]
    Analytics {
        /// Window width in days, counting back from now (default 7).
        #[arg(long, value_name = "N")]
        days: Option<i64>,
        /// Bucket by: `day` (default), `project`, `model`, or `harness`.
        #[arg(long = "by", value_name = "FIELD")]
        by: Option<String>,
    },
    /// Per-project time rollups over a window.
    #[command(after_help = "\
Examples:
  dira projects              the last 7 days
  dira projects --days 30    the last 30 days")]
    Projects {
        /// Window width in days, counting back from now (default 7).
        #[arg(long, value_name = "N")]
        days: Option<i64>,
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
  dira init opencode         also: gemini, cursor, grok"
    )]
    Init {
        /// Harness to wire: `claude` (default), `codex`, `gemini`, `cursor`, `opencode`, or `grok`.
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
    /// Update dira to the latest release (resolve, verify, atomic swap, restart).
    #[command(
        long_about = "\
Resolve the current (or a chosen) release, download the platform artifact,
verify its sha256 against the published checksum, and atomically swap both
`dira` and `dirad` in place. Restarts the daemon afterward unless
--no-restart is given or the daemon wasn't running to begin with.

`--check` only resolves — it never downloads and never touches a binary —
and exits 0 in every non-error case, including offline, so it's safe to run
speculatively (it also refreshes the cache behind the passive update
notice). `--version` allows downgrading to any published release, not only
upgrading to a newer one.",
        after_help = "\
Examples:
  dira update --check               is a newer release available?
  dira update                       update to the latest release, restart the daemon
  dira update --channel prerelease  opt into a prerelease build
  dira update --version 0.2.0       pin to (or downgrade to) an exact version
  dira update --no-restart          swap the binaries, leave the running daemon alone"
    )]
    Update {
        /// Resolve only — report what's available, change nothing.
        #[arg(long)]
        check: bool,
        /// Update (or downgrade) to this exact version instead of the latest.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        /// Release channel to resolve against: stable (default) or prerelease.
        #[arg(long, value_name = "CHANNEL")]
        channel: Option<String>,
        /// Skip the dev-install guard (never bypasses sha256 verification).
        #[arg(long)]
        force: bool,
        /// Swap the binaries but leave a running daemon on the old version.
        #[arg(long)]
        no_restart: bool,
        /// Install directory for the new binaries (default: alongside the running `dira`).
        #[arg(long, value_name = "DIR", env = "DIRA_BIN_DIR")]
        bin_dir: Option<PathBuf>,
    },
    /// Zavet knowledge module: what the tracked time produced, and why.
    #[command(
        long_about = "\
Zavet is dira's knowledge sibling: repos that carry a .zavet/ directory (see
the zavet plugin, github.com/dodi-smart/dirahq-zavet) record decisions as
append-only markdown with guard globs, and commits carry Why:/Refs: trailers.
The daemon captures those alongside its ordinary git polling and correlates
them with sessions, so every decision has both recall and a time cost.
Activation: modules.zavet knob (auto = repos with .zavet/), overridable per
repo with enable/disable.",
        after_help = "\
Examples:
  dira zavet status          is zavet active here? capture health
  dira zavet why D-0042      the decision — and what it cost
  dira zavet wiki            decisions + living specs, staleness badges
  dira zavet decisions       every captured decision in this repo
  dira zavet enable          force-on for this repo (beats the global knob)"
    )]
    Zavet {
        #[command(subcommand)]
        action: ZavetAction,
    },
}

#[derive(Subcommand)]
enum ZavetAction {
    /// Activation + capture health for this repo.
    Status {
        /// Canonical repo (e.g. github.com/org/repo); default: resolve from cwd.
        #[arg(long)]
        project: Option<String>,
    },
    /// Answer "why?" — by decision id or plain question — with what it cost.
    #[command(after_help = "\
Examples:
  dira zavet why D-0042      one decision: record, guards, commits, time cost
  dira zavet why capture-pipeline
                             a living spec by slug: document, staleness, cost
  dira zavet why polling instead of a filesystem watcher
                             free text — searches decisions AND specs;
                             a confident match answers, several list matches
  dira zavet why D-0042 --project github.com/org/repo")]
    Why {
        /// A decision id (D-0042), a spec slug, or a plain-language question.
        #[arg(value_name = "QUESTION", required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Browse the knowledge base: overview, or search a topic.
    #[command(after_help = "\
Examples:
  dira zavet wiki            decisions, living specs (staleness + confidence),
                             recent knowledge
  dira zavet wiki polling    ranked matches with excerpts")]
    Wiki {
        /// Optional topic to search for.
        #[arg(value_name = "TOPIC", num_args = 0..)]
        topic: Vec<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// List the captured decisions for this repo.
    Decisions {
        #[arg(long)]
        project: Option<String>,
    },
    /// Force zavet ON for this repo (overrides the global modules.zavet knob).
    Enable {
        #[arg(long)]
        project: Option<String>,
    },
    /// Force zavet OFF for this repo.
    Disable {
        #[arg(long)]
        project: Option<String>,
    },
    /// Clear the per-repo override (fall back to the global knob).
    Reset {
        #[arg(long)]
        project: Option<String>,
    },
    /// Emit shim: read one guard-event JSON on stdin and forward it to the
    /// daemon (fire-and-forget; always exits 0). Wired by the zavet plugin.
    Emit,
    /// Install (or update) the zavet Claude Code plugin.
    #[command(
        long_about = "\
Installs the zavet Claude Code plugin by shelling out to the `claude` CLI —
never by hand-editing its config: `claude plugin marketplace add
dodi-smart/dirahq-zavet`, then `claude plugin install zavet@dirahq --scope
<scope>`. Detects the current state first (`claude plugin list --json`,
falling back to Claude Code's own installed_plugins.json) so a repeat run
is a clean no-op instead of a duplicate install. Reports the installed
version, scope, install path, and an advisory skew line comparing this dira
build against the plugin's declared minimum — advisory only, never a hard
error, since each product works fully without the other.",
        after_help = "\
Examples:
  dira zavet install                 install at user scope (the default)
  dira zavet install --scope project
  dira zavet install --update        already installed: refresh it
  dira zavet install --dry-run       print the exact `claude` invocations only"
    )]
    Install {
        /// Installation scope passed to `claude plugin install` (default: user).
        #[arg(long, default_value = "user", value_name = "SCOPE")]
        scope: String,
        /// Already installed: refresh the marketplace + plugin instead of a no-op.
        #[arg(long)]
        update: bool,
        /// Print the exact `claude` invocations without running them.
        #[arg(long)]
        dry_run: bool,
    },
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
    /// Show whether the daemon is up (exit 0 if any daemon is running — even
    /// a pre-upgrade one; 1 if none).
    Status,
    /// Install an OS service (launchd/systemd-user/scheduled task) so it survives reboots.
    Install,
    /// Remove the OS service `install` set up (binaries and data are untouched).
    Uninstall,
    /// Restart the daemon, however it's currently supervised.
    #[command(
        long_about = "\
Restart the daemon, working out how it's currently supervised (launchd,
systemd --user, or a bare pidfile-tracked process) and restarting it the
way that supervisor expects, then waiting for it to answer again and
reporting the version that comes back up. A daemon that wasn't running is
a no-op, not an error; a service-managed restart that fails prints the
exact manual command to run instead of pretending it succeeded.",
        after_help = "\
Examples:
  dira daemon restart        works for launchd, systemd --user, or a bare process"
    )]
    Restart,
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
                Some("grok") => init::run_grok(*global, *print),
                _ => Err(anyhow::anyhow!(
                    "unknown harness '{id}' (expected: claude, codex, gemini, cursor, opencode, grok)"
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
                DaemonAction::Status => {
                    let running = daemon::status(&config).await?;
                    print_supervision(&config).await;
                    hook_health::maybe_warn();
                    update::notice::maybe_print(&config);
                    if !running {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                DaemonAction::Install => daemon::install(&config),
                DaemonAction::Uninstall => daemon::uninstall(&config),
                DaemonAction::Restart => daemon::restart(&config).await,
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
        Command::Zavet {
            action: ZavetAction::Emit,
        } => return forward_zavet_event(&config).await,
        Command::Zavet {
            action:
                ZavetAction::Install {
                    scope,
                    update,
                    dry_run,
                },
        } => {
            return zavet_install::install(zavet_install::InstallArgs {
                scope: scope.clone(),
                update: *update,
                dry_run: *dry_run,
            });
        }
        Command::Nuke { yes } => return nuke(&config, *yes).await,
        Command::Version => {
            print_version(&config).await?;
            hook_health::maybe_warn();
            update::notice::maybe_print(&config);
            return Ok(());
        }
        Command::Update {
            check,
            version,
            channel,
            force,
            no_restart,
            bin_dir,
        } => {
            return update::run(
                &config,
                update::UpdateArgs {
                    check: *check,
                    version: version.clone(),
                    channel: channel.clone(),
                    force: *force,
                    no_restart: *no_restart,
                    bin_dir: bin_dir.clone(),
                },
            )
            .await;
        }
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
        // BEFORE the send, deliberately. The case this breadcrumb exists for is a
        // daemon that is running but refusing hooks — where `status` itself fails
        // with the same access-denied error, so anything printed after the send
        // never runs. Warning first means the one command a confused user reaches
        // for always says why capture is dead.
        hook_health::maybe_warn();
        let resp = client::send(&config.socket_path, &Request::Status).await?;
        match resp {
            Response::Status(s) => render::print_status(&s, *detailed),
            other => {
                if !render::print(&other) {
                    std::process::exit(1);
                }
            }
        }
        update::notice::maybe_print(&config);
        return Ok(());
    }

    // `dira zavet status` gets an extra client-side plugin summary line
    // after the daemon's own capture-health response — same detection path
    // as `dira zavet install`, read-only. Captured by reference before the
    // by-value match below moves `cli.command`.
    let is_zavet_status = matches!(
        &cli.command,
        Command::Zavet {
            action: ZavetAction::Status { .. }
        }
    );

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
        Command::Timeline { days, before } => Request::Timeline { before, days },
        Command::Analytics { days, by } => {
            let (from, to) = window_back(days.unwrap_or(7));
            let group_by = match by.as_deref() {
                Some("project") => AnalyticsGrouping::Project,
                Some("model") => AnalyticsGrouping::Model,
                Some("harness") => AnalyticsGrouping::Harness,
                // Anything else (including `day` and nothing) buckets by day. The
                // daemon is the one place that validates a window, so an unknown
                // `--by` degrades to the default rather than failing the call.
                _ => AnalyticsGrouping::Day,
            };
            Request::Analytics { from, to, group_by }
        }
        Command::Projects { days } => {
            let (from, to) = window_back(days.unwrap_or(7));
            Request::Projects { from, to }
        }
        Command::Zavet { action } => match action {
            ZavetAction::Status { project } => Request::ZavetStatus { cwd, repo: project },
            ZavetAction::Why { query, project } => Request::ZavetWhy {
                query: query.join(" "),
                cwd,
                repo: project,
            },
            ZavetAction::Wiki { topic, project } => Request::ZavetWiki {
                topic: Some(topic.join(" ")).filter(|t| !t.trim().is_empty()),
                cwd,
                repo: project,
            },
            ZavetAction::Decisions { project } => Request::ZavetDecisions { cwd, repo: project },
            ZavetAction::Enable { project } => Request::ZavetSetMode {
                cwd,
                repo: project,
                mode: "on".into(),
            },
            ZavetAction::Disable { project } => Request::ZavetSetMode {
                cwd,
                repo: project,
                mode: "off".into(),
            },
            ZavetAction::Reset { project } => Request::ZavetSetMode {
                cwd,
                repo: project,
                mode: "clear".into(),
            },
            // handled client-side above
            ZavetAction::Emit => unreachable!(),
            ZavetAction::Install { .. } => unreachable!(),
        },
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
        | Command::Version
        | Command::Update { .. } => unreachable!(),
    };

    let resp = client::send(&config.socket_path, &req).await?;
    if cli.json {
        return emit_json(&resp);
    }
    let ok = render::print(&resp);
    if is_zavet_status {
        if let Some(line) = zavet_install::status_line() {
            println!("{line}");
        }
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Total wall-clock a hook shim may spend talking to the daemon.
///
/// Must stay strictly greater than [`HOOK_CONNECT_BUDGET`], or the transport's
/// busy-retry loop is unreachable. It previously was: a 500 ms outer timeout
/// against a 2 s retry budget meant every `ERROR_PIPE_BUSY` on windows was
/// dropped rather than retried, and each of Claude Code's eight event types
/// spawns a fresh `dira.exe` contending for one pending pipe instance.
const HOOK_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// How long the connect itself may retry a busy endpoint — strictly inside
/// [`HOOK_TOTAL_BUDGET`]. Only the *busy* case waits: a daemon that is simply not
/// running answers `NotFound` immediately, so the "no daemon" path stays as fast
/// as it ever was.
const HOOK_CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

/// Forward a JSON payload from stdin to the daemon, wrapped into a request by
/// `wrap`.
///
/// Hook shims are fire-and-forget and must never break the agent loop: this still
/// exits 0 on every path and writes nothing to stdout. What changed is that a
/// *transport* failure is no longer invisible — it leaves a breadcrumb
/// (`hook_health`) that `dira status` surfaces, because "never tell the harness"
/// had been implemented as "never tell anyone", and a dead capture channel was
/// indistinguishable from a healthy one for days.
///
/// `DIRA_HOOK_DEBUG=1` additionally prints the failure to stderr, which harnesses
/// capture into their own logs — the switch that turns a support conversation
/// into one line.
async fn forward_stdin(
    config: &Config,
    label: &str,
    wrap: impl FnOnce(serde_json::Value) -> Request,
) -> Result<()> {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        note_hook_failure(label, &format!("could not read the hook payload: {e}"));
        return Ok(());
    }
    let payload: serde_json::Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(e) => {
            note_hook_failure(label, &format!("hook payload was not JSON: {e}"));
            return Ok(());
        }
    };
    let req = wrap(payload);
    match tokio::time::timeout(
        HOOK_TOTAL_BUDGET,
        client::send_with_budget(&config.socket_path, &req, HOOK_CONNECT_BUDGET),
    )
    .await
    {
        // A `Response::Error` is the daemon answering — an unknown harness or an
        // unaccounted event kind is a *semantic* non-result, not a transport
        // failure, and stays silent by design.
        Ok(Ok(_)) => hook_health::record_success(),
        Ok(Err(e)) => note_hook_failure(label, &e.to_string()),
        Err(_) => note_hook_failure(label, "timed out reaching dirad"),
    }
    Ok(())
}

/// Print a daemon response as one JSON object on stdout (`--json`).
///
/// Exits non-zero on `Response::Error` exactly as the human renderer does, so a
/// caller can branch on the exit status without parsing — but the error message
/// is still in the JSON, so it can also report it properly.
fn emit_json(resp: &Response) -> Result<()> {
    println!("{}", serde_json::to_string(resp)?);
    if matches!(resp, Response::Error { .. }) {
        std::process::exit(1);
    }
    Ok(())
}

/// A `[from, to)` window of `days` ending now, as RFC3339 — the shape the
/// `analytics`/`projects` requests take.
///
/// Deliberately a rolling window from this instant, not calendar-aligned: the
/// daemon owns the day boundary (it knows the reporting timezone) and applies it
/// when bucketing, so the CLI must not impose a second, possibly different one.
fn window_back(days: i64) -> (String, String) {
    let fmt = &time::format_description::well_known::Rfc3339;
    let to = time::OffsetDateTime::now_utc();
    let from = to - time::Duration::days(days.max(1));
    (
        from.format(fmt).unwrap_or_default(),
        to.format(fmt).unwrap_or_default(),
    )
}

fn note_hook_failure(label: &str, reason: &str) {
    hook_health::record_failure(label, reason);
    if std::env::var("DIRA_HOOK_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0") {
        eprintln!("dira hook {label}: {reason}");
    }
}

/// Forward a harness hook payload from stdin to the daemon.
async fn forward_hook(config: &Config, harness: &str) -> Result<()> {
    let owned = harness.to_string();
    forward_stdin(config, harness, move |payload| Request::IngestHook {
        harness: owned,
        payload,
    })
    .await
}

/// Forward a zavet guard event from stdin to the daemon.
async fn forward_zavet_event(config: &Config) -> Result<()> {
    forward_stdin(config, "zavet", |payload| Request::IngestZavet { payload }).await
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
            http_ingress_error,
            control_channel_warning,
        }) => {
            println!(
                "dirad   {version}  (schema {schema_version}, pid {pid}, up {})",
                format::hms(uptime_seconds as i64)
            );
            if let Some(reason) = http_ingress_error {
                println!("warning: daemon is DEGRADED — {reason}");
            }
            // Distinct from DEGRADED on purpose: an elevated-but-reachable daemon
            // captures fine, so it gets an advisory rather than the word reserved
            // for "captures nothing".
            if let Some(reason) = control_channel_warning {
                println!("note: {reason}");
            }
            if version != cli {
                println!(
                    "warning: CLI ({cli}) and daemon ({version}) differ — restart the daemon \
                     (`dira daemon stop && dira daemon start`) so they match"
                );
            }
        }
        Ok(_) => println!("dirad   (unexpected daemon response)"),
        Err(_) => println!(
            "{}",
            daemon::version_not_running_message(
                daemon::legacy_daemon_socket_default(config)
                    .await
                    .as_deref()
            )
        ),
    }
    Ok(())
}

/// Extra line for `dira daemon status`: which supervisor (if any) is keeping
/// the daemon alive — the same probe `dira daemon restart` uses internally to
/// pick a restart strategy, surfaced here so a user can tell why a bare `kill`
/// isn't enough before reaching for `restart`.
async fn print_supervision(config: &Config) {
    let label = match daemon::detect_supervision(config).await {
        daemon::Supervision::Launchd => "launchd".to_string(),
        daemon::Supervision::SystemdUser => "systemd --user".to_string(),
        daemon::Supervision::ScheduledTask => "scheduled task".to_string(),
        daemon::Supervision::Pidfile(pid) => format!("pidfile (pid {pid})"),
        daemon::Supervision::Socket(pid) => format!("unmanaged (pid {pid}, no pidfile)"),
        daemon::Supervision::LegacySocket { pid, sock } => format!(
            "pre-upgrade daemon on legacy socket {} (pid {})",
            sock.display(),
            pid.map_or("unknown".into(), |p| p.to_string())
        ),
        daemon::Supervision::NotRunning => return,
    };
    println!("supervised by: {label}");
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
        for name in [
            "status",
            "log",
            "completions",
            "stop",
            "init",
            "device",
            "zavet",
            "update",
        ] {
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
            "update.check",
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
