//! `dira cloud init` — teleport dira into cloud agent runtimes.
//!
//! Cloud runtimes (Claude Code on the web, Cursor cloud agents) run each
//! session in a fresh ephemeral VM where the only reliable delivery channel
//! is the repository itself. `dira init`'s configs can't ride along: they
//! embed this machine's absolute `dira` path, which is meaningless in the VM.
//! This command therefore generates **repo-committed, portable** artifacts:
//!
//! - `.dira/hook.sh` — a POSIX wrapper resolving `dira` at run time
//!   (`PATH` → `~/.local/bin` → `/usr/local/bin`), carrying the hook shim's
//!   always-exit-0 contract even where no dira exists;
//! - `.dira/bootstrap.sh` — the teleport: in a cloud VM it installs the
//!   pinned release from GitHub, verified against the digest embedded at
//!   generation time — falling back to a fresh fetch of the release's own
//!   `.sha256` asset only when the bootstrap is unpinned (`--no-pin`) or
//!   `DIRA_VERSION` overrides the pin to a different version — then starts
//!   the daemon, claims a runner-token device when `DIRA_RUNNER_TOKEN` is
//!   set, then forwards the event; elsewhere it forwards straight through.
//!   It also answers `--install-only` (build phase) and `--provision-only`
//!   (boot phase), because Cursor cloud agents provision from
//!   `.cursor/environment.json` rather than from a session-start hook —
//!   Cursor documents `sessionStart`/`sessionEnd` for local runs but names
//!   them unavailable to cloud agents, whose hooks only start once the
//!   environment is writable, so provisioning cannot hang off one;
//! - hook entries in the **project** `.claude/settings.json` /
//!   `.cursor/hooks.json` invoking those wrappers — replacing any
//!   absolute-path dira entries `dira init` left there, so a repo never
//!   carries both a broken and a portable form of the same hook.
//!
//! Everything is idempotent: re-running rewrites only what drifted (a new
//! pinned version, a hand-edited script) and merges hook entries without
//! clobbering non-dira ones, the same posture as `dira init`.

use crate::init::{
    apply_json_settings, content_is_current, inject_flat_hooks, inject_nested_hooks, HookWrite,
    OnUnparseable, Wired, CLAUDE_EVENTS, CURSOR_EVENTS,
};
use crate::update;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// `"timeout"` (seconds) embedded beside the SessionStart/sessionStart
/// bootstrap entry only — see [`HookWrite::timeout_for`]. The provisioning
/// step (download + verify + `dira daemon start`) needs headroom past a
/// harness's default hook timeout; sized well under Claude Code's own 600s
/// default hook timeout (see `templates/dira-bootstrap.sh`'s budget comment).
///
/// [`HookWrite::timeout_for`]: crate::init::HookWrite::timeout_for
const BOOTSTRAP_TIMEOUT_SECS: u64 = 300;

/// Content of the sidecar `.dira/.gitattributes`: normalizes the committed
/// scripts to LF line endings regardless of a contributor's `core.autocrlf`,
/// so a Windows checkout can't silently turn `#!/bin/sh` into a CRLF file a
/// POSIX shell refuses to run.
const GITATTRIBUTES_CONTENT: &str = "*.sh text eol=lf\n";

/// The harnesses `dira cloud init` can wire. Claude Code and Cursor are the
/// ones with documented cloud runtimes; the other harnesses gain nothing
/// from a committed config until such a runtime exists for them.
pub const CLOUD_WIRABLE: &[&str] = &["claude", "cursor"];

/// The committed wrapper script names, shared with `init.rs`'s
/// [`command_invokes_hook`] reader so the writer and the reader cannot drift
/// on what a portable hook command looks like.
pub(crate) const HOOK_SCRIPT: &str = "hook.sh";
pub(crate) const BOOTSTRAP_SCRIPT: &str = "bootstrap.sh";

const HOOK_SH_TEMPLATE: &str = include_str!("../templates/dira-hook.sh");
const BOOTSTRAP_SH_TEMPLATE: &str = include_str!("../templates/dira-bootstrap.sh");

/// Render the bootstrap template: the pinned version plus the release
/// digests for the two Linux musl targets. Empty digests mean "unpinned" —
/// the script then verifies against the release's own `.sha256` asset.
fn render_bootstrap(version: &str, sha256_x86_64: &str, sha256_aarch64: &str) -> String {
    BOOTSTRAP_SH_TEMPLATE
        .replace("{{VERSION}}", version)
        .replace("{{SHA256_X86_64}}", sha256_x86_64)
        .replace("{{SHA256_AARCH64}}", sha256_aarch64)
}

/// `dira cloud init` entrypoint. `harnesses` empty means all of
/// [`CLOUD_WIRABLE`]; `print_only` renders everything to stdout and writes
/// nothing (the `--print` contract `dira init` has); `no_pin` skips the
/// release-digest fetch and writes an unpinned `bootstrap.sh` (verified at
/// install time against the release's own `.sha256` asset instead).
pub async fn run(harnesses: &[String], print_only: bool, no_pin: bool) -> Result<()> {
    let selected: Vec<&str> = if harnesses.is_empty() {
        CLOUD_WIRABLE.to_vec()
    } else {
        let mut out = Vec::new();
        for h in harnesses {
            let id = dira_sources::canonical_harness_id(h)
                .filter(|id| CLOUD_WIRABLE.contains(id))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown or unsupported cloud harness '{h}' (expected: {})",
                        CLOUD_WIRABLE.join(", ")
                    )
                })?;
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    };

    // Bare `cloud init` (or `--harness a,b`) touches several files the user
    // never individually named — same reasoning as `dira onboard` — so a
    // malformed one is refused, not silently discarded. Naming exactly one
    // harness is the same consent `dira init <harness>` already has for that
    // one file.
    let on_unparseable = if selected.len() == 1 {
        OnUnparseable::Overwrite
    } else {
        OnUnparseable::Refuse
    };

    let cwd = std::env::current_dir().context("resolve the current directory")?;
    let root = resolve_root(&cwd, print_only)?;

    let version = env!("CARGO_PKG_VERSION");
    let bootstrap_path = root.join(".dira/bootstrap.sh");
    let gh_ctx = update::resolve::GhContext::from_env();
    let (sha256_x86_64, sha256_aarch64) =
        resolve_digests(&bootstrap_path, version, no_pin, &gh_ctx).await;
    let bootstrap = render_bootstrap(version, &sha256_x86_64, &sha256_aarch64);

    if !print_only {
        warn_dev_build_or_prerelease(version);
        if cfg!(windows) {
            eprintln!(
                "warning: .dira/hook.sh and .dira/bootstrap.sh are POSIX shell scripts committed \
                 to the repo; the hook command wired for them (`sh .dira/...`) needs Git Bash's \
                 `sh` on PATH to run on this Windows machine."
            );
        }
    }

    if print_only {
        println!("# ---- .dira/hook.sh ----");
        println!("{HOOK_SH_TEMPLATE}");
        println!("# ---- .dira/bootstrap.sh ----");
        println!("{bootstrap}");
    } else {
        write_script(&root.join(".dira/hook.sh"), HOOK_SH_TEMPLATE)?;
        write_script(&bootstrap_path, &bootstrap)?;
        write_plain(&root.join(".dira/.gitattributes"), GITATTRIBUTES_CONTENT)?;
        println!("wrote .dira/hook.sh + .dira/bootstrap.sh (pinned to v{version})");
    }

    for id in &selected {
        let wired = match *id {
            "claude" => wire_claude(&root, print_only, on_unparseable)?,
            "cursor" => wire_cursor(&root, print_only, on_unparseable)?,
            other => bail!("unknown cloud harness '{other}'"),
        };
        wired.print();
    }

    if !print_only {
        let mut committed_paths = vec![
            ".dira/hook.sh",
            ".dira/bootstrap.sh",
            ".dira/.gitattributes",
        ];
        if selected.contains(&"claude") {
            committed_paths.push(".claude/settings.json");
        }
        if selected.contains(&"cursor") {
            committed_paths.push(".cursor/hooks.json");
        }
        warn_if_gitignored(&root, &committed_paths);
    }

    print_snippets(version, &selected);
    Ok(())
}

/// Where `cloud init` writes: the repo root, resolved via
/// `dira_core::project::toplevel`. `--print` is a preview with nothing to
/// commit, so it proceeds from `cwd` even outside a git work tree (unchanged
/// from before this anchoring existed); an actual write refuses outside one
/// — writing repo-committed artifacts nowhere-in-particular defeats the
/// point of committing them. Takes `cwd` rather than resolving it itself so
/// the git-requiredness split is testable without touching the process's
/// actual working directory.
fn resolve_root(cwd: &Path, print_only: bool) -> Result<PathBuf> {
    if print_only {
        return Ok(cwd.to_path_buf());
    }
    dira_core::project::toplevel(cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "`dira cloud init` writes repo-committed artifacts and must run inside a git work \
             tree (found none at or above {}) — cd into one, or pass --print to preview the \
             output without a repo",
            cwd.display()
        )
    })
}

/// Warn (never fail) when this `dira` looks like a development build, or the
/// version it is about to pin is itself a prerelease — both are things
/// `cloud init` can generate without noticing, and both are surprising to
/// discover only once a cloud VM fails to provision.
fn warn_dev_build_or_prerelease(version: &str) {
    // Reuses D-0004's own dev-install predicate (`update::replace::discover_install`,
    // the same guard `dira update`/`daemon.rs` refuse a dev install with) rather
    // than a second detector that could drift from it. `Err` (e.g. `dira` isn't
    // on PATH under `--bin-dir`-less resolution) makes no claim either way.
    if matches!(
        update::replace::discover_install(None),
        Ok(update::replace::Guard::DevBuild { .. } | update::replace::Guard::DevSymlink { .. })
    ) {
        eprintln!(
            "warning: this `dira` looks like a development build, not an installed release — \
             the v{version} it is about to pin into .dira/bootstrap.sh may not exist as a \
             published release. Run `dira cloud init` from an installed `dira` (`dira update`) \
             once that version ships, or pass --no-pin for an unpinned bootstrap in the meantime."
        );
    }
    if is_prerelease(version) {
        eprintln!(
            "warning: v{version} is a prerelease — cloud VMs provisioned from this pin will \
             install a prerelease build of dira. Re-run `dira cloud init` from a stable release \
             if that isn't intended."
        );
    }
}

fn is_prerelease(version: &str) -> bool {
    semver::Version::parse(version).is_ok_and(|v| !v.pre.is_empty())
}

/// Warn (never fail) about any of `rels` (repo-relative) that `git
/// check-ignore` matches under `root` — a committed artifact excluded by the
/// repo's own `.gitignore` silently never reaches a cloud VM, which is a
/// confusing way to discover `cloud init` "didn't work". Best-effort: a
/// missing `git`, or any other spawn failure, makes no claim.
fn warn_if_gitignored(root: &Path, rels: &[&str]) {
    for rel in rels {
        let ignored = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("check-ignore")
            .arg("--quiet")
            .arg(rel)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ignored {
            eprintln!(
                "warning: {rel} is excluded by this repo's .gitignore — as written it will never \
                 be committed. Add a negation rule (e.g. `!{rel}`, or `!.dira/`) to .gitignore, or \
                 remove the matching rule, then `git add {rel}`."
            );
        }
    }
}

/// Resolve the two musl release digests to embed, or the unpinned fallback.
///
/// `no_pin` skips the fetch outright. A fetch failure warns loudly and falls
/// back to the unpinned form — UNLESS the bootstrap already on disk pins this
/// exact version with non-empty digests, in which case that pin is reused:
/// [`render_bootstrap`] then reproduces the file byte-for-byte, so
/// [`write_script`]'s content check leaves it untouched rather than
/// downgrading a good pin because one re-fetch hiccupped.
async fn resolve_digests(
    bootstrap_path: &Path,
    version: &str,
    no_pin: bool,
    ctx: &update::resolve::GhContext,
) -> (String, String) {
    if no_pin {
        return (String::new(), String::new());
    }
    match fetch_release_digests(version, ctx).await {
        Ok(pair) => pair,
        Err(e) => {
            let kept =
                existing_pin(bootstrap_path).and_then(|(pinned_version, x86_64, aarch64)| {
                    (pinned_version == version && !x86_64.is_empty() && !aarch64.is_empty())
                        .then_some((x86_64, aarch64))
                });
            if kept.is_some() {
                eprintln!(
                    "warning: could not refresh release digests for v{version} ({e:#}) — keeping \
                     the existing pin in .dira/bootstrap.sh unchanged."
                );
            } else {
                eprintln!(
                    "warning: could not fetch release digests for v{version} ({e:#}) — writing an \
                     unpinned .dira/bootstrap.sh (it verifies against the release's own .sha256 \
                     asset at install time instead). Re-run `dira cloud init` once the release is \
                     reachable, or pass --no-pin to silence this."
                );
            }
            kept.unwrap_or_default()
        }
    }
}

/// Parse the `version`/`expected_x86_64`/`expected_aarch64` lines out of a
/// previously generated `bootstrap.sh` on disk, `None` if it doesn't exist or
/// doesn't parse. A small local mirror of `doctor::checks::parse_pinned_version`
/// (same find-a-marker-then-a-terminator approach) rather than a cross-package
/// reuse — that helper lives in a different WP's file.
fn existing_pin(path: &Path) -> Option<(String, String, String)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let version = parse_quoted_field(&contents, "${DIRA_VERSION:-", '}')?;
    let x86_64 = parse_quoted_field(&contents, "expected_x86_64=\"", '"')?;
    let aarch64 = parse_quoted_field(&contents, "expected_aarch64=\"", '"')?;
    Some((version, x86_64, aarch64))
}

fn parse_quoted_field(text: &str, marker: &str, terminator: char) -> Option<String> {
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find(terminator)?;
    Some(rest[..end].trim().to_string())
}

/// Fetch the release `.sha256` digests for the two musl Linux targets — the
/// only ones the bootstrap ever installs onto (see `install_dira`'s
/// `uname`-based target selection in the template).
async fn fetch_release_digests(
    version: &str,
    ctx: &update::resolve::GhContext,
) -> Result<(String, String)> {
    let http = dira_core::httpclient::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        // `read_timeout` alone only bounds the gap between individual reads —
        // a body trickling in just fast enough to keep resetting that timer
        // could still hang the command indefinitely. An overall `.timeout()`
        // caps the whole request (connect + body) so a stalled fetch can
        // never block `cloud init` past this.
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build HTTP client for release digest fetch")?;
    let x86_64 = fetch_one_digest(&http, ctx, version, "x86_64-unknown-linux-musl").await?;
    let aarch64 = fetch_one_digest(&http, ctx, version, "aarch64-unknown-linux-musl").await?;
    Ok((x86_64, aarch64))
}

/// Download `dira-<version>-<target>.sha256` and pull out the hex digest for
/// the matching tarball name. Goes straight at `DIRA_DOWNLOAD_URL` (or
/// GitHub's public release-asset URL) rather than through the GitHub API's
/// `/releases/...` lookup `dira update` uses to resolve "latest" — the
/// version is already known here (`CARGO_PKG_VERSION`), so there is nothing
/// to resolve, and skipping the API call is one fewer thing this can fail on
/// (and one fewer anonymous-rate-limit hit).
async fn fetch_one_digest(
    http: &reqwest::Client,
    ctx: &update::resolve::GhContext,
    version: &str,
    target: &str,
) -> Result<String> {
    let (archive_name, sha_name) = update::resolve::asset_names(version, target);
    let base = ctx.download_base.clone().unwrap_or_else(|| {
        format!(
            "https://github.com/{}/releases/download/v{version}",
            ctx.repo
        )
    });
    let base = base.trim_end_matches('/');
    let asset = update::resolve::AssetRef::Url(format!("{base}/{sha_name}"));

    // `download_checksum` writes to a path, not a buffer — stage it under a
    // pid+nanos-unique name in the scratch dir so two targets fetched in the
    // same run (or two concurrent `cloud init`s) never collide, then read it
    // back and clean up regardless of outcome.
    let dest = std::env::temp_dir().join(format!(
        "dira-cloud-init-{}-{target}-{}.sha256",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    ));
    let fetched = update::artifact::download_checksum(http, &asset, &dest).await;
    let contents = std::fs::read_to_string(&dest);
    let _ = std::fs::remove_file(&dest);
    fetched.with_context(|| format!("download {sha_name}"))?;
    let contents = contents.with_context(|| format!("read downloaded {sha_name}"))?;
    update::artifact::parse_sha256_file(&contents, &archive_name)
}

/// Write `content` to `path` if it differs (create dirs as needed). Returns
/// whether it actually wrote — identical content is a real no-op, so a
/// re-run after `git commit` leaves the tree clean. Shared by [`write_script`]
/// (which additionally sets the exec bit, only when it wrote) and the plain
/// `.dira/.gitattributes` sidecar, which must not be executable.
fn write_plain(path: &Path, content: &str) -> Result<bool> {
    if content_is_current(path, content) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(true)
}

/// [`write_plain`] plus the exec bit on unix, for the two committed scripts.
fn write_script(path: &Path, content: &str) -> Result<()> {
    let wrote = write_plain(path, content)?;
    #[cfg(unix)]
    if wrote {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = wrote;
    Ok(())
}

/// The portable command for a Claude Code event. SessionStart runs the
/// bootstrap (which provisions, then forwards the very event it was invoked
/// for); every other event goes straight through the wrapper.
///
/// `CLAUDE_PROJECT_DIR` resolves to the repo root in local and cloud sessions
/// alike (verified in a live cloud session), so the command works from any
/// working directory. It is spelled `${CLAUDE_PROJECT_DIR:-.}` rather than
/// `$CLAUDE_PROJECT_DIR` so that a harness build which does not export it
/// falls back to the working directory instead of resolving to `/.dira/…`
/// and failing the hook — the wrapper's exit-0 contract must not depend on
/// an ambient variable. The closing quote sits before the path tail so the
/// command still ends in the exact `hook.sh <harness>` suffix
/// [`command_invokes_hook`] matches on.
fn claude_command(event: &str) -> String {
    let script = if event == "SessionStart" {
        BOOTSTRAP_SCRIPT
    } else {
        HOOK_SCRIPT
    };
    format!("sh \"${{CLAUDE_PROJECT_DIR:-.}}\"/.dira/{script} claude")
}

/// The portable command for a Cursor event. Cursor runs hook commands from
/// the workspace root, so a repo-relative path is the stable form.
fn cursor_command(event: &str) -> String {
    let script = if event == "sessionStart" {
        BOOTSTRAP_SCRIPT
    } else {
        HOOK_SCRIPT
    };
    format!("sh .dira/{script} cursor")
}

/// `"timeout"` for the SessionStart bootstrap entry only; every other Claude
/// Code event goes through the plain wrapper, unbounded like before.
fn claude_timeout(event: &str) -> Option<u64> {
    (event == "SessionStart").then_some(BOOTSTRAP_TIMEOUT_SECS)
}

/// `"timeout"` for the sessionStart bootstrap entry only.
fn cursor_timeout(event: &str) -> Option<u64> {
    (event == "sessionStart").then_some(BOOTSTRAP_TIMEOUT_SECS)
}

/// `cloud init`'s write policy for `harness`: per-event portable commands, no
/// legacy spelling, a per-event `timeout` (the bootstrap entry only), and
/// **replace** semantics — any other dira-invoking entry for the harness (an
/// absolute-path `dira init` leftover, or a stale wrapper form) is stripped,
/// so a repo never carries both a broken and a portable form of the same
/// hook.
fn portable_write<'a>(
    command_for: &'a dyn Fn(&str) -> String,
    harness: &'a str,
    timeout_for: &'a dyn Fn(&str) -> Option<u64>,
) -> HookWrite<'a> {
    HookWrite {
        command_for,
        legacy_command: None,
        replace_dira: Some(harness),
        harness,
        timeout_for: Some(timeout_for),
    }
}

/// Merge portable commands into the project `.claude/settings.json`, at
/// `root` (see [`resolve_root`]).
fn wire_claude(root: &Path, print_only: bool, on_unparseable: OnUnparseable) -> Result<Wired> {
    let path = root.join(".claude/settings.json");
    let shown = claude_command("Stop");
    let wired = apply_json_settings(
        path,
        print_only,
        on_unparseable,
        "Claude Code (cloud)",
        &shown,
        |s| {
            inject_nested_hooks(
                s,
                CLAUDE_EVENTS,
                "*",
                &portable_write(&claude_command, "claude", &claude_timeout),
            )
        },
    )?;
    Ok(wired)
}

/// Merge portable commands into the project `.cursor/hooks.json`, at `root`
/// (see [`resolve_root`]).
fn wire_cursor(root: &Path, print_only: bool, on_unparseable: OnUnparseable) -> Result<Wired> {
    let path = root.join(".cursor/hooks.json");
    let shown = cursor_command("stop");
    apply_json_settings(
        path,
        print_only,
        on_unparseable,
        "Cursor (cloud)",
        &shown,
        |s| {
            inject_flat_hooks(
                s,
                CURSOR_EVENTS,
                &portable_write(&cursor_command, "cursor", &cursor_timeout),
            )
        },
    )
}

/// The operator-facing follow-ups that cannot be written into the repo:
/// per-environment configuration on the runtime vendor's side.
fn print_snippets(version: &str, selected: &[&str]) {
    println!();
    println!("next steps (per cloud environment, not per repo):");
    if selected.contains(&"claude") {
        println!(
            "
  Claude Code on the web — environment settings at claude.ai/code:
    1. Network access: Custom, allow your Dira cloud host (app.dirahq.sh)
       plus the default package-registry list.
    2. Environment variables:
         DIRA_RUNNER_TOKEN=<token from the dashboard's Connections page>
         DIRA_IDENTITY_EMAIL=<the email this work should be attributed to>
    3. Optional setup script (snapshot-cached fast lane; bootstrap
       self-installs without it):
         t=x86_64-unknown-linux-musl; v={version}
         cd /tmp \\
           && curl -fsSLO \"https://github.com/dodi-smart/dirahq-cli/releases/download/v$v/dira-$v-$t.tar.gz\" \\
           && curl -fsSLO \"https://github.com/dodi-smart/dirahq-cli/releases/download/v$v/dira-$v-$t.sha256\" \\
           && sha256sum -c \"dira-$v-$t.sha256\" \\
           && tar -xzf \"dira-$v-$t.tar.gz\" \\
           && install -m 0755 dira dirad /usr/local/bin/ || true"
        );
    }
    if selected.contains(&"cursor") {
        println!(
            "
  Cursor cloud agents — .cursor/environment.json:
    {{
      \"install\": \"sh .dira/bootstrap.sh --install-only\",
      \"start\": \"sh .dira/bootstrap.sh --provision-only\",
      \"env\": {{ \"DIRA_RUNTIME\": \"cursor-cloud\" }}
    }}
    `install` runs once per build (cached on disk); `start` runs on every
    machine boot and is what actually brings the daemon up. Cursor cloud
    agents never run sessionStart/sessionEnd (local Cursor does), and their
    hooks only start once the environment is writable — so provisioning
    belongs here, not in hooks.json.
    Set DIRA_RUNNER_TOKEN and DIRA_IDENTITY_EMAIL as environment secrets."
        );
    }
    println!(
        "\ncommit .dira/ and the hook configs; see docs/cloud-runtimes.md for the full guide."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::command_invokes_hook;
    use serde_json::json;

    #[test]
    fn generated_scripts_are_substituted_and_shell_sane() {
        let bootstrap = render_bootstrap("9.9.9", "", "");
        assert!(
            !bootstrap.contains("{{"),
            "every placeholder substituted: {bootstrap}"
        );
        assert!(bootstrap.contains("9.9.9"));
        for script in [HOOK_SH_TEMPLATE, &bootstrap] {
            assert!(script.starts_with("#!/bin/sh\n"), "POSIX shebang");
            assert!(
                !script.contains("#!/bin/bash"),
                "must not require bash — cloud VMs run it via `sh`"
            );
        }
        // The invariants the spec pins: hook.sh never fails, bootstrap
        // forwards the event it was invoked for.
        assert!(HOOK_SH_TEMPLATE.trim_end().ends_with("exit 0"));
        assert!(bootstrap
            .trim_end()
            .ends_with("exec sh \"$dir/hook.sh\" \"$harness\""));
        // All three invocation modes stay wired. --provision-only is what a
        // Cursor cloud environment's `start` calls; losing it silently would
        // leave those agents with no daemon and no error.
        for mode in ["--install-only", "--provision-only"] {
            assert!(bootstrap.contains(mode), "bootstrap must handle {mode}");
        }
    }

    #[test]
    fn portable_commands_are_recognised_by_the_shared_matcher() {
        // The reader (`doctor`, idempotency checks) must see the wrapper
        // forms as wired, or `cloud init` output reads as broken.
        for event in ["SessionStart", "Stop", "PreToolUse"] {
            assert!(
                command_invokes_hook(&claude_command(event), "claude"),
                "{event}"
            );
        }
        for event in ["sessionStart", "stop"] {
            assert!(
                command_invokes_hook(&cursor_command(event), "cursor"),
                "{event}"
            );
        }
    }

    #[test]
    fn nested_injection_replaces_absolute_path_entries_and_is_idempotent() {
        // A repo that already carries `dira init`'s machine-specific entry.
        let mut s = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": "/Users/me/.local/bin/dira hook claude" } ] },
                    { "hooks": [ { "type": "command", "command": "eslint --fix" } ] }
                ]
            }
        });
        let write = portable_write(&claude_command, "claude", &claude_timeout);
        let changed = inject_nested_hooks(&mut s, CLAUDE_EVENTS, "*", &write);
        assert!(changed > 0);

        // The absolute-path entry is gone, the non-dira hook survives, the
        // portable command is present exactly once.
        let stop = s["hooks"]["Stop"].as_array().unwrap();
        let commands: Vec<&str> = stop
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .filter_map(|e| e["command"].as_str())
            .collect();
        assert!(commands.contains(&"eslint --fix"));
        assert!(commands.contains(&claude_command("Stop").as_str()));
        assert!(
            !commands.iter().any(|c| c.starts_with("/Users/")),
            "absolute-path dira entry must be replaced: {commands:?}"
        );

        // SessionStart got the bootstrap, not the plain wrapper, and carries
        // the provisioning timeout; Stop (the plain wrapper) does not.
        let session_start = &s["hooks"]["SessionStart"][0]["hooks"][0];
        assert!(session_start["command"]
            .as_str()
            .unwrap()
            .contains("bootstrap.sh"));
        assert_eq!(
            session_start["timeout"].as_u64(),
            Some(BOOTSTRAP_TIMEOUT_SECS)
        );
        let stop_entry = stop
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .find(|e| e["command"].as_str() == Some(claude_command("Stop").as_str()))
            .unwrap();
        assert!(stop_entry.get("timeout").is_none());

        // Second run: fixpoint.
        let again = inject_nested_hooks(&mut s, CLAUDE_EVENTS, "*", &write);
        assert_eq!(again, 0, "re-run must be a no-op");
    }

    #[test]
    fn flat_injection_replaces_and_converges_like_the_nested_one() {
        let mut s = json!({
            "version": 1,
            "hooks": {
                "stop": [
                    { "command": "/opt/dira hook cursor" },
                    { "command": "make lint" }
                ]
            }
        });
        let write = portable_write(&cursor_command, "cursor", &cursor_timeout);
        let changed = inject_flat_hooks(&mut s, CURSOR_EVENTS, &write);
        assert!(changed > 0);
        let stop = s["hooks"]["stop"].as_array().unwrap();
        let commands: Vec<&str> = stop.iter().filter_map(|e| e["command"].as_str()).collect();
        assert!(commands.contains(&"make lint"));
        assert!(commands.contains(&cursor_command("stop").as_str()));
        assert!(!commands.contains(&"/opt/dira hook cursor"));
        // sessionStart carries the provisioning timeout.
        let session_start = s["hooks"]["sessionStart"][0].clone();
        assert_eq!(
            session_start["timeout"].as_u64(),
            Some(BOOTSTRAP_TIMEOUT_SECS)
        );
        let again = inject_flat_hooks(&mut s, CURSOR_EVENTS, &write);
        assert_eq!(again, 0, "re-run must be a no-op");
    }

    // --- root resolution / OnUnparseable policy ----------------------------

    #[test]
    fn resolve_root_print_only_never_requires_git() {
        // A tempdir is never a git work tree; --print must still resolve to
        // it rather than erroring.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_root(dir.path(), true).unwrap(), dir.path());
    }

    #[test]
    fn resolve_root_write_mode_requires_a_git_work_tree() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_root(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("git work tree"), "got: {err}");
    }

    // --- gitattributes ------------------------------------------------------

    #[test]
    fn gitattributes_content_pins_lf_for_shell_scripts() {
        assert_eq!(GITATTRIBUTES_CONTENT, "*.sh text eol=lf\n");
    }

    // --- prerelease detection ------------------------------------------------

    #[test]
    fn is_prerelease_detects_a_develop_suffix_only() {
        assert!(is_prerelease("0.5.2-develop.1"));
        assert!(!is_prerelease("0.5.2"));
        assert!(!is_prerelease("not-a-version"));
    }

    // --- existing_pin / parse_quoted_field ------------------------------------

    #[test]
    fn existing_pin_reads_a_generated_bootstrap() {
        let bootstrap = render_bootstrap("1.2.3", "deadbeef", "cafef00d");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap.sh");
        std::fs::write(&path, &bootstrap).unwrap();
        let (version, x86_64, aarch64) = existing_pin(&path).unwrap();
        assert_eq!(version, "1.2.3");
        assert_eq!(x86_64, "deadbeef");
        assert_eq!(aarch64, "cafef00d");
    }

    #[test]
    fn existing_pin_is_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(existing_pin(&dir.path().join("bootstrap.sh")).is_none());
    }

    #[test]
    fn existing_pin_reads_the_unpinned_form_as_empty_digests() {
        let bootstrap = render_bootstrap("1.2.3", "", "");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap.sh");
        std::fs::write(&path, &bootstrap).unwrap();
        let (version, x86_64, aarch64) = existing_pin(&path).unwrap();
        assert_eq!(version, "1.2.3");
        assert_eq!(x86_64, "");
        assert_eq!(aarch64, "");
    }

    // --- resolve_digests (no_pin / a fetch failure keeping a good pin) ------

    /// A `GhContext` pointed at `base` — no process env touched, so these
    /// tests need no lock and can run fully in parallel with each other and
    /// with `dira update`'s own env-mutating tests.
    fn ctx_at(base: &str) -> update::resolve::GhContext {
        update::resolve::GhContext {
            api_url: "http://unused.invalid".to_string(),
            repo: "dodi-smart/dirahq-cli".to_string(),
            download_base: Some(base.to_string()),
            token: None,
        }
    }

    /// Nothing listens here — any attempt to actually reach it fails fast.
    const UNREACHABLE: &str = "http://127.0.0.1:1/unreachable";

    #[tokio::test]
    async fn resolve_digests_no_pin_never_fetches_and_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap_path = dir.path().join("bootstrap.sh");
        let ctx = ctx_at(UNREACHABLE);
        let (x86_64, aarch64) = resolve_digests(&bootstrap_path, "9.9.9", true, &ctx).await;
        assert_eq!(x86_64, "");
        assert_eq!(aarch64, "");
    }

    /// A failed fetch with a same-version, fully-pinned bootstrap already on
    /// disk must keep that pin rather than downgrading it to unpinned.
    #[tokio::test]
    async fn resolve_digests_keeps_a_good_pin_on_a_failed_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap_path = dir.path().join("bootstrap.sh");
        let existing = render_bootstrap("1.2.3", "deadbeef", "cafef00d");
        std::fs::write(&bootstrap_path, &existing).unwrap();

        let ctx = ctx_at(UNREACHABLE);
        let (x86_64, aarch64) = resolve_digests(&bootstrap_path, "1.2.3", false, &ctx).await;
        assert_eq!(x86_64, "deadbeef");
        assert_eq!(aarch64, "cafef00d");
    }

    /// Same failed fetch, but the on-disk bootstrap pins a *different*
    /// version — that pin no longer applies to the version being generated,
    /// so the result must fall back to unpinned rather than reusing it.
    #[tokio::test]
    async fn resolve_digests_does_not_reuse_a_pin_for_a_different_version() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap_path = dir.path().join("bootstrap.sh");
        let existing = render_bootstrap("1.0.0", "deadbeef", "cafef00d");
        std::fs::write(&bootstrap_path, &existing).unwrap();

        let ctx = ctx_at(UNREACHABLE);
        let (x86_64, aarch64) = resolve_digests(&bootstrap_path, "2.0.0", false, &ctx).await;
        assert_eq!(x86_64, "");
        assert_eq!(aarch64, "");
    }

    // --- fetch_release_digests against a scripted server --------------------

    use crate::test_support::{scripted_server, Reply};

    /// A well-formed pair of `.sha256` responses embeds both digests exactly.
    /// `scripted_server` answers connections in order regardless of the
    /// requested path, which matches `fetch_release_digests` awaiting the
    /// x86_64 fetch fully before starting the aarch64 one.
    #[tokio::test]
    async fn fetch_release_digests_embeds_both_targets_from_a_scripted_server() {
        let version = "9.9.9";
        let (archive_x86_64, _) =
            update::resolve::asset_names(version, "x86_64-unknown-linux-musl");
        let (archive_aarch64, _) =
            update::resolve::asset_names(version, "aarch64-unknown-linux-musl");
        let x86_64_digest = "a".repeat(64);
        let aarch64_digest = "b".repeat(64);
        let body_x86_64: &'static str = format!("{x86_64_digest}  {archive_x86_64}\n").leak();
        let body_aarch64: &'static str = format!("{aarch64_digest}  {archive_aarch64}\n").leak();

        let (base, _hits) =
            scripted_server(vec![Reply::Body(body_x86_64), Reply::Body(body_aarch64)]).await;
        let ctx = ctx_at(&base);
        let result = fetch_release_digests(version, &ctx).await;

        let (x86_64, aarch64) = result.expect("both fetches should succeed");
        assert_eq!(x86_64, x86_64_digest);
        assert_eq!(aarch64, aarch64_digest);
    }

    /// A 404 on the `.sha256` asset is a fetch error — the caller
    /// (`resolve_digests`) is what turns that into the unpinned fallback; this
    /// pins the error case that decision rests on.
    #[tokio::test]
    async fn fetch_release_digests_404_is_an_error() {
        let (base, _hits) = scripted_server(vec![Reply::Status(404, "")]).await;
        let ctx = ctx_at(&base);
        let err = fetch_release_digests("9.9.9", &ctx).await.unwrap_err();
        assert!(format!("{err:#}").contains("404"), "got: {err:#}");
    }

    /// The end-to-end failure path: a 404 with no existing pin on disk falls
    /// back to the fully unpinned form.
    #[tokio::test]
    async fn resolve_digests_404_falls_back_to_unpinned_with_no_existing_pin() {
        let (base, _hits) = scripted_server(vec![Reply::Status(404, "")]).await;
        let ctx = ctx_at(&base);
        let dir = tempfile::tempdir().unwrap();
        let bootstrap_path = dir.path().join("bootstrap.sh"); // never written
        let (x86_64, aarch64) = resolve_digests(&bootstrap_path, "9.9.9", false, &ctx).await;
        assert_eq!(x86_64, "");
        assert_eq!(aarch64, "");
    }
}
