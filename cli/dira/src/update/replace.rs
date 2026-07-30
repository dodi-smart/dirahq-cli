//! Dev-install detection, atomic same-directory binary swap, and hard-link
//! backup/rollback.
//!
//! See the module docs on [`swap_in`] for the `ETXTBSY` trap this file exists
//! to avoid, and the production-distribution plan's §A3 for the full design.

use anyhow::{Context, Result};
#[cfg(unix)]
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
        if std::fs::symlink_metadata(dir.join(dira_ipc::DIRA_BIN)).is_ok() {
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

    let dira_path = bin_dir.join(dira_ipc::DIRA_BIN);
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

/// Stage `src`'s bytes into `bin_dir/.{name}.new.<unique>`, `chmod 0755` on
/// unix, then rename that staging file onto `bin_dir/{name}`.
///
/// # The `ETXTBSY` trap (unix)
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
///
/// See [`swap_in`] below (the `#[cfg(windows)]` twin of this function) for
/// why Windows needs a different strategy — it cannot rename over a path a
/// running process has open at all, even though unix can.
#[cfg(unix)]
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

/// Stage `src`'s bytes into `bin_dir/.{name}.new.<unique>`, then swap it onto
/// `bin_dir/{name}` — the Windows counterpart of the unix `swap_in` above.
///
/// # Why this differs from unix (D-0003's invariant still holds)
///
/// Windows has no `ETXTBSY`-style "rename over a running exe in place"
/// escape hatch: a file that's mapped/executing generally can't be renamed
/// *onto*. What it does allow is renaming that file *away* — the same
/// underlying trick unix gets for free (the running process keeps its
/// now-unlinked-but-open handle) — so the strategy here is: try the simple
/// direct swap first (works whenever `dest` isn't currently open, e.g. a
/// first install or updating while the daemon is stopped); if that fails and
/// `dest` exists, rename `dest` itself aside to a `.old` sidecar first
/// (freeing the path), then rename the staged file onto it. Same invariant
/// as unix either way: the destination path is only ever `rename`d onto,
/// never opened for writing.
///
/// No `chmod`: Windows executability isn't a permission bit the way it is on
/// unix (see the `#[cfg(unix)]`-gated `PermissionsExt` import at the top of
/// this file).
#[cfg(windows)]
fn swap_in(src: &Path, bin_dir: &Path, name: &str, unique: &str) -> Result<()> {
    let staging = bin_dir.join(format!(".{name}.new.{unique}"));
    std::fs::copy(src, &staging)
        .with_context(|| format!("stage {name} into {}", staging.display()))?;

    let dest = bin_dir.join(name);
    let direct_err = match std::fs::rename(&staging, &dest) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };

    // The direct rename failed. If there's nothing at `dest`, that wasn't a
    // "running exe holds it open" conflict — surface the real error rather
    // than pretending we understand it.
    if std::fs::symlink_metadata(&dest).is_err() {
        return Err(direct_err)
            .with_context(|| format!("rename {} -> {}", staging.display(), dest.display()));
    }

    windows_swap_around_a_locked_dest(&staging, &dest, name, unique)
}

/// Move a currently-locked `dest` aside, swap the staged file into its
/// place, and best-effort put the old file back if the second rename still
/// fails after retrying. Both renames are wrapped in [`retry_rename`]:
/// Windows Defender (or any other AV) briefly opens a freshly-written `.exe`
/// for on-access scanning right after it's created, which can turn an
/// immediate rename into a sharing-violation error for a few hundred
/// milliseconds — a handful of short retries clears this without needing a
/// real timeout/backoff knob.
#[cfg(windows)]
fn windows_swap_around_a_locked_dest(
    staging: &Path,
    dest: &Path,
    name: &str,
    unique: &str,
) -> Result<()> {
    const ATTEMPTS: u32 = 3;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(100);

    let bin_dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let old = bin_dir.join(format!(".{name}.old.{unique}"));

    retry_rename(dest, &old, ATTEMPTS, DELAY).with_context(|| {
        format!(
            "rename {} -> {} (moving the running binary aside)",
            dest.display(),
            old.display()
        )
    })?;

    if let Err(e) = retry_rename(staging, dest, ATTEMPTS, DELAY) {
        // Best-effort: put the previous binary back so `dest` is never left
        // missing just because the new one couldn't land.
        let _ = std::fs::rename(&old, dest);
        return Err(e).with_context(|| {
            format!(
                "rename {} -> {} after moving the running binary aside",
                staging.display(),
                dest.display()
            )
        });
    }
    Ok(())
}

/// Retry `fs::rename(from, to)` up to `attempts` times, `delay` apart,
/// returning the last error if every attempt fails. See
/// [`windows_swap_around_a_locked_dest`] for why this exists.
#[cfg(windows)]
fn retry_rename(
    from: &Path,
    to: &Path,
    attempts: u32,
    delay: std::time::Duration,
) -> std::io::Result<()> {
    let mut last_err = None;
    for attempt in 0..attempts {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < attempts {
                    std::thread::sleep(delay);
                }
            }
        }
    }
    Err(last_err.expect("attempts > 0"))
}

/// Best-effort sweep of `.{name}.old.*` sidecars left behind by the Windows
/// swap-around-a-locked-dest path above — a leftover means the old process
/// was still holding that file open at the end of that update, so it's
/// expected to fail to delete sometimes, not a bug (ignored per-file). Runs
/// at the start of every `dira update` (see `mod.rs::run`) so leftovers from
/// an earlier update get swept once the process that was locking them has
/// exited, rather than accumulating forever.
///
/// Compiles and runs on every platform: unix never creates `.old` sidecars
/// (the in-place rename-over-a-running-inode trick needs no such thing), so
/// here it's a harmless sweep that simply never finds anything.
pub fn cleanup_stale_old_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefixes = [
        format!(".{}.old.", dira_ipc::DIRA_BIN),
        format!(".{}.old.", dira_ipc::DIRAD_BIN),
    ];
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if prefixes.iter().any(|p| name.starts_with(p.as_str())) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
    let had_dirad = backup(bin_dir, dira_ipc::DIRAD_BIN)?;
    let had_dira = backup(bin_dir, dira_ipc::DIRA_BIN)?;
    let unique = std::process::id().to_string();

    let result = swap_in(
        &extracted_dir.join(dira_ipc::DIRAD_BIN),
        bin_dir,
        dira_ipc::DIRAD_BIN,
        &unique,
    )
    .and_then(|()| {
        swap_in(
            &extracted_dir.join(dira_ipc::DIRA_BIN),
            bin_dir,
            dira_ipc::DIRA_BIN,
            &unique,
        )
    });

    if let Err(e) = result {
        if had_dirad {
            let _ = restore_from_backup(bin_dir, dira_ipc::DIRAD_BIN);
        }
        if had_dira {
            let _ = restore_from_backup(bin_dir, dira_ipc::DIRA_BIN);
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
    let _ = restore_from_backup(bin_dir, dira_ipc::DIRAD_BIN);
    let _ = restore_from_backup(bin_dir, dira_ipc::DIRA_BIN);
}

/// Delete both `.bak` hard links after a confirmed-good update.
pub fn cleanup_backups(bin_dir: &Path) {
    cleanup_one_backup(bin_dir, dira_ipc::DIRAD_BIN);
    cleanup_one_backup(bin_dir, dira_ipc::DIRA_BIN);
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

/// How long to keep re-attempting the `--version` probe while the kernel
/// answers `ETXTBSY`. Deliberately a longer budget than [`retry_rename`]'s
/// three tries: that one waits out a Windows file lock held by a process that
/// is on its way out, whereas this waits out another process's fork→exec
/// window, which is short but can recur under load.
const EXEC_BUSY_ATTEMPTS: u32 = 10;
const EXEC_BUSY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Run `<dira> --version`, retrying while the kernel refuses the exec with
/// `ETXTBSY`. Any other error, and the final `ETXTBSY`, are returned as-is.
///
/// # The *other* `ETXTBSY` (unix)
///
/// This is not the trap documented on [`swap_in`] — that one is "never open the
/// destination for writing", a rule this module already follows by only ever
/// renaming onto it. This is the mirror image: Linux also refuses to **exec** a
/// file while *any* process holds a write descriptor to that inode, and nothing
/// about rename discipline prevents someone else from holding one.
///
/// The window is real and does not require a misbehaving program. Staging opens
/// the new binary for writing ([`swap_in`] uses `fs::copy`), and a concurrent
/// `Command::spawn` anywhere in the process — another thread, another `dira`
/// invocation — forks a child that inherits every open descriptor. `O_CLOEXEC`
/// only takes effect at *exec*, so between the child's fork and its exec that
/// inherited write fd is live, and an exec of the same inode in that instant
/// gets `ETXTBSY`. It is timing-dependent, which is exactly why it surfaced as
/// an intermittent CI failure (#65) on a branch that changed no Rust at all,
/// and never once on macOS.
///
/// `ErrorKind::ExecutableFileBusy` names it without a `libc` dependency
/// (stable since Rust 1.83; this workspace's MSRV is 1.88). Retrying is safe
/// because the condition is transient by construction — the holder is a child
/// that is about to exec — and bounded so a genuinely stuck writer still
/// surfaces as an error rather than a hang.
fn run_version_probe(dira: &Path) -> std::io::Result<std::process::Output> {
    let mut attempt = 0;
    loop {
        match std::process::Command::new(dira).arg("--version").output() {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt + 1 < EXEC_BUSY_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(EXEC_BUSY_DELAY);
            }
            other => return other,
        }
    }
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
    // A full path, not a bare name — `Command::new` invokes it directly
    // rather than doing a PATH lookup, so it runs exactly the binary this
    // update just swapped in regardless of platform.
    let dira = bin_dir.join(dira_ipc::DIRA_BIN);
    let output =
        run_version_probe(&dira).with_context(|| format!("run `{} --version`", dira.display()))?;
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
        std::fs::write(dir.path().join(dira_ipc::DIRA_BIN), b"not-a-symlink").unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(guard, Guard::Ok(dir.path().to_path_buf()));
    }

    // `discover_install_with`'s symlink detection itself is portable (plain
    // `std::fs::symlink_metadata`/`read_link`, no unix-only API — a `mklink`
    // dev symlink on Windows degrades through the exact same path). Only the
    // *test fixture* below needs a unix-only API to create one
    // (`std::os::windows::fs::symlink_file` needs elevated privileges in a
    // way that would make these tests flaky in CI), so the tests themselves
    // are unix-gated rather than the production code they exercise.

    #[cfg(unix)]
    #[test]
    fn dev_symlink_detected_via_relative_link_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = PathBuf::from("../dirahq-cli/target/release/dira");
        std::os::unix::fs::symlink(&target, dir.path().join(dira_ipc::DIRA_BIN)).unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(
            guard,
            Guard::DevSymlink {
                bin_dir: dir.path().to_path_buf(),
                link_target: target
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn dev_symlink_detected_via_absolute_link_target_debug_profile() {
        let dir = tempfile::tempdir().unwrap();
        let target = PathBuf::from("/home/dev/dirahq-cli/target/debug/dira");
        std::os::unix::fs::symlink(&target, dir.path().join(dira_ipc::DIRA_BIN)).unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(
            guard,
            Guard::DevSymlink {
                bin_dir: dir.path().to_path_buf(),
                link_target: target
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_non_target_dir_is_not_flagged_as_dev() {
        let dir = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join(dira_ipc::DIRA_BIN), b"real").unwrap();
        std::os::unix::fs::symlink(
            real.path().join(dira_ipc::DIRA_BIN),
            dir.path().join(dira_ipc::DIRA_BIN),
        )
        .unwrap();
        let guard = discover_install_with(Some(dir.path()), None).unwrap();
        assert_eq!(guard, Guard::Ok(dir.path().to_path_buf()));
    }

    #[test]
    fn dev_build_detected_independent_of_bin_dir_and_wins_over_dev_symlink() {
        let dir = tempfile::tempdir().unwrap();
        // Even a bin_dir with an ordinary dira present must still refuse, once
        // the *running process* is itself a target/debug build.
        std::fs::write(dir.path().join(dira_ipc::DIRA_BIN), b"ordinary").unwrap();
        let exe = PathBuf::from("/home/dev/dirahq-cli/target/debug/dira");
        let guard = discover_install_with(Some(dir.path()), Some(exe.clone())).unwrap();
        assert_eq!(guard, Guard::DevBuild { current_exe: exe });
    }

    #[test]
    fn path_scan_finds_the_directory_containing_dira() {
        let _guard = super::super::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(dira_ipc::DIRA_BIN), b"x").unwrap();
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
        std::fs::write(extracted.join(dira_ipc::DIRA_BIN), dira).unwrap();
        std::fs::write(extracted.join(dira_ipc::DIRAD_BIN), dirad).unwrap();
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

    /// Mode-assert — inherently unix-only (Windows executability isn't a
    /// permission bit; `swap_in` there skips the `chmod` step entirely, see
    /// its `#[cfg(windows)]` doc comment). Content-swap coverage for both
    /// platforms lives in the portable tests below.
    #[cfg(unix)]
    #[test]
    fn swap_binaries_replaces_content_and_sets_mode_0755() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRA_BIN), b"old-dira").unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRAD_BIN), b"old-dirad").unwrap();

        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");
        swap_binaries(&bin_dir, &extracted).unwrap();

        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"new-dira"
        );
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRAD_BIN)).unwrap(),
            b"new-dirad"
        );
        let mode = std::fs::metadata(bin_dir.join(dira_ipc::DIRA_BIN))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
        let mode = std::fs::metadata(bin_dir.join(dira_ipc::DIRAD_BIN))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    // --- content-swap / backup / rollback: portable — plain files named via
    // the platform-appropriate `dira_ipc` consts, no unix-only APIs. ---------

    #[test]
    fn swap_binaries_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRA_BIN), b"old-dira").unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRAD_BIN), b"old-dirad").unwrap();

        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");
        swap_binaries(&bin_dir, &extracted).unwrap();

        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"new-dira"
        );
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRAD_BIN)).unwrap(),
            b"new-dirad"
        );
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
        std::fs::write(bin_dir.join(dira_ipc::DIRA_BIN), b"old-dira").unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRAD_BIN), b"old-dirad").unwrap();
        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");

        let dira_bak = bin_dir.join(format!(".{}.bak", dira_ipc::DIRA_BIN));
        let dirad_bak = bin_dir.join(format!(".{}.bak", dira_ipc::DIRAD_BIN));

        swap_binaries(&bin_dir, &extracted).unwrap();
        assert_eq!(std::fs::read(&dira_bak).unwrap(), b"old-dira");
        assert_eq!(std::fs::read(&dirad_bak).unwrap(), b"old-dirad");

        cleanup_backups(&bin_dir);
        assert!(!dira_bak.exists());
        assert!(!dirad_bak.exists());
    }

    #[test]
    fn rollback_restores_pre_swap_content() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRA_BIN), b"old-dira").unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRAD_BIN), b"old-dirad").unwrap();
        let extracted = fake_extracted(dir.path(), b"new-dira", b"new-dirad");

        swap_binaries(&bin_dir, &extracted).unwrap();
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"new-dira"
        );

        rollback(&bin_dir);
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"old-dira"
        );
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRAD_BIN)).unwrap(),
            b"old-dirad"
        );
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
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"new-dira"
        );
    }

    #[test]
    fn swap_binaries_fails_cleanly_when_the_extracted_dirad_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRA_BIN), b"old-dira").unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRAD_BIN), b"old-dirad").unwrap();
        let extracted = dir.path().join("extracted-partial");
        std::fs::create_dir_all(&extracted).unwrap();
        std::fs::write(extracted.join(dira_ipc::DIRA_BIN), b"new-dira").unwrap();
        // no dirad in extracted/

        let err = swap_binaries(&bin_dir, &extracted).unwrap_err();
        assert!(err.to_string().contains("dirad"), "error was: {err}");
        // Rolled back / untouched: the old dirad must still be readable
        // (swap_in for dirad never ran, so there's nothing to restore, but
        // the original file must be intact).
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRAD_BIN)).unwrap(),
            b"old-dirad"
        );
        assert_eq!(
            std::fs::read(bin_dir.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"old-dira"
        );
    }

    // --- cleanup_stale_old_files ---------------------------------------------

    #[test]
    fn cleanup_stale_old_files_removes_old_sidecars_and_leaves_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let dira_old = bin_dir.join(format!(".{}.old.12345", dira_ipc::DIRA_BIN));
        let dirad_old = bin_dir.join(format!(".{}.old.6789", dira_ipc::DIRAD_BIN));
        std::fs::write(&dira_old, b"stale").unwrap();
        std::fs::write(&dirad_old, b"stale").unwrap();
        // Must survive: the live binaries, a `.bak`, and anything that just
        // happens to contain "old" without matching the sidecar shape.
        std::fs::write(bin_dir.join(dira_ipc::DIRA_BIN), b"live").unwrap();
        std::fs::write(bin_dir.join(dira_ipc::DIRAD_BIN), b"live").unwrap();
        std::fs::write(bin_dir.join(format!(".{}.bak", dira_ipc::DIRA_BIN)), b"bak").unwrap();
        std::fs::write(bin_dir.join("golden-oldies.txt"), b"unrelated").unwrap();

        cleanup_stale_old_files(&bin_dir);

        assert!(!dira_old.exists(), "stale dira sidecar must be removed");
        assert!(!dirad_old.exists(), "stale dirad sidecar must be removed");
        assert!(bin_dir.join(dira_ipc::DIRA_BIN).exists());
        assert!(bin_dir.join(dira_ipc::DIRAD_BIN).exists());
        assert!(bin_dir
            .join(format!(".{}.bak", dira_ipc::DIRA_BIN))
            .exists());
        assert!(bin_dir.join("golden-oldies.txt").exists());
    }

    #[test]
    fn cleanup_stale_old_files_on_a_missing_dir_is_a_harmless_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        cleanup_stale_old_files(&missing); // must not panic
    }

    // --- verify_installed_version -------------------------------------------
    //
    // Both build a `#!/bin/sh` fixture "binary" and `chmod` it executable —
    // unix-only by construction (an equivalent windows fixture would need a
    // real compiled .exe, not a text script; `parse_reported_version`'s pure
    // string-parsing logic is exercised directly above regardless of
    // platform, so this doesn't lose windows coverage of the parsing rule).

    #[cfg(unix)]
    #[test]
    fn verify_installed_version_accepts_matching_output() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\necho \"dira 0.9.9\"\n";
        std::fs::write(dir.path().join(dira_ipc::DIRA_BIN), script).unwrap();
        std::fs::set_permissions(
            dir.path().join(dira_ipc::DIRA_BIN),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        verify_installed_version(dir.path(), "0.9.9").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verify_installed_version_rejects_a_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\necho \"dira 0.1.0\"\n";
        std::fs::write(dir.path().join(dira_ipc::DIRA_BIN), script).unwrap();
        std::fs::set_permissions(
            dir.path().join(dira_ipc::DIRA_BIN),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(verify_installed_version(dir.path(), "9.9.9").is_err());
    }

    // --- the ETXTBSY retry (#65) --------------------------------------------
    //
    // Linux-only: it is the only platform in the matrix that refuses to exec a
    // file while a write descriptor to that inode is open, which is the whole
    // condition under test. Verified by hand in a container first — with a
    // write fd held, exec fails "Text file busy"; close it and the same exec
    // succeeds — so these two tests pin the retry against the real kernel
    // behaviour rather than a mocked error.

    #[cfg(target_os = "linux")]
    fn busy_fixture(dir: &Path) -> std::fs::File {
        let path = dir.join(dira_ipc::DIRA_BIN);
        std::fs::write(&path, "#!/bin/sh\necho \"dira 0.9.9\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The held write handle is what makes the exec fail — the same shape a
        // concurrent fork produces by inheriting one.
        std::fs::OpenOptions::new().write(true).open(&path).unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_installed_version_retries_past_a_transient_etxtbsy() {
        let dir = tempfile::tempdir().unwrap();
        let held = busy_fixture(dir.path());

        // Release the descriptor well inside the retry budget
        // (10 x 50ms): the first attempt must fail ETXTBSY, a later one succeed.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            drop(held);
        });

        verify_installed_version(dir.path(), "0.9.9")
            .expect("must retry past a transient ETXTBSY, not fail the update");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_installed_version_gives_up_on_a_permanent_etxtbsy() {
        let dir = tempfile::tempdir().unwrap();
        // Never dropped for the duration of the call: the retry is bounded, so
        // this must surface as an error rather than hang forever.
        let _held = busy_fixture(dir.path());

        let err = verify_installed_version(dir.path(), "0.9.9")
            .expect_err("a permanently busy binary must error, not spin");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("--version"),
            "error must name the probe it gave up on: {chain}"
        );
    }

    #[test]
    fn non_busy_spawn_errors_are_not_retried() {
        // A missing binary is NotFound, not ExecutableFileBusy — it must fail
        // immediately rather than burn the 500ms retry budget on an error that
        // will never clear.
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let err = run_version_probe(&dir.path().join(dira_ipc::DIRA_BIN)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            started.elapsed() < EXEC_BUSY_DELAY,
            "must not have slept: took {:?}",
            started.elapsed()
        );
    }
}
