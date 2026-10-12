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
use dira_core::Config;
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

/// Resolve `--harness` values to the [`CLOUD_WIRABLE`] ids they name, in
/// order and deduplicated. Empty means all of them.
pub(crate) fn select_harnesses(harnesses: &[String]) -> Result<Vec<&'static str>> {
    if harnesses.is_empty() {
        return Ok(CLOUD_WIRABLE.to_vec());
    }
    let mut out = Vec::new();
    for h in harnesses {
        let id = dira_sources::canonical_harness_id(h)
            .and_then(|id| CLOUD_WIRABLE.iter().copied().find(|w| *w == id))
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
    Ok(out)
}

/// The corrupt-config policy a given harness selection implies. Bare `cloud
/// init` (or `--harness a,b`) touches several files the user never
/// individually named — same reasoning as `dira onboard` — so a malformed
/// one is refused, not silently discarded. Naming exactly one harness is the
/// same consent `dira init <harness>` already has for that one file.
fn on_unparseable_for(selected: &[&str]) -> OnUnparseable {
    if selected.len() == 1 {
        OnUnparseable::Overwrite
    } else {
        OnUnparseable::Refuse
    }
}

/// `dira cloud init` entrypoint. `harnesses` empty means all of
/// [`CLOUD_WIRABLE`]; `print_only` renders everything to stdout and writes
/// nothing (the `--print` contract `dira init` has); `no_pin` skips the
/// release-digest fetch and writes an unpinned `bootstrap.sh` (verified at
/// install time against the release's own `.sha256` asset instead).
///
/// A thin narrating wrapper: the write path is [`apply`], shared with
/// `dira onboard`'s cloud step and `dira cloud refresh`; the read path is
/// [`status`], shared with `dira doctor`. Only `--print` is rendered here.
pub async fn run(harnesses: &[String], print_only: bool, no_pin: bool) -> Result<()> {
    let selected = select_harnesses(harnesses)?;
    let on_unparseable = on_unparseable_for(&selected);

    let cwd = std::env::current_dir().context("resolve the current directory")?;
    let root = resolve_root(&cwd, print_only)?;
    let version = env!("CARGO_PKG_VERSION");

    if print_only {
        let bootstrap_path = root.join(".dira").join(BOOTSTRAP_SCRIPT);
        let gh_ctx = update::resolve::GhContext::from_env();
        let (sha256_x86_64, sha256_aarch64, warning) =
            resolve_digests(&bootstrap_path, version, no_pin, &gh_ctx).await;
        if let Some(w) = warning {
            eprintln!("warning: {w}");
        }
        let bootstrap = render_bootstrap(version, &sha256_x86_64, &sha256_aarch64);
        println!("# ---- .dira/hook.sh ----");
        println!("{HOOK_SH_TEMPLATE}");
        println!("# ---- .dira/bootstrap.sh ----");
        println!("{bootstrap}");
        for id in &selected {
            match *id {
                "claude" => wire_claude(&root, true, on_unparseable)?,
                "cursor" => wire_cursor(&root, true, on_unparseable)?,
                other => bail!("unknown cloud harness '{other}'"),
            };
        }
        print_snippets(version, &selected);
        return Ok(());
    }

    let applied = apply(
        &root,
        &ApplyRequest {
            harnesses: &selected,
            no_pin,
            on_unparseable,
        },
    )
    .await?;
    for w in &applied.warnings {
        eprintln!("warning: {w}");
    }
    println!(
        "wrote .dira/hook.sh + .dira/bootstrap.sh (pinned to v{})",
        applied.pinned_version
    );
    for wired in &applied.wired {
        wired.print();
    }
    print_snippets(version, &selected);
    Ok(())
}

/// `dira cloud refresh` entrypoint (DIRASH-0038). `after_update` is set only
/// by `dira update`'s post-swap step: it quiets the routine "nothing to do"
/// paths (never-wired, already up to date) that would otherwise print on
/// every single update, and it adds the "other repos still pin an older
/// dira" advisory read from the local event log.
///
/// Never resolves a cwd outside the repo the command is actually run from —
/// [`stale_known_repos`] only *reads* other repos' state, never touches
/// them. Under `--after-update` this never fails the exit code: a refresh
/// glued to every `dira update` must never turn an update's own success
/// into a failure over something as recoverable as "run `dira cloud
/// refresh` by hand later". Run directly (no `--after-update`), a real
/// error still propagates normally.
pub async fn run_refresh(config: &Config, after_update: bool) -> Result<()> {
    let running = env!("CARGO_PKG_VERSION");
    let cwd = std::env::current_dir().context("resolve the current directory")?;
    let not_wired_line = |where_: &Path| {
        format!(
            "nothing to refresh: {} has no .dira/ cloud wiring (dira onboard or dira cloud init writes it)",
            where_.display()
        )
    };

    let Some(root) = dira_core::project::toplevel(&cwd) else {
        if !after_update {
            println!("{}", not_wired_line(&cwd));
        }
        return Ok(());
    };

    let outcome = match refresh(&root, running).await {
        Ok(o) => o,
        Err(e) => {
            if after_update {
                eprintln!(
                    "warning: could not refresh cloud wiring in {}: {e:#}",
                    root.display()
                );
                return Ok(());
            }
            return Err(e);
        }
    };

    match outcome {
        RefreshOutcome::NotWired => {
            if !after_update {
                println!("{}", not_wired_line(&root));
            }
        }
        RefreshOutcome::UpToDate { pin } => {
            if !after_update {
                println!(".dira/ already pins v{pin}");
            }
        }
        RefreshOutcome::PinnedNewer { pin } => {
            println!(".dira/ pins v{pin}, newer than this dira — left alone");
        }
        RefreshOutcome::Refreshed { delta, applied, .. } => {
            for w in &applied.warnings {
                eprintln!("warning: {w}");
            }
            println!(
                "refreshed cloud wiring in {}: {} — review and commit",
                root.display(),
                delta.join(", ")
            );
        }
    }

    if after_update {
        let stale = stale_known_repos(&config.db_path, running, Some(root.as_path())).await;
        if !stale.is_empty() {
            println!(
                "{} other repo(s) still pin an older dira — run `dira cloud refresh` in each:",
                stale.len()
            );
            for (path, pin) in &stale {
                println!("  {} (v{pin})", path.display());
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// The applier — everything `cloud init` writes, as a value
// ---------------------------------------------------------------------------

/// What one [`apply`] should do. `harnesses` are [`CLOUD_WIRABLE`] ids (see
/// [`select_harnesses`]); `no_pin` skips the digest fetch; `on_unparseable`
/// is the corrupt-config policy for the harness configs it merges into.
pub(crate) struct ApplyRequest<'a> {
    pub harnesses: &'a [&'a str],
    pub no_pin: bool,
    pub on_unparseable: OnUnparseable,
}

/// What one [`apply`] did. Every flag is "actually wrote", so a caller can
/// tell a fixpoint re-run (`!changed()`) from a real change and report only
/// the delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub wrote_hook_sh: bool,
    pub wrote_bootstrap: bool,
    pub wrote_gitattributes: bool,
    /// The version now pinned in `.dira/bootstrap.sh`.
    pub pinned_version: String,
    /// Whether that pin carries release digests (false: unpinned form).
    pub digests_pinned: bool,
    /// One per requested harness, in request order.
    pub wired: Vec<Wired>,
    /// Best-effort advisories (dev build, prerelease, gitignored paths, the
    /// Windows `sh` note). Never a failure; the caller decides how to show
    /// them.
    pub warnings: Vec<String>,
}

impl Applied {
    /// Whether anything on disk changed.
    pub fn changed(&self) -> bool {
        self.wrote_hook_sh
            || self.wrote_bootstrap
            || self.wrote_gitattributes
            || self.wired.iter().any(|w| w.events_added > 0)
    }
}

/// Write the cloud artifacts under `root` (a git toplevel — callers resolve
/// it; see [`resolve_root`]). Idempotent: identical content is never
/// rewritten, hook entries are merged with replace semantics, and the report
/// says exactly what changed. Network: the digest fetch only, unless
/// `no_pin`.
pub(crate) async fn apply(root: &Path, req: &ApplyRequest<'_>) -> Result<Applied> {
    let version = env!("CARGO_PKG_VERSION");
    let dir = root.join(".dira");
    let bootstrap_path = dir.join(BOOTSTRAP_SCRIPT);
    let gh_ctx = update::resolve::GhContext::from_env();
    let mut warnings = Vec::new();

    let (sha256_x86_64, sha256_aarch64, warning) =
        resolve_digests(&bootstrap_path, version, req.no_pin, &gh_ctx).await;
    warnings.extend(warning);
    let digests_pinned = !sha256_x86_64.is_empty() && !sha256_aarch64.is_empty();
    let bootstrap = render_bootstrap(version, &sha256_x86_64, &sha256_aarch64);

    warnings.extend(dev_build_or_prerelease_warnings(version));
    if cfg!(windows) {
        warnings.push(
            ".dira/hook.sh and .dira/bootstrap.sh are POSIX shell scripts committed to the \
             repo; the hook command wired for them (`sh .dira/...`) needs Git Bash's `sh` on \
             PATH to run on this Windows machine."
                .into(),
        );
    }

    let wrote_hook_sh = write_script(&dir.join(HOOK_SCRIPT), HOOK_SH_TEMPLATE)?;
    let wrote_bootstrap = write_script(&bootstrap_path, &bootstrap)?;
    let wrote_gitattributes = write_plain(&dir.join(".gitattributes"), GITATTRIBUTES_CONTENT)?;

    let mut wired = Vec::with_capacity(req.harnesses.len());
    for id in req.harnesses {
        wired.push(match *id {
            "claude" => wire_claude(root, false, req.on_unparseable)?,
            "cursor" => wire_cursor(root, false, req.on_unparseable)?,
            other => bail!("unknown cloud harness '{other}'"),
        });
    }

    let mut committed_paths = vec![
        ".dira/hook.sh",
        ".dira/bootstrap.sh",
        ".dira/.gitattributes",
    ];
    for h in harness_configs(req.harnesses) {
        committed_paths.push(h.1);
    }
    warnings.extend(gitignored_warnings(root, &committed_paths));

    Ok(Applied {
        wrote_hook_sh,
        wrote_bootstrap,
        wrote_gitattributes,
        pinned_version: version.to_string(),
        digests_pinned,
        wired,
        warnings,
    })
}

/// The project-scope config each cloud harness is wired through, as
/// `(harness id, repo-relative path)`. Reader and writer share this row so
/// `status` looks exactly where `apply` writes.
fn harness_configs(harnesses: &[&str]) -> Vec<(&'static str, &'static str)> {
    harnesses
        .iter()
        .filter_map(|id| match *id {
            "claude" => Some(("claude", ".claude/settings.json")),
            "cursor" => Some(("cursor", ".cursor/hooks.json")),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The reader — what a repo's cloud wiring looks like, without touching it
// ---------------------------------------------------------------------------

/// One generated file's relation to what this binary would write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactState {
    Missing,
    /// Byte-identical to this binary's template.
    Current,
    /// Present but different (hand-edited, or an older template).
    Stale,
}

/// `.dira/bootstrap.sh`'s state. The pin is kept as the raw string it was
/// written with; [`RepoCloudStatus::pin_relation`] does the semver reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootstrapState {
    Missing,
    /// Present, but the `${DIRA_VERSION:-X}` pin could not be read.
    Unparseable,
    Pinned(String),
}

/// How the committed pin compares to the version asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinRelation {
    Missing,
    Unparseable,
    /// The pin is older than this binary — the one case a refresh moves it.
    Older,
    Same,
    /// The pin is newer than this binary. Never ours to lower.
    Newer,
}

/// One cloud harness's project-scope wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarnessCloudWiring {
    pub id: &'static str,
    /// Repo-relative config path (`.claude/settings.json`).
    pub path: PathBuf,
    pub config_present: bool,
    /// `false` only when the file exists and is not JSON.
    pub parseable: bool,
    /// How many events [`apply`] would add or replace. `0` is the fixpoint —
    /// this is the writer's own injection run against a copy, so the reader
    /// cannot drift from what the writer considers current.
    pub events_missing: usize,
}

impl HarnessCloudWiring {
    /// Nothing for [`apply`] to do here.
    pub fn current(&self) -> bool {
        self.config_present && self.parseable && self.events_missing == 0
    }
}

/// Everything [`status`] learned. Pure data; the judgments are methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoCloudStatus {
    /// `.dira/` exists at all — the "was this repo ever wired" signal
    /// `dira doctor` skips on.
    pub dir_present: bool,
    pub hook_sh: ArtifactState,
    pub bootstrap: BootstrapState,
    pub gitattributes: ArtifactState,
    /// One per requested harness, in request order.
    pub harnesses: Vec<HarnessCloudWiring>,
}

impl RepoCloudStatus {
    /// The committed pin against `running` (a `CARGO_PKG_VERSION`).
    pub fn pin_relation(&self, running: &str) -> PinRelation {
        let pinned = match &self.bootstrap {
            BootstrapState::Missing => return PinRelation::Missing,
            BootstrapState::Unparseable => return PinRelation::Unparseable,
            BootstrapState::Pinned(v) => v,
        };
        match (
            semver::Version::parse(pinned),
            semver::Version::parse(running),
        ) {
            (Ok(p), Ok(r)) => match p.cmp(&r) {
                std::cmp::Ordering::Less => PinRelation::Older,
                std::cmp::Ordering::Equal => PinRelation::Same,
                std::cmp::Ordering::Greater => PinRelation::Newer,
            },
            _ if pinned == running => PinRelation::Same,
            _ => PinRelation::Unparseable,
        }
    }

    /// Whether the repo already holds everything this binary would write,
    /// or something newer: nothing to do. A pin newer than `running` counts
    /// as current — the rule is that a pin only ever moves forward, so an
    /// older `dira` has nothing to contribute.
    pub fn is_current(&self, running: &str) -> bool {
        self.hook_sh == ArtifactState::Current
            && self.gitattributes == ArtifactState::Current
            && matches!(
                self.pin_relation(running),
                PinRelation::Same | PinRelation::Newer
            )
            && self.harnesses.iter().all(HarnessCloudWiring::current)
    }

    /// Nothing has ever been written: no `.dira/`, no portable hook entries.
    pub fn is_fresh(&self) -> bool {
        !self.dir_present
            && self
                .harnesses
                .iter()
                .all(|h| !h.config_present || h.events_missing > 0)
    }

    /// One line per thing [`apply`] would change at `running`, in write
    /// order. Empty exactly when [`is_current`](Self::is_current).
    pub fn describe_delta(&self, running: &str) -> Vec<String> {
        let mut out = Vec::new();
        match self.hook_sh {
            ArtifactState::Missing => out.push(".dira/hook.sh missing".into()),
            ArtifactState::Stale => {
                out.push(".dira/hook.sh differs from this dira's template".into())
            }
            ArtifactState::Current => {}
        }
        match self.pin_relation(running) {
            PinRelation::Missing => {
                out.push(format!(".dira/bootstrap.sh missing (would pin v{running})"))
            }
            PinRelation::Unparseable => out.push(format!(
                ".dira/bootstrap.sh has no readable pin (would pin v{running})"
            )),
            PinRelation::Older => {
                if let BootstrapState::Pinned(v) = &self.bootstrap {
                    out.push(format!("bootstrap pin v{v} → v{running}"));
                }
            }
            PinRelation::Same | PinRelation::Newer => {}
        }
        match self.gitattributes {
            ArtifactState::Missing => out.push(".dira/.gitattributes missing".into()),
            ArtifactState::Stale => out.push(".dira/.gitattributes differs".into()),
            ArtifactState::Current => {}
        }
        for h in &self.harnesses {
            let path = h.path.display();
            if !h.config_present {
                out.push(format!("{path}: {} event(s) to wire", h.events_missing));
            } else if !h.parseable {
                out.push(format!("{path}: not valid JSON"));
            } else if h.events_missing > 0 {
                out.push(format!("{path}: {} event(s) missing", h.events_missing));
            }
        }
        out
    }
}

/// Read what `root`'s cloud wiring looks like for `harnesses` (see
/// [`CLOUD_WIRABLE`]). **Read-only by construction**: no writes, no
/// spawns, no network — it never fetches digests, so a same-version pin
/// counts as current whether or not it carries them ([`apply`]'s content
/// diff still corrects that when it runs). Safe for `dira onboard`'s
/// detection pass (DIRASH-0029) and `dira doctor`'s gather (DIRASH-0022).
pub(crate) fn status(root: &Path, harnesses: &[&str]) -> RepoCloudStatus {
    let dir = root.join(".dira");
    let bootstrap = match std::fs::read_to_string(dir.join(BOOTSTRAP_SCRIPT)) {
        Err(_) => BootstrapState::Missing,
        Ok(text) => match parse_quoted_field(&text, "${DIRA_VERSION:-", '}') {
            Some(v) if !v.is_empty() => BootstrapState::Pinned(v),
            _ => BootstrapState::Unparseable,
        },
    };
    let wiring = harness_configs(harnesses)
        .into_iter()
        .map(|(id, rel)| {
            let path = PathBuf::from(rel);
            let (config_present, parseable, mut settings) =
                match std::fs::read_to_string(root.join(rel)) {
                    Err(_) => (false, true, serde_json::json!({})),
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(v) => (true, true, v),
                        Err(_) => (true, false, serde_json::json!({})),
                    },
                };
            // The writer's own injection against a scratch copy: the count
            // it reports is exactly what `apply` would change.
            let events_missing = match id {
                "claude" => inject_nested_hooks(
                    &mut settings,
                    CLAUDE_EVENTS,
                    "*",
                    &portable_write(&claude_command, "claude", &claude_timeout),
                ),
                _ => inject_flat_hooks(
                    &mut settings,
                    CURSOR_EVENTS,
                    &portable_write(&cursor_command, "cursor", &cursor_timeout),
                ),
            };
            HarnessCloudWiring {
                id,
                path,
                config_present,
                parseable,
                events_missing,
            }
        })
        .collect();
    RepoCloudStatus {
        dir_present: dir.is_dir(),
        hook_sh: artifact_state(&dir.join(HOOK_SCRIPT), HOOK_SH_TEMPLATE),
        bootstrap,
        gitattributes: artifact_state(&dir.join(".gitattributes"), GITATTRIBUTES_CONTENT),
        harnesses: wiring,
    }
}

fn artifact_state(path: &Path, expected: &str) -> ArtifactState {
    match std::fs::read_to_string(path) {
        Err(_) => ArtifactState::Missing,
        Ok(existing) if existing == expected => ArtifactState::Current,
        Ok(_) => ArtifactState::Stale,
    }
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

/// Advisories (never failures) when this `dira` looks like a development
/// build, or the version it is about to pin is itself a prerelease — both are
/// things `cloud init` can generate without noticing, and both are surprising
/// to discover only once a cloud VM fails to provision.
fn dev_build_or_prerelease_warnings(version: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Reuses D-0004's own dev-install predicate (`update::replace::discover_install`,
    // the same guard `dira update`/`daemon.rs` refuse a dev install with) rather
    // than a second detector that could drift from it. `Err` (e.g. `dira` isn't
    // on PATH under `--bin-dir`-less resolution) makes no claim either way.
    if matches!(
        update::replace::discover_install(None),
        Ok(update::replace::Guard::DevBuild { .. } | update::replace::Guard::DevSymlink { .. })
    ) {
        out.push(format!(
            "this `dira` looks like a development build, not an installed release — \
             the v{version} it is about to pin into .dira/bootstrap.sh may not exist as a \
             published release. Run `dira cloud init` from an installed `dira` (`dira update`) \
             once that version ships, or pass --no-pin for an unpinned bootstrap in the meantime."
        ));
    }
    if is_prerelease(version) {
        out.push(format!(
            "v{version} is a prerelease — cloud VMs provisioned from this pin will \
             install a prerelease build of dira. Re-run `dira cloud init` from a stable release \
             if that isn't intended."
        ));
    }
    out
}

fn is_prerelease(version: &str) -> bool {
    semver::Version::parse(version).is_ok_and(|v| !v.pre.is_empty())
}

/// Advisories (never failures) about any of `rels` (repo-relative) that
/// `git check-ignore` matches under `root` — a committed artifact excluded by
/// the repo's own `.gitignore` silently never reaches a cloud VM, which is a
/// confusing way to discover `cloud init` "didn't work". Best-effort: a
/// missing `git`, or any other spawn failure, makes no claim.
fn gitignored_warnings(root: &Path, rels: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for rel in rels {
        let ignored = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("check-ignore")
            .arg("--quiet")
            .arg(rel)
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ignored {
            out.push(format!(
                "{rel} is excluded by this repo's .gitignore — as written it will never \
                 be committed. Add a negation rule (e.g. `!{rel}`, or `!.dira/`) to .gitignore, or \
                 remove the matching rule, then `git add {rel}`."
            ));
        }
    }
    out
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
) -> (String, String, Option<String>) {
    if no_pin {
        return (String::new(), String::new(), None);
    }
    match fetch_release_digests(version, ctx).await {
        Ok((x86_64, aarch64)) => (x86_64, aarch64, None),
        Err(e) => {
            let kept =
                existing_pin(bootstrap_path).and_then(|(pinned_version, x86_64, aarch64)| {
                    (pinned_version == version && !x86_64.is_empty() && !aarch64.is_empty())
                        .then_some((x86_64, aarch64))
                });
            let warning = if kept.is_some() {
                format!(
                    "could not refresh release digests for v{version} ({e:#}) — keeping \
                     the existing pin in .dira/bootstrap.sh unchanged."
                )
            } else {
                format!(
                    "could not fetch release digests for v{version} ({e:#}) — writing an \
                     unpinned .dira/bootstrap.sh (it verifies against the release's own .sha256 \
                     asset at install time instead). Re-run `dira cloud init` once the release is \
                     reachable, or pass --no-pin to silence this."
                )
            };
            let (x86_64, aarch64) = kept.unwrap_or_default();
            (x86_64, aarch64, Some(warning))
        }
    }
}

/// Parse the `version`/`expected_x86_64`/`expected_aarch64` lines out of a
/// previously generated `bootstrap.sh` on disk, `None` if it doesn't exist or
/// doesn't parse. [`status`] and `dira doctor` read the version half through
/// the same [`parse_quoted_field`] marker, so there is one spelling of the
/// generated pin line in the codebase.
pub(crate) fn existing_pin(path: &Path) -> Option<(String, String, String)> {
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
/// Returns whether it wrote, like [`write_plain`].
fn write_script(path: &Path, content: &str) -> Result<bool> {
    let wrote = write_plain(path, content)?;
    #[cfg(unix)]
    if wrote {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(wrote)
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

// ---------------------------------------------------------------------------
// Refresh — `dira cloud refresh` / `dira update`'s post-swap step (DIRASH-0038)
// ---------------------------------------------------------------------------

/// What [`refresh`] found, and — if anything moved — what it did.
#[derive(Debug)]
pub(crate) enum RefreshOutcome {
    /// `root` was never wired for cloud (`.dira/` does not exist). Refresh
    /// never creates it — run `dira cloud init` / `dira onboard` for that.
    NotWired,
    /// The committed pin already matches `running`; nothing to do.
    UpToDate { pin: String },
    /// The committed pin is newer than `running` — never ours to lower.
    PinnedNewer { pin: String },
    /// The pin (and/or `hook.sh` / `.gitattributes` / harness wiring) moved
    /// forward to `running`. `from`/`to` are read by tests pinning the
    /// never-downgrades rule; `run_refresh` reports the same fact via
    /// `delta`/`applied` instead.
    Refreshed {
        #[allow(dead_code)]
        from: Option<String>,
        #[allow(dead_code)]
        to: String,
        delta: Vec<String>,
        applied: Applied,
    },
}

/// Refresh `root`'s already-committed cloud wiring up to `running` (a
/// `CARGO_PKG_VERSION`): `.dira/hook.sh`, `.dira/bootstrap.sh`'s pin, and any
/// harness config already wired. Called by `dira cloud refresh`, and — via
/// `dira update`'s post-swap step, from the freshly installed binary — after
/// a successful `dira update` (DIRASH-0038).
///
/// **Never creates `.dira/`.** A repo that was never wired for cloud stays
/// unwired here; that's `dira cloud init` / `dira onboard`'s job.
/// **Never lowers a pin.** A pin newer than `running` (e.g. this machine's
/// `dira` hasn't updated yet, but a teammate's has, and pushed a newer pin)
/// is left exactly alone — see [`RepoCloudStatus::pin_relation`].
/// **Only refreshes harnesses already wired.** A repo that only ever wired
/// Claude stays Claude-only; if none of [`CLOUD_WIRABLE`]'s project configs
/// are present, the harness-agnostic scripts (`hook.sh`, `bootstrap.sh`,
/// `.gitattributes`) are still refreshed, with an empty harness set.
///
/// A thin wrapper over [`refresh_with`], which takes `no_pin` as a seam:
/// this always passes `false` (a real refresh always tries to pin release
/// digests), unit tests pass `true` to avoid the network fetch.
pub(crate) async fn refresh(root: &Path, running: &str) -> Result<RefreshOutcome> {
    refresh_with(root, running, false).await
}

async fn refresh_with(root: &Path, running: &str, no_pin: bool) -> Result<RefreshOutcome> {
    let st = status(root, CLOUD_WIRABLE);
    if !st.dir_present {
        return Ok(RefreshOutcome::NotWired);
    }
    if st.pin_relation(running) == PinRelation::Newer {
        let BootstrapState::Pinned(pin) = &st.bootstrap else {
            unreachable!("PinRelation::Newer implies a parsed pin");
        };
        return Ok(RefreshOutcome::PinnedNewer { pin: pin.clone() });
    }
    if st.is_current(running) {
        // The only way `is_current` is true without having just returned
        // `PinnedNewer` above is `PinRelation::Same`, which — like `Newer`
        // — only ever comes from a parsed pin.
        let BootstrapState::Pinned(pin) = &st.bootstrap else {
            unreachable!("is_current implies a parsed pin");
        };
        return Ok(RefreshOutcome::UpToDate { pin: pin.clone() });
    }

    let from = match &st.bootstrap {
        BootstrapState::Pinned(v) => Some(v.clone()),
        _ => None,
    };
    let delta = st.describe_delta(running);
    let harnesses: Vec<&'static str> = st
        .harnesses
        .iter()
        .filter(|h| h.config_present)
        .map(|h| h.id)
        .collect();

    let applied = apply(
        root,
        &ApplyRequest {
            harnesses: &harnesses,
            no_pin,
            on_unparseable: OnUnparseable::Refuse,
        },
    )
    .await?;

    Ok(RefreshOutcome::Refreshed {
        from,
        to: applied.pinned_version.clone(),
        delta,
        applied,
    })
}

/// Other repos this machine has worked in (per the event log's `cwd`
/// column) whose `.dira/` pin is behind `running`. Read-only by
/// construction — no writes, and a missing or unopenable store answers
/// empty rather than erroring, since this is advisory context for
/// `dira cloud refresh --after-update`, never a reason to fail it.
/// `exclude` (the repo just refreshed) is dropped so it never lists itself.
pub(crate) async fn stale_known_repos(
    db_path: &Path,
    running: &str,
    exclude: Option<&Path>,
) -> Vec<(PathBuf, String)> {
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(store) = dira_core::Store::open_readonly(db_path).await else {
        return Vec::new();
    };
    let Ok(cwds) = store.distinct_event_cwds(200).await else {
        return Vec::new();
    };

    // Compare canonical paths, report git's own. git resolves symlinks in
    // the toplevel (`/private/tmp/…` on macOS for a `/tmp/…` cwd) while the
    // caller's `exclude` is whatever `current_dir` handed it, so a raw
    // compare listed the repo just refreshed as still stale. The canonical
    // form is only for the compare: on Windows `canonicalize` yields a
    // `\\?\C:\…` verbatim path nobody wants printed, and the git spelling
    // is the one the user recognises.
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let exclude = exclude.map(canon);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cwd in cwds {
        let Some(root) = dira_core::project::toplevel(Path::new(&cwd)) else {
            continue;
        };
        let key = canon(&root);
        if exclude.as_deref() == Some(key.as_path()) {
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        let st = status(&root, CLOUD_WIRABLE);
        if !st.dir_present {
            continue;
        }
        match st.pin_relation(running) {
            PinRelation::Older => {
                if let BootstrapState::Pinned(v) = &st.bootstrap {
                    out.push((root, v.clone()));
                }
            }
            PinRelation::Unparseable => out.push((root, "?".to_string())),
            _ => {}
        }
    }
    out
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
        let (x86_64, aarch64, _) = resolve_digests(&bootstrap_path, "9.9.9", true, &ctx).await;
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
        let (x86_64, aarch64, _) = resolve_digests(&bootstrap_path, "1.2.3", false, &ctx).await;
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
        let (x86_64, aarch64, _) = resolve_digests(&bootstrap_path, "2.0.0", false, &ctx).await;
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
        let (x86_64, aarch64, _) = resolve_digests(&bootstrap_path, "9.9.9", false, &ctx).await;
        assert_eq!(x86_64, "");
        assert_eq!(aarch64, "");
    }

    // --- status / apply: the reader is the writer's own fixpoint test --------

    const RUNNING: &str = env!("CARGO_PKG_VERSION");

    fn all() -> &'static [&'static str] {
        CLOUD_WIRABLE
    }

    async fn apply_unpinned(root: &Path) -> Applied {
        apply(
            root,
            &ApplyRequest {
                harnesses: all(),
                no_pin: true,
                on_unparseable: OnUnparseable::Refuse,
            },
        )
        .await
        .expect("apply")
    }

    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    #[test]
    fn status_on_an_empty_repo_reports_everything_missing() {
        let dir = tempfile::tempdir().unwrap();
        let st = status(dir.path(), all());
        assert!(!st.dir_present);
        assert_eq!(st.hook_sh, ArtifactState::Missing);
        assert_eq!(st.bootstrap, BootstrapState::Missing);
        assert_eq!(st.gitattributes, ArtifactState::Missing);
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Missing);
        assert!(!st.is_current(RUNNING));
        assert!(st.is_fresh());
        assert_eq!(st.harnesses.len(), 2);
        for h in &st.harnesses {
            assert!(!h.config_present, "{}", h.id);
            assert!(h.parseable);
            assert!(h.events_missing > 0, "{}", h.id);
            assert!(!h.current());
        }
        let delta = st.describe_delta(RUNNING);
        assert!(
            delta.iter().any(|l| l.contains("hook.sh missing")),
            "{delta:?}"
        );
        assert!(
            delta.iter().any(|l| l.contains(".claude/settings.json")),
            "{delta:?}"
        );
        assert!(
            delta.iter().any(|l| l.contains(".cursor/hooks.json")),
            "{delta:?}"
        );
    }

    #[tokio::test]
    async fn status_after_apply_is_current_and_describes_no_delta() {
        let dir = tempfile::tempdir().unwrap();
        let first = apply_unpinned(dir.path()).await;
        assert!(first.changed());
        assert!(first.wrote_hook_sh && first.wrote_bootstrap && first.wrote_gitattributes);
        assert!(!first.digests_pinned);
        assert_eq!(first.pinned_version, RUNNING);
        assert_eq!(first.wired.len(), 2);
        assert!(first.wired.iter().all(|w| w.events_added > 0));

        let st = status(dir.path(), all());
        assert!(st.dir_present);
        assert!(st.is_current(RUNNING), "{:?}", st.describe_delta(RUNNING));
        assert!(!st.is_fresh());
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Same);
        assert!(st.describe_delta(RUNNING).is_empty());

        // And the applier agrees with the reader: a second run changes nothing.
        let before = snapshot(dir.path());
        let second = apply_unpinned(dir.path()).await;
        assert!(!second.changed(), "{second:?}");
        assert_eq!(before, snapshot(dir.path()));
    }

    #[tokio::test]
    async fn status_reports_an_older_pin_as_delta_and_a_newer_pin_as_not_ours_to_touch() {
        let dir = tempfile::tempdir().unwrap();
        apply_unpinned(dir.path()).await;
        let bootstrap = dir.path().join(".dira").join(BOOTSTRAP_SCRIPT);

        std::fs::write(&bootstrap, render_bootstrap("0.0.1", "", "")).unwrap();
        let st = status(dir.path(), all());
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Older);
        assert!(!st.is_current(RUNNING));
        let delta = st.describe_delta(RUNNING);
        assert_eq!(delta, vec![format!("bootstrap pin v0.0.1 → v{RUNNING}")]);

        std::fs::write(&bootstrap, render_bootstrap("99.0.0", "", "")).unwrap();
        let st = status(dir.path(), all());
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Newer);
        assert!(st.is_current(RUNNING), "a newer pin is left alone");
        assert!(st.describe_delta(RUNNING).is_empty());

        std::fs::write(&bootstrap, "#!/bin/sh\necho no pin here\n").unwrap();
        let st = status(dir.path(), all());
        assert_eq!(st.bootstrap, BootstrapState::Unparseable);
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Unparseable);
        assert!(!st.is_current(RUNNING));
    }

    #[tokio::test]
    async fn status_reports_a_hand_edited_hook_sh_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        apply_unpinned(dir.path()).await;
        let hook = dir.path().join(".dira").join(HOOK_SCRIPT);
        std::fs::write(&hook, format!("{HOOK_SH_TEMPLATE}# local tweak\n")).unwrap();
        let st = status(dir.path(), all());
        assert_eq!(st.hook_sh, ArtifactState::Stale);
        assert!(!st.is_current(RUNNING));
        assert!(st
            .describe_delta(RUNNING)
            .iter()
            .any(|l| l.contains("hook.sh differs")));
        // The applier repairs exactly that file and nothing else.
        let again = apply_unpinned(dir.path()).await;
        assert!(again.wrote_hook_sh);
        assert!(!again.wrote_bootstrap && !again.wrote_gitattributes);
        assert!(again.wired.iter().all(|w| w.events_added == 0));
    }

    #[tokio::test]
    async fn status_reports_a_partially_wired_harness_config() {
        let dir = tempfile::tempdir().unwrap();
        apply_unpinned(dir.path()).await;
        // Drop one Claude event and corrupt the Cursor file.
        let claude = dir.path().join(".claude/settings.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        v["hooks"].as_object_mut().unwrap().remove("Stop");
        std::fs::write(&claude, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        std::fs::write(dir.path().join(".cursor/hooks.json"), "{ not json").unwrap();

        let st = status(dir.path(), all());
        let claude = st.harnesses.iter().find(|h| h.id == "claude").unwrap();
        assert!(claude.config_present && claude.parseable);
        assert_eq!(claude.events_missing, 1);
        assert!(!claude.current());
        let cursor = st.harnesses.iter().find(|h| h.id == "cursor").unwrap();
        assert!(cursor.config_present && !cursor.parseable);
        assert!(!cursor.current());
        let delta = st.describe_delta(RUNNING);
        assert!(
            delta
                .iter()
                .any(|l| l == ".claude/settings.json: 1 event(s) missing"),
            "{delta:?}"
        );
        assert!(
            delta
                .iter()
                .any(|l| l == ".cursor/hooks.json: not valid JSON"),
            "{delta:?}"
        );
    }

    #[test]
    fn status_never_touches_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".dira")).unwrap();
        std::fs::write(
            dir.path().join(".dira").join(BOOTSTRAP_SCRIPT),
            render_bootstrap("0.0.1", "", ""),
        )
        .unwrap();
        let before = snapshot(dir.path());
        let _ = status(dir.path(), all());
        assert_eq!(before, snapshot(dir.path()));
    }

    #[test]
    fn select_harnesses_accepts_aliases_and_rejects_non_cloud_ones() {
        assert_eq!(select_harnesses(&[]).unwrap(), CLOUD_WIRABLE.to_vec());
        assert_eq!(
            select_harnesses(&["cursor".into(), "claude".into(), "cursor".into()]).unwrap(),
            vec!["cursor", "claude"]
        );
        assert!(select_harnesses(&["codex".into()]).is_err());
        assert!(select_harnesses(&["nope".into()]).is_err());
    }

    // --- refresh (DIRASH-0038) ------------------------------------------

    #[tokio::test]
    async fn refresh_never_creates_artifacts_and_never_downgrades() {
        // An empty repo: NotWired, and refresh must not create .dira/.
        let dir = tempfile::tempdir().unwrap();
        let before = snapshot(dir.path());
        let outcome = refresh(dir.path(), RUNNING).await.unwrap();
        assert!(matches!(outcome, RefreshOutcome::NotWired));
        assert_eq!(before, snapshot(dir.path()), "NotWired must not touch disk");

        // Wire it (unpinned — no network), then push the pin ahead of `running`.
        apply_unpinned(dir.path()).await;
        let bootstrap = dir.path().join(".dira").join(BOOTSTRAP_SCRIPT);
        std::fs::write(&bootstrap, render_bootstrap("99.0.0", "", "")).unwrap();
        let before = snapshot(dir.path());
        let outcome = refresh_with(dir.path(), RUNNING, true).await.unwrap();
        match outcome {
            RefreshOutcome::PinnedNewer { pin } => assert_eq!(pin, "99.0.0"),
            other => panic!("expected PinnedNewer, got {other:?}"),
        }
        assert_eq!(
            before,
            snapshot(dir.path()),
            "PinnedNewer must never lower the pin"
        );

        // Rewind the pin behind `running`: refresh must move it forward.
        std::fs::write(&bootstrap, render_bootstrap("0.0.1", "", "")).unwrap();
        let outcome = refresh_with(dir.path(), RUNNING, true).await.unwrap();
        match outcome {
            RefreshOutcome::Refreshed {
                from,
                to,
                delta,
                applied,
            } => {
                assert_eq!(from.as_deref(), Some("0.0.1"));
                assert_eq!(to, RUNNING);
                assert!(!delta.is_empty());
                assert_eq!(applied.pinned_version, RUNNING);
            }
            other => panic!("expected Refreshed, got {other:?}"),
        }
        let st = status(dir.path(), CLOUD_WIRABLE);
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Same);

        // Fixpoint: nothing left to refresh.
        let outcome = refresh_with(dir.path(), RUNNING, true).await.unwrap();
        match outcome {
            RefreshOutcome::UpToDate { pin } => assert_eq!(pin, RUNNING),
            other => panic!("expected UpToDate, got {other:?}"),
        }
    }

    // --- regressions: pin ordering, partial wiring, corrupt configs, the listing

    /// The develop channel ships `X.Y.Z-develop.N` builds. Semver orders a
    /// prerelease below its release, so a repo pinned by a prerelease is
    /// refreshed by the stable binary that follows it, and a stable pin is
    /// never lowered by a prerelease binary of the same base version.
    #[test]
    fn prerelease_pins_order_below_their_release() {
        let mut st = status(tempfile::tempdir().unwrap().path(), all());
        st.bootstrap = BootstrapState::Pinned("0.6.0-develop.3".into());
        assert_eq!(st.pin_relation("0.6.0"), PinRelation::Older);
        assert_eq!(st.pin_relation("0.6.0-develop.4"), PinRelation::Older);
        assert_eq!(st.pin_relation("0.6.0-develop.3"), PinRelation::Same);
        st.bootstrap = BootstrapState::Pinned("0.6.0".into());
        assert_eq!(st.pin_relation("0.6.0-develop.9"), PinRelation::Newer);
        assert_eq!(st.pin_relation("0.7.0-develop.1"), PinRelation::Older);
        st.bootstrap = BootstrapState::Pinned("not-a-version".into());
        assert_eq!(st.pin_relation("0.6.0"), PinRelation::Unparseable);
        assert_eq!(st.pin_relation("not-a-version"), PinRelation::Same);
    }

    #[test]
    fn applied_changed_reflects_any_write_or_any_new_event() {
        let quiet = Applied {
            wrote_hook_sh: false,
            wrote_bootstrap: false,
            wrote_gitattributes: false,
            pinned_version: RUNNING.into(),
            digests_pinned: true,
            wired: vec![Wired {
                label: "Claude Code (cloud)",
                kind: crate::init::Kind::Hooks,
                path: Some(PathBuf::from(".claude/settings.json")),
                command: String::new(),
                events_added: 0,
                note: None,
            }],
            warnings: Vec::new(),
        };
        assert!(!quiet.changed());
        let mut one_file = quiet.clone();
        one_file.wrote_gitattributes = true;
        assert!(one_file.changed());
        let mut one_event = quiet.clone();
        one_event.wired[0].events_added = 1;
        assert!(one_event.changed());
    }

    /// A repo that deliberately wires only Claude stays Claude-only across a
    /// refresh: the harness set is what the repo already carries, never the
    /// full `CLOUD_WIRABLE` list.
    #[tokio::test]
    async fn refresh_keeps_a_claude_only_repo_claude_only() {
        let dir = tempfile::tempdir().unwrap();
        apply(
            dir.path(),
            &ApplyRequest {
                harnesses: &["claude"],
                no_pin: true,
                on_unparseable: OnUnparseable::Refuse,
            },
        )
        .await
        .unwrap();
        assert!(!dir.path().join(".cursor/hooks.json").exists());
        let bootstrap = dir.path().join(".dira").join(BOOTSTRAP_SCRIPT);
        std::fs::write(&bootstrap, render_bootstrap("0.0.1", "", "")).unwrap();

        let outcome = refresh_with(dir.path(), RUNNING, true).await.unwrap();
        let RefreshOutcome::Refreshed { applied, .. } = outcome else {
            panic!("expected Refreshed, got {outcome:?}");
        };
        assert_eq!(applied.wired.len(), 1);
        assert_eq!(applied.wired[0].label, "Claude Code (cloud)");
        assert!(
            !dir.path().join(".cursor/hooks.json").exists(),
            "a refresh must not add a harness the repo never opted into"
        );
    }

    /// A refresh on a repo whose scripts drifted but whose configs are all
    /// absent still rewrites the scripts, with an empty harness set.
    #[tokio::test]
    async fn refresh_with_no_harness_configs_still_repairs_the_scripts() {
        let dir = tempfile::tempdir().unwrap();
        apply_unpinned(dir.path()).await;
        std::fs::remove_file(dir.path().join(".claude/settings.json")).unwrap();
        std::fs::remove_file(dir.path().join(".cursor/hooks.json")).unwrap();
        let hook = dir.path().join(".dira").join(HOOK_SCRIPT);
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();

        let outcome = refresh_with(dir.path(), RUNNING, true).await.unwrap();
        let RefreshOutcome::Refreshed { applied, .. } = outcome else {
            panic!("expected Refreshed, got {outcome:?}");
        };
        assert!(applied.wrote_hook_sh);
        assert!(applied.wired.is_empty());
        assert!(!dir.path().join(".claude/settings.json").exists());
        assert_eq!(status(dir.path(), all()).hook_sh, ArtifactState::Current);
    }

    /// Under `Refuse`, a corrupt harness config fails `apply` after the
    /// scripts are written, and the next `status` reports exactly the
    /// remaining delta — so a re-run after the user fixes the file finishes
    /// the job rather than starting over.
    #[tokio::test]
    async fn a_corrupt_config_fails_apply_after_the_scripts_and_leaves_a_precise_delta() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cursor")).unwrap();
        std::fs::write(dir.path().join(".cursor/hooks.json"), "{ nope").unwrap();

        let err = apply(
            dir.path(),
            &ApplyRequest {
                harnesses: all(),
                no_pin: true,
                on_unparseable: OnUnparseable::Refuse,
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("hooks.json"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".cursor/hooks.json")).unwrap(),
            "{ nope",
            "Refuse must leave the corrupt file untouched"
        );

        let st = status(dir.path(), all());
        assert_eq!(st.hook_sh, ArtifactState::Current);
        assert_eq!(st.pin_relation(RUNNING), PinRelation::Same);
        let delta = st.describe_delta(RUNNING);
        assert_eq!(
            delta,
            vec![".cursor/hooks.json: not valid JSON".to_string()]
        );

        // Fixed by hand: the re-run only wires cursor.
        std::fs::write(dir.path().join(".cursor/hooks.json"), "{}").unwrap();
        let again = apply_unpinned(dir.path()).await;
        assert!(!again.wrote_hook_sh && !again.wrote_bootstrap && !again.wrote_gitattributes);
        assert_eq!(again.wired[0].events_added, 0, "claude was already wired");
        assert!(again.wired[1].events_added > 0, "cursor is wired now");
    }

    /// `Overwrite` (the single-`--harness` consent) replaces a corrupt file
    /// instead of refusing.
    #[tokio::test]
    async fn a_corrupt_config_is_replaced_under_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join(".claude/settings.json"), "{ nope").unwrap();
        let applied = apply(
            dir.path(),
            &ApplyRequest {
                harnesses: &["claude"],
                no_pin: true,
                on_unparseable: OnUnparseable::Overwrite,
            },
        )
        .await
        .unwrap();
        assert!(applied.wired[0].events_added > 0);
        assert!(status(dir.path(), &["claude"]).harnesses[0].current());
    }

    async fn seed_event(store: &dira_core::Store, cwd: &Path, at: time::OffsetDateTime) {
        store
            .append(&dira_core::RawEvent {
                id: ulid::Ulid::generate().to_string(),
                at,
                session_id: "s".into(),
                harness: dira_contract::Harness::ClaudeCode,
                kind: dira_core::EventKind::UserPrompt,
                cwd: Some(cwd.display().to_string()),
                project: None,
                identity_email: None,
                branch: None,
                tool: None,
                label: None,
                activity: None,
                note: None,
            })
            .await
            .unwrap();
    }

    fn git_init(dir: &Path) {
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init in {}", dir.display());
    }

    /// The listing is read-only and precise: one row per repo (two cwds in
    /// the same work tree collapse), the repo just refreshed is excluded, a
    /// current pin is not listed, a cwd outside any repo is ignored, and a
    /// repo whose `.dira/` is gone is ignored. Nothing in any repo changes.
    #[tokio::test]
    async fn stale_known_repos_lists_each_older_repo_once_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let older = home.path().join("older");
        let current = home.path().join("current");
        let excluded = home.path().join("excluded");
        let unwired = home.path().join("unwired");
        let loose = home.path().join("loose"); // a cwd that is not a repo
        for d in [&older, &current, &excluded, &unwired, &loose] {
            std::fs::create_dir_all(d).unwrap();
        }
        for d in [&older, &current, &excluded, &unwired] {
            git_init(d);
        }
        for d in [&older, &current, &excluded] {
            apply_unpinned(d).await;
        }
        for d in [&older, &excluded] {
            std::fs::write(
                d.join(".dira").join(BOOTSTRAP_SCRIPT),
                render_bootstrap("0.0.1", "", ""),
            )
            .unwrap();
        }
        std::fs::create_dir_all(older.join("src/deep")).unwrap();

        let db = home.path().join("dira.db");
        let store = dira_core::Store::open(&db).await.unwrap();
        let t0 = time::OffsetDateTime::now_utc();
        seed_event(&store, &older, t0).await;
        seed_event(
            &store,
            &older.join("src/deep"),
            t0 + time::Duration::seconds(1),
        )
        .await;
        seed_event(&store, &current, t0).await;
        seed_event(&store, &excluded, t0).await;
        seed_event(&store, &unwired, t0).await;
        seed_event(&store, &loose, t0).await;
        // `stale_known_repos` opens the store immutable, which never reads
        // the WAL; checkpoint so the rows are in the main file.
        store.wal_checkpoint_truncate().await.unwrap();
        drop(store);

        let before: Vec<_> = [&older, &current, &excluded, &unwired]
            .iter()
            .map(|d| snapshot(d))
            .collect();
        let listed = stale_known_repos(&db, RUNNING, Some(&excluded)).await;
        let after: Vec<_> = [&older, &current, &excluded, &unwired]
            .iter()
            .map(|d| snapshot(d))
            .collect();
        assert_eq!(before, after, "the listing must not write anywhere");

        let roots: Vec<PathBuf> = listed.iter().map(|(p, _)| p.clone()).collect();
        let older_root = dira_core::project::toplevel(&older).unwrap();
        assert_eq!(roots, vec![older_root], "{listed:?}");
        assert_eq!(listed[0].1, "0.0.1");

        // No store at all: silently nothing.
        assert!(
            stale_known_repos(&home.path().join("missing.db"), RUNNING, None)
                .await
                .is_empty()
        );
    }
}
