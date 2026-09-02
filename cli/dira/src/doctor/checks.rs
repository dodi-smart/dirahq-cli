//! The checks themselves: pure judges over already-gathered facts.
//!
//! Every function here is `fn`, not `async fn`, and touches no IO. That is
//! deliberate — the interesting part of a diagnostic is its *judgement*, and
//! judgement that needs a live daemon to test is judgement nobody tests. The
//! `Reach::Denied` arm in particular is unreachable from CI on every platform
//! we build on, and it is the arm the whole command exists for.

use super::{Check, Facts};
use crate::client::Reach;
use crate::daemon::{Info, Supervision};
use crate::device::DeviceProbe;
use crate::hook_health::Health;
use serde_json::json;
use std::path::Path;

/// How one harness config file is wired, as read off disk.
pub(crate) struct HarnessWiring {
    pub harness: &'static str,
    pub scope: &'static str,
    pub path: String,
    pub expected: usize,
    pub missing: Vec<String>,
    /// Every dira-looking command string in the file, including ones pointing
    /// at a different binary — but NOT a portable wrapper. Filled by
    /// `init::dira_hook_commands`, which matches the literal substring
    /// `" hook "`; a portable command like `sh .dira/hook.sh claude` has no
    /// space before `hook.sh` and never matches, so a project entry that is
    /// purely the portable wrapper reads as `commands: vec![]` here, not as a
    /// command that then gets filtered out as portable.
    pub commands: Vec<String>,
}

impl HarnessWiring {
    fn wired(&self) -> usize {
        self.expected - self.missing.len()
    }
    fn label(&self) -> String {
        format!("{} ({})", self.harness, self.path)
    }
}

/// Read every harness config `dira init` can write. Files that do not exist
/// are absent from the result — "not configured" is a different verdict from
/// "configured wrong", and only the caller can weigh them.
pub(crate) fn read_harness_wiring() -> Vec<HarnessWiring> {
    crate::init::harness_config_paths()
        .into_iter()
        .filter_map(|h| {
            let raw = std::fs::read_to_string(&h.path).ok()?;
            // An unparseable config is still a config: report it as fully
            // unwired rather than skipping it, or a corrupt settings.json
            // reads as "no hooks here" and the user never learns why.
            let settings: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
            Some(HarnessWiring {
                harness: h.harness,
                scope: h.scope,
                path: h.path.display().to_string(),
                expected: h.events.len(),
                missing: crate::init::missing_hooks(&settings, &h.events, h.harness),
                commands: crate::init::dira_hook_commands(&settings),
            })
        })
        .collect()
}

/// Can we talk to the daemon at all?
///
/// The `Denied` arm is the reason `dira doctor` exists. A daemon started from
/// an elevated shell carries that token's DACL on its control channel and
/// refuses every ordinary-token hook — while answering nothing differently to
/// any other check. It must never be answered with `dira daemon start`:
/// a daemon IS running, and starting a second one overwrites its pidfile and
/// makes the situation strictly worse (D-0016).
pub(crate) fn reachable(f: &Facts) -> Check {
    const ID: &str = "daemon.reachable";
    let sock = &f.daemon_socket;
    match f.daemon.reach {
        Reach::Up => Check::ok(ID, format!("daemon answering on {sock}"))
            .detail(json!({ "reach": "up", "socket": sock })),
        Reach::Denied => Check::fail(
            ID,
            format!(
                "a daemon is listening on {sock} but refused this client \
                 (access denied) — every hook is being dropped the same way"
            ),
        )
        .remedy(dira_ipc::elevation::access_denied_advice(f.doctor_elevated))
        .detail(json!({
            "reach": "denied",
            "socket": sock,
            "client_elevated": f.doctor_elevated,
        })),
        Reach::Busy => Check::warn(ID, "every daemon channel instance was momentarily taken")
            .remedy("re-run `dira doctor`; if it persists, `dira daemon restart`")
            .detail(json!({ "reach": "busy", "socket": sock })),
        Reach::Down | Reach::Other => match &f.daemon.legacy {
            Some(legacy) => Check::warn(
                ID,
                format!(
                    "nothing on {sock} — but a pre-upgrade daemon is answering on {}",
                    legacy.display()
                ),
            )
            .remedy("dira daemon restart")
            .detail(
                json!({ "reach": "down", "socket": sock, "legacy": legacy.display().to_string() }),
            ),
            None => Check::fail(ID, format!("nothing is listening on {sock}"))
                .remedy("dira daemon start")
                .detail(json!({ "reach": "down", "socket": sock })),
        },
    }
}

/// Will capture survive a reboot?
pub(crate) fn supervision(f: &Facts) -> Check {
    const ID: &str = "daemon.supervision";
    let Some(label) = crate::daemon::supervision_label(&f.supervision) else {
        // `daemon.reachable` already said this in full; a second red line only
        // dilutes it.
        return Check::skip(ID, "nothing is running");
    };
    let detail = json!({ "supervision": label });
    match &f.supervision {
        Supervision::Launchd | Supervision::SystemdUser | Supervision::ScheduledTask => {
            Check::ok(ID, label).detail(detail)
        }
        Supervision::Pidfile(_) | Supervision::Socket(_) => Check::warn(
            ID,
            format!("{label} — nothing will restart it after a reboot"),
        )
        .remedy("dira daemon install")
        .detail(detail),
        Supervision::LegacySocket { .. } => Check::warn(ID, label)
            .remedy("dira daemon restart")
            .detail(detail),
        Supervision::NotRunning => unreachable!("labelled above"),
    }
}

/// Do the CLI and the daemon agree on version and wire schema?
///
/// A warning, never a failure: a skewed daemon still captures.
pub(crate) fn version_skew(i: &Info) -> Check {
    const ID: &str = "daemon.version_skew";
    let cli = env!("CARGO_PKG_VERSION");
    let cli_schema = dira_contract::SCHEMA_VERSION;
    let detail = json!({
        "cli": cli, "daemon": i.version,
        "cli_schema": cli_schema, "daemon_schema": i.schema_version,
    });
    let remedy = "dira daemon stop && dira daemon start";
    // Schema first: a wire mismatch is the more consequential of the two, and
    // saying "0.3.0 vs 0.3.0" while the schemas differ would be baffling.
    if i.schema_version != cli_schema {
        return Check::warn(
            ID,
            format!(
                "wire schema differs — this CLI speaks {cli_schema}, the daemon speaks {}",
                i.schema_version
            ),
        )
        .remedy(remedy)
        .detail(detail);
    }
    if i.version != cli {
        Check::warn(
            ID,
            format!(
                "dira is {cli} but the daemon is {} — an older daemon is capturing",
                i.version
            ),
        )
        .remedy(remedy)
        .detail(detail)
    } else {
        Check::ok(
            ID,
            format!("dira and dirad are both {cli} (schema {cli_schema})"),
        )
        .detail(detail)
    }
}

/// Did the daemon's hook ingress actually bind?
///
/// A failure, not a warning: a daemon whose ingress did not bind answers every
/// control request and captures nothing, which is precisely the state D-0009
/// decided must not read as healthy.
pub(crate) fn ingress(i: &Info) -> Check {
    const ID: &str = "daemon.ingress";
    match &i.http_ingress_error {
        None => Check::ok(ID, "hook ingress is listening"),
        Some(reason) => Check::fail(ID, format!("hook ingress is not listening — {reason}"))
            .remedy(
                "free the port (or pick another with `dira config set http_port <n>`), \
                 then `dira daemon restart`",
            )
            .detail(json!({ "error": reason })),
    }
}

/// The daemon's own report on its control channel.
///
/// On Windows this is where the named-pipe descriptor ladder surfaces — often
/// the single most diagnostic line available. A warning rather than a failure:
/// if we are reading this, the daemon answered us, and a hard failure is
/// `daemon.reachable`'s job.
pub(crate) fn control_channel(i: &Info) -> Check {
    const ID: &str = "daemon.control_channel";
    match &i.control_channel_warning {
        None => Check::ok(ID, "control channel is in its intended state"),
        Some(reason) => Check::warn(ID, reason.clone())
            .remedy(
                "restart the daemon from an ordinary (non-elevated) shell so its \
                 control channel carries your own token",
            )
            .detail(json!({ "warning": reason })),
    }
}

/// Does the local store open and accept a write?
pub(crate) fn store_writable(f: &Facts) -> Check {
    const ID: &str = "store.writable";
    let db = f.db_path.display().to_string();
    match &f.store_write {
        Err(e) => Check::fail(ID, format!("{db} is not usable — {e}"))
            .remedy(
                "check ownership of the directory it lives in; a daemon started \
                 elevated writes a store you cannot read",
            )
            .detail(json!({ "db_path": db, "error": e })),
        Ok(()) => match f
            .daemon
            .info
            .as_ref()
            .and_then(|i| i.storage_warning.as_ref())
        {
            Some(w) => Check::warn(ID, format!("{db} opens, but the daemon reports: {w}"))
                .detail(json!({ "db_path": db, "storage_warning": w })),
            None => Check::ok(ID, format!("{db} opens and accepts writes"))
                .detail(json!({ "db_path": db })),
        },
    }
}

/// Are the CLI and the daemon reading the same store?
///
/// A failure: divergence means the daemon is writing a history the user is not
/// reading, and nothing about the machine is working the way they believe.
pub(crate) fn store_divergence(cli_db: &Path, daemon_db: Option<&str>) -> Check {
    const ID: &str = "store.divergence";
    // An unknown is not a divergence — see `store_divergence_line`.
    let Some(daemon_db) = daemon_db else {
        return Check::skip(ID, "the daemon is too old to report its store path");
    };
    match crate::daemon::store_divergence_line(cli_db, Some(daemon_db)) {
        None => Check::ok(
            ID,
            format!("the CLI and the daemon share {}", cli_db.display()),
        )
        .detail(json!({ "db_path": daemon_db })),
        Some(line) => Check::fail(ID, "the CLI and the daemon are using different stores")
            .remedy(format!(
                "{line}\nstop the daemon from the shell that started it, then \
                 `dira daemon start` as your own user"
            ))
            .detail(json!({ "cli_db": cli_db.display().to_string(), "daemon_db": daemon_db })),
    }
}

const WAL_WARN_BYTES: u64 = 64 * 1024 * 1024;
const WAL_FAIL_BYTES: u64 = 512 * 1024 * 1024;

/// Is the write-ahead log checkpointing?
///
/// `None` (no `-wal` beside the db) is fine — it only means no process
/// currently holds the database open.
pub(crate) fn wal_size(bytes: Option<u64>) -> Check {
    const ID: &str = "store.wal_size";
    let Some(bytes) = bytes else {
        return Check::ok(ID, "no write-ahead log (nothing has the store open)");
    };
    let mib = bytes / (1024 * 1024);
    let detail = json!({ "bytes": bytes });
    if bytes >= WAL_FAIL_BYTES {
        Check::fail(
            ID,
            format!("the write-ahead log is {mib} MiB — checkpoints are not completing"),
        )
        .remedy("dira daemon restart — if it comes back, file an issue with `dira doctor --json`")
        .detail(detail)
    } else if bytes >= WAL_WARN_BYTES {
        Check::warn(ID, format!("the write-ahead log is {mib} MiB and growing"))
            .remedy("dira daemon restart")
            .detail(detail)
    } else {
        Check::ok(ID, format!("write-ahead log is {mib} MiB")).detail(detail)
    }
}

/// Is any harness wired to report to us?
///
/// Never a failure. A machine with no hooks is a legitimate not-yet-set-up
/// state, and the exit code has to distinguish that from configured-and-broken
/// or `install.sh` cannot use it.
pub(crate) fn hooks_config(wiring: &[HarnessWiring]) -> Check {
    const ID: &str = "hooks.config";
    let detail = json!(wiring
        .iter()
        .map(|w| json!({
            "harness": w.harness, "scope": w.scope, "path": w.path,
            "expected": w.expected, "wired": w.wired(), "missing": w.missing,
        }))
        .collect::<Vec<_>>());

    if wiring.is_empty() {
        return Check::warn(
            ID,
            "no harness hooks are configured — nothing is capturing agent activity",
        )
        // A machine with nothing wired is a machine that hasn't been set up,
        // so the remedy is the setup command rather than the six-way `dira
        // init` menu it used to print — that menu asked the user to already
        // know which harnesses they run, which is exactly what `onboard`
        // detects for them. Still a prescription, never an action: doctor
        // prints this and stops (DIRASH-0022). The partial-wiring remedy
        // below keeps naming the specific harness, because there the answer
        // is already known.
        .remedy("dira onboard");
    }

    let partial: Vec<&HarnessWiring> = wiring
        .iter()
        .filter(|w| !w.missing.is_empty() && w.wired() > 0)
        .collect();
    let complete: Vec<&HarnessWiring> = wiring.iter().filter(|w| w.missing.is_empty()).collect();

    if let Some(w) = partial.first() {
        return Check::warn(
            ID,
            format!(
                "{}: {}/{} events wired — missing {}",
                w.label(),
                w.wired(),
                w.expected,
                w.missing.join(", ")
            ),
        )
        .remedy(format!("dira init {}", w.harness))
        .detail(detail);
    }
    if complete.is_empty() {
        return Check::warn(
            ID,
            "harness configs exist but none of them invoke dira".to_string(),
        )
        .remedy("dira init")
        .detail(detail);
    }
    Check::ok(
        ID,
        complete
            .iter()
            .map(|w| format!("{}: {}/{} events wired", w.label(), w.wired(), w.expected))
            .collect::<Vec<_>>()
            .join("; "),
    )
    .detail(detail)
}

/// Do the configured hook commands point at a binary that still exists?
///
/// A missing binary is a failure: it is total capture loss, and it is the
/// classic breakage after reinstalling or moving `dira` without re-running
/// `init`. A binary that exists but is not *this* one is a warning — an older
/// dira is capturing, which works but is not what the user thinks.
/// What we could establish about one configured executable path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExePath {
    Exists(String),
    Missing(String),
    /// Contains a shell variable we cannot expand ourselves. The harness runs
    /// hook commands through a shell, so `$HOME/.local/bin/dira` is a
    /// perfectly working config — calling it missing would be a false alarm on
    /// a healthy machine, which is worse than saying nothing.
    Unverifiable(String),
}

/// Expand the two references we can resolve without a shell: a leading `~` and
/// `$HOME`/`${HOME}`.
///
/// Not shell expansion — no word splitting, no globbing, no command
/// substitution. Just the one variable every harness expands and that appears
/// in real hook configs often enough that refusing to understand it would make
/// both the path check and the capture probe useless on ordinary machines.
pub(crate) fn expand_home(s: &str) -> String {
    let Ok(home) = dira_core::config::home_dir() else {
        return s.to_string();
    };
    let home = home.display().to_string();
    let expanded = match s.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => s.to_string(),
    };
    expanded.replace("${HOME}", &home).replace("$HOME", &home)
}

/// Resolve a configured executable path far enough to test it.
///
/// Gives up honestly on anything [`expand_home`] cannot resolve.
pub(crate) fn resolve_exe(exe: &str) -> ExePath {
    let path = expand_home(exe);
    if path.contains('$') || (path.contains('%') && cfg!(windows)) {
        return ExePath::Unverifiable(exe.to_string());
    }
    if Path::new(&path).exists() {
        ExePath::Exists(path)
    } else {
        ExePath::Missing(path)
    }
}

pub(crate) fn hooks_exe_path(wiring: &[HarnessWiring], current_exe: Option<&str>) -> Check {
    const ID: &str = "hooks.exe_path";
    let commands: Vec<&String> = wiring.iter().flat_map(|w| w.commands.iter()).collect();
    if commands.is_empty() {
        return Check::skip(ID, "no harness hooks are configured");
    }

    let mut exes: Vec<String> = Vec::new();
    for c in &commands {
        if let Some(exe) = crate::init::hook_command_exe(c) {
            if !exes.contains(&exe) {
                exes.push(exe);
            }
        }
    }
    let (mut missing, mut unverifiable, mut existing) = (Vec::new(), Vec::new(), Vec::new());
    for exe in &exes {
        match resolve_exe(exe) {
            ExePath::Missing(p) => missing.push(p),
            ExePath::Unverifiable(p) => unverifiable.push(p),
            ExePath::Exists(p) => existing.push(p),
        }
    }
    let detail = json!({ "executables": exes, "current_exe": current_exe });

    if !missing.is_empty() {
        return Check::fail(
            ID,
            format!(
                "the hook config invokes a binary that does not exist: {}",
                missing.join(", ")
            ),
        )
        .remedy("dira init  — rewrites the hook command with this binary's path")
        .detail(detail);
    }
    if !unverifiable.is_empty() {
        return Check::ok(
            ID,
            format!(
                "hooks invoke {} — the harness expands the shell variable, so this \
                 can't be checked here",
                unverifiable.join(", ")
            ),
        )
        .detail(detail);
    }
    match current_exe {
        Some(current) if existing.iter().any(|e| *e != current) => Check::warn(
            ID,
            format!(
                "hooks invoke {}, but this is {current} — a different binary is capturing",
                existing.join(", ")
            ),
        )
        .remedy("dira init")
        .detail(detail),
        _ => Check::ok(
            ID,
            format!("all hook entries invoke {}", existing.join(", ")),
        )
        .detail(detail),
    }
}

/// For a harness wired at both project and global scope, does the project
/// entry double-deliver, or does it yield?
///
/// `dira cloud init` writes project-scope entries as a portable wrapper
/// (`.dira/hook.sh`) that exports `DIRA_HOOK_VIA=portable` before forwarding;
/// `dira hook` reads that marker and, when the *same event* for the *same
/// harness* is also wired at user scope with a resolvable executable, exits 0
/// without forwarding a second time. So a wrapper entry overlapping global
/// scope is not double delivery — it is exactly the design working. A
/// machine-specific project entry (an absolute path, or one `dira init`
/// wrote directly rather than the wrapper) has no such yield check, and the
/// same event really does fire from both scopes.
///
/// Never a failure: the worst case here is one hook running twice, not lost
/// capture. A harness wired at only one scope has nothing to overlap, so it
/// is absent from the verdict rather than counted as "fine" — per
/// DIRASH-0022, absent evidence is a skip.
///
/// Judges the same `Vec<HarnessWiring>` facts `hooks.config`/`hooks.exe_path`
/// read off disk; a scope counts as wired for a harness when that scope's
/// file wires at least one of its events (`HarnessWiring::wired() > 0`).
pub(crate) fn scope_overlap(wiring: &[HarnessWiring]) -> Check {
    const ID: &str = "hooks.scope_overlap";

    let mut harnesses: Vec<&'static str> = wiring.iter().map(|w| w.harness).collect();
    harnesses.sort_unstable();
    harnesses.dedup();

    let mut duplicating: Option<(&HarnessWiring, Vec<&str>)> = None;
    let mut yielding: Vec<&'static str> = Vec::new();

    for harness in harnesses {
        let project = wiring
            .iter()
            .find(|w| w.harness == harness && w.scope == "project" && w.wired() > 0);
        let global_wired = wiring
            .iter()
            .any(|w| w.harness == harness && w.scope == "global" && w.wired() > 0);
        let Some(project) = project else { continue };
        if !global_wired {
            continue;
        }
        let non_portable: Vec<&str> = project
            .commands
            .iter()
            .filter(|c| !crate::init::command_is_portable_wrapper(c, harness))
            .map(|s| s.as_str())
            .collect();
        if non_portable.is_empty() {
            yielding.push(harness);
        } else if duplicating.is_none() {
            duplicating = Some((project, non_portable));
        }
    }

    if let Some((w, cmds)) = duplicating {
        return Check::warn(
            ID,
            format!(
                "{} is wired at both project and global scope with a machine-specific \
                 command ({}) — the event runs twice on this machine",
                w.label(),
                cmds.join(", ")
            ),
        )
        .remedy("dira cloud init  — rewrites the project entry as the portable wrapper, which yields to global-scope wiring")
        .detail(json!({ "harness": w.harness, "path": w.path, "commands": cmds }));
    }
    if !yielding.is_empty() {
        return Check::ok(
            ID,
            format!(
                "{} wired at both scopes via the portable wrapper — it yields to \
                 global-scope wiring, so the event runs once",
                yielding.join(", ")
            ),
        )
        .detail(json!({ "harnesses": yielding }));
    }
    Check::skip(ID, "no harness is wired at both project and global scope")
}

/// Did a hook recently fail to reach the daemon?
///
/// A failure. The breadcrumb's own wording is "captured activity is being lost
/// right now" — that is not a warning.
pub(crate) fn breadcrumb(h: Option<&Health>) -> Check {
    const ID: &str = "hook.breadcrumb";
    let Some(h) = h else {
        return Check::ok(ID, "no failed hook deliveries recorded");
    };
    Check::fail(
        ID,
        format!(
            "{} {} hook(s) could not reach dirad — captured activity is being lost",
            h.consecutive, h.harness
        ),
    )
    .remedy(h.last_error.clone())
    .detail(json!({
        "harness": h.harness,
        "consecutive": h.consecutive,
        "last_error": h.last_error,
        "last_error_at": h.last_error_at,
    }))
}

/// Does the invoking directory resolve to a project ref?
///
/// Capture works regardless — this is about ATTRIBUTION. Work with no project
/// ref ships `repoCanonical: null`, and the cloud can never anchor it: no repo
/// means no commit to match, so no sweep will ever promote it and it stays
/// unbillable unless a human signs it off row by row. That was invisible before
/// this check: a session with no project still resolves its BRANCH (that only
/// needs `rev-parse`), so it arrives looking well-captured (#111).
///
/// Never a failure, and deliberately so. A directory that is not a git repo, or
/// a local-only repo with no remote, is a legitimate place to work — dira
/// anchors against commits confirmed in a remote, so there is genuinely nothing
/// to anchor to. The verdicts split on whether anything is actionable:
///
/// - not a repo — skip; there is no misconfiguration to report.
/// - no usable remote — warn; it may be intentional, but this is the silent
///   attribution loss, and only the user can say which.
pub(crate) fn project_resolves(f: &Facts) -> Check {
    const ID: &str = "project.resolves";
    use dira_core::project::ProjectMiss;

    let where_ = f.cwd.clone().unwrap_or_else(|| "this directory".into());
    match &f.project {
        Ok(project) => Check::ok(ID, format!("{where_} resolves to {project}"))
            .detail(json!({ "cwd": f.cwd, "project": project })),
        Err(ProjectMiss::NotAGitRepo) => {
            Check::skip(ID, format!("{where_} is not a git repository"))
                .detail(json!({ "cwd": f.cwd, "reason": "not_a_git_repo" }))
        }
        Err(miss) => {
            let remedy = match miss {
                ProjectMiss::NoRemotes => "add a remote (`git remote add origin <url>`) if this work should be anchored and billable. A local-only repo has nothing to anchor against, so leaving it is a valid choice — its time will show as `Unattributed · no repo`.".to_string(),
                ProjectMiss::AmbiguousRemotes(rs) => format!(
                    "name one of them `origin`, or set an upstream for this branch (`git branch --set-upstream-to <remote>/<branch>`), so the project ref is not a guess between {}.",
                    rs.join(", ")
                ),
                ProjectMiss::UnparseableRemote { remote, url } => format!(
                    "remote `{remote}` is `{url}`, which has no host and owner/name to canonicalize. Point it at an http(s) or ssh URL if this work should be attributed."
                ),
                ProjectMiss::NotAGitRepo => unreachable!("handled above"),
            };
            Check::warn(
                ID,
                format!("{where_} has no project ref — {miss}; work here is captured but cannot be anchored"),
            )
            .remedy(remedy)
            .detail(json!({ "cwd": f.cwd, "reason": miss.to_string() }))
        }
    }
}

/// Is the daemon's own verdict on sync healthy?
///
/// Never a failure: a stalled sync loses nothing locally, it only delays the
/// cloud. The remedy is chosen by the error kind, because "resync" is useless
/// advice for a rejected signature.
pub(crate) fn sync_health(d: &DeviceProbe) -> Check {
    const ID: &str = "sync.health";
    if d.device_id.is_none() {
        // Contradicting `device.link` with a red sync line helps nobody.
        return Check::skip(ID, "the device is not linked");
    }
    let Some(h) = &d.sync_health else {
        return Check::ok(ID, "sync has not reported a problem");
    };
    match crate::render::health_line(
        h.consecutive_failures,
        h.last_error_kind.as_deref(),
        h.backoff_secs,
    ) {
        None => Check::ok(ID, "sync healthy"),
        Some(line) => Check::warn(ID, line)
            .remedy(match h.last_error_kind.as_deref() {
                Some("signature_rejected") => "dira device rotate-key",
                Some("unauthorized") | Some("device_unknown") => "dira device link",
                _ => "dira device resync  (and `dira status` for the reason)",
            })
            .detail(json!({
                "consecutive_failures": h.consecutive_failures,
                "last_error_kind": h.last_error_kind,
                "backoff_secs": h.backoff_secs,
            })),
    }
}

/// Is this device linked and pointed at a cloud?
///
/// Never a failure — every state here is a legitimate configuration, and local
/// capture works regardless.
pub(crate) fn device_link(d: &DeviceProbe, cloud_url: Option<&str>) -> Check {
    const ID: &str = "device.link";
    let detail = json!({
        "device_id": d.device_id,
        "cloud_url": cloud_url,
        "pending": d.pending,
        "cursor": d.cursor,
        "cloud_watermark": d.cloud_watermark,
        "local_head": d.local_head,
    });
    if d.device_id.is_none() {
        return Check::warn(
            ID,
            "not linked — local capture works, nothing reaches the cloud",
        )
        .remedy("dira device link")
        .detail(detail);
    }
    if cloud_url.is_none() {
        return Check::warn(ID, "linked, but no cloud URL is configured")
            .remedy("dira config set cloud_url <url>")
            .detail(detail);
    }
    if let Some(at) = &d.pending_rotation_at {
        return Check::warn(ID, format!("a key rotation has been pending since {at}"))
            .remedy("dira device rotate-key")
            .detail(detail);
    }
    Check::ok(ID, format!("linked, {} event(s) awaiting sync", d.pending)).detail(detail)
}

// --- is `dira update` actually landing? ---------------------------------
//
// The passive notice escalates on repeated *failures*. These three cover the
// ways an update silently does not land while nothing reports a failure at all
// — the forensics the #113 reporter did by hand.
//
// Three ids rather than one, because they can co-occur and a check reports one
// level: a machine with both a rollback and a shadowing PATH entry would
// otherwise be told about the rollback, act on it, and only meet the shadowing
// on the next run — the two-round diagnosis these exist to end. Adding ids is
// free under DIRASH-0022's schema rule; growing one id's meaning is not.
//
// None of them ever returns `Fail`. A stale binary is not broken capture, and
// the exit code is a contract an installer reads: `2` has to keep meaning
// capture is broken. They report only — no remedy here runs anything.

/// Did a completed swap get rolled back?
///
/// A `.dira*.old.restore.<pid>` sidecar is written only by
/// `update::replace::restore_from_backup`, so a leftover is proof of a specific
/// past event rather than a tally — this is the one signal that survives across
/// runs and needs no cache.
pub(crate) fn update_rollback(f: &super::UpdateFacts) -> Check {
    const ID: &str = "update.rollback";
    match f.stale_sidecars.first() {
        Some(name) => Check::warn(
            ID,
            format!("a previous update was rolled back — {name} is still here"),
        )
        .remedy("dira update — and if it fails again, file an issue with `dira doctor --json`")
        .detail(json!({ "stale_sidecars": f.stale_sidecars })),
        None => Check::ok(ID, "no rolled-back update left behind"),
    }
}

/// Is an earlier `PATH` entry shadowing the binary updates install to?
///
/// The updater writes into the PATH entry it found; if this process came from
/// somewhere else, a successful update writes a binary the user never executes.
/// The "is there only one `dira` on PATH?" step, automated.
pub(crate) fn update_shadowed(f: &super::UpdateFacts, current_exe: Option<&str>) -> Check {
    const ID: &str = "update.shadowed";
    // No install dir means `dira` is not on PATH, or this is a dev build the
    // updater refuses outright — either way the question has no answer, and a
    // guess is worse than a skip.
    let (Some(dir), Some(exe)) = (&f.install_dir, current_exe) else {
        return Check::skip(ID, "no install directory to compare against");
    };
    let Some(running_from) = Path::new(exe).parent() else {
        return Check::skip(ID, "the running executable has no parent directory");
    };
    let detail = json!({
        "install_dir": dir.display().to_string(),
        "running_from": running_from.display().to_string(),
    });
    if running_from == dir.as_path() {
        return Check::ok(ID, "updates install where this dira runs from").detail(detail);
    }
    Check::warn(
        ID,
        format!(
            "updates install to {} but this dira runs from {} — an earlier PATH entry is shadowing it",
            dir.display(),
            running_from.display()
        ),
    )
    .remedy("remove the shadowing copy, or put the install directory earlier on PATH")
    .detail(detail)
}

/// Have recent `dira update` attempts been failing?
///
/// The threshold is the passive notice's own, not a second copy of it: the two
/// report the same condition and must agree on when it becomes interesting.
pub(crate) fn update_failures(f: &super::UpdateFacts) -> Check {
    const ID: &str = "update.failures";
    let Some(cache) = &f.cache else {
        // No cache means no update check has ever run here. Absent evidence is
        // a skip, never a verdict.
        return Check::skip(ID, "no update check has run on this machine yet");
    };
    let detail = json!({
        "update_failures": cache.update_failures,
        "latest": cache.latest,
        "checked_at": cache.checked_at,
    });
    if cache.update_failures >= crate::update::notice::ESCALATE_AFTER_FAILURES {
        let n = cache.update_failures;
        let latest = cache.latest.as_deref().unwrap_or("a newer version");
        return Check::warn(
            ID,
            format!("{n} recent update attempts failed; still not on {latest}"),
        )
        .remedy("dira update — it prints why")
        .detail(detail);
    }
    Check::ok(ID, "no recent update failures").detail(detail)
}

// ---- cloud runtime -------------------------------------------------------

/// Facts for the three `cloud.*` checks, gathered in `doctor::gather` (the
/// reachability GET and the env/tree reads live there; the judges stay pure).
pub(crate) struct CloudFacts {
    pub runtime: Option<dira_core::runtime::CloudRuntime>,
    /// `DIRA_RUNNER_TOKEN` present and non-blank (its value is never read
    /// into the facts — a diagnostic must not carry a credential).
    pub runner_token_set: bool,
    /// `DIRA_EXTRA_CA_CERTS` value, and whether the file it names exists.
    pub extra_ca: Option<(String, bool)>,
    /// `DIRA_IDENTITY_EMAIL` present and non-blank.
    pub identity_email_env: bool,
    /// The committed teleport artifacts in the current directory, `None`
    /// when the repo has no `.dira/` at all (most repos — not a finding).
    pub bootstrap: Option<BootstrapFacts>,
    /// One GET against `{cloud_url}/api/v1/meta`: `Ok(status)` = the wire
    /// works; `Err(msg)` = transport failure. `None` = no cloud_url.
    pub meta_probe: Option<Result<u16, String>>,
}

/// What `.dira/` holds, as read off disk.
pub(crate) struct BootstrapFacts {
    pub hook_sh: bool,
    pub bootstrap_sh: bool,
    /// The `version="${DIRA_VERSION:-X.Y.Z}"` pin in bootstrap.sh, if parseable.
    pub pinned_version: Option<String>,
}

/// Read the teleport artifacts under `root/.dira`, `None` when absent.
pub(crate) fn read_bootstrap_artifacts(root: &Path) -> Option<BootstrapFacts> {
    let dir = root.join(".dira");
    if !dir.is_dir() {
        return None;
    }
    let bootstrap = std::fs::read_to_string(dir.join("bootstrap.sh")).ok();
    Some(BootstrapFacts {
        hook_sh: dir.join("hook.sh").is_file(),
        bootstrap_sh: bootstrap.is_some(),
        pinned_version: bootstrap.as_deref().and_then(parse_pinned_version),
    })
}

/// Pull `X.Y.Z` out of the generated `version="${DIRA_VERSION:-X.Y.Z}"` line.
fn parse_pinned_version(bootstrap: &str) -> Option<String> {
    let marker = "${DIRA_VERSION:-";
    let start = bootstrap.find(marker)? + marker.len();
    let rest = &bootstrap[start..];
    let end = rest.find('}')?;
    let v = rest[..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Which cloud runtime this process is in, and whether the provisioning env
/// a capture-and-sync session needs is present.
///
/// The `DIRA_EXTRA_CA_CERTS` readability arm runs before the runtime gate: a
/// bundle file that has gone missing or unreadable is a real misconfiguration
/// on ANY machine, not just a cloud one — the variable was set by something,
/// and every device→cloud client silently drops it (DIRASH-0033's
/// never-brick posture). Reporting that only inside a detected runtime would
/// hide it on the machine where the operator actually set it by hand.
pub(crate) fn cloud_runtime(c: &CloudFacts) -> Check {
    const ID: &str = "cloud.runtime";
    if let Some((path, false)) = &c.extra_ca {
        return Check::warn(
            ID,
            format!("DIRA_EXTRA_CA_CERTS names a file that cannot be opened ({path})"),
        )
        .remedy("point DIRA_EXTRA_CA_CERTS at a readable PEM bundle, or unset it")
        .detail(json!({ "extra_ca": path }));
    }
    let Some(rt) = &c.runtime else {
        // Symmetric with `cloud_bootstrap`: on a plain machine there is
        // nothing to report, and per DIRASH-0022 absent evidence is a skip,
        // never an `Ok` verdict about a state that was never evaluated.
        return Check::skip(
            ID,
            "not a cloud runtime (no runtime marker in the environment)",
        )
        .detail(json!({ "runtime": null }));
    };
    let detail = json!({
        "runtime": rt.id,
        "session_ref": rt.session_ref,
        "runner_token_set": c.runner_token_set,
        "identity_email_set": c.identity_email_env,
        "extra_ca": c.extra_ca.as_ref().map(|(p, _)| p.clone()),
    });
    if !c.runner_token_set {
        // Without a token the VM captures locally and syncs nothing — real
        // work, but it dies with the VM. Worth a nudge, not a failure.
        return Check::warn(
            ID,
            format!("cloud runtime '{}', but DIRA_RUNNER_TOKEN is not set — capture stays local to this ephemeral VM", rt.id),
        )
        .remedy("set DIRA_RUNNER_TOKEN in the environment settings (see docs/cloud-runtimes.md)")
        .detail(detail);
    }
    Check::ok(
        ID,
        format!("cloud runtime '{}', provisioning env present", rt.id),
    )
    .detail(detail)
}

/// Does the wire to the cloud work — DNS, egress policy, proxy, and TLS?
///
/// Any HTTP status is a pass: even a 404 proves the transport; what the
/// status *means* is `sync.health`'s business. A transport error is judged
/// by its text, because the two remedies are disjoint: a certificate error
/// wants `DIRA_EXTRA_CA_CERTS`, everything else wants the network allowlist.
///
/// Never `Fail`, and never runs unprompted (DIRASH-0022's probe-policy
/// exemption from D-0006, recorded in `.zavet/specs/doctor.md`): `gather`
/// only performs the GET inside a detected cloud runtime, or when this id is
/// named explicitly via `--check`. A bare `dira doctor` on an offline machine
/// must never fail because it could not reach a network it was never told to
/// probe.
pub(crate) fn cloud_reachability(c: &CloudFacts, cloud_url: Option<&str>) -> Check {
    const ID: &str = "cloud.reachability";
    let Some(url) = cloud_url else {
        return Check::skip(ID, "no cloud_url configured");
    };
    let Some(probe) = c.meta_probe.as_ref() else {
        return Check::skip(
            ID,
            "not probed outside a cloud runtime; run `dira doctor --check cloud.reachability`",
        );
    };
    match probe {
        Ok(status) => Check::ok(ID, format!("{url} answered (HTTP {status})")).detail(json!({
            "url": url, "status": status,
        })),
        Err(msg) => {
            let tls = msg.to_lowercase();
            let tls = tls.contains("certificate") || tls.contains("tls") || tls.contains("ssl");
            let check = Check::warn(ID, format!("cannot reach {url}: {msg}"))
                .detail(json!({ "url": url, "error": msg, "tls_suspected": tls }));
            if tls {
                check.remedy(
                    "the egress proxy re-terminates TLS — set DIRA_EXTRA_CA_CERTS to the \
                     proxy's CA bundle: check whether $SSL_CERT_FILE or $NODE_EXTRA_CA_CERTS \
                     already names it, or run this repo's .dira/bootstrap.sh if it has one; \
                     otherwise the system bundle (in a cloud VM, usually \
                     /etc/ssl/certs/ca-certificates.crt) is the fallback. See DIRASH-0033",
                )
            } else {
                check.remedy(
                    "allow the cloud host in this environment's network settings (Custom \
                     network access), or check cloud_url — see docs/cloud-runtimes.md",
                )
            }
        }
    }
}

/// Are the committed teleport artifacts in this repo whole and current?
pub(crate) fn cloud_bootstrap(c: &CloudFacts) -> Check {
    const ID: &str = "cloud.bootstrap";
    let Some(b) = &c.bootstrap else {
        return Check::skip(
            ID,
            "this repo has no .dira/ teleport artifacts (fine unless you expected them)",
        );
    };
    let detail = json!({
        "hook_sh": b.hook_sh,
        "bootstrap_sh": b.bootstrap_sh,
        "pinned_version": b.pinned_version,
        "current_version": env!("CARGO_PKG_VERSION"),
    });
    if !b.hook_sh || !b.bootstrap_sh {
        return Check::warn(ID, ".dira/ exists but is missing generated scripts")
            .remedy("dira cloud init — regenerates hook.sh + bootstrap.sh")
            .detail(detail);
    }
    match b.pinned_version.as_deref() {
        None => Check::warn(ID, ".dira/bootstrap.sh has no parseable version pin")
            .remedy("dira cloud init — regenerates the script with a pinned release")
            .detail(detail),
        Some(v) => {
            let mut summary = format!("teleport artifacts present (pinned v{v}");
            if v != env!("CARGO_PKG_VERSION") {
                summary.push_str(&format!("; this binary is v{}", env!("CARGO_PKG_VERSION")));
            }
            summary.push(')');
            Check::ok(ID, summary).detail(detail)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::doctor::Level;

    pub(crate) fn facts_with_no_daemon() -> Facts {
        Facts {
            daemon: crate::daemon::DaemonProbe {
                reach: Reach::Down,
                info: None,
                answered_ping: false,
                unexpected: false,
                legacy: None,
            },
            daemon_socket: "/run/dira.sock".into(),
            supervision: Supervision::NotRunning,
            store_write: Err("no such file".into()),
            device: None,
            db_path: std::path::PathBuf::from("/tmp/dira.db"),
            cloud_url: None,
            wal_bytes: None,
            hooks: Vec::new(),
            breadcrumb: None,
            current_exe: None,
            doctor_elevated: false,
            cwd: Some("/work/api".into()),
            project: Ok("github.com/acme/api".into()),
            update: super::super::UpdateFacts::default(),
            cloud: cloud_facts_plain(),
        }
    }

    /// A machine that is not a cloud runtime and has no teleport artifacts.
    pub(crate) fn cloud_facts_plain() -> CloudFacts {
        CloudFacts {
            runtime: None,
            runner_token_set: false,
            extra_ca: None,
            identity_email_env: false,
            bootstrap: None,
            meta_probe: None,
        }
    }

    // --- project.resolves (#111) --------------------------------------------

    fn facts_with_project(p: Result<String, dira_core::project::ProjectMiss>) -> Facts {
        Facts {
            project: p,
            ..facts_with_no_daemon()
        }
    }

    #[test]
    fn a_resolved_project_is_ok_and_names_it() {
        let c = project_resolves(&facts_with_project(Ok("github.com/acme/api".into())));
        assert_eq!(c.level, Level::Ok);
        assert!(c.summary.contains("github.com/acme/api"), "{}", c.summary);
    }

    /// Not a misconfiguration — there is nothing to report, and per DIRASH-0022
    /// absent evidence must never raise the exit code.
    #[test]
    fn a_plain_directory_skips_rather_than_warning() {
        let c = project_resolves(&facts_with_project(Err(
            dira_core::project::ProjectMiss::NotAGitRepo,
        )));
        assert_eq!(c.level, Level::Skip);
    }

    /// The silent case #111 is about. A warning, never a failure: a local-only
    /// repo is a legitimate place to work, and doctor reports rather than judges
    /// the user's setup.
    #[test]
    fn a_repo_with_no_remote_warns_and_never_fails() {
        let c = project_resolves(&facts_with_project(Err(
            dira_core::project::ProjectMiss::NoRemotes,
        )));
        assert_eq!(c.level, Level::Warn);
        assert!(c.remedy.is_some(), "a warning must be actionable");
        assert!(
            c.summary.contains("cannot be anchored"),
            "must say what is actually lost: {}",
            c.summary
        );
    }

    #[test]
    fn ambiguous_remotes_name_the_candidates_in_the_remedy() {
        let c = project_resolves(&facts_with_project(Err(
            dira_core::project::ProjectMiss::AmbiguousRemotes(vec![
                "fork".into(),
                "upstream".into(),
            ]),
        )));
        assert_eq!(c.level, Level::Warn);
        let remedy = c.remedy.unwrap_or_default();
        assert!(
            remedy.contains("fork") && remedy.contains("upstream"),
            "{remedy}"
        );
    }

    #[test]
    fn an_unparseable_remote_reports_which_one() {
        let c = project_resolves(&facts_with_project(Err(
            dira_core::project::ProjectMiss::UnparseableRemote {
                remote: "origin".into(),
                url: "/srv/git/api.git".into(),
            },
        )));
        assert_eq!(c.level, Level::Warn);
        assert!(c.remedy.unwrap_or_default().contains("/srv/git/api.git"));
    }

    fn info() -> Info {
        Info {
            version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: dira_contract::SCHEMA_VERSION.to_string(),
            pid: 1,
            uptime_seconds: 10,
            http_ingress_error: None,
            control_channel_warning: None,
            db_path: None,
            storage_warning: None,
        }
    }

    /// **The regression guard for the incident that produced this command.**
    ///
    /// A daemon running under an elevated token refuses ordinary clients. The
    /// advice that follows must never be `dira daemon start`: one is already
    /// running, and starting another makes it worse. Mirrors
    /// `client.rs`'s `the_denied_message_never_advises_a_bare_daemon_start`.
    #[test]
    fn denied_is_a_failure_and_never_advises_daemon_start() {
        // Both elevation states, explicitly — `doctor_elevated` is a gathered
        // fact precisely so this test is the same on every host. An earlier
        // version read `is_elevated()` inside the judge and passed on unix
        // while failing on an elevated Windows CI runner.
        for elevated in [false, true] {
            let mut f = facts_with_no_daemon();
            f.daemon.reach = Reach::Denied;
            f.doctor_elevated = elevated;

            let c = reachable(&f);
            assert_eq!(c.level, Level::Fail, "elevated={elevated}");
            assert!(c.summary.contains("refused"), "elevated={elevated}");
            // Never the original misdiagnosis.
            assert!(
                !c.summary.contains("nothing is listening"),
                "elevated={elevated}"
            );

            let remedy = c.remedy.expect("denied must carry advice");
            // Not "must never mention `daemon start`" — the correct fix for a
            // non-elevated client IS a stop-then-start sequence. The property
            // is that it is never the BARE advice, and that it explains a
            // daemon is already running.
            assert_ne!(remedy.trim(), "dira daemon start", "elevated={elevated}");
            assert!(
                remedy.to_lowercase().contains("running"),
                "denied advice must explain a daemon IS running (elevated={elevated}): {remedy}"
            );
            assert_eq!(
                c.detail["client_elevated"], elevated,
                "the report must record which token asked"
            );
        }

        // The non-elevated case is the incident's own shape, and it carries the
        // explicit warning against starting one as Administrator.
        let mut f = facts_with_no_daemon();
        f.daemon.reach = Reach::Denied;
        let remedy = reachable(&f).remedy.expect("advice");
        assert!(remedy.to_lowercase().contains("do not run"), "{remedy}");
    }

    #[test]
    fn down_with_a_legacy_daemon_warns_and_says_restart() {
        let mut f = facts_with_no_daemon();
        f.daemon.legacy = Some(std::path::PathBuf::from("/tmp/dira.sock"));
        let c = reachable(&f);
        assert_eq!(c.level, Level::Warn);
        assert_eq!(c.remedy.as_deref(), Some("dira daemon restart"));
    }

    #[test]
    fn down_with_nothing_anywhere_is_a_failure_that_does_advise_start() {
        let c = reachable(&facts_with_no_daemon());
        assert_eq!(c.level, Level::Fail);
        assert_eq!(c.remedy.as_deref(), Some("dira daemon start"));
    }

    #[test]
    fn supervision_not_running_skips_rather_than_failing() {
        assert_eq!(supervision(&facts_with_no_daemon()).level, Level::Skip);
    }

    #[test]
    fn an_unmanaged_daemon_warns_about_the_next_reboot() {
        let mut f = facts_with_no_daemon();
        f.supervision = Supervision::Socket(42);
        let c = supervision(&f);
        assert_eq!(c.level, Level::Warn);
        assert!(c.summary.contains("reboot"));
        assert_eq!(c.remedy.as_deref(), Some("dira daemon install"));
    }

    #[test]
    fn version_skew_folds_version_and_schema() {
        assert_eq!(version_skew(&info()).level, Level::Ok);

        let mut old = info();
        old.version = "0.0.1".into();
        assert_eq!(version_skew(&old).level, Level::Warn);

        let mut skewed = info();
        skewed.schema_version = "999".into();
        let c = version_skew(&skewed);
        assert_eq!(c.level, Level::Warn);
        assert!(c.summary.contains("schema"), "{}", c.summary);
    }

    /// D-0009: a daemon whose ingress did not bind captures nothing and must
    /// not read as healthy.
    #[test]
    fn an_ingress_error_is_a_failure_not_a_warning() {
        assert_eq!(ingress(&info()).level, Level::Ok);
        let mut i = info();
        i.http_ingress_error = Some("address in use".into());
        assert_eq!(ingress(&i).level, Level::Fail);
    }

    #[test]
    fn a_control_channel_warning_is_a_warning_not_a_failure() {
        assert_eq!(control_channel(&info()).level, Level::Ok);
        let mut i = info();
        i.control_channel_warning = Some("pipe fell back to a DACL-only descriptor".into());
        let c = control_channel(&i);
        assert_eq!(c.level, Level::Warn);
        assert!(c.remedy.expect("advice").contains("non-elevated"));
    }

    #[test]
    fn wal_thresholds_are_exact_at_the_boundary() {
        assert_eq!(wal_size(None).level, Level::Ok);
        assert_eq!(wal_size(Some(WAL_WARN_BYTES - 1)).level, Level::Ok);
        assert_eq!(wal_size(Some(WAL_WARN_BYTES)).level, Level::Warn);
        assert_eq!(wal_size(Some(WAL_FAIL_BYTES - 1)).level, Level::Warn);
        assert_eq!(wal_size(Some(WAL_FAIL_BYTES)).level, Level::Fail);
    }

    #[test]
    fn store_divergence_is_a_failure_but_an_unknown_is_a_skip() {
        let cli = Path::new("/home/me/dira.db");
        assert_eq!(store_divergence(cli, None).level, Level::Skip);
        assert_eq!(
            store_divergence(cli, Some("/home/me/dira.db")).level,
            Level::Ok
        );
        let c = store_divergence(cli, Some("/root/dira.db"));
        assert_eq!(c.level, Level::Fail);
        assert!(c.remedy.expect("advice").contains("your own user"));
    }

    fn wiring(
        harness: &'static str,
        expected: usize,
        missing: &[&str],
        cmd: &str,
    ) -> HarnessWiring {
        HarnessWiring {
            harness,
            scope: "project",
            path: format!(".{harness}/settings.json"),
            expected,
            missing: missing.iter().map(|s| s.to_string()).collect(),
            commands: vec![cmd.to_string()],
        }
    }

    #[test]
    fn no_harness_config_warns_rather_than_fails() {
        let c = hooks_config(&[]);
        assert_eq!(c.level, Level::Warn);
        // Nothing wired means nothing has been set up, so the advice is the
        // setup command — the user has no way to answer "which harness?" yet.
        assert!(c.remedy.expect("advice").contains("dira onboard"));
    }

    #[test]
    fn partial_wiring_names_the_missing_events() {
        let c = hooks_config(&[wiring(
            "claude",
            8,
            &["Stop", "Notification"],
            "/bin/dira hook claude",
        )]);
        assert_eq!(c.level, Level::Warn);
        assert!(c.summary.contains("6/8"), "{}", c.summary);
        assert!(c.summary.contains("Stop, Notification"), "{}", c.summary);
    }

    #[test]
    fn fully_wired_is_ok() {
        let c = hooks_config(&[wiring("claude", 8, &[], "/bin/dira hook claude")]);
        assert_eq!(c.level, Level::Ok);
        assert!(c.summary.contains("8/8"));
    }

    #[test]
    fn a_missing_hook_binary_fails_but_a_stale_one_only_warns() {
        let missing = wiring("claude", 8, &[], "/nope/does/not/exist/dira hook claude");
        let c = hooks_exe_path(std::slice::from_ref(&missing), Some("/usr/bin/dira"));
        assert_eq!(c.level, Level::Fail);
        assert!(c.remedy.expect("advice").contains("dira init"));

        // An existing-but-different binary: use a path we know is there.
        let exe = std::env::current_exe().expect("current exe");
        let stale = wiring("claude", 8, &[], &format!("{} hook claude", exe.display()));
        let c = hooks_exe_path(std::slice::from_ref(&stale), Some("/some/other/dira"));
        assert_eq!(c.level, Level::Warn);

        // No configs at all: skip, so it doesn't repeat `hooks.config`.
        assert_eq!(
            hooks_exe_path(&[], Some("/usr/bin/dira")).level,
            Level::Skip
        );
    }

    fn scoped_wiring(harness: &'static str, scope: &'static str, cmd: &str) -> HarnessWiring {
        HarnessWiring {
            harness,
            scope,
            path: format!("{scope}/.{harness}/settings.json"),
            expected: 1,
            missing: Vec::new(),
            commands: vec![cmd.to_string()],
        }
    }

    // --- hooks.scope_overlap ---------------------------------------------

    #[test]
    fn scope_overlap_skips_when_only_one_scope_is_wired() {
        let project = scoped_wiring("claude", "project", "/bin/dira hook claude");
        assert_eq!(
            scope_overlap(std::slice::from_ref(&project)).level,
            Level::Skip
        );

        let global = scoped_wiring("claude", "global", "/bin/dira hook claude");
        assert_eq!(
            scope_overlap(std::slice::from_ref(&global)).level,
            Level::Skip
        );
    }

    /// The design's own point: a portable-wrapper project entry has a yield
    /// check built in (`DIRA_HOOK_VIA=portable`), so overlapping global-scope
    /// wiring is not double delivery.
    #[test]
    fn scope_overlap_is_ok_when_the_project_entry_is_the_portable_wrapper() {
        let project = scoped_wiring("claude", "project", "sh .dira/hook.sh claude");
        let global = scoped_wiring("claude", "global", "/home/u/.local/bin/dira hook claude");
        let c = scope_overlap(&[project, global]);
        assert_eq!(c.level, Level::Ok);
        assert!(c.summary.contains("claude"), "{}", c.summary);
    }

    /// Same verdict, but with a fixture that matches the real data flow
    /// instead of `scoped_wiring`'s fabricated `commands` entry: a project
    /// entry that is purely the portable wrapper never puts anything in
    /// `HarnessWiring.commands` at all (`init::dira_hook_commands` filters on
    /// the literal substring `" hook "`, which `sh .dira/hook.sh claude`
    /// never contains — see the `commands` field doc). `missing` shorter than
    /// `expected` is what makes `wired() > 0` true here, the same way a real
    /// partially-wired portable entry would.
    #[test]
    fn scope_overlap_is_ok_when_the_project_entry_has_no_recorded_commands() {
        let project = HarnessWiring {
            harness: "claude",
            scope: "project",
            path: ".claude/settings.json".to_string(),
            expected: 2,
            missing: vec!["Notification".to_string()],
            commands: vec![],
        };
        let global = scoped_wiring("claude", "global", "/home/u/.local/bin/dira hook claude");
        let c = scope_overlap(&[project, global]);
        assert_eq!(c.level, Level::Ok);
        assert!(c.summary.contains("claude"), "{}", c.summary);
    }

    /// A machine-specific project entry has no yield check, so the same event
    /// really does fire from both scopes — worth a warning, never a failure.
    #[test]
    fn scope_overlap_warns_on_a_machine_specific_project_entry() {
        let project = scoped_wiring("claude", "project", "/home/u/.local/bin/dira hook claude");
        let global = scoped_wiring("claude", "global", "/home/u/.local/bin/dira hook claude");
        let c = scope_overlap(&[project, global]);
        assert_eq!(c.level, Level::Warn);
        assert!(c.remedy.expect("advice").contains("dira cloud init"));
    }

    #[test]
    fn scope_overlap_skips_with_no_wiring_at_all() {
        assert_eq!(scope_overlap(&[]).level, Level::Skip);
    }

    /// The harness runs hook commands through a shell, so `$HOME/...` is a
    /// working config. Reporting it as a missing binary is a false alarm on a
    /// healthy machine — worse than saying nothing.
    #[test]
    fn a_shell_variable_in_the_hook_path_is_not_a_missing_binary() {
        let home = dira_core::config::home_dir().expect("home");
        // $HOME expands to something that exists, so it resolves normally.
        assert_eq!(
            resolve_exe("$HOME/definitely/not/here/dira"),
            ExePath::Missing(format!("{}/definitely/not/here/dira", home.display()))
        );
        // A variable we cannot expand is reported honestly, not as missing.
        assert_eq!(
            resolve_exe("$XDG_BIN_HOME/dira"),
            ExePath::Unverifiable("$XDG_BIN_HOME/dira".into())
        );
        let w = wiring("claude", 8, &[], "$XDG_BIN_HOME/dira hook claude");
        let c = hooks_exe_path(std::slice::from_ref(&w), Some("/usr/bin/dira"));
        assert_eq!(c.level, Level::Ok, "{}", c.summary);

        // `~` expands too.
        let exe = std::env::current_exe().expect("current exe");
        assert!(matches!(
            resolve_exe(&exe.display().to_string()),
            ExePath::Exists(_)
        ));
    }

    #[test]
    fn a_breadcrumb_is_a_failure_because_capture_is_being_lost_now() {
        assert_eq!(breadcrumb(None).level, Level::Ok);
        let h = Health {
            last_error_at: 1_700_000_000,
            last_error: "access denied".into(),
            harness: "claude".into(),
            consecutive: 12,
        };
        let c = breadcrumb(Some(&h));
        assert_eq!(c.level, Level::Fail);
        assert!(c.summary.contains("12 claude"));
    }

    fn device(linked: bool) -> DeviceProbe {
        DeviceProbe {
            device_id: linked.then(|| "dev_1".to_string()),
            cursor: None,
            pending: 3,
            cloud_watermark: None,
            local_head: None,
            sync_health: None,
            pending_rotation_at: None,
        }
    }

    fn health(kind: &str) -> dira_core::sync::SyncHealth {
        dira_core::sync::SyncHealth {
            consecutive_failures: 4,
            last_error_kind: Some(kind.to_string()),
            backoff_secs: 60,
            ..Default::default()
        }
    }

    #[test]
    fn sync_remedy_is_chosen_by_the_error_kind() {
        for (kind, want) in [
            ("signature_rejected", "dira device rotate-key"),
            ("unauthorized", "dira device link"),
            ("device_unknown", "dira device link"),
            (
                "network",
                "dira device resync  (and `dira status` for the reason)",
            ),
        ] {
            let mut d = device(true);
            d.sync_health = Some(health(kind));
            let c = sync_health(&d);
            assert_eq!(c.level, Level::Warn, "{kind}");
            assert_eq!(c.remedy.as_deref(), Some(want), "{kind}");
        }
    }

    /// A sync failure delays the cloud; it loses nothing locally. Never a fail.
    #[test]
    fn sync_is_never_a_failure_and_skips_when_unlinked() {
        assert_eq!(sync_health(&device(true)).level, Level::Ok);
        assert_eq!(sync_health(&device(false)).level, Level::Skip);
    }

    #[test]
    fn device_link_states_are_all_warnings_at_worst() {
        assert_eq!(device_link(&device(false), None).level, Level::Warn);
        assert_eq!(device_link(&device(true), None).level, Level::Warn);
        assert_eq!(
            device_link(&device(true), Some("https://api.dira.sh")).level,
            Level::Ok
        );
        let mut rotating = device(true);
        rotating.pending_rotation_at = Some("2026-08-01".into());
        let c = device_link(&rotating, Some("https://api.dira.sh"));
        assert_eq!(c.level, Level::Warn);
        assert_eq!(c.remedy.as_deref(), Some("dira device rotate-key"));
    }

    // --- update.* checks (#117) ----------------------------------------

    fn cache(update_failures: u32, latest: Option<&str>) -> crate::update::notice::CacheFacts {
        crate::update::notice::CacheFacts {
            checked_at: 1_760_000_000,
            latest: latest.map(str::to_string),
            update_failures,
        }
    }

    fn update_facts(cache: Option<crate::update::notice::CacheFacts>) -> super::super::UpdateFacts {
        super::super::UpdateFacts {
            cache,
            install_dir: Some("/home/u/.local/bin".into()),
            stale_sidecars: Vec::new(),
        }
    }

    const RUNNING: &str = "/home/u/.local/bin/dira";
    const ELSEWHERE: &str = "/usr/local/bin/dira";

    /// DIRASH-0022's rule, on the commonest case there is: a machine that has
    /// never run an update check has no evidence either way.
    #[test]
    fn no_update_cache_is_a_skip_not_a_verdict() {
        assert_eq!(update_failures(&update_facts(None)).level, Level::Skip);
    }

    #[test]
    fn a_healthy_install_is_ok_on_every_update_check() {
        let f = update_facts(Some(cache(0, Some("9.9.9"))));
        assert_eq!(update_rollback(&f).level, Level::Ok);
        assert_eq!(update_shadowed(&f, Some(RUNNING)).level, Level::Ok);
        assert_eq!(update_failures(&f).level, Level::Ok);
    }

    /// The `.dirad.exe.old.restore.11648` the #113 reporter found by hand. Only
    /// the rollback path writes one, so it is proof a swap completed and was
    /// then undone.
    #[test]
    fn a_leftover_restore_sidecar_reports_the_rollback() {
        let mut f = update_facts(Some(cache(0, Some("9.9.9"))));
        f.stale_sidecars = vec![".dirad.exe.old.restore.11648".into()];
        let c = update_rollback(&f);
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.summary.contains("rolled back"),
            "the summary must name what happened: {}",
            c.summary
        );
        assert!(c.remedy.is_some(), "a warning must say what to do");
    }

    /// The other half of the manual diagnosis: `Get-Command dira.exe` proving
    /// which copy actually runs. An update that installs somewhere the user
    /// never executes from reports success and changes nothing they see.
    #[test]
    fn an_install_dir_that_is_not_where_dira_runs_from_is_shadowing() {
        let c = update_shadowed(&update_facts(Some(cache(0, None))), Some(ELSEWHERE));
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.summary.contains("shadowing"),
            "the summary must name the cause: {}",
            c.summary
        );
        assert!(c.remedy.is_some());
    }

    /// The reason these are three ids and not one: a machine can have both, and
    /// a single check reports a single level — so the user would fix the
    /// rollback and only meet the shadowing on the next run.
    #[test]
    fn a_rollback_and_a_shadowed_install_are_both_reported_at_once() {
        let mut f = update_facts(Some(cache(9, Some("9.9.9"))));
        f.stale_sidecars = vec![".dira.old.restore.1".into()];
        assert_eq!(update_rollback(&f).level, Level::Warn);
        assert_eq!(update_shadowed(&f, Some(ELSEWHERE)).level, Level::Warn);
        assert_eq!(update_failures(&f).level, Level::Warn);
    }

    #[test]
    fn update_failures_warn_at_the_notices_own_threshold() {
        let threshold = crate::update::notice::ESCALATE_AFTER_FAILURES;
        let at = |n| update_failures(&update_facts(Some(cache(n, Some("9.9.9"))))).level;
        assert_eq!(at(threshold - 1), Level::Ok);
        assert_eq!(at(threshold), Level::Warn);
        assert_eq!(at(threshold + 5), Level::Warn);
    }

    /// A stale binary is not broken capture, and doctor's exit code is a
    /// contract an installer reads: `2` must keep meaning "capture is broken".
    #[test]
    fn no_update_state_is_ever_a_failure() {
        let mut worst = update_facts(Some(cache(99, Some("9.9.9"))));
        worst.stale_sidecars = vec![".dira.old.restore.1".into()];
        for exe in [Some(ELSEWHERE), Some(RUNNING), None] {
            for level in [
                update_rollback(&worst).level,
                update_shadowed(&worst, exe).level,
                update_failures(&worst).level,
            ] {
                assert_ne!(
                    level,
                    Level::Fail,
                    "an un-landed update must never claim capture is broken"
                );
            }
        }
    }

    /// With no install directory (not on PATH, or a dev build the updater
    /// refuses) the shadowing question has no answer — it must not be guessed.
    #[test]
    fn an_unknown_install_dir_never_claims_shadowing() {
        let mut f = update_facts(Some(cache(0, Some("9.9.9"))));
        f.install_dir = None;
        assert_eq!(
            update_shadowed(&f, Some("/anywhere/at/all/dira")).level,
            Level::Skip
        );
    }

    // --- cloud.* -------------------------------------------------------------

    fn in_claude_web() -> CloudFacts {
        CloudFacts {
            runtime: Some(dira_core::runtime::CloudRuntime {
                id: "claude-web".into(),
                session_ref: Some("cse_01A".into()),
            }),
            runner_token_set: true,
            extra_ca: Some(("/etc/ssl/certs/ca-certificates.crt".into(), true)),
            identity_email_env: true,
            bootstrap: None,
            meta_probe: None,
        }
    }

    /// Every `cloud.*` id is absent evidence on a plain machine, not a
    /// verdict: `cloud_runtime` was symmetric with `cloud_bootstrap` even
    /// before this test existed for it, and DIRASH-0022 says absent evidence
    /// is `Skip`, never `Ok`.
    #[test]
    fn a_plain_machine_skips_every_cloud_check() {
        let c = cloud_facts_plain();
        assert_eq!(cloud_runtime(&c).level, Level::Skip);
        // No cloud_url: absent evidence is a skip (DIRASH-0022).
        assert_eq!(cloud_reachability(&c, None).level, Level::Skip);
        assert_eq!(cloud_bootstrap(&c).level, Level::Skip);
    }

    /// `cloud.reachability` never fires the GET on its own — `gather` only
    /// probes inside a detected runtime or when named via `--check`. A
    /// configured `cloud_url` with no probe result must still skip, not
    /// silently read as "unreachable".
    #[test]
    fn cloud_reachability_skips_when_not_probed_even_with_a_url_configured() {
        let mut c = cloud_facts_plain();
        c.meta_probe = None;
        let check = cloud_reachability(&c, Some("https://app.dirahq.sh"));
        assert_eq!(check.level, Level::Skip);
        assert!(
            check.summary.contains("--check cloud.reachability"),
            "{}",
            check.summary
        );
    }

    /// DIRASH-0033: a bundle the caller can no longer open is a real
    /// misconfiguration on ANY machine, not just inside a cloud runtime —
    /// this arm must fire even on a plain one.
    #[test]
    fn a_missing_extra_ca_file_warns_on_a_plain_machine_too() {
        let mut c = cloud_facts_plain();
        c.extra_ca = Some(("/nope.pem".into(), false));
        let check = cloud_runtime(&c);
        assert_eq!(check.level, Level::Warn);
        assert!(check.summary.contains("/nope.pem"), "{}", check.summary);
    }

    #[test]
    fn a_provisioned_cloud_vm_is_ok_and_named() {
        let c = cloud_runtime(&in_claude_web());
        assert_eq!(c.level, Level::Ok);
        assert!(c.summary.contains("claude-web"), "{}", c.summary);
    }

    /// A cloud VM without a runner token captures into a store that dies with
    /// the VM — worth a nudge with the fix, never a failure.
    #[test]
    fn a_cloud_vm_without_a_runner_token_warns_with_the_remedy() {
        let mut f = in_claude_web();
        f.runner_token_set = false;
        let c = cloud_runtime(&f);
        assert_eq!(c.level, Level::Warn);
        assert!(c.summary.contains("DIRA_RUNNER_TOKEN"), "{}", c.summary);
        assert!(c.remedy.is_some());
    }

    #[test]
    fn a_missing_extra_ca_file_warns_before_anything_tries_to_use_it() {
        let mut f = in_claude_web();
        f.extra_ca = Some(("/nope.pem".into(), false));
        let c = cloud_runtime(&f);
        assert_eq!(c.level, Level::Warn);
        assert!(c.summary.contains("/nope.pem"), "{}", c.summary);
    }

    /// Any HTTP status proves the wire; the two transport-failure remedies are
    /// disjoint and must each name their own fix. Per DIRASH-0022's
    /// probe-policy exemption, a transport error is `Warn`, never `Fail` —
    /// an unreachable cloud is not broken local capture.
    #[test]
    fn reachability_judges_transport_not_status_and_never_fails() {
        let url = Some("https://app.dirahq.sh");
        let mut f = in_claude_web();

        f.meta_probe = Some(Ok(404));
        assert_eq!(cloud_reachability(&f, url).level, Level::Ok);

        f.meta_probe = Some(Err(
            "error sending request: invalid peer certificate: UnknownIssuer".into(),
        ));
        let c = cloud_reachability(&f, url);
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.remedy
                .as_deref()
                .unwrap_or_default()
                .contains("DIRA_EXTRA_CA_CERTS"),
            "a TLS failure must point at the extra-CA opt-in: {:?}",
            c.remedy
        );

        f.meta_probe = Some(Err("error sending request: dns error".into()));
        let c = cloud_reachability(&f, url);
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.remedy.as_deref().unwrap_or_default().contains("network"),
            "a non-TLS failure must point at the egress policy: {:?}",
            c.remedy
        );
    }

    #[test]
    fn bootstrap_artifacts_judge_completeness_and_read_the_pin() {
        let mut f = in_claude_web();
        // Partial artifacts: a repo where someone deleted hook.sh.
        f.bootstrap = Some(BootstrapFacts {
            hook_sh: false,
            bootstrap_sh: true,
            pinned_version: Some("0.5.0".into()),
        });
        let c = cloud_bootstrap(&f);
        assert_eq!(c.level, Level::Warn);
        assert!(c
            .remedy
            .as_deref()
            .unwrap_or_default()
            .contains("cloud init"));

        // Whole artifacts: ok, naming the pin even when it trails this binary.
        f.bootstrap = Some(BootstrapFacts {
            hook_sh: true,
            bootstrap_sh: true,
            pinned_version: Some("0.5.0".into()),
        });
        let c = cloud_bootstrap(&f);
        assert_eq!(c.level, Level::Ok);
        assert!(c.summary.contains("0.5.0"), "{}", c.summary);
    }

    /// The pin parser reads exactly what the generator writes — the same
    /// template, so the two cannot drift without this failing.
    #[test]
    fn the_pin_parser_reads_the_generated_template() {
        let generated =
            include_str!("../../templates/dira-bootstrap.sh").replace("{{VERSION}}", "1.2.3");
        assert_eq!(parse_pinned_version(&generated).as_deref(), Some("1.2.3"));
        assert_eq!(parse_pinned_version("nothing here"), None);
    }
}
