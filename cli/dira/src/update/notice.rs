//! Passive update notice — a cached, rate-limited nudge printed to stderr
//! after `dira status`, `dira version`, and `dira daemon status`.
//!
//! Per the production-distribution plan's §A5, **the foreground process
//! never performs network I/O for this**: [`maybe_print`] reads a small
//! TTL'd cache file (`project_dirs()?.cache_dir()/update-check.json` — a
//! ~100-byte `fs::read` + `serde_json::from_slice`; a missing or corrupt file
//! just means "no notice", never an error) and, only when that cache is
//! stale, writes a `checked_at` sentinel *before* spawning (so two
//! concurrent invocations don't both spawn a checker) and then launches a
//! fully detached `dira update --check` without waiting on it.
//!
//! Output goes to stderr, never stdout, so `dira status | cat` stays
//! byte-identical whether or not a notice is pending. `cache_dir()` is
//! otherwise unused in this codebase (only `config_dir`/`data_dir` are) —
//! that's deliberate: this cache is pure disposable derived state, so
//! deleting it is always harmless and it never ends up in a config backup.

use dira_core::{config::project_dirs, Config};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    env, fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// On-disk shape of `update-check.json`:
/// ```jsonc
/// { "checked_at": 1770000000, "latest": "0.3.0", "channel": "stable", "error": null }
/// ```
/// Written by `dira update --check` (T4); read (never written to with real
/// data) here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
struct Cache {
    /// Unix seconds when this entry was written.
    checked_at: i64,
    /// The resolved latest version for `channel`, once a check has
    /// succeeded. `None` for a fresh sentinel or a failed check.
    #[serde(default)]
    latest: Option<String>,
    /// The channel the check resolved against (`stable`/`prerelease`).
    #[serde(default)]
    channel: Option<String>,
    /// The last check's error message, if it failed. Its presence (not its
    /// content) selects the shorter negative TTL below.
    #[serde(default)]
    error: Option<String>,
    /// How many `dira update` attempts have failed in a row, reset to 0 by the
    /// next success.
    ///
    /// Distinct from [`Cache::error`], which is about the *resolve* half
    /// (`--check`): a resolve can succeed — telling us a new version exists —
    /// while every attempt to actually install it fails. That combination is
    /// what stranded a user on an old version for a week while the notice kept
    /// cheerfully advertising the upgrade, so it needs its own counter.
    ///
    /// Written by `update::run` (the command), never by [`maybe_print`] — the
    /// foreground notice path stays read-only and network-free per D-0006.
    ///
    /// Only the count is persisted, not the last error: the escalated notice
    /// tells the user to re-run `dira update` to see why, which prints the
    /// real, current error rather than a stale one from a previous release.
    #[serde(default)]
    update_failures: u32,
}

/// Consecutive failures before the notice stops repeating an unqualified "run
/// `dira update`". One failure is a blip — and is now retried internally
/// (`update::artifact`), so a single one that still surfaced is worth
/// mentioning but not alarming. Two in a row means the retries aren't covering
/// it and the user deserves to know before they lose a week to it.
pub(crate) const ESCALATE_AFTER_FAILURES: u32 = 2;

/// Re-check at most once a day after a successful check.
const SUCCESS_TTL_SECS: i64 = 24 * 60 * 60;
/// Re-check sooner after a failed check (e.g. offline), but still
/// rate-limited — an offline laptop must not respawn a checker every command.
const FAILURE_TTL_SECS: i64 = 6 * 60 * 60;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cache_path() -> Option<PathBuf> {
    project_dirs().map(|d| d.cache_dir().join("update-check.json"))
}

/// Read the cache file. A missing path, unreadable file, or corrupt JSON all
/// collapse to "no cache" — this must never surface as an error to the
/// caller, and a corrupt ~100-byte file is exactly the kind of thing a user
/// can safely `rm` if it ever matters.
fn read_cache(path: &Path) -> Option<Cache> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// What the update cache knows, for a reader outside this module.
///
/// A flattened copy rather than exposing [`Cache`]: the on-disk shape is this
/// module's business, and the only other consumer — `doctor`'s `update.lands`
/// — wants a snapshot to judge, not a file format to maintain.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CacheFacts {
    /// Unix seconds of the last check.
    pub checked_at: i64,
    /// Latest version the last successful check resolved, if any.
    pub latest: Option<String>,
    /// Consecutive `dira update` failures — see [`Cache::update_failures`].
    pub update_failures: u32,
}

/// Read the update-check cache, or `None` when nothing has ever written it.
///
/// Read-only and network-free, so it is safe on any path D-0006 governs.
/// `None` means "no evidence", which every caller must treat as a skip rather
/// than as a healthy answer.
pub(crate) fn cache_facts() -> Option<CacheFacts> {
    let cache = read_cache(&cache_path()?)?;
    Some(CacheFacts {
        checked_at: cache.checked_at,
        latest: cache.latest,
        update_failures: cache.update_failures,
    })
}

/// True once `cache` is old enough to warrant a refresh. `checked_at: 0`
/// (never written) is always stale. Uses the success or failure TTL
/// depending on whether the last check errored.
fn is_stale(cache: &Cache, now: i64) -> bool {
    let ttl = if cache.error.is_some() {
        FAILURE_TTL_SECS
    } else {
        SUCCESS_TTL_SECS
    };
    now.saturating_sub(cache.checked_at) >= ttl
}

/// Environment inputs to the suppression matrix, gathered once so
/// [`should_notify`] takes no direct env/fs dependency and the whole matrix
/// is unit-testable without touching real env vars or the filesystem.
#[derive(Debug, Clone, Copy, Default)]
struct Env {
    /// `CI` is set to anything.
    ci: bool,
    /// `NO_UPDATE_NOTIFIER` is set (the npm-ecosystem convention).
    no_update_notifier: bool,
    /// `DIRA_NO_UPDATE_CHECK=1` (or any non-`0` value).
    no_update_check: bool,
    /// The `update.check` config knob is `off`.
    knob_off: bool,
    /// The running binary looks like a `target/{release,debug}` dev build.
    dev_build: bool,
}

/// A boolean-ish env var value counts as "set" for anything except `"0"` —
/// lets `DIRA_NO_UPDATE_CHECK=0` explicitly mean "not disabled", distinct
/// from the var being merely absent (both leave checking enabled, but for
/// different reasons worth being able to state).
fn is_truthy_env_value(v: &std::ffi::OsStr) -> bool {
    v != std::ffi::OsStr::new("0")
}

impl Env {
    fn from_process(config: &Config) -> Self {
        Env {
            ci: env::var_os("CI").is_some(),
            no_update_notifier: env::var_os("NO_UPDATE_NOTIFIER").is_some(),
            no_update_check: env::var_os("DIRA_NO_UPDATE_CHECK")
                .is_some_and(|v| is_truthy_env_value(&v)),
            knob_off: !config.update.check,
            dev_build: is_dev_build(),
        }
    }

    /// The subset of the matrix that means "checking should not happen at
    /// all" (explicit user/environment intent), as opposed to "don't print
    /// it *this time*" (`ci`/`no_update_notifier`/a non-TTY stderr, which are
    /// about display context and still leave the background refresh free to
    /// keep the cache warm for a later interactive session). This also gates
    /// [`maybe_print`]'s background-refresh spawn, not just the notice text.
    fn checking_disabled(&self) -> bool {
        self.no_update_check || self.knob_off || self.dev_build
    }
}

/// Minimal dev-build detection: true if the running executable's resolved
/// path has a `target/{release,debug}` ancestor. Nagging someone running
/// straight out of `just install` (which symlinks into `target/release`) is
/// pure noise.
fn is_dev_build() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    is_dev_build_path(&exe)
}

/// The path-comparison half of [`is_dev_build`], split out so it is testable
/// against an arbitrary path instead of only the test binary's own
/// `current_exe()` (which — being built by `cargo test` — always resolves
/// under `target/debug` regardless of what this function does).
///
/// The predicate is
/// [`replace::under_target_release_or_debug`](super::replace::under_target_release_or_debug),
/// shared with the D-0004 install guard so the two cannot disagree.
///
/// They did. This side matched a `target` component and a `release`/`debug`
/// component *anywhere* in the path; the guard requires them *adjacent*. So
/// `/home/target/x/release/dira` read as a dev build here and as an ordinary
/// install there — the passive notice nagging about an upgrade `dira update`
/// would then refuse, which is precisely the disagreement D-0006's directive
/// anticipated. Sharing the predicate is that directive's own escape clause
/// (unify if the detection rule changes) taken up.
///
/// What is deliberately NOT shared is the subject. The guard reads the *PATH
/// entry* via `symlink_metadata`, because it must tell a dev symlink from a
/// dev build to decide install behaviour (refuse a symlink unless `--force`,
/// always refuse a build). This only needs to know what is running now.
///
/// Canonicalizing first is load-bearing and stays here: `current_exe()` does
/// not resolve symlinks consistently across platforms — on Linux it reads
/// `/proc/self/exe`, already fully resolved, but on macOS it can return the
/// path as invoked. A `just install` PATH entry (`~/.local/bin/dira` ->
/// `target/release/dira`) therefore carries no literal `target`/`release`
/// component on macOS until this resolves it, and without that the notice
/// recognizes strictly fewer dev installs than the D-0004 guard does.
/// `canonicalize` failing (a dangling symlink, a removed exe) falls back to
/// the raw path rather than erroring — "no signal" here, same as everywhere
/// else in this module.
fn is_dev_build_path(exe: &Path) -> bool {
    let resolved = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    super::replace::under_target_release_or_debug(&resolved)
}

/// Decide whether a notice should be printed, and if so, its exact text.
/// Pure — no env/fs/network access — so the whole suppression matrix is
/// unit-testable directly.
/// "Newer" is SemVer 2.0 §11 ordering, not string inequality — see
/// [`resolve::compare_versions`](super::resolve::compare_versions). Being
/// *ahead* of the cached channel head (a prerelease build against a stable
/// cache) is silence, not a nag to install an older release: this fires on
/// ordinary commands like `dira status`, so the only thing worth interrupting
/// someone for is a genuine upgrade. `update --check` is where "you are ahead"
/// gets said out loud, because there the user asked.
fn should_notify(cache: &Cache, env: &Env, is_tty: bool, current_version: &str) -> Option<String> {
    if !is_tty || env.ci || env.no_update_notifier || env.checking_disabled() {
        return None;
    }
    let latest = cache.latest.as_deref()?;
    if super::resolve::compare_versions(latest, current_version) != Some(Ordering::Greater) {
        return None;
    }
    // Repeated failures change the advice, not just its tone: telling someone
    // to run the command that has already failed three times is what turned a
    // retryable network blip into a week of silent confusion.
    if cache.update_failures >= ESCALATE_AFTER_FAILURES {
        let n = cache.update_failures;
        return Some(format!(
            "dira {latest} is available (you have {current_version}) — {n} recent update \
             attempts failed; run `dira update` to see why"
        ));
    }
    Some(format!(
        "dira {latest} is available (you have {current_version}) — run `dira update`"
    ))
}

/// Overwrite the cache file, best-effort. Shared by every writer so the
/// on-disk shape is produced in exactly one place (the previous split between
/// this module and `mod.rs`'s hand-rolled `json!` is what made it easy to add
/// a field in one and silently drop it in the other).
///
/// Writes to a same-directory temp file, then `rename`s it onto `path` —
/// same D-0003 spirit as the binary swap, applied to a 100-byte JSON file
/// instead of an executable. A bare `fs::write` truncates in place, and this
/// file has a genuine writer race: `dira update`'s own `record_update_failure`
/// / `clear_update_failures` can run concurrently with the *detached*
/// `dira update --check` this same module spawns (`spawn_refresh`) reading
/// and rewriting the very same path from a separate process. A reader mid-way
/// through a truncated write sees a torn, unparseable file — harmless here
/// only because [`read_cache`] already treats corrupt JSON as "no cache", but
/// that silently drops whatever the truncated writer was recording (e.g. a
/// bumped failure count) rather than actually losing nothing. `rename` makes
/// every reader see either the old, complete file or the new, complete file,
/// never a partial one.
fn write_cache(path: &Path, cache: &Cache) {
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    // `ulid` is already a direct dependency (see `mod.rs`'s `Workdir`), so a
    // per-write unique suffix costs nothing new and rules out two concurrent
    // writers colliding on the same staging path.
    let staging = path.with_extension(format!("json.tmp.{}", ulid::Ulid::generate()));
    if fs::write(&staging, bytes).is_err() {
        let _ = fs::remove_file(&staging);
        return;
    }
    if fs::rename(&staging, path).is_err() {
        let _ = fs::remove_file(&staging);
    }
}

/// Read-modify-write the cache through `edit`. One place to decide what a
/// missing path or an unreadable file means (both: start from the default and
/// carry on — this is disposable derived state), so the three callers below
/// can't drift apart on that policy.
fn edit_cache(edit: impl FnOnce(&mut Cache)) {
    let Some(path) = cache_path() else {
        return;
    };
    let mut cache = read_cache(&path).unwrap_or_default();
    edit(&mut cache);
    write_cache(&path, &cache);
}

/// Refresh the *resolve* half of the cache after `dira update --check`,
/// preserving the update-failure half: a successful resolve says nothing about
/// whether installing works.
pub(super) fn record_check(
    checked_at: i64,
    latest: Option<&str>,
    channel: &str,
    error: Option<&str>,
) {
    edit_cache(|c| {
        c.checked_at = checked_at;
        c.latest = latest.map(str::to_string);
        c.channel = Some(channel.to_string());
        c.error = error.map(str::to_string);
    });
}

/// Record that a `dira update` attempt failed, incrementing the consecutive
/// count without disturbing `checked_at`/`latest` — the resolve half is still
/// accurate, and expiring it here would cost the user a background refresh for
/// no reason.
pub(super) fn record_update_failure() {
    edit_cache(|c| c.update_failures = c.update_failures.saturating_add(1));
}

/// Clear the failure count after a successful `dira update`. Called even
/// though the new binary's version makes the notice fall silent anyway: the
/// count must not survive to haunt the *next* upgrade cycle.
pub(super) fn clear_update_failures() {
    edit_cache(|c| c.update_failures = 0);
}

/// Write a `checked_at` sentinel (no `latest`/`error` yet) *before* spawning,
/// so two concurrent `dira status` invocations don't both spawn a checker.
/// Best-effort: failure to write is swallowed, matching the "never blocks,
/// never errors" contract of the whole feature.
///
/// Carries `prior`'s update-failure count through: a stale *resolve* cache
/// says nothing about whether installing works, and clearing the count here
/// would silently un-escalate the notice on every TTL rollover — exactly the
/// amnesia this feature exists to fix.
fn write_sentinel(path: &Path, now: i64, prior: &Cache) {
    write_cache(
        path,
        &Cache {
            checked_at: now,
            latest: None,
            channel: None,
            error: None,
            update_failures: prior.update_failures,
        },
    );
}

/// Spawn a fully detached `dira update --check` to refresh the cache. Never
/// waited on; any failure to even spawn (missing `current_exe()`, exec
/// failure, ...) is swallowed — this must be invisible to the caller. `dira
/// update --check` may itself still be a stub (T4 lands concurrently); that
/// is fine, since this call site never inspects its outcome.
fn spawn_refresh() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args(["update", "--check"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Without this, spawning a console-subsystem child (`dira.exe` is one)
    // from another console app briefly flashes a new console window on
    // Windows — visible noise for what's supposed to be a fully detached,
    // invisible background refresh (D-0006's "never blocks the foreground"
    // covers *behavior*; this covers the equivalent visual guarantee).
    // `CREATE_NO_WINDOW` suppresses that; it has no unix equivalent (no
    // console to flash), hence the platform gate rather than a portable flag.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd.spawn();
}

/// Print a cached "update available" notice to stderr, if one is warranted.
///
/// Never performs network I/O itself: reads the small TTL'd cache file and,
/// only when it's stale (and checking isn't disabled), writes a `checked_at`
/// sentinel and spawns a fully detached `dira update --check` without
/// waiting on it. See the module docs for the full design.
pub fn maybe_print(config: &Config) {
    let is_tty = std::io::stderr().is_terminal();
    let env = Env::from_process(config);
    let now = now_unix();

    let Some(path) = cache_path() else {
        return;
    };
    let cache = read_cache(&path).unwrap_or_default();

    if !env.checking_disabled() && is_stale(&cache, now) {
        write_sentinel(&path, now, &cache);
        spawn_refresh();
    }

    if let Some(msg) = should_notify(&cache, &env, is_tty, env!("CARGO_PKG_VERSION")) {
        eprintln!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(checked_at: i64, latest: Option<&str>, error: Option<&str>) -> Cache {
        Cache {
            checked_at,
            latest: latest.map(str::to_string),
            channel: Some("stable".to_string()),
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    /// A cache carrying `n` consecutive `dira update` failures on top of a
    /// perfectly healthy resolve — the combination that stranded a user.
    fn cache_with_failures(latest: &str, n: u32) -> Cache {
        Cache {
            update_failures: n,
            ..cache(1_770_000_000, Some(latest), None)
        }
    }

    fn allow_env() -> Env {
        Env {
            ci: false,
            no_update_notifier: false,
            no_update_check: false,
            knob_off: false,
            dev_build: false,
        }
    }

    // --- serde round-trip -------------------------------------------------

    #[test]
    fn cache_serde_round_trips_the_documented_shape() {
        let c = cache(1_770_000_000, Some("0.3.0"), None);
        let json = serde_json::to_string(&c).unwrap();
        let back: Cache = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn cache_round_trips_through_the_documented_json_literal() {
        let raw = r#"{ "checked_at": 1770000000, "latest": "0.3.0", "channel": "stable", "error": null }"#;
        let c: Cache = serde_json::from_str(raw).unwrap();
        assert_eq!(c.checked_at, 1_770_000_000);
        assert_eq!(c.latest.as_deref(), Some("0.3.0"));
        assert_eq!(c.channel.as_deref(), Some("stable"));
        assert_eq!(c.error, None);
    }

    #[test]
    fn missing_or_corrupt_cache_reads_as_none_never_an_error() {
        let dir = tempfile_dir();
        let path = dir.join("update-check.json");
        // Missing file.
        assert_eq!(read_cache(&path), None);
        // Corrupt file.
        fs::write(&path, b"not json").unwrap();
        assert_eq!(read_cache(&path), None);
        // Empty file.
        fs::write(&path, b"").unwrap();
        assert_eq!(read_cache(&path), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_cache_behaves_as_an_always_stale_default() {
        // `maybe_print` treats a read failure as `Cache::default()`, which
        // must be stale (checked_at: 0) so a first-ever run always refreshes.
        let c = Cache::default();
        assert!(is_stale(&c, now_unix()));
        assert_eq!(c.latest, None);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dira-notice-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- TTL arithmetic -----------------------------------------------------

    #[test]
    fn success_cache_is_fresh_under_24h_and_stale_at_24h() {
        let now = 1_000_000_000;
        let fresh = cache(now - (SUCCESS_TTL_SECS - 1), Some("0.3.0"), None);
        assert!(!is_stale(&fresh, now));
        let boundary = cache(now - SUCCESS_TTL_SECS, Some("0.3.0"), None);
        assert!(is_stale(&boundary, now));
    }

    #[test]
    fn failure_cache_is_fresh_under_6h_and_stale_at_6h() {
        let now = 1_000_000_000;
        let fresh = cache(now - (FAILURE_TTL_SECS - 1), None, Some("offline"));
        assert!(!is_stale(&fresh, now));
        let boundary = cache(now - FAILURE_TTL_SECS, None, Some("offline"));
        assert!(is_stale(&boundary, now));
    }

    #[test]
    fn failure_ttl_is_shorter_than_success_ttl() {
        const { assert!(FAILURE_TTL_SECS < SUCCESS_TTL_SECS) };
    }

    #[test]
    fn never_written_cache_checked_at_zero_is_always_stale() {
        let c = Cache::default();
        assert!(is_stale(&c, now_unix()));
    }

    #[test]
    fn future_checked_at_never_underflows_and_is_not_stale() {
        // Defensive: a clock skew that makes checked_at look like it's in the
        // future must saturate, not panic or wrap, and reads as fresh.
        let now = 1_000_000_000;
        let c = cache(now + 1000, Some("0.3.0"), None);
        assert!(!is_stale(&c, now));
    }

    // --- suppression matrix (should_notify) ----------------------------------

    #[test]
    fn notifies_when_nothing_suppresses_and_a_newer_version_is_cached() {
        let c = cache(0, Some("0.3.0"), None);
        let msg = should_notify(&c, &allow_env(), true, "0.2.0").unwrap();
        assert_eq!(
            msg,
            "dira 0.3.0 is available (you have 0.2.0) — run `dira update`"
        );
    }

    // --- repeated-failure escalation -----------------------------------------

    /// A single failure stays quiet about itself: the download now retries
    /// internally, so one that still surfaced is most likely a one-off, and
    /// crying wolf on it would devalue the escalated line below.
    #[test]
    fn one_failure_does_not_change_the_notice() {
        let c = cache_with_failures("0.4.0", 1);
        assert_eq!(
            should_notify(&c, &allow_env(), true, "0.2.3").unwrap(),
            "dira 0.4.0 is available (you have 0.2.3) — run `dira update`"
        );
    }

    /// The regression guard for the reported incident: after repeated
    /// failures the notice must stop implying nothing is wrong.
    #[test]
    fn repeated_failures_escalate_the_notice() {
        let c = cache_with_failures("0.4.0", 3);
        let msg = should_notify(&c, &allow_env(), true, "0.2.3").unwrap();
        assert_eq!(
            msg,
            "dira 0.4.0 is available (you have 0.2.3) — 3 recent update attempts failed; run \
             `dira update` to see why"
        );
    }

    #[test]
    fn escalation_starts_at_the_documented_threshold() {
        let below = cache_with_failures("0.4.0", ESCALATE_AFTER_FAILURES - 1);
        assert!(!should_notify(&below, &allow_env(), true, "0.2.3")
            .unwrap()
            .contains("failed"));

        let at = cache_with_failures("0.4.0", ESCALATE_AFTER_FAILURES);
        assert!(should_notify(&at, &allow_env(), true, "0.2.3")
            .unwrap()
            .contains("recent update attempts failed"));
    }

    /// Escalation reports a failure, it never manufactures a reason to speak:
    /// every existing suppression still wins, and an up-to-date machine stays
    /// silent no matter what the counter says.
    #[test]
    fn escalation_never_overrides_suppression_or_speaks_when_up_to_date() {
        let c = cache_with_failures("0.4.0", 5);
        assert!(should_notify(&c, &allow_env(), false, "0.2.3").is_none());
        let mut env = allow_env();
        env.ci = true;
        assert!(should_notify(&c, &env, true, "0.2.3").is_none());
        // Already on the latest — nothing to nag about, failures or not.
        assert!(should_notify(&c, &allow_env(), true, "0.4.0").is_none());
    }

    /// The counter must survive a cache refresh. `write_sentinel` runs on
    /// every TTL rollover, and clearing the count there would silently
    /// un-escalate the notice roughly once a day.
    #[test]
    fn the_sentinel_preserves_the_failure_count() {
        let dir = std::env::temp_dir().join(format!("dira-notice-{}", ulid::Ulid::generate()));
        let path = dir.join("update-check.json");
        let prior = cache_with_failures("0.4.0", 4);

        write_sentinel(&path, 1_770_000_123, &prior);

        let after = read_cache(&path).expect("sentinel should be readable");
        assert_eq!(after.checked_at, 1_770_000_123);
        assert_eq!(after.latest, None, "the resolve half is deliberately reset");
        assert_eq!(after.update_failures, 4, "the failure half must survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `write_cache` must land the final file via `rename`, not a bare
    /// truncating `fs::write` — and must leave no `.tmp.<ulid>` staging file
    /// behind on the successful path (the detached-checker race this exists
    /// for is exactly what would otherwise litter the cache directory over
    /// time).
    #[test]
    fn write_cache_lands_atomically_and_leaves_no_staging_file_behind() {
        let dir = std::env::temp_dir().join(format!("dira-notice-{}", ulid::Ulid::generate()));
        let path = dir.join("update-check.json");
        let c = cache_with_failures("0.5.0", 2);

        write_cache(&path, &c);

        let read_back = read_cache(&path).expect("write_cache must produce a readable file");
        assert_eq!(read_back, c);

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            leftovers,
            vec![std::ffi::OsString::from("update-check.json")],
            "no staging file should survive a successful write: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second write must fully replace the first, not merge with it —
    /// pins that `rename` (not an append or a partial overwrite) is really
    /// what lands the file.
    #[test]
    fn write_cache_a_second_write_fully_replaces_the_first() {
        let dir = std::env::temp_dir().join(format!("dira-notice-{}", ulid::Ulid::generate()));
        let path = dir.join("update-check.json");

        write_cache(&path, &cache(1, Some("0.1.0"), None));
        write_cache(&path, &cache(2, Some("0.2.0"), None));

        let read_back = read_cache(&path).unwrap();
        assert_eq!(read_back.checked_at, 2);
        assert_eq!(read_back.latest.as_deref(), Some("0.2.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Old cache files predate both fields; they must still parse.
    #[test]
    fn a_cache_without_the_failure_fields_still_reads() {
        let dir = std::env::temp_dir().join(format!("dira-notice-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update-check.json");
        std::fs::write(
            &path,
            br#"{"checked_at":1770000000,"latest":"0.4.0","channel":"stable","error":null}"#,
        )
        .unwrap();

        let c = read_cache(&path).expect("a pre-existing cache must still parse");
        assert_eq!(c.latest.as_deref(), Some("0.4.0"));
        assert_eq!(c.update_failures, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suppressed_when_stderr_is_not_a_tty() {
        let c = cache(0, Some("0.3.0"), None);
        assert!(should_notify(&c, &allow_env(), false, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_ci_is_set() {
        let c = cache(0, Some("0.3.0"), None);
        let env = Env {
            ci: true,
            ..allow_env()
        };
        assert!(should_notify(&c, &env, true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_no_update_notifier_is_set() {
        let c = cache(0, Some("0.3.0"), None);
        let env = Env {
            no_update_notifier: true,
            ..allow_env()
        };
        assert!(should_notify(&c, &env, true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_dira_no_update_check_is_set() {
        let c = cache(0, Some("0.3.0"), None);
        let env = Env {
            no_update_check: true,
            ..allow_env()
        };
        assert!(should_notify(&c, &env, true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_the_config_knob_is_off() {
        let c = cache(0, Some("0.3.0"), None);
        let env = Env {
            knob_off: true,
            ..allow_env()
        };
        assert!(should_notify(&c, &env, true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_on_a_dev_build() {
        let c = cache(0, Some("0.3.0"), None);
        let env = Env {
            dev_build: true,
            ..allow_env()
        };
        assert!(should_notify(&c, &env, true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_cache_has_no_resolved_latest() {
        // Sentinel-only cache (a refresh is in flight, or the only prior
        // check failed): nothing to report yet.
        let c = cache(0, None, None);
        assert!(should_notify(&c, &allow_env(), true, "0.2.0").is_none());
        let c = cache(0, None, Some("offline"));
        assert!(should_notify(&c, &allow_env(), true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_already_current() {
        let c = cache(0, Some("0.2.0"), None);
        assert!(should_notify(&c, &allow_env(), true, "0.2.0").is_none());
    }

    #[test]
    fn suppressed_when_the_cached_release_is_older_than_the_running_build() {
        // #63: the v0.1.1-develop.1 smoke run nagged "dira 0.1.0 is available
        // (you have 0.1.1-develop.1)". 0.1.0 < 0.1.1-develop.1 under SemVer
        // §11, so it is a downgrade — the strings merely differ, which is all
        // the old `!=` test could see.
        let c = cache(0, Some("0.1.0"), None);
        assert!(
            should_notify(&c, &allow_env(), true, "0.1.1-develop.1").is_none(),
            "a prerelease ahead of the cached stable head must not be nagged"
        );
    }

    #[test]
    fn notifies_a_prerelease_about_a_newer_prerelease() {
        // Ordering is numeric, not lexical: develop.10 > develop.9.
        let c = cache(0, Some("0.2.0-develop.10"), None);
        assert!(should_notify(&c, &allow_env(), true, "0.2.0-develop.9").is_some());
    }

    #[test]
    fn notifies_a_prerelease_about_its_finished_stable_release() {
        // 0.2.0 outranks 0.2.0-develop.10 — the one case where a prerelease
        // user genuinely should upgrade to a stable tag.
        let c = cache(0, Some("0.2.0"), None);
        assert!(should_notify(&c, &allow_env(), true, "0.2.0-develop.10").is_some());
    }

    #[test]
    fn suppressed_when_a_version_cannot_be_ordered() {
        // "Different" is not "newer" — an unorderable cache entry makes no
        // claim rather than nagging.
        let c = cache(0, Some("not-a-version"), None);
        assert!(should_notify(&c, &allow_env(), true, "0.2.0").is_none());
    }

    #[test]
    fn env_var_value_of_zero_is_not_truthy_but_other_values_are() {
        // `DIRA_NO_UPDATE_CHECK=0` is the explicit "not disabled" spelling.
        // Pure predicate test — no real process env touched, so it's safe
        // under parallel test execution.
        use std::ffi::OsStr;
        assert!(!is_truthy_env_value(OsStr::new("0")));
        assert!(is_truthy_env_value(OsStr::new("1")));
        assert!(is_truthy_env_value(OsStr::new("")));
        assert!(is_truthy_env_value(OsStr::new("true")));
    }

    #[test]
    fn checking_disabled_is_the_hard_disable_subset_only() {
        // ci/no_update_notifier alone must NOT disable checking (background
        // refresh should still keep the cache warm for a later interactive
        // session) — only no_update_check/knob_off/dev_build do.
        assert!(!Env {
            ci: true,
            ..allow_env()
        }
        .checking_disabled());
        assert!(!Env {
            no_update_notifier: true,
            ..allow_env()
        }
        .checking_disabled());
        assert!(Env {
            no_update_check: true,
            ..allow_env()
        }
        .checking_disabled());
        assert!(Env {
            knob_off: true,
            ..allow_env()
        }
        .checking_disabled());
        assert!(Env {
            dev_build: true,
            ..allow_env()
        }
        .checking_disabled());
    }

    // --- is_dev_build_path (symlink canonicalization) ------------------------

    #[test]
    fn dev_build_path_matches_a_plain_target_debug_path() {
        assert!(is_dev_build_path(Path::new(
            "/home/dev/dirahq-cli/target/debug/dira"
        )));
        assert!(is_dev_build_path(Path::new(
            "/home/dev/dirahq-cli/target/release/dira"
        )));
    }

    #[test]
    fn dev_build_path_does_not_match_an_installed_path() {
        assert!(!is_dev_build_path(Path::new("/home/user/.local/bin/dira")));
    }

    /// #124: the notice and the D-0004 install guard must answer the same
    /// question the same way. They did not — this side matched `target` and
    /// `release` as *separate* components anywhere in the path, so a user
    /// whose home happened to sit under a `target/` directory got nagged
    /// about an upgrade `dira update` would then refuse (or, with the paths
    /// reversed, silently never got told). Both now go through
    /// `replace::under_target_release_or_debug`, which requires adjacency.
    #[test]
    fn dev_build_path_agrees_with_the_install_guards_predicate() {
        for path in [
            // The trap: `target` and `release` both present, not adjacent.
            "/home/target/projects/release/dira",
            "/target/x/debug/dira",
            "/home/user/.local/bin/dira",
            "/home/dev/dirahq-cli/target/debug/dira",
            "/home/dev/dirahq-cli/target/release/dira",
        ] {
            let p = Path::new(path);
            assert_eq!(
                is_dev_build_path(p),
                super::super::replace::under_target_release_or_debug(p),
                "the notice and the install guard disagree about {path}"
            );
        }
    }

    /// The concrete case the shared predicate fixes, pinned on its own so a
    /// regression names the behaviour rather than just "they disagree".
    #[test]
    fn a_non_adjacent_target_and_release_is_not_a_dev_build() {
        assert!(!is_dev_build_path(Path::new(
            "/home/target/projects/release/dira"
        )));
    }

    /// The regression this exists to fix: on macOS `current_exe()` can return
    /// the path as invoked — a `just install` symlink, unresolved — which
    /// carries no `target`/`release` component at all until canonicalized.
    /// Builds a real symlink chain (`bin/dira` -> `target/release/dira`) and
    /// confirms `is_dev_build_path` still recognizes it via the *unresolved*
    /// symlink path, exactly the shape `current_exe()` can hand back.
    #[cfg(unix)]
    #[test]
    fn dev_build_path_resolves_a_symlink_into_target_release() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("target").join("release");
        fs::create_dir_all(&real_dir).unwrap();
        let real_exe = real_dir.join("dira");
        fs::write(&real_exe, b"fake").unwrap();

        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let symlink_path = bin_dir.join("dira");
        std::os::unix::fs::symlink(&real_exe, &symlink_path).unwrap();

        assert!(
            is_dev_build_path(&symlink_path),
            "an unresolved symlink into target/release must still canonicalize to a dev build"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dev_build_path_falls_back_to_the_raw_path_on_a_dangling_symlink() {
        // canonicalize() fails on a dangling symlink; this must not panic or
        // error, and must not lose the signal either — fall back to judging
        // the unresolved path itself, which is still under target/release
        // here even though the symlink's *target* does not exist.
        let dir = tempfile::tempdir().unwrap();
        let release_dir = dir.path().join("target").join("release");
        fs::create_dir_all(&release_dir).unwrap();
        let symlink_path = release_dir.join("dira");
        std::os::unix::fs::symlink(dir.path().join("nonexistent-target-binary"), &symlink_path)
            .unwrap();
        assert!(
            fs::canonicalize(&symlink_path).is_err(),
            "the fixture must actually be dangling for this test to mean anything"
        );

        assert!(
            is_dev_build_path(&symlink_path),
            "a dangling symlink whose own path is under target/release must still fall back to matching"
        );

        let installed_like = dir.path().join("bin").join("dira-does-not-exist");
        assert!(!is_dev_build_path(&installed_like));
    }

    // --- Env::from_process wiring -------------------------------------------

    #[test]
    fn from_process_maps_the_config_knob() {
        let off = Config {
            update: dira_core::config::UpdateKnobs { check: false },
            ..Config::default()
        };
        assert!(Env::from_process(&off).knob_off);
        let on = Config {
            update: dira_core::config::UpdateKnobs { check: true },
            ..Config::default()
        };
        assert!(!Env::from_process(&on).knob_off);
    }
}
