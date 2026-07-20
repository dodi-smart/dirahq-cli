//! `dira zavet install` — install/update the zavet Claude Code plugin, and
//! the plugin summary line `dira zavet status` prints alongside it.
//!
//! **Shells out to the `claude` CLI. Never hand-edits Claude Code's own
//! config.** `dira init` writes `.claude/settings.json` directly, but that
//! precedent does not transfer here: plugin install is *stateful* (clone a
//! marketplace into `~/.claude/plugins/marketplaces/`, checkout a version
//! into `~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/`, plus a
//! registry file — `installed_plugins.json` — that already carries a
//! `"version": 2` schema of its own). Hand-writing `extraKnownMarketplaces`
//! would declare a marketplace that was never actually cloned, and would
//! race a live Claude Code session rewriting the same files.
//!
//! ## The four-item stable contract this module depends on
//!
//! Documented in full in `docs/zavet.md`'s "Install" section — changing any
//! of these is a breaking change needing a coordinated release:
//!
//! 1. marketplace `dirahq` + plugin `zavet` ⇒ id [`PLUGIN_ID`] (`zavet@dirahq`).
//! 2. repo slug [`MARKETPLACE_REPO`], manifest at `.claude-plugin/marketplace.json`,
//!    default branch `main` (a github marketplace source records no `ref`,
//!    so `claude plugin marketplace add` always clones the default branch).
//! 3. `bin/zavet` at the plugin root supports `version` / `version --json`.
//! 4. guard-event schema v1 on stdin of `dira zavet emit` (unrelated to
//!    installation, but part of the same cross-repo contract).
//!
//! ## Detection, not assumption
//!
//! State is always read back from `claude` itself before anything runs:
//! `claude plugin list --json` first (the documented machine-readable API,
//! confirmed against `claude` 2.1.211's own `--help`), falling back to
//! reading `installed_plugins.json` directly only when that fails — and only
//! trusting the fallback when its top-level `"version"` field is exactly
//! `2`, since a schema bump would otherwise silently misparse. When neither
//! source resolves cleanly, detection reports [`Detection::Unknown`] and
//! [`install`] falls through to the same marketplace-add + install commands
//! it would run for a fresh machine — `claude` itself is the authority on
//! whether that is a no-op, an install, or a state it (correctly) rejects.
//!
//! ## Advisory skew only
//!
//! The version-skew line compares this dira build against the plugin's
//! self-reported `min_dira` (`<installPath>/bin/zavet version --json`). It
//! is informational only and never blocks anything — the whole product
//! promise is that dira and zavet each work fully without the other. An
//! installed plugin build that predates the `version` subcommand (true of
//! every zavet build before the T15 landing) degrades to an "unknown" skew
//! line rather than an error.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Marketplace name Claude Code registers `dirahq-zavet` under.
pub const MARKETPLACE_NAME: &str = "dirahq";
/// The repo slug passed to `claude plugin marketplace add`.
pub const MARKETPLACE_REPO: &str = "dodi-smart/dirahq-zavet";
/// `claude plugin list --json` / `installed_plugins.json` id: `<plugin>@<marketplace>`.
pub const PLUGIN_ID: &str = "zavet@dirahq";

/// A parsed `dira zavet install` invocation, independent of clap.
#[derive(Debug, Clone)]
pub struct InstallArgs {
    /// `claude plugin install --scope` value: `user` (default), `project`, or `local`.
    pub scope: String,
    /// Already installed: refresh the marketplace + plugin instead of a no-op.
    pub update: bool,
    /// Print the exact `claude` invocations without running them.
    pub dry_run: bool,
}

/// `dira zavet install`.
pub fn install(args: InstallArgs) -> Result<()> {
    let home = home_dir()?;
    let claude_present = resolve_claude_on_path().is_some();
    install_with(&args, &SystemRunner, &home, claude_present)
}

/// The plugin summary line for `dira zavet status`. Best-effort and
/// read-only: `None` when `claude` isn't on `PATH` or detection is
/// inconclusive, so a machine without Claude Code installed sees an
/// unchanged `zavet status`.
pub fn status_line() -> Option<String> {
    resolve_claude_on_path()?;
    let home = home_dir().ok()?;
    match detect(&SystemRunner, &home) {
        Detection::Installed(info) => Some(format!(
            "plugin: {PLUGIN_ID}  version {}  scope {}  enabled {}",
            info.version,
            info.scope,
            info.enabled
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        )),
        Detection::NotInstalled => Some("plugin: not installed (`dira zavet install`)".to_string()),
        Detection::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// Runner — the same shell-out-and-mock pattern as `daemon::Runner`, so
// detection + install are unit-testable without touching a real `claude`
// install or the user's real plugin registry.
// ---------------------------------------------------------------------------

/// A probe for an external command's presence/exit status/stdout.
pub trait Runner {
    /// Run `prog args…` and return its `Output`, or `None` if the command
    /// could not even be spawned — mirrors `Command::spawn` failing with
    /// `NotFound`.
    fn run(&self, prog: &str, args: &[&str]) -> Option<Output>;
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, prog: &str, args: &[&str]) -> Option<Output> {
        Command::new(prog).args(args).output().ok()
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// The zavet plugin's state as read back from Claude Code.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detection {
    Installed(InstalledInfo),
    NotInstalled,
    /// Neither `claude plugin list --json` nor a schema-`2`
    /// `installed_plugins.json` resolved cleanly.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstalledInfo {
    version: String,
    scope: String,
    install_path: String,
    /// `None` when the source that reported this record doesn't carry
    /// `enabled` (the `installed_plugins.json` fallback doesn't).
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PluginListEntry {
    id: String,
    version: String,
    scope: String,
    #[serde(default)]
    enabled: bool,
    #[serde(rename = "installPath")]
    install_path: String,
}

#[derive(Debug, Deserialize)]
struct InstalledPluginsFile {
    version: u64,
    plugins: HashMap<String, Vec<InstalledPluginRecord>>,
}

#[derive(Debug, Deserialize)]
struct InstalledPluginRecord {
    scope: String,
    #[serde(rename = "installPath")]
    install_path: String,
    version: String,
}

/// The `installed_plugins.json` schema this module knows how to read. Guards
/// the fallback path: a future schema bump changes this number, and an old
/// binary correctly refuses to trust a file it no longer understands.
const KNOWN_INSTALLED_PLUGINS_SCHEMA: u64 = 2;

/// Detect the current state of `zavet@dirahq`: `claude plugin list --json`
/// first, falling back to `installed_plugins.json` (only when its top-level
/// `"version"` is [`KNOWN_INSTALLED_PLUGINS_SCHEMA`]) only if that fails.
fn detect(runner: &dyn Runner, home: &Path) -> Detection {
    if let Some(out) = runner.run("claude", &["plugin", "list", "--json"]) {
        if out.status.success() {
            if let Ok(entries) = serde_json::from_slice::<Vec<PluginListEntry>>(&out.stdout) {
                return match entries.into_iter().find(|e| e.id == PLUGIN_ID) {
                    Some(e) => Detection::Installed(InstalledInfo {
                        version: e.version,
                        scope: e.scope,
                        install_path: e.install_path,
                        enabled: Some(e.enabled),
                    }),
                    None => Detection::NotInstalled,
                };
            }
        }
    }

    if let Some(detected) = detect_from_installed_plugins_json(home) {
        return detected;
    }

    Detection::Unknown
}

fn detect_from_installed_plugins_json(home: &Path) -> Option<Detection> {
    let path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    let text = std::fs::read_to_string(path).ok()?;
    let file: InstalledPluginsFile = serde_json::from_str(&text).ok()?;
    if file.version != KNOWN_INSTALLED_PLUGINS_SCHEMA {
        return None;
    }
    Some(match file.plugins.get(PLUGIN_ID).and_then(|v| v.first()) {
        Some(rec) => Detection::Installed(InstalledInfo {
            version: rec.version.clone(),
            scope: rec.scope.clone(),
            install_path: rec.install_path.clone(),
            enabled: None,
        }),
        None => Detection::NotInstalled,
    })
}

#[derive(Debug, Deserialize)]
struct MarketplaceEntry {
    source: MarketplaceSource,
    #[serde(rename = "installLocation")]
    install_location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarketplaceSource {
    source: String,
    path: Option<String>,
}

/// Resolve the directory that actually holds the plugin's files.
///
/// Both detection sources report a versioned cache path
/// (`~/.claude/plugins/cache/<marketplace>/<plugin>/<version>`), but that
/// directory only exists for `github`-sourced marketplaces. A `directory`
/// source — which is how anyone *developing* zavet has it registered, and
/// therefore the common case for `dira zavet status` — is loaded in place
/// and never populates the cache, so the reported path names a directory
/// that does not exist. Without this, the skew check would always degrade
/// to "unknown" for exactly the people most likely to look at it.
///
/// Prefer the reported path when it really exists; otherwise fall back to
/// the marketplace's own on-disk location.
fn resolve_plugin_root(home: &Path, reported: &str) -> String {
    if Path::new(reported).is_dir() {
        return reported.to_string();
    }
    marketplace_directory_location(home).unwrap_or_else(|| reported.to_string())
}

/// The on-disk location of the `dirahq` marketplace, but only when it is a
/// `directory` source. A `github` source's `installLocation` points at the
/// bare clone, not a usable plugin root, so it is deliberately ignored.
fn marketplace_directory_location(home: &Path) -> Option<String> {
    let path = home
        .join(".claude")
        .join("plugins")
        .join("known_marketplaces.json");
    let text = std::fs::read_to_string(path).ok()?;
    let all: HashMap<String, MarketplaceEntry> = serde_json::from_str(&text).ok()?;
    let entry = all.get(MARKETPLACE_NAME)?;
    if entry.source.source != "directory" {
        return None;
    }
    entry
        .source
        .path
        .clone()
        .or_else(|| entry.install_location.clone())
        .filter(|p| Path::new(p).is_dir())
}

// ---------------------------------------------------------------------------
// PATH resolution for `claude` — no `which`-style crate dependency; every
// dep this module needs (`serde`, `serde_json`, `semver`) is already a
// direct dep of `cli/dira`.
// ---------------------------------------------------------------------------

fn resolve_claude_on_path() -> Option<PathBuf> {
    resolve_on_path_in("claude", std::env::var_os("PATH")?.as_os_str())
}

fn resolve_on_path_in(prog: &str, path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var).find_map(|dir| {
        let candidate = dir.join(prog);
        is_executable_file(&candidate).then_some(candidate)
    })
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

/// The two-line manual recipe printed (and the reason `install` bails
/// non-zero) when `claude` isn't on `PATH`. **Never half-install**: without
/// `claude` there is nothing safe to shell out to, so this is the only exit
/// early enough that no marketplace/plugin state has been touched.
const NO_CLAUDE_RECIPE: &str = "\
`claude` was not found on PATH — install zavet by hand from inside a Claude Code session:
  /plugin marketplace add dodi-smart/dirahq-zavet
  /plugin install zavet@dirahq";

fn validate_scope(scope: &str) -> Result<()> {
    match scope {
        "user" | "project" | "local" => Ok(()),
        other => anyhow::bail!("unknown --scope `{other}` (expected: user, project, local)"),
    }
}

// ---------------------------------------------------------------------------
// Install / update
// ---------------------------------------------------------------------------

fn install_with(
    args: &InstallArgs,
    runner: &dyn Runner,
    home: &Path,
    claude_present: bool,
) -> Result<()> {
    if !claude_present {
        anyhow::bail!(NO_CLAUDE_RECIPE);
    }
    validate_scope(&args.scope)?;

    match detect(runner, home) {
        Detection::Installed(info) if !args.update => {
            println!("zavet@dirahq is already installed — no-op (pass --update to refresh)");
            report(&info, runner, home);
            Ok(())
        }
        Detection::Installed(info) => {
            run_or_print(
                runner,
                args.dry_run,
                &["plugin", "marketplace", "update", MARKETPLACE_NAME],
            )?;
            run_or_print(
                runner,
                args.dry_run,
                &["plugin", "update", PLUGIN_ID, "--scope", &info.scope],
            )?;
            finish(args, runner, home)
        }
        Detection::NotInstalled => {
            run_or_print(
                runner,
                args.dry_run,
                &["plugin", "marketplace", "add", MARKETPLACE_REPO],
            )?;
            run_or_print(
                runner,
                args.dry_run,
                &["plugin", "install", PLUGIN_ID, "--scope", &args.scope],
            )?;
            finish(args, runner, home)
        }
        Detection::Unknown => {
            println!(
                "could not determine zavet's current state locally (`claude plugin list --json` \
                 and `installed_plugins.json` were both inconclusive) — asking `claude` directly"
            );
            run_or_print(
                runner,
                args.dry_run,
                &["plugin", "marketplace", "add", MARKETPLACE_REPO],
            )?;
            run_or_print(
                runner,
                args.dry_run,
                &["plugin", "install", PLUGIN_ID, "--scope", &args.scope],
            )?;
            finish(args, runner, home)
        }
    }
}

/// After a real (non-dry-run) install/update: re-detect so the report
/// reflects the freshly-written state, then remind the user to restart.
fn finish(args: &InstallArgs, runner: &dyn Runner, home: &Path) -> Result<()> {
    if args.dry_run {
        println!("(dry run — nothing was changed)");
        return Ok(());
    }
    match detect(runner, home) {
        Detection::Installed(info) => report(&info, runner, home),
        _ => println!(
            "ran the `claude` commands above, but re-detection came back inconclusive — \
             run `dira zavet status` to confirm"
        ),
    }
    println!("restart Claude Code to apply");
    Ok(())
}

/// Print (dry-run) or run (real) one `claude` subcommand. Real runs echo the
/// command before executing it, matching the dry-run's own line, so the two
/// modes read identically apart from the `[dry-run]` prefix.
fn run_or_print(runner: &dyn Runner, dry_run: bool, args: &[&str]) -> Result<()> {
    let line = command_line(args);
    if dry_run {
        println!("[dry-run] {line}");
        return Ok(());
    }
    println!("{line}");
    match runner.run("claude", args) {
        Some(out) if out.status.success() => Ok(()),
        Some(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "`{line}` failed (exit {:?}){}",
                out.status.code(),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            );
        }
        None => anyhow::bail!("failed to run `{line}` — is `claude` still on PATH?"),
    }
}

fn command_line(args: &[&str]) -> String {
    std::iter::once("claude")
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn report(info: &InstalledInfo, runner: &dyn Runner, home: &Path) {
    println!(
        "{PLUGIN_ID}  version {}  scope {}",
        info.version, info.scope
    );
    let root = resolve_plugin_root(home, &info.install_path);
    if root == info.install_path {
        println!("install path: {}", info.install_path);
    } else {
        // The recorded cache path doesn't exist — say so rather than
        // printing a directory the user cannot inspect.
        println!("install path: {root}  (directory-sourced marketplace)");
    }
    if let Some(enabled) = info.enabled {
        println!("enabled: {enabled}");
    }
    println!("{}", skew_line(runner, &root, env!("CARGO_PKG_VERSION")));
}

// ---------------------------------------------------------------------------
// Advisory version skew
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ZavetVersionInfo {
    min_dira: String,
}

/// `<installPath>/bin/zavet version --json`, compared against this dira
/// build. Always advisory text, never an error — an installed plugin build
/// older than the T15 `version` subcommand (or any other failure: missing
/// binary, non-zero exit, unparseable output) degrades to "unknown".
fn skew_line(runner: &dyn Runner, install_path: &str, dira_version: &str) -> String {
    let bin = format!("{}/bin/zavet", install_path.trim_end_matches('/'));
    let min_dira = match runner.run(&bin, &["version", "--json"]) {
        Some(out) if out.status.success() => {
            match serde_json::from_slice::<ZavetVersionInfo>(&out.stdout) {
                Ok(v) => v.min_dira,
                Err(_) => {
                    return "skew: unknown (`zavet version --json` output was not valid JSON)"
                        .to_string()
                }
            }
        }
        _ => {
            return "skew: unknown (installed zavet build has no `version --json` — advisory \
                     only, never gates anything)"
                .to_string()
        }
    };

    match (
        semver::Version::parse(dira_version),
        semver::Version::parse(&min_dira),
    ) {
        // Compare release triples only (major.minor.patch), dropping any
        // prerelease tag on dira's own side: a `-develop.N` build's
        // semver-spec ordering is *below* its own release
        // (`0.1.0-develop.10 < 0.1.0`), which would otherwise flag every
        // ordinary prerelease dev build as "too old" against a release-line
        // `min_dira` it is about to become.
        (Ok(mine), Ok(min))
            if (mine.major, mine.minor, mine.patch) < (min.major, min.minor, min.patch) =>
        {
            format!(
                "skew: this dira build ({dira_version}) is older than zavet's advertised \
                 minimum ({min_dira}) — advisory only, nothing is blocked"
            )
        }
        (Ok(_), Ok(_)) => {
            format!("skew: dira {dira_version} satisfies zavet's min_dira {min_dira}")
        }
        _ => "skew: unknown (could not parse a version string as semver)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// A scripted [`Runner`]: `key(prog, args) -> (exit_code, stdout, stderr)`.
    /// A missing key means the command wasn't stubbed and behaves like
    /// `Command::spawn` failing to find it — mirrors `daemon::tests::FakeRunner`.
    #[derive(Default)]
    struct FakeRunner {
        responses: HashMap<String, (i32, String, String)>,
    }

    impl FakeRunner {
        fn returning(mut self, prog: &str, args: &[&str], code: i32, stdout: &str) -> Self {
            self.responses
                .insert(key(prog, args), (code, stdout.to_string(), String::new()));
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
            let (code, stdout, stderr) = self.responses.get(&key(prog, args))?;
            Some(Output {
                status: ExitStatus::from_raw(*code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            })
        }
    }

    fn temp_home(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "dira-zavet-install-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join(".claude").join("plugins")).unwrap();
        base
    }

    fn write_installed_plugins_json(home: &Path, contents: &str) {
        std::fs::write(
            home.join(".claude")
                .join("plugins")
                .join("installed_plugins.json"),
            contents,
        )
        .unwrap();
    }

    fn write_known_marketplaces_json(home: &Path, contents: &str) {
        std::fs::write(
            home.join(".claude")
                .join("plugins")
                .join("known_marketplaces.json"),
            contents,
        )
        .unwrap();
    }

    /// A `directory`-sourced marketplace is loaded in place and never
    /// populates the versioned cache dir, yet the registry still reports a
    /// cache path. Resolution must fall back to the real directory, or the
    /// skew check is permanently "unknown" for everyone developing zavet.
    #[test]
    fn resolve_plugin_root_falls_back_for_directory_sourced_marketplace() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude").join("plugins")).unwrap();
        let real = home.path().join("workspace").join("dirahq-zavet");
        std::fs::create_dir_all(&real).unwrap();

        write_known_marketplaces_json(
            home.path(),
            &format!(
                r#"{{"dirahq":{{"source":{{"source":"directory","path":"{}"}},
                     "installLocation":"{}"}}}}"#,
                real.display(),
                real.display()
            ),
        );

        let reported = home
            .path()
            .join(".claude/plugins/cache/dirahq/zavet/0.1.0")
            .display()
            .to_string();
        assert!(!Path::new(&reported).is_dir(), "cache path must not exist");

        assert_eq!(
            resolve_plugin_root(home.path(), &reported),
            real.display().to_string()
        );
    }

    /// A github-sourced marketplace really does populate the cache dir, so
    /// the reported path wins and nothing is second-guessed.
    #[test]
    fn resolve_plugin_root_keeps_reported_path_when_it_exists() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude").join("plugins")).unwrap();
        let cache = home.path().join("cache-real");
        std::fs::create_dir_all(&cache).unwrap();

        write_known_marketplaces_json(
            home.path(),
            r#"{"dirahq":{"source":{"source":"github","repo":"dodi-smart/dirahq-zavet"}}}"#,
        );

        let reported = cache.display().to_string();
        assert_eq!(resolve_plugin_root(home.path(), &reported), reported);
    }

    /// Missing/unreadable marketplace file: keep the reported path rather
    /// than inventing one.
    #[test]
    fn resolve_plugin_root_keeps_reported_path_when_marketplace_unknown() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude").join("plugins")).unwrap();
        assert_eq!(
            resolve_plugin_root(home.path(), "/nope/missing"),
            "/nope/missing"
        );
    }

    fn fake_bin_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dira-zavet-install-bin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    // -- resolve_on_path_in ---------------------------------------------------

    #[test]
    fn resolve_on_path_finds_executable_file() {
        let dir = fake_bin_dir("found");
        write_executable(&dir.join("claude"), "#!/bin/sh\n");
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        let got = resolve_on_path_in("claude", &path_var);
        assert_eq!(got, Some(dir.join("claude")));
    }

    #[test]
    fn resolve_on_path_none_when_not_executable() {
        let dir = fake_bin_dir("not-exec");
        std::fs::write(dir.join("claude"), "#!/bin/sh\n").unwrap();
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        assert_eq!(resolve_on_path_in("claude", &path_var), None);
    }

    #[test]
    fn resolve_on_path_none_when_absent() {
        let dir = fake_bin_dir("absent");
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        assert_eq!(resolve_on_path_in("claude", &path_var), None);
    }

    // -- detect ---------------------------------------------------------------

    #[test]
    fn detect_installed_via_plugin_list_json() {
        let home = temp_home("list-installed");
        let runner = FakeRunner::default().returning(
            "claude",
            &["plugin", "list", "--json"],
            0,
            r#"[{"id":"other@mp","version":"1.0.0","scope":"user","enabled":true,"installPath":"/x"},
               {"id":"zavet@dirahq","version":"0.1.0","scope":"local","enabled":false,"installPath":"/y/zavet"}]"#,
        );
        let got = detect(&runner, &home);
        assert_eq!(
            got,
            Detection::Installed(InstalledInfo {
                version: "0.1.0".into(),
                scope: "local".into(),
                install_path: "/y/zavet".into(),
                enabled: Some(false),
            })
        );
    }

    #[test]
    fn detect_not_installed_when_list_json_omits_zavet() {
        let home = temp_home("list-absent");
        let runner = FakeRunner::default().returning(
            "claude",
            &["plugin", "list", "--json"],
            0,
            r#"[{"id":"other@mp","version":"1.0.0","scope":"user","enabled":true,"installPath":"/x"}]"#,
        );
        assert_eq!(detect(&runner, &home), Detection::NotInstalled);
    }

    #[test]
    fn detect_falls_back_to_installed_plugins_json_when_claude_missing() {
        let home = temp_home("fallback-installed");
        write_installed_plugins_json(
            &home,
            r#"{"version":2,"plugins":{"zavet@dirahq":[{"scope":"user","installPath":"/z","version":"0.2.0"}]}}"#,
        );
        let runner = FakeRunner::default(); // `claude` not stubbed -> None, like NotFound
        let got = detect(&runner, &home);
        assert_eq!(
            got,
            Detection::Installed(InstalledInfo {
                version: "0.2.0".into(),
                scope: "user".into(),
                install_path: "/z".into(),
                enabled: None,
            })
        );
    }

    #[test]
    fn detect_fallback_not_installed_when_schema_2_but_no_zavet_key() {
        let home = temp_home("fallback-not-installed");
        write_installed_plugins_json(&home, r#"{"version":2,"plugins":{}}"#);
        let runner = FakeRunner::default();
        assert_eq!(detect(&runner, &home), Detection::NotInstalled);
    }

    #[test]
    fn detect_unknown_when_schema_is_not_2() {
        let home = temp_home("fallback-bad-schema");
        write_installed_plugins_json(
            &home,
            r#"{"version":3,"plugins":{"zavet@dirahq":[{"scope":"user","installPath":"/z","version":"0.2.0"}]}}"#,
        );
        let runner = FakeRunner::default();
        assert_eq!(detect(&runner, &home), Detection::Unknown);
    }

    #[test]
    fn detect_unknown_when_neither_source_resolves() {
        let home = temp_home("fallback-none");
        let runner = FakeRunner::default();
        assert_eq!(detect(&runner, &home), Detection::Unknown);
    }

    #[test]
    fn detect_prefers_plugin_list_json_over_fallback_file() {
        let home = temp_home("prefers-list");
        write_installed_plugins_json(
            &home,
            r#"{"version":2,"plugins":{"zavet@dirahq":[{"scope":"user","installPath":"/stale","version":"0.0.1"}]}}"#,
        );
        let runner = FakeRunner::default().returning(
            "claude",
            &["plugin", "list", "--json"],
            0,
            r#"[{"id":"zavet@dirahq","version":"0.1.0","scope":"local","enabled":true,"installPath":"/fresh"}]"#,
        );
        let got = detect(&runner, &home);
        assert_eq!(
            got,
            Detection::Installed(InstalledInfo {
                version: "0.1.0".into(),
                scope: "local".into(),
                install_path: "/fresh".into(),
                enabled: Some(true),
            })
        );
    }

    // -- skew_line --------------------------------------------------------------

    #[test]
    fn skew_line_unknown_when_zavet_version_subcommand_missing() {
        // No stub for `<path>/bin/zavet version --json` -> FakeRunner returns
        // None, exactly like the real degradation path this machine's
        // installed (pre-T15) zavet build hits.
        let runner = FakeRunner::default();
        let got = skew_line(&runner, "/does/not/exist", "0.1.0-develop.10");
        assert_eq!(
            got,
            "skew: unknown (installed zavet build has no `version --json` — advisory \
             only, never gates anything)"
        );
    }

    #[test]
    fn skew_line_unknown_on_unparseable_json() {
        let runner =
            FakeRunner::default().returning("/y/bin/zavet", &["version", "--json"], 0, "not json");
        let got = skew_line(&runner, "/y", "0.1.0");
        assert_eq!(
            got,
            "skew: unknown (`zavet version --json` output was not valid JSON)"
        );
    }

    #[test]
    fn skew_line_flags_dira_older_than_min_dira() {
        let runner = FakeRunner::default().returning(
            "/y/bin/zavet",
            &["version", "--json"],
            0,
            r#"{"v":1,"plugin":"zavet","version":"0.1.0","emit_schema":1,"min_dira":"0.5.0"}"#,
        );
        let got = skew_line(&runner, "/y", "0.1.0-develop.10");
        assert!(
            got.contains("older than zavet's advertised minimum (0.5.0)"),
            "{got}"
        );
    }

    #[test]
    fn skew_line_satisfied_when_dira_meets_min_dira() {
        let runner = FakeRunner::default().returning(
            "/y/bin/zavet",
            &["version", "--json"],
            0,
            r#"{"v":1,"plugin":"zavet","version":"0.1.0","emit_schema":1,"min_dira":"0.1.0"}"#,
        );
        let got = skew_line(&runner, "/y", "0.1.0-develop.10");
        assert_eq!(
            got,
            "skew: dira 0.1.0-develop.10 satisfies zavet's min_dira 0.1.0"
        );
    }

    // -- install_with: no mutation on dry-run / no-op ----------------------------
    //
    // `claude_present` is injected directly (rather than mutating the
    // process-global `PATH` env var, which `cargo test`'s parallel threads
    // would race) — [`resolve_on_path_in`]'s own tests above cover the PATH
    // scan itself.

    #[test]
    fn install_dry_run_not_installed_only_prints_planned_commands() {
        let home = temp_home("dry-run-not-installed");
        let runner =
            FakeRunner::default().returning("claude", &["plugin", "list", "--json"], 0, "[]");
        let args = InstallArgs {
            scope: "user".into(),
            update: false,
            dry_run: true,
        };
        // Only `plugin list --json` is stubbed; `marketplace add` / `install`
        // are deliberately NOT stubbed, so if `install_with` ever tried to
        // actually run them in dry-run mode, `FakeRunner` would return `None`
        // and `run_or_print` would bail — the test passing at all proves
        // those commands were only printed, never run.
        assert!(install_with(&args, &runner, &home, true).is_ok());
    }

    #[test]
    fn install_already_installed_no_update_is_pure_readonly_noop() {
        let home = temp_home("noop-installed");
        let runner = FakeRunner::default().returning(
            "claude",
            &["plugin", "list", "--json"],
            0,
            r#"[{"id":"zavet@dirahq","version":"0.1.0","scope":"user","enabled":true,"installPath":"/y"}]"#,
        );
        let args = InstallArgs {
            scope: "user".into(),
            update: false,
            dry_run: false,
        };
        // No `marketplace`/`install`/`update` command is stubbed at all — a
        // no-op path that tried to run one would bail with "failed to run",
        // so success here proves nothing beyond the initial read-only
        // `plugin list --json` was invoked.
        assert!(install_with(&args, &runner, &home, true).is_ok());
    }

    #[test]
    fn install_bails_with_manual_recipe_when_claude_absent_from_path() {
        let home = temp_home("no-claude");
        let runner = FakeRunner::default();
        let args = InstallArgs {
            scope: "user".into(),
            update: false,
            dry_run: false,
        };
        let err = install_with(&args, &runner, &home, false).unwrap_err();
        assert!(err
            .to_string()
            .contains("/plugin marketplace add dodi-smart/dirahq-zavet"));
        assert!(err.to_string().contains("/plugin install zavet@dirahq"));
    }

    #[test]
    fn install_rejects_unknown_scope() {
        let home = temp_home("bad-scope");
        let runner =
            FakeRunner::default().returning("claude", &["plugin", "list", "--json"], 0, "[]");
        let args = InstallArgs {
            scope: "global".into(),
            update: false,
            dry_run: false,
        };
        let err = install_with(&args, &runner, &home, true).unwrap_err();
        assert!(err.to_string().contains("unknown --scope"));
    }

    #[test]
    fn install_dry_run_update_prints_marketplace_and_plugin_update_only() {
        let home = temp_home("dry-run-update");
        let runner = FakeRunner::default().returning(
            "claude",
            &["plugin", "list", "--json"],
            0,
            r#"[{"id":"zavet@dirahq","version":"0.1.0","scope":"local","enabled":true,"installPath":"/y"}]"#,
        );
        let args = InstallArgs {
            scope: "user".into(),
            update: true,
            dry_run: true,
        };
        // `marketplace update` / `plugin update` are deliberately not
        // stubbed — same "would bail if actually run" proof as the
        // not-installed dry-run test above.
        assert!(install_with(&args, &runner, &home, true).is_ok());
    }

    #[test]
    fn install_unknown_detection_falls_through_to_install_commands() {
        let home = temp_home("unknown-detection");
        // `claude plugin list --json` not stubbed (simulates it failing to
        // run), and no `installed_plugins.json` fallback file exists either
        // -> `Detection::Unknown`, which should drive the same
        // marketplace-add + install dry-run path as `NotInstalled`.
        let runner = FakeRunner::default();
        let args = InstallArgs {
            scope: "user".into(),
            update: false,
            dry_run: true,
        };
        assert!(install_with(&args, &runner, &home, true).is_ok());
    }
}
