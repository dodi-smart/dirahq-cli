//! Step 1 — work out what this machine already has, before asking anything.
//!
//! Everything here is read-only and non-fatal. A probe that cannot answer
//! reports "unknown" and the wizard asks the user rather than guessing; the
//! cost of a false negative is one keystroke, never a wrong write.

use crate::daemon::Supervision;
use crate::zavet_adapters::RepoGate;
use std::path::{Path, PathBuf};

/// The six harnesses `dira init` can wire, in the order the wizard shows
/// them. The ids are canonical (`dira_sources::canonical_harness_id`), and
/// each carries the CLI name to probe on `PATH` plus the config directory
/// whose existence corroborates a PATH miss.
///
/// Claude Code first because it is the common case and the `dira init`
/// default; the rest follow the order the root help lists them in, so the two
/// surfaces read the same way.
pub(crate) const HARNESSES: &[HarnessProbe] = &[
    HarnessProbe {
        id: "claude",
        label: "Claude Code",
        exe: "claude",
        config_dir: ".claude",
    },
    HarnessProbe {
        id: "codex",
        label: "Codex CLI",
        exe: "codex",
        config_dir: ".codex",
    },
    HarnessProbe {
        id: "gemini",
        label: "Gemini CLI",
        exe: "gemini",
        config_dir: ".gemini",
    },
    // Cursor's agent CLI is `cursor-agent`; plain `cursor` is the editor's
    // launcher shim and is absent on a machine that only has the GUI app, so
    // the config dir does most of the work for this one.
    HarnessProbe {
        id: "cursor",
        label: "Cursor",
        exe: "cursor-agent",
        config_dir: ".cursor",
    },
    HarnessProbe {
        id: "opencode",
        label: "OpenCode",
        exe: "opencode",
        config_dir: ".config/opencode",
    },
    HarnessProbe {
        id: "grok",
        label: "Grok Build",
        exe: "grok",
        config_dir: ".grok",
    },
];

/// How to look for one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HarnessProbe {
    pub id: &'static str,
    pub label: &'static str,
    pub exe: &'static str,
    pub config_dir: &'static str,
}

/// What the probe concluded about one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Harness {
    pub probe: HarnessProbe,
    /// Its CLI resolved on `PATH`.
    pub on_path: bool,
    /// Its config directory exists under `$HOME`.
    pub has_config_dir: bool,
    /// dira's hooks are already present in its config.
    pub wired: bool,
}

impl Harness {
    /// Whether to pre-select this harness.
    ///
    /// Either signal is enough. A harness can be installed without having
    /// been run (no config dir yet), and it can have been run via an
    /// installer that never put a CLI on `PATH` — Cursor being the obvious
    /// case, where the GUI app writes `~/.cursor` and ships no `cursor-agent`
    /// unless you ask for it. Requiring both would silently skip real
    /// installs; requiring either costs a deselect at worst.
    pub fn present(&self) -> bool {
        self.on_path || self.has_config_dir
    }
}

/// Everything step 1 learned. Steps take this by reference and never re-probe.
#[derive(Debug, Clone)]
pub(crate) struct State {
    pub harnesses: Vec<Harness>,
    /// How (and whether) the daemon is currently supervised.
    pub supervision: Supervision,
    /// Git toplevel of cwd, when cwd is inside a work tree.
    pub repo_root: Option<PathBuf>,
    /// That toplevel already carries `.zavet/`.
    pub has_zavet_dir: bool,
    /// `claude` is on `PATH` — the precondition for any plugin operation.
    pub claude_present: bool,
    /// The zavet plugin is already installed (detection resolved a plugin
    /// root). `false` also covers "detection was inconclusive", which is the
    /// safe direction: `dira zavet install` is itself a no-op when the plugin
    /// is present, so offering it costs nothing, while wrongly skipping it
    /// leaves the user without the thing they asked for.
    pub zavet_installed: bool,
    /// This device is already linked to a cloud workspace.
    pub device_linked: bool,
    /// The currently resolved knowledge sync tier.
    pub knowledge: dira_core::config::KnowledgeSyncMode,
}

impl State {
    /// The harnesses a bare run would wire: detected and not already wired.
    pub fn wirable(&self) -> Vec<&Harness> {
        self.harnesses
            .iter()
            .filter(|h| h.present() && !h.wired)
            .collect()
    }

    /// Whether the daemon is under a real service manager — the thing step 3
    /// exists to establish. A bare `start`ed daemon (`Pidfile`/`Socket`) is
    /// running but does not survive a reboot, which is exactly the state the
    /// old installer's suggested `daemon start` left everyone in.
    pub fn supervised(&self) -> bool {
        matches!(
            self.supervision,
            Supervision::Launchd | Supervision::SystemdUser | Supervision::ScheduledTask
        )
    }

    /// Running at all, supervised or not.
    pub fn daemon_running(&self) -> bool {
        !matches!(self.supervision, Supervision::NotRunning)
    }
}

/// Probe one harness against an explicit `$HOME` and wiring report. Pure
/// apart from the `PATH` lookup, so tests drive it with a temp home.
pub(crate) fn probe_harness(
    probe: HarnessProbe,
    home: &Path,
    wired_ids: &[&str],
    on_path: bool,
) -> Harness {
    Harness {
        probe,
        on_path,
        has_config_dir: home.join(probe.config_dir).is_dir(),
        wired: wired_ids.contains(&probe.id),
    }
}

/// Which harnesses already carry dira hooks, at **global** scope.
///
/// Reuses `doctor`'s reader (`read_harness_wiring`) rather than re-parsing
/// config files, so "wired" means exactly what `dira doctor` means by it. A
/// harness counts as wired only when nothing is missing — a partially wired
/// config (new events added by an upgrade) must still be offered, since
/// re-running `init` is what fills the gap.
pub(crate) fn wired_harness_ids() -> Vec<&'static str> {
    globally_wired(crate::doctor::checks::read_harness_wiring())
}

/// Filter a wiring report down to the fully-wired, **global**-scope entries.
///
/// `read_harness_wiring()` reports project scope too — Claude Code's own
/// precedence puts it first — but onboarding wires at user scope
/// (DIRASH-0029's "why user scope": a machine-setup command, not a per-repo
/// one). Counting a project-scope wiring here meant a user who had run a bare
/// `dira init` inside one repo got `AlreadyDone` from onboarding's harness
/// step, and every other repo on the machine stayed silently uncaptured with
/// no signal why — the exact silent-undercapture failure DIRASH-0029 exists
/// to prevent.
///
/// Split out from [`wired_harness_ids`] so the scope filter is testable
/// without a real `$HOME`: `read_harness_wiring()` has no injection point,
/// it always reads the process's real config paths.
fn globally_wired(wiring: Vec<crate::doctor::checks::HarnessWiring>) -> Vec<&'static str> {
    wiring
        .into_iter()
        .filter(|w| w.missing.is_empty() && w.scope == "global")
        .map(|w| w.harness)
        .collect()
}

/// Run every probe. `home` is injected so tests never touch the real one.
pub(crate) fn harnesses(home: &Path) -> Vec<Harness> {
    let wired = wired_harness_ids();
    HARNESSES
        .iter()
        .map(|p| probe_harness(*p, home, &wired, crate::which::on_path(p.exe).is_some()))
        .collect()
}

/// Resolve the repo gate for cwd: the git toplevel, and whether it has
/// `.zavet/`.
///
/// Deliberately the same `dira_core::project::toplevel` call
/// `zavet_adapters::repo_gate` uses (DIRASH-0024): the wizard must agree with
/// the adapter gate about what "this repo" means, or it could scaffold one
/// directory and refresh adapters in another.
pub(crate) fn repo(cwd: &Path) -> (Option<PathBuf>, bool) {
    // Calls the adapter gate rather than re-deriving it. The two MUST agree —
    // scaffolding one directory while refreshing adapters in another is the
    // failure DIRASH-0024 exists to prevent — and a shared call enforces that
    // where a comment only asserted it.
    match crate::zavet_adapters::gate_from_toplevel(dira_core::project::toplevel(cwd)) {
        RepoGate::Eligible(root) => (Some(root), true),
        RepoGate::NoZavetDir(root) => (Some(root), false),
        RepoGate::NotGit => (None, false),
    }
}

/// The full detection pass.
pub(crate) async fn run(config: &dira_core::Config, cwd: &Path) -> State {
    let (repo_root, has_zavet_dir) = repo(cwd);
    let detected_harnesses = match dira_core::config::home_dir() {
        Ok(home) => harnesses(&home),
        // No resolvable `$HOME`: the old fallback probed `.` (cwd) instead,
        // which reads cwd's own *project*-scope config directories (a
        // `./.claude` committed to the repo you happen to be standing in) as
        // if they were the user's real, global harness installs — a false
        // "Claude Code detected" in any repo that merely has one. Reporting
        // no harnesses is the safe direction: the wizard offers nothing
        // instead of offering, and possibly wiring, the wrong thing.
        Err(_) => Vec::new(),
    };
    State {
        harnesses: detected_harnesses,
        supervision: crate::daemon::detect_supervision(config).await,
        repo_root,
        has_zavet_dir,
        claude_present: crate::zavet_install::claude_present(),
        // Offline on purpose: spawning `claude` would bootstrap its own
        // config files, making detection a write and breaking `--print`.
        zavet_installed: crate::zavet_install::plugin_root_offline().is_some(),
        device_linked: device_linked(config).await,
        knowledge: config.sync.knowledge,
    }
}

/// Whether a device id is recorded in the store.
///
/// **Never creates the store, and never writes even when it exists.**
/// `Store::open` would create the database and run migrations on a missing
/// file, and — the part an existence check alone missed — `Store::open` on
/// an *existing* WAL-mode database still calls `Store::open`'s writable
/// path, which creates `-wal`/`-shm` sidecars and can run a pending
/// migration under an older running daemon. Both are writes, and
/// `dira onboard --print` promises to change nothing, a promise the whole
/// dry-run mode rests on. `Store::open_readonly` opens read-only and
/// immutable, and never migrates; an absent database still short-circuits
/// first since an absent file cannot contain a device id.
///
/// Otherwise best-effort: a store that will not open reports "not linked",
/// which makes the wizard *offer* the link step. Offering a step that turns
/// out to be done is recoverable — `device::link` itself reports "already
/// linked" and changes nothing. Skipping a step that was actually needed is
/// not.
async fn device_linked(config: &dira_core::Config) -> bool {
    if !config.db_path.exists() {
        return false;
    }
    let Ok(store) = dira_core::Store::open_readonly(&config.db_path).await else {
        return false;
    };
    matches!(dira_core::identity::device_id(&store).await, Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dira-onboard-detect-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn claude() -> HarnessProbe {
        HARNESSES[0]
    }

    /// The detection matrix: present/absent × wired/unwired. Only the
    /// present-and-unwired cell is something the wizard should offer to do.
    #[test]
    fn presence_needs_only_one_signal() {
        let home = tmp_home("either");

        // Neither signal: not present.
        let h = probe_harness(claude(), &home, &[], false);
        assert!(!h.present());

        // PATH only — installed but never run.
        let h = probe_harness(claude(), &home, &[], true);
        assert!(h.present(), "a CLI on PATH is enough");

        // Config dir only — the Cursor case: GUI app, no CLI shim.
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let h = probe_harness(claude(), &home, &[], false);
        assert!(h.present(), "a config directory is enough");
    }

    #[test]
    fn wired_is_read_from_the_supplied_ids() {
        let home = tmp_home("wired");
        let h = probe_harness(claude(), &home, &["claude"], true);
        assert!(h.wired);
        let h = probe_harness(claude(), &home, &["gemini"], true);
        assert!(!h.wired);
    }

    /// A project-scope wiring (a bare `dira init` run inside one repo) must
    /// not count as "done" for onboarding, which wires user/global scope —
    /// otherwise a user who ran `dira init` in one repo gets `AlreadyDone`
    /// from the harness step and every other repo on the machine stays
    /// silently uncaptured (DIRASH-0029's "why user scope").
    #[test]
    fn only_global_scope_counts_as_wired_for_onboarding() {
        use crate::doctor::checks::HarnessWiring;
        let wiring = vec![
            HarnessWiring {
                harness: "claude",
                scope: "project",
                path: "./.claude/settings.json".to_string(),
                expected: 5,
                missing: Vec::new(),
                commands: Vec::new(),
            },
            HarnessWiring {
                harness: "gemini",
                scope: "global",
                path: "/home/x/.gemini/settings.json".to_string(),
                expected: 5,
                missing: Vec::new(),
                commands: Vec::new(),
            },
        ];
        assert_eq!(
            globally_wired(wiring),
            vec!["gemini"],
            "the fully-wired project-scope claude entry must not count"
        );
    }

    /// An already-wired harness is not offered again — that is what makes a
    /// second `dira onboard` run report "already done" instead of redoing
    /// work.
    #[test]
    fn wirable_excludes_already_wired_and_absent_harnesses() {
        let home = tmp_home("wirable");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();

        let harnesses = vec![
            probe_harness(HARNESSES[0], &home, &["claude"], true), // present, wired
            probe_harness(HARNESSES[2], &home, &[], false),        // present, unwired
            probe_harness(HARNESSES[5], &home, &[], false),        // absent
        ];
        let state = State {
            harnesses,
            supervision: Supervision::NotRunning,
            repo_root: None,
            has_zavet_dir: false,
            claude_present: false,
            zavet_installed: false,
            device_linked: false,
            knowledge: dira_core::config::KnowledgeSyncMode::Off,
        };

        let ids: Vec<_> = state.wirable().iter().map(|h| h.probe.id).collect();
        assert_eq!(ids, vec!["gemini"]);
    }

    /// A running-but-unsupervised daemon is the trap the old installer left
    /// people in: `dira daemon start` works, and then nothing survives a
    /// reboot. The wizard has to treat that as work still to do.
    #[test]
    fn a_bare_started_daemon_is_running_but_not_supervised() {
        let base = State {
            harnesses: Vec::new(),
            supervision: Supervision::Pidfile(42),
            repo_root: None,
            has_zavet_dir: false,
            claude_present: false,
            zavet_installed: false,
            device_linked: false,
            knowledge: dira_core::config::KnowledgeSyncMode::Off,
        };
        assert!(base.daemon_running());
        assert!(!base.supervised(), "a pidfile daemon dies with the session");

        for sup in [
            Supervision::Launchd,
            Supervision::SystemdUser,
            Supervision::ScheduledTask,
        ] {
            let s = State {
                supervision: sup,
                ..base.clone()
            };
            assert!(s.supervised());
            assert!(s.daemon_running());
        }

        let s = State {
            supervision: Supervision::NotRunning,
            ..base.clone()
        };
        assert!(!s.daemon_running());
        assert!(!s.supervised());
    }

    /// Per-harness knowledge is spread over three tables that no compiler
    /// check ties together: `dira_sources::canonical_harness_id` (the wire
    /// ids and their aliases), `init::WIRABLE` (what can actually be wired),
    /// and this module's `HARNESSES` (what onboarding probes for). A harness
    /// added to one and forgotten in another fails at runtime, not at build
    /// time — and did: `--harness generic` passed validation against the
    /// alias table and then had no dispatch arm.
    ///
    /// This pins all three together. It is not a substitute for merging them,
    /// but it is what turns the next omission into a red test.
    #[test]
    fn the_three_harness_tables_agree() {
        for p in HARNESSES {
            assert_eq!(
                dira_sources::canonical_harness_id(p.id),
                Some(p.id),
                "{} is not a canonical harness id",
                p.id
            );
            assert!(
                crate::init::is_wirable(p.id),
                "{} is probed for but cannot be wired",
                p.id
            );
        }
        for id in crate::init::WIRABLE {
            assert!(
                HARNESSES.iter().any(|p| p.id == *id),
                "{id} can be wired but is never detected, so onboarding will not offer it"
            );
        }
    }

    /// `generic` is a real wire id — the hook ingest path uses it for payloads
    /// from an unrecognised harness — but there is no config to write for it.
    /// It must therefore be rejected by the wirable gate, which is what stops
    /// `--harness generic` from failing mid-run.
    #[test]
    fn generic_is_a_wire_id_but_not_a_wirable_harness() {
        assert_eq!(
            dira_sources::canonical_harness_id("generic"),
            Some("generic")
        );
        assert!(!crate::init::is_wirable("generic"));
    }

    #[test]
    fn repo_reports_no_root_outside_a_work_tree() {
        let dir = tmp_home("norepo");
        let (root, has) = repo(&dir);
        assert!(root.is_none(), "a bare temp dir is not a work tree");
        assert!(!has);
    }
}
