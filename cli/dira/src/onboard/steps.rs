//! The individual steps of the waterfall.
//!
//! Every step takes `&State` plus a `&mut dyn Ui` and returns a
//! [`StepOutcome`]. None of them abort the run: a failure is recorded and the
//! wizard continues, because the steps are independent enough that one broken
//! harness config should not cost you the daemon service or the device link.

use super::detect::State;
use super::prompt::Ui;
use super::{Options, StepOutcome};
use crate::init::{self, OnUnparseable};
use dira_core::config::KnowledgeSyncMode;
use dira_core::Config;
use std::path::{Path, PathBuf};

/// Which harness ids to wire, before any of them are actually wired.
///
/// Pulled out of [`harnesses`] so target *selection* is testable without
/// paying for target *wiring*: the loop in `harnesses()` dispatches to
/// `init::wire`, which writes real harness configs (project- or
/// global-scope files under `$HOME`). A test must never call `harnesses()`
/// in-process for the same reason B1 moved `steps::knowledge` off a direct
/// `config_cmd::set_quiet` call — it is a real write with no test seam.
///
/// An explicit `--harness` list bypasses detection entirely and is returned
/// as-is, unprompted: the user has told us what they run, and a probe that
/// disagrees is the probe's problem, not theirs.
pub(crate) fn wiring_targets(state: &State, opts: &Options, ui: &mut dyn Ui) -> Vec<String> {
    if !opts.harness.is_empty() {
        return opts.harness.clone();
    }
    state
        .wirable()
        .into_iter()
        .filter(|h| {
            let how = match (h.on_path, h.has_config_dir) {
                (true, true) => "found on PATH and configured",
                (true, false) => "found on PATH",
                _ => "config directory found",
            };
            ui.confirm(&format!("Wire {} ({how})?", h.probe.label), true)
        })
        .map(|h| h.probe.id.to_string())
        .collect()
}

/// Step 2 — wire the harnesses.
///
/// Wires every harness the user confirms, in one pass. This is the step that
/// removes the "one `dira init` per harness" trap: previously nothing wired
/// more than one at a time, so the landing page had to warn people not to
/// assume a flag existed.
pub(crate) async fn harnesses(
    config: &Config,
    state: &State,
    opts: &Options,
    ui: &mut dyn Ui,
) -> Vec<(String, StepOutcome)> {
    let mut out = Vec::new();

    // Nothing to offer and nothing named explicitly: a distinct outcome from
    // "asked and the user declined everything", which `wiring_targets`
    // collapses to the same empty `Vec` — so this has to be checked before
    // calling it, not after.
    if opts.harness.is_empty() && state.wirable().is_empty() {
        let all_wired = state.harnesses.iter().any(|h| h.wired);
        return vec![(
            "harnesses".into(),
            if all_wired {
                StepOutcome::AlreadyDone("every detected harness is already wired".into())
            } else {
                StepOutcome::Skipped(
                    "no AI harness detected — install one, then re-run `dira onboard`".into(),
                )
            },
        )];
    }

    let targets = wiring_targets(state, opts, ui);

    for id in targets {
        let id = id.as_str();
        let label = super::detect::HARNESSES
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.label)
            .unwrap_or(id);

        // Two deliberate differences from a bare `dira init`:
        //
        // **Global scope.** `dira init` defaults to project scope, writing
        // `.claude/settings.json` into cwd — right for a command you run
        // inside the repo you want tracked. Onboarding is setting up a
        // *machine*, and project scope would silently wire only whichever
        // directory you happened to be standing in, leaving every other repo
        // uncaptured with no indication why. Grok ignores this and is always
        // user-level regardless.
        //
        // **`Refuse`, not `Overwrite`.** Onboarding writes several files the
        // user never named individually, so silently discarding an
        // unparseable config would be a surprise in a way it isn't for `dira
        // init`, where the user typed that exact path.
        let res = init::wire(id, config, true, false, OnUnparseable::Refuse).await;

        let outcome = match res {
            Ok(w) if w.path.is_none() => {
                StepOutcome::Skipped(w.note.unwrap_or_else(|| format!("{label} is print-only")))
            }
            Ok(w) if w.already_wired() => {
                StepOutcome::AlreadyDone(format!("{label} hooks already wired"))
            }
            Ok(w) => StepOutcome::Done(format!(
                "wired {label} ({} event(s)) → {}",
                w.events_added,
                w.path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )),
            Err(e) => StepOutcome::Failed(format!("{label}: {e}")),
        };
        out.push((format!("harness:{id}"), outcome));
    }
    out
}

/// Step 3 — put the daemon under a service manager.
///
/// The trap this closes: `dira daemon install` cannot bind the control socket
/// while a bare-started daemon holds it (D-0009 makes the socket the
/// single-instance guard), and the old installer's Next-steps told everyone
/// to run `dira daemon start` first. So a plain "run install" here would fail
/// for exactly the users who followed the documented path. Stopping first is
/// not a convenience — it is the only ordering that works.
pub(crate) async fn daemon(
    config: &Config,
    state: &State,
    opts: &Options,
    ui: &mut dyn Ui,
) -> StepOutcome {
    if opts.no_service {
        return StepOutcome::Skipped("--no-service".into());
    }
    if state.supervised() {
        let how = crate::daemon::supervision_label(&state.supervision)
            .unwrap_or_else(|| "a service manager".to_string());
        return StepOutcome::AlreadyDone(format!("daemon already supervised ({how})"));
    }
    if !ui.confirm(
        "Install dirad as a login service so it survives reboots?",
        true,
    ) {
        // Declining the service is not declining the daemon: an unsupervised
        // daemon still captures for this session, which is strictly better
        // than nothing running at all.
        if state.daemon_running() {
            return StepOutcome::Skipped("declined; daemon is running unsupervised".into());
        }
        return match crate::daemon::start(config).await {
            Ok(()) => {
                StepOutcome::Done("started dirad (not supervised — dies with a reboot)".into())
            }
            Err(e) => StepOutcome::Failed(format!("could not start dirad: {e}")),
        };
    }

    // No pre-stop here any more: `daemon::install` stops an unmanaged daemon
    // itself and waits for it to exit. This step used to do it, and so did both
    // installers — while a bare `dira daemon install`, the caller that needed it
    // most, did not. The ordering is unchanged; it just lives where it cannot be
    // forgotten (#123).
    match crate::daemon::install_with_supervision(config, state.supervision.clone()).await {
        Ok(()) => StepOutcome::Done("installed dirad as a login service".into()),
        // Falling back to a bare start is the honest answer when the service
        // manager refuses (a container with no systemd session, a locked-down
        // launchd): capture works now, and the summary says it will not
        // survive a reboot.
        Err(e) => match crate::daemon::start(config).await {
            Ok(()) => StepOutcome::Done(format!(
                "service install failed ({e}); started dirad unsupervised instead — \
                 it will not survive a reboot"
            )),
            Err(e2) => StepOutcome::Failed(format!(
                "service install failed ({e}); start also failed ({e2})"
            )),
        },
    }
}

/// Step 4 — link this device.
///
/// The one step that needs something from outside the terminal, so empty
/// input means skip and the run continues. Local capture is fully functional
/// unlinked; only sync and billables need this.
///
/// Takes `state` by mutable reference and flips `device_linked` on a
/// successful link: the knowledge step's "(pending — nothing syncs until this
/// device is linked)" caveat and `print_open_items`' both-ends dashboard hint
/// both read that flag, and both run after this step in the same `run()` —
/// without the write-back, a device linked mid-run still read as unlinked to
/// everything downstream of it.
pub(crate) async fn device(config: &Config, state: &mut State, ui: &mut dyn Ui) -> StepOutcome {
    if state.device_linked {
        return StepOutcome::AlreadyDone("device already linked".into());
    }
    let base = config
        .cloud_url
        .clone()
        .unwrap_or_else(|| "https://app.dirahq.sh".to_string());
    ui.say(&format!(
        "Link this device to sync and bill: open {base}/connections for a one-time code."
    ));
    let code = ui.line("Enter link code (blank to skip): ");
    if code.is_empty() {
        return StepOutcome::Skipped(
            "no code entered — run `dira device link` when you have one".into(),
        );
    }
    match crate::device::link(config, Some(code), None, None).await {
        Ok(()) => {
            state.device_linked = true;
            StepOutcome::Done("device linked".into())
        }
        Err(e) => StepOutcome::Failed(format!("link failed: {e}")),
    }
}

/// The consent text for step 5's knowledge prompt.
///
/// Named, and asserted on by a test, because it is the whole justification
/// for defaulting this to `full`: the user has to be told exactly what
/// leaves the machine. There is no other consent UX for this channel — no
/// prompt, no tier in `dira status` or `dira doctor` — so if this sentence
/// is wrong or missing, nothing else catches it.
pub(crate) const KNOWLEDGE_DISCLOSURE: &str = "\
Knowledge sync is a separate channel from time tracking, with its own consent.
  metadata  decision + spec ids, titles, status, guard globs, record hashes
  full      all of the above, plus the record bodies, commit trailer values,
            and guard check commands — the text of your decisions and specs";

/// Step 7 — the knowledge consent tier.
///
/// Last of the mutating steps, deliberately: it writes config the daemon
/// reads at startup, and step 3 (the daemon step) may have just restarted
/// it. Running after zavet's two steps (5 and 6 — see `mod::run`) means the
/// value lands on disk before the *next* daemon start rather than racing it.
///
/// Kept apart from the plugin install and the scaffold so that declining one
/// does not decline the others: a user may well want the knowledge layer
/// locally and no content sync at all.
///
/// `write_tier` is injected rather than calling `config_cmd::set_quiet`
/// directly: that function resolves its target via `project_dirs()` and
/// ignores whatever `Config` it is handed, so an in-process unit test that
/// called it wrote the developer's real `config.toml` — `onboard_e2e.rs`'s
/// `isolate_user_dirs` only contains the real binary's *subprocess*, not
/// `cargo test --bin dira` running this function in-process. `mod::run`
/// passes a closure over the real `set_quiet`; tests pass a recording stub.
pub(crate) fn knowledge(
    state: &State,
    opts: &Options,
    ui: &mut dyn Ui,
    write_tier: &dyn Fn(&str) -> anyhow::Result<PathBuf>,
) -> StepOutcome {
    // Unconditional, and above the `opts.knowledge` match on purpose: per
    // DIRASH-0030 every consent path — the interactive prompt, `--yes`, and
    // an explicit `--knowledge <tier>` — has to name exactly what `full`
    // sends before this step acts, not just the one that stops to ask.
    ui.say(KNOWLEDGE_DISCLOSURE);

    let want = match opts.knowledge {
        Some(tier) => tier,
        None => {
            if ui.confirm("Send full knowledge content to your workspace?", true) {
                KnowledgeSyncMode::Full
            } else {
                KnowledgeSyncMode::Metadata
            }
        }
    };

    if state.knowledge == want {
        return StepOutcome::AlreadyDone(format!("knowledge sync already `{}`", want.as_str()));
    }

    // Writes through `dira config set`'s own validation rather than editing
    // the TOML here, so there is exactly one place that decides what a valid
    // tier is.
    match write_tier(want.as_str()) {
        Ok(_) => {
            let mut msg = format!("knowledge sync set to `{}`", want.as_str());
            if !state.device_linked {
                // Honest rather than encouraging: the daemon's flush is gated
                // on a cloud URL and a linked device, so without the link
                // this setting is recorded and inert.
                msg.push_str(" (pending — nothing syncs until this device is linked)");
            } else if want == KnowledgeSyncMode::Full {
                msg.push_str(
                    " (your workspace must also be set to `full` for bodies to be stored)",
                );
            }
            StepOutcome::Done(msg)
        }
        Err(e) => StepOutcome::Failed(format!("could not set sync.knowledge: {e}")),
    }
}

/// Step 5 — install the zavet plugin.
pub(crate) fn zavet_plugin(state: &State, opts: &Options, ui: &mut dyn Ui) -> StepOutcome {
    if opts.no_zavet {
        return StepOutcome::Skipped("--no-zavet".into());
    }
    if !state.claude_present {
        return StepOutcome::Skipped(
            "`claude` not on PATH — install zavet from inside Claude Code with \
             `/plugin marketplace add dodi-smart/dirahq-zavet`"
                .into(),
        );
    }
    if state.zavet_installed {
        return StepOutcome::AlreadyDone(
            "zavet plugin already installed (`dira zavet install --update` to refresh)".into(),
        );
    }
    if !ui.confirm(
        "Install zavet, the knowledge layer that records why decisions were made?",
        true,
    ) {
        return StepOutcome::Skipped("declined".into());
    }
    match crate::zavet_install::install(crate::zavet_install::InstallArgs {
        scope: "user".into(),
        update: false,
        dry_run: false,
        no_adapters: false,
    }) {
        Ok(()) => StepOutcome::Done("zavet plugin installed (restart Claude Code to apply)".into()),
        Err(e) => StepOutcome::Failed(format!("zavet install failed: {e}")),
    }
}

/// Step 6 — scaffold `.zavet/` in this repo and turn the module on for it.
///
/// Shells out to the plugin's own `bin/zavet` rather than reimplementing
/// `init`. That script is ~2900 lines of POSIX sh and is also the *runtime*
/// (`gate`, `index`, `emit`), so it has to be vendored into the repo
/// regardless — a Rust reimplementation would be a second copy of logic that
/// must agree with the first, forever.
///
/// Two hard boundaries, both from DIRASH-0024:
///
/// - `zavet hooks install` is never run and `core.hooksPath` is never
///   written. That setting is exclusive and shared with Husky/lefthook; zavet
///   itself refuses to seize it, and dira silently doing so would be worse
///   than the tool that owns the feature.
/// - Nothing runs unless cwd resolves to a git toplevel, and every command is
///   pinned to that toplevel rather than inheriting the process cwd.
pub(crate) fn zavet_repo(
    runner: &dyn crate::zavet_install::Runner,
    state: &State,
    opts: &Options,
    ui: &mut dyn Ui,
) -> StepOutcome {
    if opts.no_zavet {
        return StepOutcome::Skipped("--no-zavet".into());
    }
    let Some(root) = &state.repo_root else {
        return StepOutcome::Skipped(
            "not inside a git repository — run `dira onboard` from a repo to set up its \
             knowledge layer"
                .into(),
        );
    };
    if state.has_zavet_dir {
        return StepOutcome::AlreadyDone(format!("{} already has .zavet/", root.display()));
    }
    // The scaffolder is POSIX sh. On Windows there is no interpreter for it,
    // and shipping a half-scaffolded repo would be worse than saying so.
    if cfg!(windows) {
        return StepOutcome::Skipped(
            "scaffolding needs a POSIX shell — run `/zavet:init` inside Claude Code instead".into(),
        );
    }
    // Offline first: the plugin install that just ran in `zavet_plugin`
    // wrote `installed_plugins.json`, so the cheap read answers on the common
    // path. Falling back to the spawning probe only when it cannot — each
    // `claude` invocation is a Node startup, ~0.5-2s in the middle of an
    // interactive wizard, and the previous step already paid for one.
    let Some(plugin_root) =
        crate::zavet_install::plugin_root_offline().or_else(crate::zavet_install::plugin_root)
    else {
        return StepOutcome::Skipped(
            "zavet plugin not detected yet — restart Claude Code, then run `/zavet:init`".into(),
        );
    };
    let bin = Path::new(&plugin_root).join("bin").join("zavet");
    if !bin.is_file() {
        return StepOutcome::Skipped(format!("no zavet binary at {}", bin.display()));
    }
    if !ui.confirm(
        &format!("Scaffold a .zavet/ knowledge layer in {}?", root.display()),
        true,
    ) {
        return StepOutcome::Skipped("declined".into());
    }

    let bin_str = bin.display().to_string();
    // `init` derives a sane decision-id prefix on its own when none is
    // passed; picking one is a conversation the plugin's `/zavet:init` holds,
    // not something to guess at here.
    let Some(out) = runner.run_in(root, &bin_str, &["init"]) else {
        return StepOutcome::Failed(format!("could not run {bin_str}"));
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return StepOutcome::Failed(format!("zavet init failed: {}", err.trim()));
    }
    // Adapters second: AGENTS.md's marker block, the .grok rules, and the
    // git-hook scripts under .zavet/githooks/. Writing the hook *files* is
    // fine — it is pointing `core.hooksPath` at them that is off-limits.
    let adapters_ok = runner
        .run_in(root, &bin_str, &["adapters"])
        .map(|o| o.status.success())
        .unwrap_or(false);

    let mut msg = format!("scaffolded .zavet/ in {}", root.display());
    if !adapters_ok {
        msg.push_str("; adapters not refreshed (run `zavet adapters` yourself)");
    }
    StepOutcome::Done(msg)
}

// ---------------------------------------------------------------------------
// Step — cloud:repo. Commit the portable cloud-agent wiring for this repo.
// ---------------------------------------------------------------------------

/// The `cloud:repo` decision, computed without touching disk or the network.
///
/// Shared by [`cloud_repo`] (which acts on it) and `mod::print_plan` (which
/// only describes it), so the two can never disagree about what a run would
/// do — the failure mode a hand-duplicated `if` chain in both places would
/// otherwise invite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudPlan {
    /// Nothing to do, with the reason as it should be reported.
    Skip(String),
    /// Already wired at the version now running.
    Current { root: PathBuf, ver: String },
    /// Wired, but pinned to a version newer than this binary — never ours to
    /// lower.
    NewerPin { root: PathBuf, pin: String },
    /// Nothing has ever been written here.
    Wire {
        root: PathBuf,
        harnesses: Vec<&'static str>,
    },
    /// Something has been written before, but has drifted from what this
    /// binary would produce.
    Refresh {
        root: PathBuf,
        harnesses: Vec<&'static str>,
        delta: Vec<String>,
    },
}

/// Resolve `--harness` against [`crate::cloud_init::CLOUD_WIRABLE`], in
/// `CLOUD_WIRABLE`'s own order (not the order the flags were typed in) so the
/// report and the wire entries always read claude-then-cursor. Empty
/// `--harness` means every cloud-wirable harness.
fn select_cloud_harnesses(opts: &Options) -> Vec<&'static str> {
    if opts.harness.is_empty() {
        return crate::cloud_init::CLOUD_WIRABLE.to_vec();
    }
    let requested: std::collections::HashSet<&'static str> = opts
        .harness
        .iter()
        .filter_map(|h| dira_sources::canonical_harness_id(h))
        .collect();
    crate::cloud_init::CLOUD_WIRABLE
        .iter()
        .copied()
        .filter(|id| requested.contains(id))
        .collect()
}

/// `state.cloud` was read for every `CLOUD_WIRABLE` harness (see
/// `detect::run`); reuse it when the requested set is the same, so an
/// unrestricted run costs one read instead of two. A narrower `--harness`
/// selection re-reads for exactly the harnesses asked about.
fn cloud_status_for(
    state: &State,
    root: &Path,
    selected: &[&'static str],
) -> crate::cloud_init::RepoCloudStatus {
    if selected == crate::cloud_init::CLOUD_WIRABLE {
        if let Some(s) = &state.cloud {
            return s.clone();
        }
    }
    crate::cloud_init::status(root, selected)
}

/// The pure decision behind `cloud:repo`, in the same order the step reads
/// it in.
pub(crate) fn cloud_plan(state: &State, opts: &Options) -> CloudPlan {
    if opts.no_cloud {
        return CloudPlan::Skip("--no-cloud".into());
    }
    let Some(root) = state.repo_root.clone() else {
        return CloudPlan::Skip(
            "not inside a git repository — run `dira onboard` from a repo to wire it for \
             cloud agents"
                .into(),
        );
    };
    let selected = select_cloud_harnesses(opts);
    if selected.is_empty() {
        return CloudPlan::Skip("--harness names no cloud-capable harness (claude, cursor)".into());
    }

    let running = env!("CARGO_PKG_VERSION");
    let status = cloud_status_for(state, &root, &selected);

    if status.is_current(running) {
        return if status.pin_relation(running) == crate::cloud_init::PinRelation::Newer {
            let pin = match &status.bootstrap {
                crate::cloud_init::BootstrapState::Pinned(v) => v.clone(),
                _ => running.to_string(),
            };
            CloudPlan::NewerPin { root, pin }
        } else {
            CloudPlan::Current {
                root,
                ver: running.to_string(),
            }
        };
    }

    if status.is_fresh() {
        CloudPlan::Wire {
            root,
            harnesses: selected,
        }
    } else {
        let delta = status.describe_delta(running);
        CloudPlan::Refresh {
            root,
            harnesses: selected,
            delta,
        }
    }
}

/// The project-scope config path a cloud harness is wired through, for
/// prompt wording only — `cloud_init`'s own `harness_configs` is private to
/// that module.
fn cloud_config_path(id: &str) -> &'static str {
    match id {
        "claude" => ".claude/settings.json",
        "cursor" => ".cursor/hooks.json",
        _ => "",
    }
}

/// Seam over [`crate::cloud_init::apply`] so unit tests never write a real
/// `.dira/` or `.claude/settings.json` / `.cursor/hooks.json`. Boxed-future
/// rather than `async_trait` — the crate does not depend on it, and this is
/// the only trait in the module that needs the shape.
pub(crate) trait CloudApplier {
    fn apply<'a>(
        &'a self,
        root: &'a Path,
        req: &'a crate::cloud_init::ApplyRequest<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::cloud_init::Applied>> + 'a>,
    >;
}

/// The real applier: `dira onboard`'s own writes, shared with `dira cloud
/// init` and `dira cloud refresh`.
pub(crate) struct SystemApplier;

impl CloudApplier for SystemApplier {
    fn apply<'a>(
        &'a self,
        root: &'a Path,
        req: &'a crate::cloud_init::ApplyRequest<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::cloud_init::Applied>> + 'a>,
    > {
        Box::pin(crate::cloud_init::apply(root, req))
    }
}

/// Run `apply`, report its warnings, and turn the result into a
/// [`StepOutcome`]. `fresh`/`delta` decide the wording: a first wire names
/// what got written, a refresh names the delta it was asked about before
/// applying (not a second `describe_delta` call, which after a write would
/// read empty and say nothing).
async fn apply_cloud(
    applier: &dyn CloudApplier,
    root: &Path,
    harnesses: &[&'static str],
    ui: &mut dyn Ui,
    fresh: bool,
    delta: &[String],
) -> StepOutcome {
    let req = crate::cloud_init::ApplyRequest {
        harnesses,
        no_pin: false,
        on_unparseable: OnUnparseable::Refuse,
    };
    let applied = match applier.apply(root, &req).await {
        Ok(a) => a,
        Err(e) => return StepOutcome::Failed(format!("cloud wiring: {e:#}")),
    };
    for w in &applied.warnings {
        ui.say(&format!("warning: {w}"));
    }
    if !applied.changed() {
        // Defensive: the plan said there was work, but the apply landed on a
        // fixpoint anyway (a concurrent run finished it first, say).
        return StepOutcome::AlreadyDone(format!(
            "{} already wired for cloud agents (pinned v{})",
            root.display(),
            applied.pinned_version
        ));
    }

    if fresh {
        let mut parts = vec![format!(
            ".dira/ (pinned v{}{})",
            applied.pinned_version,
            if applied.digests_pinned {
                ""
            } else {
                ", unpinned digests"
            }
        )];
        for (id, w) in harnesses.iter().zip(applied.wired.iter()) {
            if w.events_added > 0 {
                parts.push(format!("{id} {} event(s)", w.events_added));
            }
        }
        StepOutcome::Done(format!(
            "wired {} for cloud agents: {}",
            root.display(),
            parts.join(", ")
        ))
    } else {
        StepOutcome::Done(format!(
            "refreshed cloud wiring in {}: {}",
            root.display(),
            delta.join(", ")
        ))
    }
}

/// Step — commit this repo's portable cloud-agent wiring: `.dira/hook.sh` +
/// `.dira/bootstrap.sh` (pinned to this binary's version) and hook entries in
/// `.claude/settings.json` / `.cursor/hooks.json`, so Claude Code on the web
/// and Cursor cloud agents capture the repo too, not just this machine.
///
/// On by default and delta-only: a re-run reports `AlreadyDone` once the
/// pinned version and every requested harness's config match what this
/// binary would write, and a stale repo is offered exactly the lines that
/// changed, never a blind rewrite. See `cli/dira/src/cloud_init.rs`.
pub(crate) async fn cloud_repo(
    applier: &dyn CloudApplier,
    state: &State,
    opts: &Options,
    ui: &mut dyn Ui,
) -> StepOutcome {
    match cloud_plan(state, opts) {
        CloudPlan::Skip(reason) => StepOutcome::Skipped(reason),
        CloudPlan::NewerPin { root, pin } => StepOutcome::AlreadyDone(format!(
            "{} pins v{pin}, newer than this dira v{} — left alone",
            root.display(),
            env!("CARGO_PKG_VERSION")
        )),
        CloudPlan::Current { root, ver } => StepOutcome::AlreadyDone(format!(
            "{} already wired for cloud agents (pinned v{ver})",
            root.display()
        )),
        CloudPlan::Wire { root, harnesses } => {
            let paths: Vec<&str> = harnesses.iter().map(|id| cloud_config_path(id)).collect();
            let question = format!(
                "Wire {} for cloud agents? Writes .dira/ and portable hook entries in {} \
                 (commit them afterwards)",
                root.display(),
                paths.join(" + ")
            );
            if !ui.confirm(&question, true) {
                return StepOutcome::Skipped("declined".into());
            }
            apply_cloud(applier, &root, &harnesses, ui, true, &[]).await
        }
        CloudPlan::Refresh {
            root,
            harnesses,
            delta,
        } => {
            let question = format!(
                "Refresh cloud wiring in {}? ({})",
                root.display(),
                delta.join("; ")
            );
            if !ui.confirm(&question, true) {
                return StepOutcome::Skipped("declined".into());
            }
            apply_cloud(applier, &root, &harnesses, ui, false, &delta).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Supervision;
    use crate::onboard::detect::{Harness, HarnessProbe, HARNESSES};
    use crate::onboard::prompt::test_ui::ScriptedUi;

    fn cfg() -> Config {
        Config::default()
    }

    fn state() -> State {
        State {
            harnesses: Vec::new(),
            supervision: Supervision::NotRunning,
            repo_root: None,
            has_zavet_dir: false,
            claude_present: false,
            zavet_installed: false,
            device_linked: false,
            knowledge: KnowledgeSyncMode::Off,
            cloud: None,
        }
    }

    fn present(probe: HarnessProbe) -> Harness {
        Harness {
            probe,
            on_path: true,
            has_config_dir: false,
            wired: false,
        }
    }

    /// A `set_quiet` stand-in that records the tier it was asked to write
    /// instead of touching disk.
    ///
    /// Every `knowledge()` test uses this, never `config_cmd::set_quiet`
    /// directly: that function resolves `project_dirs()` regardless of the
    /// `Config` passed to it, so calling it in-process (as `cargo test --bin
    /// dira` does, unlike the e2e suite's isolated subprocess) wrote the
    /// developer's real `config.toml`. See DIRASH-0030's B1 fix.
    struct RecordingWriter(std::cell::RefCell<Vec<String>>);

    impl RecordingWriter {
        fn new() -> Self {
            Self(std::cell::RefCell::new(Vec::new()))
        }

        /// Every tier this was asked to write, in call order.
        fn calls(&self) -> Vec<String> {
            self.0.borrow().clone()
        }

        /// Borrows `self`, so the returned closure — and the `&dyn Fn` made
        /// from it — cannot outlive this recorder.
        fn as_fn(&self) -> impl Fn(&str) -> anyhow::Result<PathBuf> + '_ {
            move |raw: &str| {
                self.0.borrow_mut().push(raw.to_string());
                Ok(PathBuf::from("/dev/null/recording-writer-stub"))
            }
        }
    }

    /// The disclosure has to name the content, not just the tier. This is the
    /// only place the user is told what `full` sends.
    #[test]
    fn the_knowledge_prompt_names_what_it_sends() {
        let mut ui = ScriptedUi::new();
        let opts = Options::default();
        let writer = RecordingWriter::new();
        let _ = knowledge(&state(), &opts, &mut ui, &writer.as_fn());
        let t = ui.transcript();
        for phrase in ["record bodies", "trailer values", "check commands"] {
            assert!(
                t.contains(phrase),
                "consent text must mention {phrase:?}; got:\n{t}"
            );
        }
    }

    /// `--knowledge <tier>` is an answer, so the prompt must not appear.
    #[test]
    fn an_explicit_tier_skips_the_prompt() {
        let mut ui = ScriptedUi::new();
        let opts = Options {
            knowledge: Some(KnowledgeSyncMode::Metadata),
            ..Options::default()
        };
        let writer = RecordingWriter::new();
        let _ = knowledge(&state(), &opts, &mut ui, &writer.as_fn());
        assert!(
            !ui.transcript().contains("Send full knowledge content"),
            "an explicit --knowledge must not re-ask"
        );
    }

    /// `--yes` resolves to `opts.knowledge = Some(Full)` before this step
    /// ever runs (`Options::resolve_defaults`), so it takes the same
    /// no-prompt path as an explicit `--knowledge full`. Per DIRASH-0030 that
    /// must not mean silent: the disclosure has to name what `full` sends on
    /// this path too, not only the interactive one.
    #[test]
    fn a_yes_shaped_run_still_shows_the_disclosure() {
        let mut ui = ScriptedUi::new();
        let opts = Options {
            knowledge: Some(KnowledgeSyncMode::Full),
            ..Options::default()
        };
        let writer = RecordingWriter::new();
        let _ = knowledge(&state(), &opts, &mut ui, &writer.as_fn());
        let t = ui.transcript();
        for phrase in ["record bodies", "trailer values", "check commands"] {
            assert!(
                t.contains(phrase),
                "a --yes-shaped run must still disclose {phrase:?}; got:\n{t}"
            );
        }
        assert!(
            !t.contains("Send full knowledge content"),
            "an explicit tier must still not re-ask"
        );
    }

    /// Declining the prompt lands on `metadata`, not `off`: the user said no
    /// to *content*, not to the channel.
    #[test]
    fn declining_content_falls_back_to_metadata_not_off() {
        let mut ui = ScriptedUi::new().with_confirms(&[false]);
        let st = State {
            knowledge: KnowledgeSyncMode::Metadata,
            ..state()
        };
        let writer = RecordingWriter::new();
        let outcome = knowledge(&st, &Options::default(), &mut ui, &writer.as_fn());
        assert!(
            matches!(&outcome, StepOutcome::AlreadyDone(m) if m.contains("metadata")),
            "got {outcome:?}"
        );
        assert!(
            writer.calls().is_empty(),
            "already at the target tier — must not write"
        );
    }

    /// Setting a tier without a linked device is recorded but inert — the
    /// daemon's flush is gated on the link. Saying "done" without that caveat
    /// would be a lie.
    ///
    /// Uses an explicit tier + stub writer so the write actually happens
    /// (`state.knowledge` starts `Off`, distinct from the requested `Full`):
    /// the previous version of this test fixed `state.knowledge` to the
    /// requested tier, which forced the `AlreadyDone` early return and so
    /// never reached the "pending" wording at all.
    #[test]
    fn an_unlinked_device_reports_the_tier_as_pending() {
        let mut ui = ScriptedUi::new();
        let st = State {
            knowledge: KnowledgeSyncMode::Off,
            device_linked: false,
            ..state()
        };
        let writer = RecordingWriter::new();
        let outcome = knowledge(
            &st,
            &Options {
                knowledge: Some(KnowledgeSyncMode::Full),
                ..Options::default()
            },
            &mut ui,
            &writer.as_fn(),
        );
        assert_eq!(writer.calls(), vec!["full".to_string()]);
        match &outcome {
            StepOutcome::Done(m) => assert!(
                m.contains("pending"),
                "an unlinked device must caveat the tier as pending: {m}"
            ),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// The linked twin of the test above: once the device is linked, the
    /// caveat shifts from "pending" (the daemon can't flush at all) to the
    /// workspace side of the double-ended gate (the daemon can flush, but
    /// bodies still need the workspace to also say `full`).
    #[test]
    fn a_linked_device_reports_the_workspace_caveat_instead() {
        let mut ui = ScriptedUi::new();
        let st = State {
            knowledge: KnowledgeSyncMode::Off,
            device_linked: true,
            ..state()
        };
        let writer = RecordingWriter::new();
        let outcome = knowledge(
            &st,
            &Options {
                knowledge: Some(KnowledgeSyncMode::Full),
                ..Options::default()
            },
            &mut ui,
            &writer.as_fn(),
        );
        assert_eq!(writer.calls(), vec!["full".to_string()]);
        match &outcome {
            StepOutcome::Done(m) => {
                assert!(
                    !m.contains("pending"),
                    "a linked device must not say pending: {m}"
                );
                assert!(
                    m.contains("workspace must also be set to `full`"),
                    "a linked device must state the workspace caveat: {m}"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// Empty input means skip, and the skip must be first-class: the run
    /// continues and the reason names the command to run later. This is the
    /// only step that needs something from outside the terminal, so it is the
    /// one most likely to be deferred.
    #[tokio::test]
    async fn a_blank_link_code_skips_without_failing() {
        let mut ui = ScriptedUi::new().with_lines(&[""]);
        let outcome = device(&cfg(), &mut state(), &mut ui).await;
        match outcome {
            StepOutcome::Skipped(m) => assert!(m.contains("dira device link"), "got {m}"),
            other => panic!("a blank code must skip, not {other:?}"),
        }
        // And the user must have been told where to get one.
        assert!(
            ui.transcript().contains("/connections"),
            "the prompt must point at the dashboard: {}",
            ui.transcript()
        );
    }

    /// An already-linked device is never re-prompted — the idempotency
    /// property, on the step where re-running would be most annoying.
    #[tokio::test]
    async fn an_already_linked_device_is_not_prompted() {
        let mut st = State {
            device_linked: true,
            ..state()
        };
        let mut ui = ScriptedUi::new();
        assert!(matches!(
            device(&cfg(), &mut st, &mut ui).await,
            StepOutcome::AlreadyDone(_)
        ));
        assert!(ui.transcript().is_empty(), "must ask nothing");
    }

    #[test]
    fn zavet_repo_outside_a_git_repo_does_nothing() {
        struct Boom;
        impl crate::zavet_install::Runner for Boom {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<std::process::Output> {
                panic!("no command may run outside a repo (DIRASH-0024)")
            }
        }
        let mut ui = ScriptedUi::new();
        let outcome = zavet_repo(&Boom, &state(), &Options::default(), &mut ui);
        assert!(
            matches!(&outcome, StepOutcome::Skipped(m) if m.contains("not inside a git repository")),
            "got {outcome:?}"
        );
    }

    #[test]
    fn zavet_repo_is_a_noop_when_the_dir_already_exists() {
        struct Boom;
        impl crate::zavet_install::Runner for Boom {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<std::process::Output> {
                panic!("must not re-scaffold an existing .zavet/")
            }
        }
        let st = State {
            repo_root: Some(std::path::PathBuf::from("/tmp/repo")),
            has_zavet_dir: true,
            ..state()
        };
        let mut ui = ScriptedUi::new();
        assert!(matches!(
            zavet_repo(&Boom, &st, &Options::default(), &mut ui),
            StepOutcome::AlreadyDone(_)
        ));
    }

    #[test]
    fn no_zavet_skips_both_zavet_steps() {
        let opts = Options {
            no_zavet: true,
            ..Options::default()
        };
        let mut ui = ScriptedUi::new();
        assert!(matches!(
            zavet_plugin(&state(), &opts, &mut ui),
            StepOutcome::Skipped(_)
        ));
        struct Boom;
        impl crate::zavet_install::Runner for Boom {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<std::process::Output> {
                panic!("--no-zavet must not spawn anything")
            }
        }
        assert!(matches!(
            zavet_repo(&Boom, &state(), &opts, &mut ui),
            StepOutcome::Skipped(_)
        ));
    }

    /// Without `claude` there is nothing to shell out to, and the step has to
    /// hand back the manual recipe rather than fail the run.
    #[test]
    fn zavet_plugin_without_claude_hands_back_the_manual_recipe() {
        let mut ui = ScriptedUi::new();
        let outcome = zavet_plugin(&state(), &Options::default(), &mut ui);
        match outcome {
            StepOutcome::Skipped(m) => {
                assert!(m.contains("dodi-smart/dirahq-zavet"), "got {m}");
            }
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    /// The `--harness` list is an override: detection is not consulted and no
    /// confirmation is asked — even when the detected candidates disagree
    /// with what was named explicitly.
    ///
    /// Drives `wiring_targets` (the pure selection logic `harnesses()`
    /// dispatches to), not `harnesses()` itself: the latter ends by calling
    /// `init::wire`, which writes real per-harness config files, and no unit
    /// test may pay that cost or fake it away in-process (the same hazard
    /// class B1 closed for `config_cmd::set_quiet`).
    #[test]
    fn an_explicit_harness_list_bypasses_detection() {
        let st = State {
            harnesses: vec![present(HARNESSES[0])],
            ..state()
        };
        assert_eq!(st.wirable().len(), 1, "claude is present and unwired");

        let opts = Options {
            harness: vec!["gemini".into()],
            ..Options::default()
        };
        let mut ui = ScriptedUi::new();
        let targets = wiring_targets(&st, &opts, &mut ui);
        assert_eq!(
            targets,
            vec!["gemini".to_string()],
            "the explicit list wins even though claude, not gemini, is what was detected"
        );
        assert!(
            ui.transcript().is_empty(),
            "an explicit list must not prompt"
        );
    }

    /// The complement: with no explicit `--harness`, detection drives
    /// selection and each wirable harness gets its own confirmation.
    #[test]
    fn detection_prompts_once_per_wirable_harness() {
        let st = State {
            harnesses: vec![present(HARNESSES[0]), present(HARNESSES[2])],
            ..state()
        };
        assert_eq!(st.wirable().len(), 2, "both claude and gemini are present");

        // Accept the first, decline the second.
        let mut ui = ScriptedUi::new().with_confirms(&[true, false]);
        let targets = wiring_targets(&st, &Options::default(), &mut ui);
        assert_eq!(targets, vec![HARNESSES[0].id.to_string()]);
        assert_eq!(
            ui.asked.len(),
            2,
            "must ask once per wirable harness, got: {:?}",
            ui.asked
        );
    }

    // -----------------------------------------------------------------
    // cloud:repo
    // -----------------------------------------------------------------

    use crate::cloud_init::{
        Applied, ArtifactState, BootstrapState, HarnessCloudWiring, RepoCloudStatus,
    };
    use crate::init::{Kind, Wired};
    use crate::onboard::prompt::Auto;
    use std::cell::RefCell;

    fn running() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn repo_root() -> PathBuf {
        PathBuf::from("/tmp/dira-onboard-cloud-repo-test")
    }

    fn harness_wiring(id: &'static str, current: bool) -> HarnessCloudWiring {
        HarnessCloudWiring {
            id,
            path: PathBuf::from(cloud_config_path(id)),
            config_present: current,
            parseable: true,
            events_missing: if current { 0 } else { 8 },
        }
    }

    /// Everything already matches what this binary would write.
    fn current_status() -> RepoCloudStatus {
        RepoCloudStatus {
            dir_present: true,
            hook_sh: ArtifactState::Current,
            bootstrap: BootstrapState::Pinned(running().to_string()),
            gitattributes: ArtifactState::Current,
            harnesses: crate::cloud_init::CLOUD_WIRABLE
                .iter()
                .map(|id| harness_wiring(id, true))
                .collect(),
        }
    }

    /// Pinned to a version newer than this binary — never ours to lower.
    fn newer_status() -> RepoCloudStatus {
        RepoCloudStatus {
            bootstrap: BootstrapState::Pinned("99.0.0".to_string()),
            ..current_status()
        }
    }

    /// Nothing has ever been written.
    fn fresh_status() -> RepoCloudStatus {
        RepoCloudStatus {
            dir_present: false,
            hook_sh: ArtifactState::Missing,
            bootstrap: BootstrapState::Missing,
            gitattributes: ArtifactState::Missing,
            harnesses: crate::cloud_init::CLOUD_WIRABLE
                .iter()
                .map(|id| harness_wiring(id, false))
                .collect(),
        }
    }

    /// Written before, but the hook script and the pin have both drifted.
    fn stale_status() -> RepoCloudStatus {
        RepoCloudStatus {
            dir_present: true,
            hook_sh: ArtifactState::Stale,
            bootstrap: BootstrapState::Pinned("0.1.0".to_string()),
            gitattributes: ArtifactState::Current,
            harnesses: crate::cloud_init::CLOUD_WIRABLE
                .iter()
                .map(|id| harness_wiring(id, true))
                .collect(),
        }
    }

    /// An applier that panics if called — proves a skip/already-done path
    /// never reaches the writer.
    struct Boom;
    impl CloudApplier for Boom {
        fn apply<'a>(
            &'a self,
            _root: &'a Path,
            _req: &'a crate::cloud_init::ApplyRequest<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<crate::cloud_init::Applied>> + 'a>,
        > {
            panic!("must not apply")
        }
    }

    /// Records the harness set it was asked to apply and returns a canned
    /// `Applied`, so a test can assert both what was requested and what the
    /// step reports without touching disk.
    struct RecordingApplier {
        applied: Applied,
        requested: RefCell<Vec<String>>,
    }

    impl RecordingApplier {
        fn new(applied: Applied) -> Self {
            Self {
                applied,
                requested: RefCell::new(Vec::new()),
            }
        }
    }

    impl CloudApplier for RecordingApplier {
        fn apply<'a>(
            &'a self,
            _root: &'a Path,
            req: &'a crate::cloud_init::ApplyRequest<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<crate::cloud_init::Applied>> + 'a>,
        > {
            *self.requested.borrow_mut() = req.harnesses.iter().map(|s| s.to_string()).collect();
            let applied = self.applied.clone();
            Box::pin(async move { Ok(applied) })
        }
    }

    fn fresh_applied() -> Applied {
        Applied {
            wrote_hook_sh: true,
            wrote_bootstrap: true,
            wrote_gitattributes: true,
            pinned_version: running().to_string(),
            digests_pinned: true,
            wired: vec![
                Wired {
                    label: "Claude Code",
                    kind: Kind::Hooks,
                    path: Some(PathBuf::from(".claude/settings.json")),
                    command: "dira hook claude".into(),
                    events_added: 8,
                    note: None,
                },
                Wired {
                    label: "Cursor",
                    kind: Kind::Hooks,
                    path: Some(PathBuf::from(".cursor/hooks.json")),
                    command: "dira hook cursor".into(),
                    events_added: 7,
                    note: None,
                },
            ],
            warnings: Vec::new(),
        }
    }

    #[tokio::test]
    async fn cloud_repo_outside_a_git_repository_is_skipped() {
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Boom, &state(), &Options::default(), &mut ui).await;
        assert!(
            matches!(&outcome, StepOutcome::Skipped(m) if m.contains("not inside a git repository")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn no_cloud_skips_the_step() {
        let opts = Options {
            no_cloud: true,
            ..Options::default()
        };
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(fresh_status()),
            ..state()
        };
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Boom, &st, &opts, &mut ui).await;
        assert_eq!(outcome, StepOutcome::Skipped("--no-cloud".into()));
    }

    #[tokio::test]
    async fn already_current_is_already_done_and_never_applies() {
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(current_status()),
            ..state()
        };
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Boom, &st, &Options::default(), &mut ui).await;
        assert!(
            matches!(&outcome, StepOutcome::AlreadyDone(m) if m.contains("already wired for cloud agents")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_newer_pin_is_already_done_and_says_so() {
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(newer_status()),
            ..state()
        };
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Boom, &st, &Options::default(), &mut ui).await;
        assert!(
            matches!(&outcome, StepOutcome::AlreadyDone(m) if m.contains("newer")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn declining_the_prompt_skips_without_applying() {
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(fresh_status()),
            ..state()
        };
        let mut ui = ScriptedUi::new().with_confirms(&[false]);
        let outcome = cloud_repo(&Boom, &st, &Options::default(), &mut ui).await;
        assert_eq!(outcome, StepOutcome::Skipped("declined".into()));
    }

    #[tokio::test]
    async fn a_fresh_wire_names_dira_and_the_per_harness_event_counts() {
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(fresh_status()),
            ..state()
        };
        let applier = RecordingApplier::new(fresh_applied());
        let mut ui = ScriptedUi::new().with_confirms(&[true]);
        let outcome = cloud_repo(&applier, &st, &Options::default(), &mut ui).await;
        match &outcome {
            StepOutcome::Done(m) => {
                assert!(m.contains(".dira/"), "must name .dira/: {m}");
                assert!(m.contains("claude 8 event(s)"), "got {m}");
                assert!(m.contains("cursor 7 event(s)"), "got {m}");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let expected: Vec<String> = crate::cloud_init::CLOUD_WIRABLE
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(*applier.requested.borrow(), expected);
    }

    #[tokio::test]
    async fn a_refresh_names_the_pre_apply_delta() {
        let status = stale_status();
        let expected_delta = status.describe_delta(running()).join(", ");
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(status),
            ..state()
        };
        let applied = Applied {
            wrote_hook_sh: true,
            wrote_bootstrap: true,
            wrote_gitattributes: false,
            pinned_version: running().to_string(),
            digests_pinned: true,
            wired: vec![
                Wired {
                    label: "Claude Code",
                    kind: Kind::Hooks,
                    path: Some(PathBuf::from(".claude/settings.json")),
                    command: "dira hook claude".into(),
                    events_added: 0,
                    note: None,
                },
                Wired {
                    label: "Cursor",
                    kind: Kind::Hooks,
                    path: Some(PathBuf::from(".cursor/hooks.json")),
                    command: "dira hook cursor".into(),
                    events_added: 0,
                    note: None,
                },
            ],
            warnings: Vec::new(),
        };
        let applier = RecordingApplier::new(applied);
        let mut ui = ScriptedUi::new().with_confirms(&[true]);
        let outcome = cloud_repo(&applier, &st, &Options::default(), &mut ui).await;
        match &outcome {
            StepOutcome::Done(m) => {
                assert!(m.contains("refreshed cloud wiring"), "got {m}");
                assert!(
                    m.contains(&expected_delta),
                    "got {m}, wanted {expected_delta}"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_harness_with_no_cloud_capable_id_is_skipped() {
        let st = State {
            repo_root: Some(repo_root()),
            ..state()
        };
        let opts = Options {
            harness: vec!["codex".into()],
            ..Options::default()
        };
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Boom, &st, &opts, &mut ui).await;
        assert!(
            matches!(&outcome, StepOutcome::Skipped(m) if m.contains("no cloud-capable harness")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_cursor_only_harness_list_narrows_the_apply_request() {
        // No `state.cloud` fixture: the selected set differs from
        // `CLOUD_WIRABLE`, so the step re-reads via `cloud_init::status`
        // against a repo root that does not exist on disk — a read-only,
        // deterministic "nothing here yet" for a path that never resolves.
        let st = State {
            repo_root: Some(repo_root()),
            ..state()
        };
        let opts = Options {
            harness: vec!["cursor".into()],
            ..Options::default()
        };
        let applied = Applied {
            wrote_hook_sh: true,
            wrote_bootstrap: true,
            wrote_gitattributes: true,
            pinned_version: running().to_string(),
            digests_pinned: true,
            wired: vec![Wired {
                label: "Cursor",
                kind: Kind::Hooks,
                path: Some(PathBuf::from(".cursor/hooks.json")),
                command: "dira hook cursor".into(),
                events_added: 7,
                note: None,
            }],
            warnings: Vec::new(),
        };
        let applier = RecordingApplier::new(applied);
        let mut ui = ScriptedUi::new().with_confirms(&[true]);
        let outcome = cloud_repo(&applier, &st, &opts, &mut ui).await;
        assert!(matches!(outcome, StepOutcome::Done(_)), "got {outcome:?}");
        assert_eq!(*applier.requested.borrow(), vec!["cursor".to_string()]);
    }

    #[tokio::test]
    async fn an_auto_ui_accepts_the_fresh_wire_prompt() {
        let st = State {
            repo_root: Some(repo_root()),
            cloud: Some(fresh_status()),
            ..state()
        };
        let applier = RecordingApplier::new(fresh_applied());
        let mut ui = Auto;
        let outcome = cloud_repo(&applier, &st, &Options::default(), &mut ui).await;
        assert!(matches!(outcome, StepOutcome::Done(_)), "got {outcome:?}");
    }

    /// An applier error (a corrupt config under `Refuse`, say) is recorded
    /// as `Failed` and never propagates: the run must go on to the knowledge
    /// step, and the summary names what went wrong.
    #[tokio::test]
    async fn an_apply_error_is_a_failed_outcome_never_a_panic_or_abort() {
        struct Broken;
        impl CloudApplier for Broken {
            fn apply<'a>(
                &'a self,
                _root: &'a Path,
                _req: &'a crate::cloud_init::ApplyRequest<'a>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = anyhow::Result<crate::cloud_init::Applied>>
                        + 'a,
                >,
            > {
                Box::pin(async {
                    Err(anyhow::anyhow!(
                        ".cursor/hooks.json is not valid JSON — refusing to overwrite it"
                    ))
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let st = State {
            repo_root: Some(dir.path().to_path_buf()),
            cloud: Some(crate::cloud_init::status(
                dir.path(),
                crate::cloud_init::CLOUD_WIRABLE,
            )),
            ..state()
        };
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Broken, &st, &Options::default(), &mut ui).await;
        match outcome {
            StepOutcome::Failed(m) => {
                assert!(m.starts_with("cloud wiring:"), "{m}");
                assert!(m.contains("hooks.json"), "{m}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The applier's advisories reach the user through the wizard's own
    /// channel, not a stderr line lost between prompts.
    #[tokio::test]
    async fn applier_warnings_are_said_through_the_ui() {
        struct Warns;
        impl CloudApplier for Warns {
            fn apply<'a>(
                &'a self,
                _root: &'a Path,
                _req: &'a crate::cloud_init::ApplyRequest<'a>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = anyhow::Result<crate::cloud_init::Applied>>
                        + 'a,
                >,
            > {
                Box::pin(async {
                    let mut a = fresh_applied();
                    a.warnings
                        .push(".dira/hook.sh is excluded by this repo's .gitignore".into());
                    Ok(a)
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let st = State {
            repo_root: Some(dir.path().to_path_buf()),
            cloud: Some(crate::cloud_init::status(
                dir.path(),
                crate::cloud_init::CLOUD_WIRABLE,
            )),
            ..state()
        };
        let mut ui = ScriptedUi::new();
        let outcome = cloud_repo(&Warns, &st, &Options::default(), &mut ui).await;
        assert!(matches!(outcome, StepOutcome::Done(_)), "{outcome:?}");
        // `ScriptedUi` records `say` lines alongside the questions it was asked.
        assert!(
            ui.asked
                .iter()
                .any(|l| l.contains("warning:") && l.contains(".gitignore")),
            "said: {:?}",
            ui.asked
        );
    }
}
