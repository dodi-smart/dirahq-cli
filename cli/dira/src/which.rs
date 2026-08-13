//! PATH resolution — "is this program installed on this machine?"
//!
//! No `which`-style crate dependency: every dep `cli/dira` needs is already
//! a direct one, and the whole job is `split_paths` plus an is-it-runnable
//! test that differs on exactly one axis between unix and windows.
//!
//! Two callers with different stakes. [`zavet_install`] asks about `claude`
//! and *refuses to act* without it — a missing `claude` is a hard bail,
//! because there is nothing safe to shell out to. [`onboard`] asks about
//! each harness CLI and only uses the answer to pre-select a checkbox; a
//! false negative there costs a keystroke, never correctness. Detection is
//! therefore allowed to be conservative, and the harness probe corroborates
//! a PATH miss against the harness's config directory before concluding
//! anything (see [`crate::onboard::detect`]).
//!
//! [`zavet_install`]: crate::zavet_install
//! [`onboard`]: crate::onboard

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Resolve `prog` against the process's own `PATH`. `None` when `PATH` is
/// unset, which is indistinguishable from "not found" for every caller here.
pub(crate) fn on_path(prog: &str) -> Option<PathBuf> {
    on_path_in(prog, std::env::var_os("PATH")?.as_os_str())
}

/// The testable core: resolve `prog` against an explicit `PATH` value.
pub(crate) fn on_path_in(prog: &str, path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var).find_map(|dir| candidate_in_dir(&dir, prog))
}

/// unix: `dir/prog` is directly runnable or it isn't — no extension games.
#[cfg(not(windows))]
fn candidate_in_dir(dir: &Path, prog: &str) -> Option<PathBuf> {
    let candidate = dir.join(prog);
    is_executable_file(&candidate).then_some(candidate)
}

/// Windows has no executable bit and (unlike unix) usually doesn't ship
/// `prog` as a bare extensionless file — npm installs `claude` as a
/// `claude.cmd` shim, for example. Try the bare name first (covers a real
/// `.exe`), then each extension `PATHEXT` lists, falling back to the
/// documented Windows default order when the var is unset or empty.
#[cfg(windows)]
fn candidate_in_dir(dir: &Path, prog: &str) -> Option<PathBuf> {
    let bare = dir.join(prog);
    if is_executable_file(&bare) {
        return Some(bare);
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    pathext
        .split(';')
        .filter(|ext| !ext.is_empty())
        .find_map(|ext| {
            let candidate = dir.join(format!("{prog}{ext}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bin_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dira-which-{tag}-{}",
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

    /// Windows has no executable bit, and `is_executable_file`'s non-unix
    /// stub is just `is_file()` — so unconditionally writing the file already
    /// makes it "executable" by that stub. Needed only so this test module
    /// compiles and runs on windows CI.
    #[cfg(windows)]
    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn resolve_on_path_finds_executable_file() {
        let dir = fake_bin_dir("found");
        write_executable(&dir.join("claude"), "#!/bin/sh\n");
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        let got = on_path_in("claude", &path_var);
        assert_eq!(got, Some(dir.join("claude")));
    }

    /// Unix-only by design: the exec-bit is the discriminator here, and windows
    /// has no such bit — `is_executable_file`'s non-unix arm is deliberately
    /// just `is_file()`, so a bare non-executable file DOES resolve there.
    #[cfg(unix)]
    #[test]
    fn resolve_on_path_none_when_not_executable() {
        let dir = fake_bin_dir("not-exec");
        std::fs::write(dir.join("claude"), "#!/bin/sh\n").unwrap();
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        assert_eq!(on_path_in("claude", &path_var), None);
    }

    #[test]
    fn resolve_on_path_none_when_absent() {
        let dir = fake_bin_dir("absent");
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        assert_eq!(on_path_in("claude", &path_var), None);
    }

    /// Several PATH entries, only the last of which holds the program —
    /// `split_paths` order is the contract, and a miss must not short-circuit
    /// the search.
    #[test]
    fn resolve_on_path_scans_every_entry() {
        let empty = fake_bin_dir("scan-empty");
        let holding = fake_bin_dir("scan-holding");
        write_executable(&holding.join("codex"), "#!/bin/sh\n");
        let path_var = std::env::join_paths([empty.as_path(), holding.as_path()]).unwrap();
        assert_eq!(on_path_in("codex", &path_var), Some(holding.join("codex")));
    }

    /// A directory named like the program is not the program.
    #[test]
    fn resolve_on_path_ignores_directories() {
        let dir = fake_bin_dir("is-a-dir");
        std::fs::create_dir_all(dir.join("gemini")).unwrap();
        let path_var = std::ffi::OsString::from(dir.display().to_string());
        assert_eq!(on_path_in("gemini", &path_var), None);
    }
}
