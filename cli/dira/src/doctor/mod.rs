//! `dira doctor` — one command that answers "is capture actually working?"
//!
//! Every signal here already existed somewhere; none of them composed. A
//! machine ran for days with a completely dead capture channel while
//! `dira daemon status` reported a healthy daemon, commit capture worked, and
//! cloud sync stayed green — because the one broken link (the hook shim's
//! connect to the control channel) was the only thing nothing reported on.
//!
//! Three properties make this useful rather than decorative:
//!
//! 1. **Absent evidence is `Skip`, never `Fail`.** Failing on inputs we could
//!    not gather turns one real cause into a wall of red and buries it — the
//!    exact experience this command exists to replace. [`Level`] is ordered so
//!    a skip can never raise the exit code.
//! 2. **It reports, it never repairs.** Every remedy is either destructive or
//!    a one-liner the user should run knowingly. In the incident above the
//!    "obvious" automatic fix would have been `dira daemon start`, which
//!    `daemon::start` already refuses in that state because it makes things
//!    strictly worse.
//! 3. **Exit codes are a contract**: 0 all clear, 1 any warning, 2 any
//!    failure, so an install script can tell "works, could be better" from
//!    "capture is broken".

pub(crate) mod capture;
pub(crate) mod checks;
pub(crate) mod render;

/// Meta key the store-write proof stamps. One idempotent row; also a record of
/// when doctor last ran on this machine.
const META_LAST_RUN: &str = "doctor_last_run_at";

use crate::update::replace::Guard;
use dira_core::{Config, Store};
use serde::Serialize;

/// A check's verdict.
///
/// `Ord` is the point: `checks.iter().map(|c| c.level).max()` is the report's
/// verdict, and `Skip` sorting below `Warn` is what guarantees a skipped check
/// can never move the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Level {
    Ok,
    Skip,
    Warn,
    Fail,
}

/// One diagnosis.
///
/// `summary` is a single line, present tense, no trailing period. `remedy` is
/// the action — a command where one exists, prose where none does. `detail` is
/// the machine-readable payload for `--json`, and `Null` when there is none.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Check {
    pub id: &'static str,
    pub level: Level,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

impl Check {
    fn new(id: &'static str, level: Level, summary: impl Into<String>) -> Self {
        Self {
            id,
            level,
            summary: summary.into(),
            remedy: None,
            detail: serde_json::Value::Null,
        }
    }
    pub(crate) fn ok(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, Level::Ok, summary)
    }
    pub(crate) fn warn(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, Level::Warn, summary)
    }
    pub(crate) fn fail(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, Level::Fail, summary)
    }
    /// A check whose inputs could not be gathered. Renders as
    /// "skipped — {reason}" and never affects the exit code.
    pub(crate) fn skip(id: &'static str, reason: impl Into<String>) -> Self {
        Self::new(id, Level::Skip, reason)
    }
    pub(crate) fn remedy(mut self, r: impl Into<String>) -> Self {
        self.remedy = Some(r.into());
        self
    }
    pub(crate) fn detail(mut self, d: serde_json::Value) -> Self {
        self.detail = d;
        self
    }
    /// Attach a daemon-reported aside to a verdict that is already bad.
    ///
    /// On windows the control-channel warning is often the most diagnostic line
    /// available, so it rides along on the summary as well as the detail.
    pub(crate) fn note(mut self, w: &str) -> Self {
        match self.detail.as_object_mut() {
            Some(map) => {
                map.insert("control_channel_warning".into(), serde_json::json!(w));
            }
            None => self.detail = serde_json::json!({ "control_channel_warning": w }),
        }
        self.summary = format!("{} (daemon notes: {w})", self.summary);
        self
    }
}

/// Every check id, in the order they run and render.
///
/// Cause first: daemon reachability, then the daemon's own self-report, then
/// the store, then the wiring, then the cloud. A user reading top to bottom
/// meets the root cause before its consequences.
///
/// Doubles as the `--check` allow-list, and is pinned by a test asserting the
/// runner emits exactly these ids in exactly this order — the registry cannot
/// drift silently.
pub(crate) const CHECK_IDS: &[&str] = &[
    "daemon.reachable",
    "daemon.supervision",
    "daemon.version_skew",
    "daemon.ingress",
    "daemon.control_channel",
    "store.writable",
    "store.divergence",
    "store.wal_size",
    "hooks.config",
    "hooks.exe_path",
    "hook.breadcrumb",
    "project.resolves",
    "sync.health",
    "device.link",
    "update.rollback",
    "update.shadowed",
    "update.failures",
    // Opt-in: only ever emitted with `--probe`, because it spawns a child
    // process and writes+deletes a row. Keeping the default side-effect-free
    // is what makes `dira doctor` safe to run from install.sh and CI.
    capture::ID,
];

pub(crate) struct Args {
    pub json: bool,
    pub verbose: bool,
    /// Run the end-to-end capture probe.
    pub probe: bool,
    /// Empty ⇒ run everything. Validated against [`CHECK_IDS`] before any IO.
    pub only: Vec<String>,
}

impl Args {
    fn wants(&self, id: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|s| s == id)
    }
}

/// Everything gathered once, up front: one `DaemonInfo` round-trip, one
/// supervision probe, one `Store::open`.
///
/// The checks are not independent — `daemon.version_skew` needs the same
/// round-trip `daemon.reachable` made, and `sync.health`/`device.link` need the
/// same open store. A registry of independent async closures would force
/// either N round-trips or a shared mutable context; gathering once and then
/// running pure judges keeps all the decision logic IO-free and unit-testable.
pub(crate) struct Facts {
    pub daemon: crate::daemon::DaemonProbe,
    /// Rendered once so the judges stay free of platform path formatting.
    pub daemon_socket: String,
    pub supervision: crate::daemon::Supervision,
    /// Did the store open *and* accept a write? `Err` carries the message.
    pub store_write: Result<(), String>,
    pub device: Option<crate::device::DeviceProbe>,
    pub db_path: std::path::PathBuf,
    pub cloud_url: Option<String>,
    pub wal_bytes: Option<u64>,
    pub hooks: Vec<checks::HarnessWiring>,
    pub breadcrumb: Option<crate::hook_health::Health>,
    pub current_exe: Option<String>,
    /// The directory doctor was invoked from, and whether it resolves to a
    /// canonical project ref. Gathered here (it shells out to git) so the judge
    /// stays a pure function of already-gathered facts.
    pub cwd: Option<String>,
    pub project: Result<String, dira_core::project::ProjectMiss>,
    /// Is THIS process elevated? Gathered rather than probed inside a judge:
    /// the advice for a refused channel differs by it, and a judge that asks
    /// the OS is a judge whose verdict changes with the host it runs on — which
    /// is exactly how the Windows arm escapes its own tests.
    pub doctor_elevated: bool,
    /// What the update-check cache knows, and where an update would land.
    pub update: UpdateFacts,
}

/// Everything the three `update.*` checks judge, gathered up front like the rest.
///
/// All three fields are "no evidence" by absence, and the judge treats them
/// that way: a machine that has never run an update check has no cache, which
/// is a skip and not a verdict (DIRASH-0022).
#[derive(Debug, Default)]
pub(crate) struct UpdateFacts {
    pub cache: Option<crate::update::notice::CacheFacts>,
    /// The directory `dira update` would install into, per the D-0004
    /// PATH-entry probe. `None` when `dira` is not on `PATH`, or when this
    /// process is itself a dev build — the updater refuses those outright, so
    /// "where it would land" has no answer worth reporting.
    pub install_dir: Option<std::path::PathBuf>,
    /// `.dira*.old.*` sidecars beside the installed binary. A
    /// `.old.restore.<pid>` is written only by the rollback path, so a leftover
    /// is proof a swap completed and was then undone.
    pub stale_sidecars: Vec<String>,
}

/// Run every requested check and print the report. Returns the process exit
/// code.
///
/// Never returns `Result`. An `Err` out of `main` exits 1 — which is doctor's
/// "warnings" code — so conflating a broken probe with a warning would be a
/// silent lie to an install script. Every gather failure becomes a `Fail`
/// check at the point it happens instead, and the signature is what enforces
/// that.
pub(crate) async fn run(config: &Config, args: Args) -> i32 {
    if let Some(bad) = args
        .only
        .iter()
        .find(|id| !CHECK_IDS.contains(&id.as_str()))
    {
        eprintln!("dira doctor: unknown check `{bad}`");
        eprintln!("valid checks: {}", CHECK_IDS.join(", "));
        return 2;
    }

    let facts = gather(config).await;
    let mut results = run_checks(&facts, &args);
    // Last, and only on request: it is the one check with side effects, and by
    // now every cheaper explanation for a broken capture path has been ruled
    // in or out — so its verdict reads against that context.
    if args.probe && args.wants(capture::ID) {
        results.push(if facts.daemon.info.is_some() {
            capture::run(config, &facts).await
        } else {
            Check::skip(capture::ID, "the daemon is not reachable")
        });
    }
    let code = exit_code(&results);
    if args.json {
        render::print_json(&results, code);
    } else {
        render::print_human(&results, args.verbose, code);
    }
    code
}

async fn gather(config: &Config) -> Facts {
    let daemon = crate::daemon::probe(config).await;
    let supervision = crate::daemon::detect_supervision(config).await;

    // Open once and share: `device::probe` and the write proof both need it,
    // and opening `dira.db` twice risks two different answers.
    let store = Store::open(&config.db_path).await;
    let (store_write, device) = match &store {
        // Prove durability rather than assuming it: a `meta` round-trip is one
        // idempotent row, no event data, and it doubles as a record of when
        // doctor last ran.
        Ok(s) => {
            let now = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            let write = match s.meta_set(META_LAST_RUN, &now).await {
                Ok(()) => match s.meta_get(META_LAST_RUN).await {
                    Ok(Some(v)) if v == now => Ok(()),
                    Ok(_) => Err("a write to the store did not read back".to_string()),
                    Err(e) => Err(format!("{e}")),
                },
                Err(e) => Err(format!("{e}")),
            };
            (write, crate::device::probe(s).await.ok())
        }
        Err(e) => (Err(format!("{e}")), None),
    };

    let cwd = std::env::current_dir().ok();
    let project = match cwd.as_deref() {
        Some(dir) => dira_core::project::explain_project(dir),
        None => Err(dira_core::project::ProjectMiss::NotAGitRepo),
    };

    // Where an update would land, and what the last one left behind. Read-only:
    // `discover_install` only probes `PATH` and `symlink_metadata`, and the
    // sidecar listing is a `read_dir`. `Guard::DevSymlink` still yields a
    // directory — a `just install` tree is exactly where a shadowing question
    // is worth asking — while `DevBuild` yields none, because `dira update`
    // refuses that outright and has no install directory to speak of.
    let install_dir = match crate::update::replace::discover_install(None) {
        Ok(Guard::Ok(dir) | Guard::DevSymlink { bin_dir: dir, .. }) => Some(dir),
        _ => None,
    };
    // Reported by filename: the directory is already named separately, and the
    // sidecar's own name is the diagnostic part (`.old.restore.<pid>`).
    let stale_sidecars = install_dir
        .as_deref()
        .map(crate::update::replace::stale_old_files)
        .unwrap_or_default()
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    Facts {
        cwd: cwd.map(|d| d.display().to_string()),
        project,
        update: UpdateFacts {
            cache: crate::update::notice::cache_facts(),
            install_dir,
            stale_sidecars,
        },
        daemon_socket: config.socket_path.display().to_string(),
        daemon,
        supervision,
        wal_bytes: std::fs::metadata(config.db_path.with_extension("db-wal"))
            .ok()
            .map(|m| m.len()),
        db_path: config.db_path.clone(),
        cloud_url: config.cloud_url.clone(),
        hooks: checks::read_harness_wiring(),
        breadcrumb: crate::hook_health::snapshot(),
        current_exe: Some(crate::init::dira_exe_path()),
        doctor_elevated: dira_ipc::elevation::is_elevated(),
        store_write,
        device,
    }
}

/// The registry: an ordered sequence of pure judges over the gathered facts.
///
/// A check whose inputs are missing reports `Skip` here rather than being
/// omitted, so `--json` consumers see a stable set of ids.
pub(crate) fn run_checks(f: &Facts, args: &Args) -> Vec<Check> {
    // `Some` only when the daemon actually answered `DaemonInfo`; the four
    // checks below read nothing else, so they skip together.
    let info = f.daemon.info.as_ref();
    let no_daemon = "the daemon is not reachable";

    let mut out = Vec::new();
    let mut push = |c: Check| {
        if args.wants(c.id) {
            out.push(c);
        }
    };

    push(checks::reachable(f));
    push(checks::supervision(f));

    push(match info {
        Some(i) => checks::version_skew(i),
        None => Check::skip("daemon.version_skew", no_daemon),
    });
    push(match info {
        Some(i) => checks::ingress(i),
        None => Check::skip("daemon.ingress", no_daemon),
    });
    push(match info {
        Some(i) => checks::control_channel(i),
        None => Check::skip("daemon.control_channel", no_daemon),
    });

    push(checks::store_writable(f));
    push(match info {
        Some(i) => checks::store_divergence(&f.db_path, i.db_path.as_deref()),
        None => Check::skip("store.divergence", no_daemon),
    });
    push(checks::wal_size(f.wal_bytes));

    push(checks::hooks_config(&f.hooks));
    push(checks::hooks_exe_path(&f.hooks, f.current_exe.as_deref()));
    push(checks::breadcrumb(f.breadcrumb.as_ref()));
    push(checks::project_resolves(f));

    push(match &f.device {
        Some(d) => checks::sync_health(d),
        None => Check::skip("sync.health", "the local store could not be read"),
    });
    push(match &f.device {
        Some(d) => checks::device_link(d, f.cloud_url.as_deref()),
        None => Check::skip("device.link", "the local store could not be read"),
    });

    push(checks::update_rollback(&f.update));
    push(checks::update_shadowed(&f.update, f.current_exe.as_deref()));
    push(checks::update_failures(&f.update));

    out
}

/// 0 all clear, 1 any warning, 2 any failure.
pub(crate) fn exit_code(checks: &[Check]) -> i32 {
    match checks.iter().map(|c| c.level).max() {
        Some(Level::Fail) => 2,
        Some(Level::Warn) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            json: false,
            verbose: false,
            probe: false,
            only: Vec::new(),
        }
    }

    fn c(level: Level) -> Check {
        Check::new("daemon.reachable", level, "x")
    }

    #[test]
    fn exit_code_is_the_worst_level() {
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&[c(Level::Ok)]), 0);
        assert_eq!(exit_code(&[c(Level::Ok), c(Level::Warn)]), 1);
        assert_eq!(exit_code(&[c(Level::Warn), c(Level::Fail)]), 2);
        assert_eq!(exit_code(&[c(Level::Fail), c(Level::Ok)]), 2);
    }

    /// The invariant that keeps a down daemon from reading as a wall of red.
    #[test]
    fn skip_never_raises_the_exit_code() {
        assert_eq!(exit_code(&[c(Level::Ok), c(Level::Skip)]), 0);
        assert_eq!(exit_code(&[c(Level::Skip)]), 0);
        assert!(Level::Skip < Level::Warn);
        assert!(Level::Skip > Level::Ok);
    }

    /// The registry cannot drift from `CHECK_IDS` silently — that constant is
    /// the `--check` allow-list and the documented set of ids in `--json`.
    ///
    /// `capture.e2e` is the one id `run_checks` does not emit: it is opt-in
    /// behind `--probe` and appended by `run`, which is what keeps a bare
    /// `dira doctor` free of side effects.
    #[test]
    fn the_registry_emits_exactly_check_ids_in_order() {
        let f = checks::tests::facts_with_no_daemon();
        let ids: Vec<&str> = run_checks(&f, &args()).iter().map(|c| c.id).collect();
        let expected: Vec<&str> = CHECK_IDS
            .iter()
            .copied()
            .filter(|id| *id != capture::ID)
            .collect();
        assert_eq!(ids, expected);
        assert!(CHECK_IDS.contains(&capture::ID), "--check must accept it");
    }

    /// A daemon-dependent check with no daemon must skip, not fail — otherwise
    /// one dead daemon produces five failures and hides which one matters.
    #[test]
    fn daemon_dependent_checks_skip_when_the_daemon_is_down() {
        let f = checks::tests::facts_with_no_daemon();
        let out = run_checks(&f, &args());
        for id in [
            "daemon.version_skew",
            "daemon.ingress",
            "daemon.control_channel",
            "store.divergence",
        ] {
            let c = out.iter().find(|c| c.id == id).expect(id);
            assert_eq!(c.level, Level::Skip, "{id} should skip when down");
        }
    }

    #[test]
    fn check_filter_selects_a_single_check() {
        let f = checks::tests::facts_with_no_daemon();
        let args = Args {
            only: vec!["store.wal_size".into()],
            ..args()
        };
        let ids: Vec<&str> = run_checks(&f, &args).iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["store.wal_size"]);
    }
}
