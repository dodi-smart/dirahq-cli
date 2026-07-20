//! Dev-install detection, atomic same-directory binary swap, and hard-link
//! backup/rollback.
//!
//! See the module docs on [`swap_in`] for the `ETXTBSY` trap this file exists
//! to avoid, and the production-distribution plan's §A3 for the full design.

use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The result of [`discover_install`]: either a safe install directory, or
/// one of two reasons `dira update` must refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    /// Safe to install into.
    Ok(PathBuf),
    /// `<bin_dir>/dira` is a symlink into a `just install` dev build
    /// (`target/{release,debug}`). `--force` may override this — the
    /// symlink is just a pointer, overwriting it costs nothing real.
    DevSymlink {
        bin_dir: PathBuf,
        link_target: PathBuf,
    },
    /// The *running* `dira update` process is itself a `target/{release,debug}`
    /// build — i.e. this is `cargo run`/`cargo test`/a bare `./target/debug/dira`
    /// invocation, not an installed copy. `--force` must NOT override this:
    /// there is no "old" binary to overwrite, only the build directory
    /// `cargo` itself is about to rebuild into.
    DevBuild { current_exe: PathBuf },
}

/// True if `path`'s components contain `target` immediately followed by
/// `release` or `debug` — a `just install`/`cargo build` output directory.
fn under_target_release_or_debug(path: &Path) -> bool {
    let comps: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    comps
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "release" || w[1] == "debug"))
}

/// Scan `PATH` for the first directory containing a `dira` entry (file or
/// symlink — `symlink_metadata` so a dangling dev symlink still counts, since
/// that's exactly the case this exists to catch). Deliberately ~15 lines
/// instead of pulling in a `which` crate for one lookup.
fn path_scan_for_dira() -> Result<PathBuf> {
    let path_var = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("PATH is not set — pass --bin-dir explicitly"))?;
    for dir in std::env::split_paths(&path_var) {
        if std::fs::symlink_metadata(dir.join("dira")).is_ok() {
            return Ok(dir);
        }
    }
    anyhow::bail!("could not find `dira` on PATH — pass --bin-dir explicitly, or set DIRA_BIN_DIR")
}

/// Work out where `dira update` should install, and whether it's safe to.
///
/// Deliberately reads the PATH entry (`symlink_metadata` on `<dir>/dira`),
/// not `current_exe()` — `current_exe()` resolves symlinks on both Linux and
/// macOS, so it can never observe a `just install` symlink to flag it. The
/// `DevBuild` check is independent of `bin_dir` entirely: it asks whether
/// *this running process* is itself an unlinked build output.
pub fn discover_install(bin_dir_override: Option<&Path>) -> Result<Guard> {
    let current_exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(&p).ok());
    discover_install_with(bin_dir_override, current_exe)
}

fn discover_install_with(
    bin_dir_override: Option<&Path>,
    current_exe: Option<PathBuf>,
) -> Result<Guard> {
    if let Some(exe) = &current_exe {
        if under_target_release_or_debug(exe) {
            return Ok(Guard::DevBuild {
                current_exe: exe.clone(),
            });
        }
    }

    let bin_dir = match bin_dir_override {
        Some(d) => d.to_path_buf(),
        None => path_scan_for_dira()?,
    };

    let dira_path = bin_dir.join("dira");
    if let Ok(meta) = std::fs::symlink_metadata(&dira_path) {
        if meta.file_type().is_symlink() {
            if let Ok(link_target) = std::fs::read_link(&dira_path) {
                let resolved = if link_target.is_relative() {
                    bin_dir.join(&link_target)
                } else {
                    link_target.clone()
                };
                if under_target_release_or_debug(&link_target)
                    || under_target_release_or_debug(&resolved)
                {
                    return Ok(Guard::DevSymlink {
                        bin_dir,
                        link_target,
                    });
                }
            }
        }
    }

    Ok(Guard::Ok(bin_dir))
}

/// Stage `src`'s bytes into `bin_dir/.{name}.new.<unique>`, `chmod 0755`,
/// then rename that staging file onto `bin_dir/{name}`.
///
/// # The `ETXTBSY` trap
///
/// `rename(2)` onto a path that a running process is currently executing is
/// legal on Linux and macOS — the running process holds the *inode* open,
/// not the *path*; renaming just repoints the directory entry, and the old
/// process keeps executing its now-unlinked-but-still-open inode until it
/// exits. But *opening* that same destination path for writing
/// (`File::create`, `OpenOptions::write(true)`) is a different syscall, and
/// the kernel refuses it with `ETXTBSY` ("text file busy") whenever that
/// inode is mapped executable by a running process. So: **this function
/// must never `File::create`/open-for-write the destination path — only
/// ever stage into a fresh file and `rename` onto the destination.** A
/// `create`-based swap passes every test (nothing is running against the
/// test fixture) and then fails the moment it hits a machine where `dirad`
/// is actually alive.
///
/// Same-directory staging (not `$TMPDIR`) guarantees the rename is a
/// same-filesystem inode swap and therefore atomic; a cross-device rename
/// fails `EXDEV`, and a copy-based fallback would not be atomic.
fn swap_in(src: &Path, bin_dir: &Path, name: &str, unique: &str) -> Result<()> {
    let staging = bin_dir.join(format!(".{name}.new.{unique}"));
    std::fs::copy(src, &staging)
        .with_context(|| format!("stage {name} into {}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod 0755 {}", staging.display()))?;

    let dest = bin_dir.join(name);
    // See the `ETXTBSY` trap above: `rename`, never `File::create`, onto `dest`.
    std::fs::rename(&staging, &dest)
        .with_context(|| format!("rename {} -> {}", staging.display(), dest.display()))?;
    Ok(())
}

fn backup_path(bin_dir: &Path, name: &str) -> PathBuf {
    bin_dir.join(format!(".{name}.bak"))
}

/// Hard-link the existing `bin_dir/{name}` to `.{name}.bak` for rollback.
/// Returns `false` (not an error) if there's nothing to back up yet — a
/// first-ever install into an empty directory.
fn backup(bin_dir: &Path, name: &str) -> Result<bool> {
    let src = bin_dir.join(name);
    if std::fs::symlink_metadata(&src).is_err() {
        return Ok(false);
    }
    let bak = backup_path(bin_dir, name);
    let _ = std::fs::remove_file(&bak);
    std::fs::hard_link(&src, &bak)
        .with_context(|| format!("hard-link backup {} -> {}", src.display(), bak.display()))?;
    Ok(true)
}

/// Restore `bin_dir/{name}` from its `.bak` hard link, via the same
/// rename-only swap `swap_in` uses (a rollback onto a live path is exactly
/// as `ETXTBSY`-prone as the original swap). No-op if there is no backup.
fn restore_from_backup(bin_dir: &Path, name: &str) -> Result<()> {
    let bak = backup_path(bin_dir, name);
    if std::fs::symlink_metadata(&bak).is_err() {
        return Ok(());
    }
    swap_in(
        &bak,
        bin_dir,
        name,
        &format!("restore.{}", std::process::id()),
    )
}

fn cleanup_one_backup(bin_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(backup_path(bin_dir, name));
}

/// Back up, then atomically swap both binaries found in `extracted_dir` into
/// `bin_dir` — `dirad` first, then `dira`. Dying between the two leaves the
/// recoverable version-skew state `print_version` (main.rs) already warns
/// about, never a missing binary. On any failure mid-swap, best-effort rolls
/// both back to their pre-swap content before returning the error.
pub fn swap_binaries(bin_dir: &Path, extracted_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;
    let had_dirad = backup(bin_dir, "dirad")?;
    let had_dira = backup(bin_dir, "dira")?;
    let unique = std::process::id().to_string();

    let result = swap_in(&extracted_dir.join("dirad"), bin_dir, "dirad", &unique)
        .and_then(|()| swap_in(&extracted_dir.join("dira"), bin_dir, "dira", &unique));

    if let Err(e) = result {
        if had_dirad {
            let _ = restore_from_backup(bin_dir, "dirad");
        }
        if had_dira {
            let _ = restore_from_backup(bin_dir, "dira");
        }
        return Err(e);
    }
    Ok(())
}

/// Roll both binaries back to their pre-swap content. Used when the
/// post-swap `dira --version` check or the post-restart daemon health check
/// fails. Best-effort and idempotent: a missing backup (nothing was there
/// before the very first install) simply leaves the new binary in place,
/// since there is nothing older to restore.
pub fn rollback(bin_dir: &Path) {
    let _ = restore_from_backup(bin_dir, "dirad");
    let _ = restore_from_backup(bin_dir, "dira");
}

/// Delete both `.bak` hard links after a confirmed-good update.
pub fn cleanup_backups(bin_dir: &Path) {
    cleanup_one_backup(bin_dir, "dirad");
    cleanup_one_backup(bin_dir, "dira");
}

/// Extract the version token from `dira --version` output.
///
/// clap prints `dira <version>` on the first line, followed by the wire
/// schema line. Returns `None` if the shape is anything else.
fn parse_reported_version(out: &str) -> Option<&str> {
    let first = out.lines().next()?.trim();
    let (name, version) = first.split_once(char::is_whitespace)?;
    if name != "dira" {
        return None;
    }
    let version = version.trim();
    (!version.is_empty()).then_some(version)
}

/// Run `<bin_dir>/dira --version` and assert it reports EXACTLY
/// `expected_version`. Catches a wrong-target download (or a corrupted swap)
/// *before* the daemon is ever touched — see the module doc on why this must
/// happen before `daemon::restart`.
///
/// The comparison is deliberately exact, not a substring test: version
/// strings nest (`0.1.1` is a prefix of `0.1.10`), and `--version <v>`
/// supports downgrades, so a containment check could accept the very binary
/// this exists to reject and then hand it to `daemon::restart` as a success.
pub fn verify_installed_version(bin_dir: &Path, expected_version: &str) -> Result<()> {
    let dira = bin_dir.join("dira");
    let output = std::process::Command::new(&dira)
        .arg("--version")
        .output()
        .with_context(|| format!("run `{} --version`", dira.display()))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let reported = parse_reported_version(&combined);
    if !output.status.success() || reported != Some(expected_version) {
        anyhow::bail!(
            "post-swap verification failed: `{} --version` did not report {expected_version} \
             (got: {:?}, exit {})",
            dira.display(),
            combined.trim(),
            output.status
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- discover_install ----------------------------------------------------

    #[test]
    fn ok_when_bin_dir_has_no_existing_dira() {
        let dir = tempfile::tempdir().unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(guard, Guard::Ok(dir.path().to_path_buf()));
    }

    #[test]
    fn ok_when_bin_dir_has_a_plain_regular_dira() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dira"), b"not-a-symlink").unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(guard, Guard::Ok(dir.path().to_path_buf()));
    }

    #[test]
    fn dev_symlink_detected_via_relative_link_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = PathBuf::from("../dirahq-cli/target/release/dira");
        std::os::unix::fs::symlink(&target, dir.path().join("dira")).unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(
            guard,
            Guard::DevSymlink {
                bin_dir: dir.path().to_path_buf(),
                link_target: target
            }
        );
    }

    #[test]
    fn dev_symlink_detected_via_absolute_link_target_debug_profile() {
        let dir = tempfile::tempdir().unwrap();
        let target = PathBuf::from("/home/dev/dirahq-cli/target/debug/dira");
        std::os::unix::fs::symlink(&target, dir.path().join("dira")).unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(
            guard,
            Guard::DevSymlink {
                bin_dir: dir.path().to_path_buf(),
                link_target: target
            }
        );
    }

    #[test]
    fn symlink_to_a_non_target_dir_is_not_flagged_as_dev() {
        let dir = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join("dira"), b"real").unwrap();
        std::os::unix::fs::symlink(real.path().join("dira"), dir.path().join("dira")).unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(guard, Guard::Ok(dir.path().to_path_buf()));
    }

    #[test]
    fn dev_build_detected_independent_of_bin_dir_and_wins_over_dev_symlink() {
        let dir = tempfile::tempdir().unwrap();
        // Even a bin_dir with an ordinary dira present must still refuse, once
        // the *running process* is itself a target/debug build.
        std::fs::write(dir.path().join("dira"), b"ordinary").unwrap();
        let exe = PathBuf::from("/home/dev/dirahq-cli/target/debug/dira");
        let guard = discover_install_with(Some(dir.path()), Some(exe.clone())).unwrap();
        assert_eq!(guard, Guard::DevBuild { current_exe: exe });
    }

    #[test]
    fn path_scan_finds_the_directory_containing_dira() {
        let _guard = super::super::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dira"), b"x").unwrap();
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.path());
        let result = discover_install_with(None, None);
        if let Some(p) = old_path {
            std::env::set_var("PATH", p);
        } else {
            std::env::remove_var("PATH");
        }
        assert_eq!(result.unwrap(), Guard::Ok(dir.path().to_path_buf()));
    }

    // --- swap_in / swap_binaries -----------------------------------------------

    fn fake_extracted(dir: &Path, dira: &[u8], dirad: &[u8]) -> PathBuf {
        let extracted = dir.join("extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        std::fs::write(extracted.join("dira"), dira).unwrap();
        std::fs::write(extracted.join("dirad"), dirad).unwrap();
        extracted
    }

    #[test]
    fn parse_reported_version_reads_the_first_line_token() {
        assert_eq!(
            parse_reported_version("dira 0.2.0\nwire schema: 1.2.0\n"),
            Some("0.2.0")
        );
        assert_eq!(
            parse_reported_version("dira 0.1.0-develop.10\nwire schema: 1.2.0\n"),
            Some("0.1.0-develop.10")
        );
        // Shapes that must not be mistaken for a version.
        assert_eq!(parse_reported_version(""), None);
        assert_eq!(parse_reported_version("dira\n"), None);
        assert_eq!(parse_reported_version("dirad 0.2.0\n"), None);
        assert_eq!(parse_reported_version("bash: dira: not found\n"), None);
    }

    /// The whole point of the exact comparison: `0.1.1` must not be accepted
    /// by a binary reporting `0.1.10`. A `contains` test would pass here.
    #[test]
    fn parse_reported_version_does_not_confuse_nested_versions() {
        let reported = parse_reported_version("dira 0.1.10\nwire schema: 1.2.0\n");
        assert_eq!(reported, Some("0.1.10"));
        assert_ne!(reported, Some("0.1.1"));
        assert!("dira 0.1.10".contains("0.1.1"), "premise of this test");
    }

    #[test]
    fn swap_binaries_replaces_content_and_sets_mode_0755() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("dira"), b"old-dira").unwrap();
        std::fs::write(bin_dir.join("dirad"), b"old-dirad").unwrap();

        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");
        swap_binaries(&bin_dir, &extracted).unwrap();

        assert_eq!(std::fs::read(bin_dir.join("dira")).unwrap(), b"new-dira");
        assert_eq!(std::fs::read(bin_dir.join("dirad")).unwrap(), b"new-dirad");
        let mode = std::fs::metadata(bin_dir.join("dira"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
        let mode = std::fs::metadata(bin_dir.join("dirad"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn swap_binaries_leaves_no_staging_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let extracted = fake_extracted(dir.path(), b"a", b"b");
        swap_binaries(&bin_dir, &extracted).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&bin_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".new."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn swap_binaries_creates_backups_which_cleanup_removes() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("dira"), b"old-dira").unwrap();
        std::fs::write(bin_dir.join("dirad"), b"old-dirad").unwrap();
        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");

        swap_binaries(&bin_dir, &extracted).unwrap();
        assert_eq!(
            std::fs::read(bin_dir.join(".dira.bak")).unwrap(),
            b"old-dira"
        );
        assert_eq!(
            std::fs::read(bin_dir.join(".dirad.bak")).unwrap(),
            b"old-dirad"
        );

        cleanup_backups(&bin_dir);
        assert!(!bin_dir.join(".dira.bak").exists());
        assert!(!bin_dir.join(".dirad.bak").exists());
    }

    #[test]
    fn rollback_restores_pre_swap_content() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("dira"), b"old-dira").unwrap();
        std::fs::write(bin_dir.join("dirad"), b"old-dirad").unwrap();
        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");

        swap_binaries(&bin_dir, &extracted).unwrap();
        assert_eq!(std::fs::read(bin_dir.join("dira")).unwrap(), b"new-dira");

        rollback(&bin_dir);
        assert_eq!(std::fs::read(bin_dir.join("dira")).unwrap(), b"old-dira");
        assert_eq!(std::fs::read(bin_dir.join("dirad")).unwrap(), b"old-dirad");
    }

    #[test]
    fn rollback_on_a_first_ever_install_is_a_harmless_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");
        swap_binaries(&bin_dir, &extracted).unwrap();
        // No backups existed (nothing was there before); rollback must not
        // delete the freshly-installed binaries or panic.
        rollback(&bin_dir);
        assert_eq!(std::fs::read(bin_dir.join("dira")).unwrap(), b"new-dira");
    }

    #[test]
    fn swap_binaries_fails_cleanly_when_the_extracted_dirad_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("dira"), b"old-dira").unwrap();
        std::fs::write(bin_dir.join("dirad"), b"old-dirad").unwrap();
        let extracted = dir.path().join("extracted-partial");
        std::fs::create_dir_all(&extracted).unwrap();
        std::fs::write(extracted.join("dira"), b"new-dira").unwrap();
        // no "dirad" in extracted/

        let err = swap_binaries(&bin_dir, &extracted).unwrap_err();
        assert!(err.to_string().contains("dirad"), "error was: {err}");
        // Rolled back / untouched: the old dirad must still be readable
        // (swap_in for dirad never ran, so there's nothing to restore, but
        // the original file must be intact).
        assert_eq!(std::fs::read(bin_dir.join("dirad")).unwrap(), b"old-dirad");
        assert_eq!(std::fs::read(bin_dir.join("dira")).unwrap(), b"old-dira");
    }

    // --- verify_installed_version ------------------------------------------

    #[test]
    fn verify_installed_version_accepts_matching_output() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\necho \"dira 0.9.9\"\n";
        std::fs::write(dir.path().join("dira"), script).unwrap();
        std::fs::set_permissions(
            dir.path().join("dira"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        verify_installed_version(dir.path(), "0.9.9").unwrap();
    }

    #[test]
    fn verify_installed_version_rejects_a_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\necho \"dira 0.1.0\"\n";
        std::fs::write(dir.path().join("dira"), script).unwrap();
        std::fs::set_permissions(
            dir.path().join("dira"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(verify_installed_version(dir.path(), "9.9.9").is_err());
    }
}
