//! `dira update` — the built-in self-updater.
//!
//! Resolves the current (or a requested) release, downloads the platform
//! artifact, verifies its sha256 against the published checksum, and
//! atomically swaps both `dira` and `dirad` in place, then restarts the
//! daemon (unless told not to). See the production-distribution plan's
//! §A3 "`dira update`" for the full resolve → download → verify →
//! atomic-swap → restart design and the `ETXTBSY` rename-not-create trap
//! (see [`replace::swap_binaries`]'s doc comment for that trap in detail).
//!
//! - [`resolve`] talks to the GitHub Releases API: channel/version
//!   resolution, asset-name construction, and the public-vs-authenticated
//!   (private-repo) request split — mirrors `install.sh` closely.
//! - [`artifact`] detects the host's release target, downloads + sha256
//!   verifies the archive, and extracts it (`tar -xzf`, shelled out to
//!   rather than a `tar`/`flate2` dependency — see that module's doc).
//! - [`replace`] discovers the install location (refusing a `just install`
//!   dev build or dev symlink unless `--force`, which never overrides a dev
//!   *build*), and performs the atomic, backed-up, same-directory swap.
//!
//! `run` never reimplements daemon supervision — the post-update restart
//! goes through [`crate::daemon::restart`], which already knows how to
//! detect launchd/systemd-user/bare supervision and poll for a healthy
//! comeback (see `daemon.rs`, landed by T6).

pub mod artifact;
pub mod notice;
pub mod replace;
pub mod resolve;

use crate::daemon;
use anyhow::{Context, Result};
use dira_core::Config;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Release channel to resolve a version against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Regular tagged releases — the default.
    Stable,
    /// `-develop.N` prereleases.
    Prerelease,
}

impl Channel {
    /// Parse the raw `--channel` flag value.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stable" => Ok(Channel::Stable),
            "prerelease" => Ok(Channel::Prerelease),
            other => anyhow::bail!("unknown channel `{other}` (expected: stable, prerelease)"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Prerelease => "prerelease",
        }
    }
}

/// A parsed `dira update` invocation, independent of clap so the engine and
/// its tests don't need to construct a `Command::Update` value.
#[derive(Debug, Clone, Default)]
pub struct UpdateArgs {
    /// Resolve only — report what's available, never download or touch a binary.
    pub check: bool,
    /// Update (or downgrade) to this exact version instead of the latest.
    pub version: Option<String>,
    /// Raw `--channel` value (`None` means the default, stable).
    pub channel: Option<String>,
    /// Skip the dev-install guard for a `DevSymlink` install (never bypasses
    /// sha256 verification, and never overrides a `DevBuild` refusal).
    pub force: bool,
    /// Swap the binaries but leave a running daemon on the old version.
    pub no_restart: bool,
    /// Install directory for the new binaries (default: alongside the
    /// running `dira`). Also settable via `DIRA_BIN_DIR`.
    pub bin_dir: Option<PathBuf>,
}

/// An RAII scratch directory for the download/extract workspace, under
/// `std::env::temp_dir()` with a `ulid`-unique name (already a direct dep —
/// no need to add `tempfile` to non-test code for this). Removed on drop, so
/// a `?` early-return anywhere in [`run`] still cleans up.
struct Workdir(PathBuf);

impl Workdir {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("dira-update-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create temp working dir {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `dira update`.
pub async fn run(config: &Config, args: UpdateArgs) -> Result<()> {
    let channel = match &args.channel {
        Some(raw) => Channel::parse(raw)?,
        None => resolve::default_channel(),
    };

    let http = reqwest::Client::builder()
        .build()
        .context("build HTTP client")?;

    if args.check {
        run_check(&http, args.version.as_deref(), channel).await;
        return Ok(());
    }

    let target = artifact::detect_target()?;

    let resolved = resolve::resolve(&http, &target, args.version.as_deref(), channel)
        .await
        .context("resolve release")?;

    let guard = replace::discover_install(args.bin_dir.as_deref())?;
    let bin_dir = guard_to_bin_dir(guard, args.force)?;

    println!(
        "dira update: {} -> {} ({}, {target}, {} channel)",
        env!("CARGO_PKG_VERSION"),
        resolved.version,
        resolved.tag,
        channel.label()
    );

    let workdir = Workdir::new()?;
    let tarball_path = workdir.path().join(&resolved.tarball_name);
    let sha_path = workdir.path().join(&resolved.sha_name);

    artifact::download(&http, &resolved.tarball, &tarball_path)
        .await
        .with_context(|| format!("download {}", resolved.tarball_name))?;
    artifact::download(&http, &resolved.sha, &sha_path)
        .await
        .with_context(|| format!("download {}", resolved.sha_name))?;

    let sha_contents =
        std::fs::read_to_string(&sha_path).context("read the downloaded checksum file")?;
    let expected = artifact::parse_sha256_file(&sha_contents, &resolved.tarball_name)?;
    artifact::verify_sha256(&tarball_path, &expected)?;
    println!("checksum OK for {}", resolved.tarball_name);

    let extract_dir = workdir.path().join("extract");
    artifact::extract(&tarball_path, &extract_dir)?;

    replace::swap_binaries(&bin_dir, &extract_dir)?;

    // Verify the swap landed the right thing *before* we ever touch the
    // daemon — catches a wrong-target download or a corrupt swap while it's
    // still cheap (and reversible) to back out.
    if let Err(e) = replace::verify_installed_version(&bin_dir, &resolved.version) {
        replace::rollback(&bin_dir);
        return Err(e.context("rolled back dira + dirad to their previous versions"));
    }

    if args.no_restart {
        replace::cleanup_backups(&bin_dir);
        println!(
            "dira + dirad updated to {} in {} — daemon left untouched (--no-restart)",
            resolved.version,
            bin_dir.display()
        );
        return Ok(());
    }

    // Reuse the daemon's own supervision-aware restart (launchd / systemd
    // --user / bare pidfile) rather than reimplementing it — it already
    // polls for a healthy comeback and reports the version that answers.
    match daemon::restart(config).await {
        Ok(()) => {
            replace::cleanup_backups(&bin_dir);
            println!(
                "dira + dirad updated to {} and the daemon restarted",
                resolved.version
            );
            Ok(())
        }
        Err(e) => {
            // The post-restart health check failed: the new dirad may be
            // broken even though the new dira's own --version looked fine.
            // Roll back both binaries so the machine is left on a known-good
            // version rather than stranded mid-update.
            replace::rollback(&bin_dir);
            Err(e.context(format!(
                "the daemon did not come back up healthy after updating to {} — rolled back to \
                 the previous binaries; run `dira daemon status` to check on it",
                resolved.version
            )))
        }
    }
}

/// Interpret [`replace::discover_install`]'s guard into a usable `bin_dir`,
/// or a clear refusal. Pure (no FS/env access of its own — `discover_install`
/// already did that), so the refusal messages and the `--force` semantics
/// are unit-testable without needing to fake `current_exe()` or PATH.
fn guard_to_bin_dir(guard: replace::Guard, force: bool) -> Result<PathBuf> {
    match guard {
        replace::Guard::Ok(dir) => Ok(dir),
        replace::Guard::DevSymlink {
            bin_dir,
            link_target,
        } => {
            if !force {
                anyhow::bail!(
                    "{} is a symlink into a `just install` dev build ({}) — refusing to \
                     overwrite it. Re-run with --force, or use `just install` to update a dev \
                     setup.",
                    bin_dir.join("dira").display(),
                    link_target.display()
                );
            }
            eprintln!(
                "warning: overwriting dev symlink at {} (--force)",
                bin_dir.join("dira").display()
            );
            Ok(bin_dir)
        }
        replace::Guard::DevBuild { current_exe } => {
            anyhow::bail!(
                "this `dira` ({}) is itself an unlinked target/{{release,debug}} build, not an \
                 installed copy — `dira update` refuses to touch it (not overridable by \
                 --force). Use `just install` for a dev setup.",
                current_exe.display()
            );
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `--check`: resolve only, write nothing to disk except (when not pinned to
/// an exact version) a refresh of [`notice`]'s update-check cache, and exit
/// 0 in every non-error case — including offline. Errors are printed to
/// stderr, never propagated, so this stays safe to run speculatively (e.g.
/// from [`notice::maybe_print`]'s detached background refresh).
async fn run_check(http: &reqwest::Client, version_pin: Option<&str>, channel: Channel) {
    let now = now_unix();

    let target = match artifact::detect_target() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("dira update --check: {e:#}");
            return;
        }
    };

    match resolve::resolve(http, &target, version_pin, channel).await {
        Ok(resolved) => {
            let current = env!("CARGO_PKG_VERSION");
            if resolved.version == current {
                println!("dira is up to date ({current})");
            } else {
                println!(
                    "dira {} is available (you have {current}) — run `dira update`",
                    resolved.version
                );
            }
            // A pinned `--version` check isn't "the latest" — don't let it
            // clobber the passive notice's idea of what's current.
            if version_pin.is_none() {
                write_check_cache(now, Some(&resolved.version), channel, None);
            }
        }
        Err(e) => {
            eprintln!("dira update --check: {e:#}");
            if version_pin.is_none() {
                write_check_cache(now, None, channel, Some(&e.to_string()));
            }
        }
    }
}

/// Write `project_dirs()?.cache_dir()/update-check.json`, matching
/// [`notice`]'s documented on-disk shape exactly:
/// `{ "checked_at", "latest", "channel", "error" }`. Best-effort: this is
/// disposable derived state, so a write failure is swallowed rather than
/// surfaced (mirrors `notice::write_sentinel`).
fn write_check_cache(checked_at: i64, latest: Option<&str>, channel: Channel, error: Option<&str>) {
    let Some(dirs) = dira_core::config::project_dirs() else {
        return;
    };
    let path = dirs.cache_dir().join("update-check.json");
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let value = serde_json::json!({
        "checked_at": checked_at,
        "latest": latest,
        "channel": channel.label(),
        "error": error,
    });
    if let Ok(bytes) = serde_json::to_vec(&value) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Serializes every test in this crate that mutates process-global env vars
/// (`PATH`, `DIRA_TARGET`, `DIRA_API_URL`, `GH_TOKEN`, …) so parallel `cargo
/// test` threads never race each other's `set_var`/`remove_var`. Acquire
/// this *before* touching any such var and hold the guard for the test's
/// duration — mirrors `test_support::keychain_lock`'s reasoning for the
/// shared mock keychain.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_parse_accepts_stable_and_prerelease_case_insensitively() {
        assert_eq!(Channel::parse("stable").unwrap(), Channel::Stable);
        assert_eq!(Channel::parse("Prerelease").unwrap(), Channel::Prerelease);
        assert_eq!(
            Channel::parse("  PRERELEASE  ").unwrap(),
            Channel::Prerelease
        );
    }

    #[test]
    fn channel_parse_rejects_garbage() {
        assert!(Channel::parse("nightly").is_err());
        assert!(Channel::parse("").is_err());
    }

    #[test]
    fn channel_label_matches_the_flag_spelling() {
        assert_eq!(Channel::Stable.label(), "stable");
        assert_eq!(Channel::Prerelease.label(), "prerelease");
    }

    #[test]
    fn guard_to_bin_dir_ok_passes_through() {
        let dir = PathBuf::from("/opt/dira/bin");
        let got = guard_to_bin_dir(replace::Guard::Ok(dir.clone()), false).unwrap();
        assert_eq!(got, dir);
    }

    #[test]
    fn guard_to_bin_dir_dev_symlink_refuses_without_force() {
        let guard = replace::Guard::DevSymlink {
            bin_dir: PathBuf::from("/home/dev/.local/bin"),
            link_target: PathBuf::from("/home/dev/dirahq-cli/target/debug/dira"),
        };
        let err = guard_to_bin_dir(guard, false).unwrap_err();
        assert!(err.to_string().contains("just install"), "error was: {err}");
    }

    #[test]
    fn guard_to_bin_dir_dev_symlink_proceeds_with_force() {
        let bin_dir = PathBuf::from("/home/dev/.local/bin");
        let guard = replace::Guard::DevSymlink {
            bin_dir: bin_dir.clone(),
            link_target: PathBuf::from("/home/dev/dirahq-cli/target/debug/dira"),
        };
        let got = guard_to_bin_dir(guard, true).unwrap();
        assert_eq!(got, bin_dir);
    }

    #[test]
    fn guard_to_bin_dir_dev_build_refuses_even_with_force() {
        let guard = replace::Guard::DevBuild {
            current_exe: PathBuf::from("/home/dev/dirahq-cli/target/debug/dira"),
        };
        assert!(guard_to_bin_dir(guard.clone(), false).is_err());
        assert!(
            guard_to_bin_dir(guard, true).is_err(),
            "--force must never override a DevBuild refusal"
        );
    }

    #[test]
    fn workdir_is_removed_on_drop() {
        let path = {
            let wd = Workdir::new().unwrap();
            std::fs::write(wd.path().join("marker"), b"x").unwrap();
            wd.path().to_path_buf()
        };
        assert!(!path.exists());
    }
}
