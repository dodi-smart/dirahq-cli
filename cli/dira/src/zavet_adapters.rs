//! Repo-scope zavet adapter/git-hook refresh, invoked as a best-effort tail
//! of `dira zavet install`/`status` — never as a standalone command.
//!
//! `zavet_install.rs` is machine-scope: it neither knows nor cares which repo
//! `cwd` is in. This module is the opposite — it exists to run
//! `zavet adapters`, which writes TRACKED files into whatever working tree
//! the process happens to be standing in. That asymmetry is why this is a
//! separate module with its own safety spine rather than a few lines bolted
//! onto `install_with`: folding it in unconditionally would mean a machine-
//! scope install command silently rewriting committed files in an unrelated
//! repo the moment it's run from the wrong `cwd`.
//!
//! Two rules are non-negotiable:
//!
//! 1. This module **never** runs `zavet hooks install`. `core.hooksPath` is
//!    not zavet's alone to own — Husky, lefthook, and plain pre-commit all
//!    set it too, and zavet itself refuses to take it over. The most this
//!    prints is a `githooks: inactive — run …` line; `hooks install` must
//!    never appear in an argv this module builds.
//! 2. This module **never** invokes `zavet adapters` (or even
//!    `adapters --check`) unless `cwd` resolves to a git toplevel that
//!    already carries a `.zavet/` directory. `zavet adapters` run from a
//!    non-git directory writes its artifacts into that directory — verified
//!    empirically: from `$HOME` it prints "not inside a git repository" and
//!    then reports all six artifacts missing and exits 1, which is
//!    indistinguishable by exit code alone from "adapters are stale". Gating
//!    happens in THIS module, before zavet is ever spawned — zavet's own
//!    non-zero exit is not a substitute.
//!
//! A third trap this module guards against: zavet 1.2.0 has no `adapters`
//! subcommand at all. Running `adapters --check` against it prints general
//! usage/help text and exits 1 — the SAME exit code 1.3.0 uses for "stale".
//! Exit code alone cannot feature-detect, so [`supports_adapters`] parses
//! `zavet version --json` and gates on it before `adapters --check` is ever
//! invoked.

use crate::zavet_install::{command_line, Runner};
use std::path::{Path, PathBuf};

/// Where `cwd` stands relative to a repo `zavet adapters` may safely touch.
pub(crate) enum RepoGate {
    /// `cwd` resolves to a git toplevel that carries `.zavet/`.
    Eligible(PathBuf),
    /// `cwd` is not inside a git work tree at all.
    NotGit,
    /// `cwd` is a git toplevel, but it has never adopted zavet (no `.zavet/`).
    NoZavetDir(PathBuf),
}

/// What [`adapter_lines`] should do once the repo gate passes and the
/// installed zavet is new enough.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdapterMode {
    /// Read-only: report staleness, never write (`dira zavet install`'s
    /// already-installed-no-update no-op, and `dira zavet status`).
    CheckOnly,
    /// Write when stale (the real, non-dry-run post-update path).
    Refresh,
    /// Print what WOULD run, without running anything (`--dry-run`).
    Plan,
}

/// Pure: derive a [`RepoGate`] from an already-resolved git toplevel (or
/// `None` when `cwd` isn't inside a work tree). Split out from [`repo_gate`]
/// so the gate logic is unit-testable without shelling out to `git`.
fn gate_from_toplevel(toplevel: Option<PathBuf>) -> RepoGate {
    match toplevel {
        None => RepoGate::NotGit,
        Some(root) => {
            if root.join(dira_core::zavet::ZAVET_DIR).is_dir() {
                RepoGate::Eligible(root)
            } else {
                RepoGate::NoZavetDir(root)
            }
        }
    }
}

/// Resolve the [`RepoGate`] for `cwd` by shelling out to `git` (via
/// `dira_core::project::toplevel`).
fn repo_gate(cwd: &Path) -> RepoGate {
    gate_from_toplevel(dira_core::project::toplevel(cwd))
}

/// Whether an installed zavet version string supports `zavet adapters`
/// (introduced in 1.3.0). Mirrors `zavet_install::skew_line`'s own handling
/// of prerelease tags: only the release triple is compared, exactly the
/// comparison zavet's own `min_dira` skew check already performs, so the two
/// version guards in this codebase agree on what "at least X.Y.Z" means.
///
/// zavet's own dep tree is not this crate's to inspect, and `semver` is
/// already a workspace dependency of `dira`, so this reuses it directly
/// rather than hand-rolling a triple parse.
fn supports_adapters(version: &str) -> bool {
    match semver::Version::parse(version) {
        Ok(v) => (v.major, v.minor, v.patch) >= (1, 3, 0),
        Err(_) => false,
    }
}

/// `zavet version --json`'s `version` field, pinned to `root` so a fresh
/// clone-not-yet-cwd'd process still asks the right binary about the right
/// repo (matters once zavet supports per-repo overrides; harmless today).
#[derive(Debug, serde::Deserialize)]
struct VersionInfo {
    version: String,
}

/// The full adapters + git-hook report for one gated (or not) repo. Never
/// returns `Err` — by the time this runs, the plugin install this is a tail
/// of has already succeeded, so any repo-side problem is a printed warning,
/// never a reason to fail the whole `dira zavet install`/`status` call.
pub(crate) fn adapter_lines(
    runner: &dyn Runner,
    zavet_bin: &str,
    gate: &RepoGate,
    mode: AdapterMode,
) -> Vec<String> {
    let root = match gate {
        RepoGate::NotGit => {
            return vec!["adapters: not checked (not inside a git repo)".to_string()]
        }
        RepoGate::NoZavetDir(root) => {
            return vec![format!(
                "adapters: not checked ({} has no .zavet/)",
                root.display()
            )]
        }
        RepoGate::Eligible(root) => root,
    };

    let version = match runner.run_in(root, zavet_bin, &["version", "--json"]) {
        Some(out) if out.status.success() => {
            match serde_json::from_slice::<VersionInfo>(&out.stdout) {
                Ok(v) => v.version,
                Err(_) => {
                    return vec![
                        "adapters: unknown (`zavet version --json` did not answer)".to_string()
                    ]
                }
            }
        }
        _ => return vec!["adapters: unknown (`zavet version --json` did not answer)".to_string()],
    };
    if !supports_adapters(&version) {
        return vec![format!(
            "adapters: not checked (installed zavet {version} has no `adapters` — needs 1.3.0+)"
        )];
    }

    let mut lines = Vec::new();

    if mode == AdapterMode::Plan {
        lines.push(format!(
            "[dry-run] {}   # in {}",
            command_line(zavet_bin, &["adapters"]),
            root.display()
        ));
        lines.push("(runs only if `zavet adapters --check` reports stale)".to_string());
        return lines;
    }

    match runner.run_in(root, zavet_bin, &["adapters", "--check"]) {
        None => lines.push(format!(
            "adapters: unknown (could not run `{zavet_bin} adapters --check` in {})",
            root.display()
        )),
        Some(out) if out.status.success() => {
            lines.push(format!("adapters: in sync ({})", root.display()))
        }
        Some(_) if mode == AdapterMode::CheckOnly => lines.push(format!(
            "adapters: stale ({}) — run `zavet adapters`",
            root.display()
        )),
        Some(_) => {
            // mode == Refresh: echo the command (same convention as
            // `run_or_print`'s real-run echo) then actually run it.
            println!("{}", command_line(zavet_bin, &["adapters"]));
            match runner.run_in(root, zavet_bin, &["adapters"]) {
                Some(out) if out.status.success() => {
                    lines.push(format!("adapters: refreshed ({})", root.display()))
                }
                Some(out) => lines.push(format!(
                    "adapters: `zavet adapters` failed (exit {:?}) — run it by hand in {}",
                    out.status.code(),
                    root.display()
                )),
                None => lines.push(format!(
                    "adapters: `zavet adapters` failed to run — run it by hand in {}",
                    root.display()
                )),
            }
        }
    }

    // Git-hook floor: appended only once we know the installed zavet is new
    // enough to have an opinion, but this module NEVER installs hooks — see
    // the module doc. `hooks install` must never appear in any argv built
    // below.
    match runner.run_in(root, zavet_bin, &["hooks", "--check"]) {
        Some(out) if out.status.success() => lines.push("githooks: active".to_string()),
        Some(_) => lines.push(format!(
            "githooks: inactive — run `zavet hooks install` in {} (dira never sets core.hooksPath)",
            root.display()
        )),
        None => {} // omit the line entirely rather than guess
    }

    lines
}

/// Entry point used by `zavet_install`: gate `cwd`, then delegate to
/// [`adapter_lines`] using the PLUGIN ROOT's own `bin/zavet` — never the
/// repo's vendored `.zavet/bin/zavet` copy, which is precisely the stale
/// artifact being regenerated.
pub(crate) fn status_lines(
    runner: &dyn Runner,
    plugin_root: &str,
    cwd: &Path,
    mode: AdapterMode,
) -> Vec<String> {
    let bin = format!("{}/bin/zavet", plugin_root.trim_end_matches('/'));
    let gate = repo_gate(cwd);
    adapter_lines(runner, &bin, &gate, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zavet_install::test_support::FakeRunner;

    fn eligible(root: &Path) -> RepoGate {
        RepoGate::Eligible(root.to_path_buf())
    }

    // -- the two mandatory safety regression tests (Step 1) --------------------

    /// `RepoGate::NotGit` must short-circuit before ANY zavet call. The
    /// `FakeRunner` here has zero commands stubbed, so if `adapter_lines` ever
    /// tried to shell out, `run_in` would return `None` and downstream code
    /// would either bail or misreport — the test asserting a specific,
    /// correct "not a git repo" line, with nothing stubbed, is itself the
    /// proof that no call was attempted.
    #[test]
    fn adapters_never_run_outside_a_gated_repo() {
        let runner = FakeRunner::default();
        let lines = adapter_lines(
            &runner,
            "/plugin/bin/zavet",
            &RepoGate::NotGit,
            AdapterMode::Refresh,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("not inside a git repo"), "{:?}", lines);
        assert!(
            runner.call_log().is_empty(),
            "no zavet call should have been attempted: {:?}",
            runner.call_log()
        );
    }

    /// The exit-code trap: zavet 1.2.0 has no `adapters` subcommand, and
    /// `adapters --check` against it prints help text and exits 1 — the same
    /// code 1.3.0 uses for "stale". Only `version --json` is stubbed here;
    /// `adapters --check` is deliberately left UNSTUBBED, so if the version
    /// guard were missing or wrong, this test would fail via `FakeRunner`
    /// returning `None` for an unstubbed call and the wrong line coming back.
    #[test]
    fn adapters_skipped_when_installed_zavet_predates_1_3_0() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let runner = FakeRunner::default().returning_in(
            root.path(),
            "/plugin/bin/zavet",
            &["version", "--json"],
            0,
            r#"{"v":1,"plugin":"zavet","version":"1.2.0","emit_schema":1,"min_dira":"0.1.0"}"#,
        );
        let lines = adapter_lines(
            &runner,
            "/plugin/bin/zavet",
            &eligible(root.path()),
            AdapterMode::Refresh,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("1.2.0"), "{:?}", lines);
        assert!(lines[0].contains("1.3.0"), "{:?}", lines);
        assert!(
            !runner
                .call_log()
                .iter()
                .any(|(_, prog, args)| prog == "/plugin/bin/zavet"
                    && args.first().map(String::as_str) == Some("adapters")),
            "adapters --check must never appear in the call log: {:?}",
            runner.call_log()
        );
    }

    // -- gate_from_toplevel -----------------------------------------------------

    #[test]
    fn gate_from_toplevel_not_git_when_no_toplevel() {
        assert!(matches!(gate_from_toplevel(None), RepoGate::NotGit));
    }

    #[test]
    fn gate_from_toplevel_no_zavet_dir_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        match gate_from_toplevel(Some(dir.path().to_path_buf())) {
            RepoGate::NoZavetDir(root) => assert_eq!(root, dir.path()),
            _ => panic!("expected NoZavetDir"),
        }
    }

    #[test]
    fn gate_from_toplevel_eligible_when_zavet_dir_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".zavet")).unwrap();
        match gate_from_toplevel(Some(dir.path().to_path_buf())) {
            RepoGate::Eligible(root) => assert_eq!(root, dir.path()),
            _ => panic!("expected Eligible"),
        }
    }

    // -- supports_adapters --------------------------------------------------------

    #[test]
    fn supports_adapters_semver_boundaries() {
        assert!(!supports_adapters("1.2.9"));
        assert!(supports_adapters("1.3.0"));
        assert!(supports_adapters("1.4.0"));
        assert!(supports_adapters("2.0.0"));
        // Prerelease tags compare on the release triple only, same as
        // `zavet_install::skew_line`.
        assert!(supports_adapters("1.3.0-develop.4"));
        assert!(!supports_adapters("1.2.9-develop.4"));
        assert!(!supports_adapters("not-a-version"));
    }

    // -- adapter_lines: in-sync / stale / refresh / dry-run / hooks -------------

    fn stub_version(runner: FakeRunner, root: &Path, bin: &str, version: &str) -> FakeRunner {
        runner.returning_in(
            root,
            bin,
            &["version", "--json"],
            0,
            &format!(r#"{{"v":1,"plugin":"zavet","version":"{version}","emit_schema":1,"min_dira":"0.1.0"}}"#),
        )
    }

    #[test]
    fn adapters_in_sync_is_a_read_only_noop() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let bin = "/plugin/bin/zavet";
        let runner = stub_version(FakeRunner::default(), root.path(), bin, "1.3.0")
            .returning_in(root.path(), bin, &["adapters", "--check"], 0, "")
            .returning_in(root.path(), bin, &["hooks", "--check"], 0, "");
        let lines = adapter_lines(&runner, bin, &eligible(root.path()), AdapterMode::Refresh);
        assert!(lines.iter().any(|l| l.contains("in sync")), "{:?}", lines);
        assert!(
            !runner
                .call_log()
                .iter()
                .any(|(_, _, args)| args == &vec!["adapters".to_string()]),
            "a bare `adapters` write must never run when in sync: {:?}",
            runner.call_log()
        );
    }

    #[test]
    fn adapters_stale_check_only_reports_but_does_not_write() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let bin = "/plugin/bin/zavet";
        let runner = stub_version(FakeRunner::default(), root.path(), bin, "1.3.0")
            .returning_in(root.path(), bin, &["adapters", "--check"], 1, "")
            .returning_in(root.path(), bin, &["hooks", "--check"], 0, "");
        let lines = adapter_lines(&runner, bin, &eligible(root.path()), AdapterMode::CheckOnly);
        assert!(lines.iter().any(|l| l.contains("stale")), "{:?}", lines);
        assert!(
            !runner
                .call_log()
                .iter()
                .any(|(_, _, args)| args == &vec!["adapters".to_string()]),
            "CheckOnly must never run a bare `adapters`: {:?}",
            runner.call_log()
        );
    }

    #[test]
    fn adapters_stale_refresh_runs_adapters_in_the_repo_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let bin = "/plugin/bin/zavet";
        let runner = stub_version(FakeRunner::default(), root.path(), bin, "1.3.0")
            .returning_in(root.path(), bin, &["adapters", "--check"], 1, "")
            .returning_in(root.path(), bin, &["adapters"], 0, "wrote 6 files")
            .returning_in(root.path(), bin, &["hooks", "--check"], 0, "");
        let lines = adapter_lines(&runner, bin, &eligible(root.path()), AdapterMode::Refresh);
        assert!(lines.iter().any(|l| l.contains("refreshed")), "{:?}", lines);
        assert!(
            runner
                .call_log()
                .iter()
                .any(|(dir, _, args)| dir == root.path() && args == &vec!["adapters".to_string()]),
            "adapters must run pinned to the repo root: {:?}",
            runner.call_log()
        );
    }

    #[test]
    fn adapters_refresh_failure_is_reported_not_fatal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let bin = "/plugin/bin/zavet";
        let runner = stub_version(FakeRunner::default(), root.path(), bin, "1.3.0")
            .returning_in(root.path(), bin, &["adapters", "--check"], 1, "")
            .returning_in_failing(root.path(), bin, &["adapters"], 3, "boom")
            .returning_in(root.path(), bin, &["hooks", "--check"], 0, "");
        let lines = adapter_lines(&runner, bin, &eligible(root.path()), AdapterMode::Refresh);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("failed") && l.contains("by hand")),
            "{:?}",
            lines
        );
    }

    #[test]
    fn dry_run_plans_adapters_without_running_them() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let bin = "/plugin/bin/zavet";
        // `adapters --check` / `adapters` / `hooks --check` are deliberately
        // NOT stubbed: Plan must return before any of them is attempted.
        let runner = stub_version(FakeRunner::default(), root.path(), bin, "1.3.0");
        let lines = adapter_lines(&runner, bin, &eligible(root.path()), AdapterMode::Plan);
        assert!(
            lines.iter().any(|l| l.starts_with("[dry-run]")),
            "{:?}",
            lines
        );
        assert!(
            !runner
                .call_log()
                .iter()
                .any(
                    |(_, _, args)| args.first().map(String::as_str) == Some("adapters")
                        && args != &vec!["version".to_string(), "--json".to_string()]
                ),
            "Plan must not invoke adapters/hooks at all: {:?}",
            runner.call_log()
        );
    }

    #[test]
    fn githook_floor_is_reported_never_installed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".zavet")).unwrap();
        let bin = "/plugin/bin/zavet";
        let runner = stub_version(FakeRunner::default(), root.path(), bin, "1.3.0")
            .returning_in(root.path(), bin, &["adapters", "--check"], 0, "")
            .returning_in(root.path(), bin, &["hooks", "--check"], 1, "");
        let lines = adapter_lines(&runner, bin, &eligible(root.path()), AdapterMode::Refresh);
        assert!(
            lines.iter().any(|l| l.contains("githooks: inactive")),
            "{:?}",
            lines
        );
        assert!(
            !runner
                .call_log()
                .iter()
                .any(|(_, _, args)| args.contains(&"install".to_string())),
            "`hooks install` must never appear in any argv: {:?}",
            runner.call_log()
        );
    }
}
