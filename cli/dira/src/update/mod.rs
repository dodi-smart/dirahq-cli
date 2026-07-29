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

    // Refuse an implicit downgrade before anything is downloaded or swapped.
    // `--check` and the passive notice already decline to *offer* an older
    // release (see `check_message` / `notice::should_notify`), and this is the
    // matching guard on the command they point at: a prerelease user on the
    // default stable channel would otherwise be moved silently backwards.
    if let Some(refusal) = downgrade_refusal(
        &resolved.version,
        env!("CARGO_PKG_VERSION"),
        channel,
        args.version.is_some(),
    ) {
        anyhow::bail!(refusal);
    }

    let guard = replace::discover_install(args.bin_dir.as_deref())?;
    let bin_dir = guard_to_bin_dir(guard, args.force)?;

    println!(
        "dira update: {} -> {} ({}, {target}, {} channel)",
        env!("CARGO_PKG_VERSION"),
        resolved.version,
        resolved.tag,
        channel.label()
    );

    // Best-effort sweep of `.{name}.old.*` leftovers from a prior update that
    // couldn't clean up because the old process still held its file locked
    // (windows swap-away path — see replace.rs) — before this run adds its
    // own. Harmless (and simply finds nothing) on unix.
    replace::cleanup_stale_old_files(&bin_dir);

    let workdir = Workdir::new()?;
    let archive_path = workdir.path().join(&resolved.archive_name);
    let sha_path = workdir.path().join(&resolved.sha_name);

    artifact::download(&http, &resolved.archive, &archive_path)
        .await
        .with_context(|| format!("download {}", resolved.archive_name))?;
    artifact::download(&http, &resolved.sha, &sha_path)
        .await
        .with_context(|| format!("download {}", resolved.sha_name))?;

    let sha_contents =
        std::fs::read_to_string(&sha_path).context("read the downloaded checksum file")?;
    let expected = artifact::parse_sha256_file(&sha_contents, &resolved.archive_name)?;
    artifact::verify_sha256(&archive_path, &expected)?;
    println!("checksum OK for {}", resolved.archive_name);

    let extract_dir = workdir.path().join("extract");
    artifact::extract(&archive_path, &extract_dir)?;

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
        println!("{}", no_restart_notice(&resolved.version, &bin_dir));
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
                    bin_dir.join(dira_ipc::DIRA_BIN).display(),
                    link_target.display()
                );
            }
            eprintln!(
                "warning: overwriting dev symlink at {} (--force)",
                bin_dir.join(dira_ipc::DIRA_BIN).display()
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

/// `--no-restart`'s success line. Pure so the cost disclosure is testable
/// without exercising the whole `run` pipeline. In a post-fix→post-fix
/// upgrade the socket path is stable and capture merely runs version-skewed
/// until the next restart; only a transitional upgrade (still-running
/// pre-D-0008 daemon) actually goes dark — hence "not guaranteed" rather
/// than "stopped".
fn no_restart_notice(version: &str, bin_dir: &Path) -> String {
    format!(
        "dira + dirad updated to {version} in {} — daemon left untouched (--no-restart)\n  \
         warning: the running dirad is still the previous version — hook capture is not \
         guaranteed until you run `dira daemon restart`",
        bin_dir.display(),
    )
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The refusal text when the channel resolved to an *older* version than the
/// one running, or `None` when the update may proceed. Pure, so the whole
/// matrix is testable without the network.
///
/// `version_pinned` is the escape hatch: `--version` is documented as "update
/// (or downgrade) to this exact version" (see [`UpdateArgs::version`]), so an
/// explicit request is always honoured — it is the *implicit*, channel-resolved
/// downgrade that is never what anyone asked for. `--force` deliberately does
/// not override this; that flag means one thing (the D-0004 dev-install guard)
/// and widening it would make both meanings vaguer.
///
/// An unparseable version on either side yields `None` — no refusal. This guard
/// exists to stop a *known* downgrade, not to become a second, stricter version
/// gate that could block a legitimate update over a tag it cannot order.
fn downgrade_refusal(
    resolved: &str,
    current: &str,
    channel: Channel,
    version_pinned: bool,
) -> Option<String> {
    if version_pinned
        || resolve::compare_versions(resolved, current) != Some(std::cmp::Ordering::Less)
    {
        return None;
    }
    Some(format!(
        "refusing to downgrade: the {} channel's latest is {resolved}, but you are running \
         {current}\n  \
         you are ahead of that channel — most likely a prerelease build against the stable \
         channel\n  \
         to move anyway: `dira update --version {resolved}` (explicit downgrade)\n  \
         to stay on prereleases: `dira update --channel prerelease`",
        channel.label()
    ))
}

/// What `--check` prints for a successful resolve. Pure, so the whole
/// three-way outcome is unit-testable without the network (same reasoning as
/// [`no_restart_notice`] above and `notice::should_notify`).
///
/// The third arm is the one that used to be wrong: this compared `resolved ==
/// current` and called *any* difference an upgrade, so a `0.1.1-develop.1`
/// build resolving stable `0.1.0` announced a **downgrade** as available. Being
/// ahead of the channel is reported rather than silently flattened into "up to
/// date", because a prerelease user who asked deserves to know the channel head
/// is behind them — that is why they see no upgrade.
///
/// Exits 0 either way; the caller prints this and returns. That contract is
/// load-bearing — [`notice::spawn_refresh`] runs this detached purely to warm
/// the cache.
fn check_message(resolved: &str, current: &str, channel: Channel) -> String {
    match resolve::compare_versions(resolved, current) {
        Some(std::cmp::Ordering::Greater) => {
            format!("dira {resolved} is available (you have {current}) — run `dira update`")
        }
        Some(std::cmp::Ordering::Equal) => format!("dira is up to date ({current})"),
        // Older, or a version either side of the comparison can't parse: in
        // both cases there is nothing to offer, and claiming otherwise is the
        // bug. `None` is near-unreachable (`pick_latest` drops unparseable
        // tags and `current` is `CARGO_PKG_VERSION`) but must not fall through
        // to "available".
        _ => format!(
            "dira is up to date ({current} — ahead of the {} channel's {resolved})",
            channel.label()
        ),
    }
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
            println!(
                "{}",
                check_message(&resolved.version, env!("CARGO_PKG_VERSION"), channel)
            );
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
///
/// **Readers count too.** `path_scan_finds_the_directory_containing_dira`
/// points `PATH` at a temp dir for the length of its call, so any test that
/// resolves a bare program name through `PATH` — every `extract`/`build_tarball`
/// test spawning `tar` — races it and fails with a spurious "failed to spawn
/// `tar`" unless it holds this lock as well.
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

    // --- version comparison (#63) -------------------------------------------
    //
    // The bug these pin down: every one of these compared with `==` and read
    // any inequality as an upgrade, so a prerelease build announced — and then
    // installed — an older stable release.

    #[test]
    fn check_message_offers_a_genuinely_newer_release() {
        let msg = check_message("0.3.0", "0.2.0", Channel::Stable);
        assert_eq!(
            msg,
            "dira 0.3.0 is available (you have 0.2.0) — run `dira update`"
        );
    }

    #[test]
    fn check_message_reports_up_to_date_when_equal() {
        assert_eq!(
            check_message("0.2.0", "0.2.0", Channel::Stable),
            "dira is up to date (0.2.0)"
        );
    }

    #[test]
    fn check_message_says_ahead_instead_of_offering_a_downgrade() {
        // The exact v0.1.1-develop.1 smoke-run case: stable resolves 0.1.0
        // while a prerelease is installed. 0.1.0 < 0.1.1-develop.1 under
        // SemVer §11, so this is a downgrade and must never be "available".
        let msg = check_message("0.1.0", "0.1.1-develop.1", Channel::Stable);
        assert!(
            msg.starts_with("dira is up to date (0.1.1-develop.1"),
            "message was: {msg}"
        );
        assert!(msg.contains("ahead of the stable channel's 0.1.0"), "{msg}");
        assert!(
            !msg.contains("is available"),
            "must not offer a downgrade: {msg}"
        );
    }

    #[test]
    fn check_message_names_the_channel_it_is_ahead_of() {
        let msg = check_message("0.2.0-develop.1", "0.3.0", Channel::Prerelease);
        assert!(msg.contains("ahead of the prerelease channel's"), "{msg}");
    }

    #[test]
    fn check_message_makes_no_claim_on_an_unparseable_version() {
        // `pick_latest` drops tags it can't order, so this is near-unreachable
        // — but it must not fall through to "available", which is precisely
        // what string inequality did.
        let msg = check_message("not-a-version", "0.2.0", Channel::Stable);
        assert!(!msg.contains("is available"), "message was: {msg}");
    }

    #[test]
    fn downgrade_refusal_blocks_an_implicit_channel_downgrade() {
        let msg = downgrade_refusal("0.1.0", "0.1.1-develop.1", Channel::Stable, false)
            .expect("an older channel head must be refused");
        assert!(msg.contains("refusing to downgrade"), "{msg}");
        // Both escape hatches have to be spelled out, or the refusal is a
        // dead end for someone who genuinely wants to move.
        assert!(msg.contains("--version 0.1.0"), "{msg}");
        assert!(msg.contains("--channel prerelease"), "{msg}");
    }

    #[test]
    fn downgrade_refusal_allows_an_explicit_version_downgrade() {
        // `--version` is documented as "update (or downgrade) to this exact
        // version" — an explicit request is always honoured.
        assert!(
            downgrade_refusal("0.1.0", "0.1.1-develop.1", Channel::Stable, true).is_none(),
            "an explicit --version must never be refused"
        );
    }

    #[test]
    fn downgrade_refusal_allows_upgrades_and_reinstalls() {
        assert!(downgrade_refusal("0.3.0", "0.2.0", Channel::Stable, false).is_none());
        assert!(downgrade_refusal("0.2.0", "0.2.0", Channel::Stable, false).is_none());
    }

    #[test]
    fn downgrade_refusal_allows_a_prerelease_to_its_own_newer_channel_head() {
        // The prerelease channel legitimately serves 0.2.0-develop.10 to
        // someone on 0.2.0-develop.9, and stable 0.2.0 outranks both.
        assert!(downgrade_refusal(
            "0.2.0-develop.10",
            "0.2.0-develop.9",
            Channel::Prerelease,
            false
        )
        .is_none());
        assert!(
            downgrade_refusal("0.2.0", "0.2.0-develop.10", Channel::Prerelease, false).is_none()
        );
    }

    #[test]
    fn downgrade_refusal_makes_no_claim_on_an_unparseable_version() {
        // This guard stops a *known* downgrade; it must not become a second
        // version gate that blocks an update over a tag it cannot order.
        assert!(downgrade_refusal("weird-tag", "0.2.0", Channel::Stable, false).is_none());
    }

    #[test]
    fn no_restart_notice_warns_that_capture_waits_on_a_restart() {
        let msg = no_restart_notice("1.2.3", Path::new("/opt/dira/bin"));
        assert!(msg.contains("--no-restart"), "message was: {msg}");
        assert!(msg.contains("dira daemon restart"), "message was: {msg}");
        assert!(msg.contains("capture"), "message was: {msg}");
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
