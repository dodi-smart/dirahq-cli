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
    match crate::device::link(config, Some(code), None).await {
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

/// The consent text for the telemetry prompt.
///
/// Named, and asserted on by a test, for the same reason
/// [`KNOWLEDGE_DISCLOSURE`] is: this is the only consent UX for the channel,
/// so if this sentence is wrong or missing, nothing else catches it.
pub(crate) const TELEMETRY_DISCLOSURE: &str = "\
Anonymous product analytics, on by default — its own channel, separate from
knowledge sync and billing consent.
  sent   command name, duration, success/failure kind; inside a repo: host
         type (github/gitlab/bitbucket/self-hosted), public/private when
         determinable, and a one-way salted hash of the repo identity
  never  repo names, git identity or email, file paths, command arguments,
         error text
Tagged by a random install id, not your device key. Sent to Dira's EU
analytics (PostHog EU, via the Dira cloud); once this device is linked,
later usage may be associated with your workspace account. Silent in dev
builds and CI.
Turn off anytime: `dira config set telemetry.enabled false`,
DIRA_TELEMETRY_ENABLED=0, or DO_NOT_TRACK=1.";

/// Step 8 — the telemetry consent.
///
/// Last of the mutating steps: same reasoning as [`knowledge`] for running
/// after the daemon step (a restart it may have just done should not race
/// this write), and after knowledge itself so the two consent prompts read
/// as a pair in the transcript rather than being split by the zavet steps.
///
/// `write_enabled` is injected for the same reason [`knowledge`]'s
/// `write_tier` is: `config_cmd::set_quiet` resolves its target via
/// `project_dirs()` regardless of the `Config` it is handed, so an
/// in-process unit test calling it directly would write the developer's real
/// `config.toml`. `mod::run` passes a closure over the real `set_quiet`
/// (translating the bool to the `"on"`/`"off"` spelling `telemetry.enabled`
/// expects); tests pass a recording stub.
///
/// `record_consent` is injected for the same client-agnosticism reason: the
/// real one (bound by `mod::run`) is `telemetry::record_consent`, a fire-
/// and-forget send over the control socket, which no unit test in this
/// module may perform. It is only ever called after `write_enabled`
/// succeeds — a `cli_consent_recorded` event must report what was actually
/// persisted, never an attempted write that failed.
pub(crate) async fn telemetry<F, Fut>(
    state: &State,
    opts: &Options,
    ui: &mut dyn Ui,
    write_enabled: &dyn Fn(bool) -> anyhow::Result<PathBuf>,
    record_consent: F,
) -> StepOutcome
where
    F: FnOnce(bool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // Unconditional, and above the `opts.telemetry` match on purpose — the
    // same DIRASH-0030 rule `knowledge` follows: every consent path
    // (interactive, `--yes`, an explicit `--telemetry` flag) has to see
    // exactly what is sent before this step acts, not just the one that
    // stops to ask.
    ui.say(TELEMETRY_DISCLOSURE);

    let want = match opts.telemetry {
        Some(explicit) => explicit,
        None => ui.confirm("Keep anonymous telemetry on?", true),
    };

    if state.telemetry_enabled == want {
        return StepOutcome::AlreadyDone(format!(
            "telemetry already {}",
            if want { "on" } else { "off" }
        ));
    }

    match write_enabled(want) {
        Ok(_) => {
            record_consent(want).await;
            if want {
                StepOutcome::Done(
                    "telemetry stays on — anonymous usage analytics will be sent".into(),
                )
            } else {
                StepOutcome::Done("telemetry turned off — nothing will be sent".into())
            }
        }
        Err(e) => StepOutcome::Failed(format!("could not set telemetry.enabled: {e}")),
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
            telemetry_enabled: true,
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

    /// A `set_quiet`-style stand-in for `telemetry()`'s `write_enabled`
    /// closure — same `RecordingWriter` shape, `bool` in place of the raw
    /// TOML string, and the same DIRASH-0030 justification for why tests
    /// never call `config_cmd::set_quiet` directly.
    struct BoolRecorder(std::cell::RefCell<Vec<bool>>);

    impl BoolRecorder {
        fn new() -> Self {
            Self(std::cell::RefCell::new(Vec::new()))
        }

        fn calls(&self) -> Vec<bool> {
            self.0.borrow().clone()
        }

        fn as_fn(&self) -> impl Fn(bool) -> anyhow::Result<PathBuf> + '_ {
            move |enabled: bool| {
                self.0.borrow_mut().push(enabled);
                Ok(PathBuf::from("/dev/null/recording-writer-stub"))
            }
        }
    }

    /// The disclosure has to name the content, not just the toggle. Same
    /// justification as `the_knowledge_prompt_names_what_it_sends`: this is
    /// the only place the user is told what telemetry sends.
    #[tokio::test]
    async fn the_telemetry_prompt_names_what_it_sends() {
        let mut ui = ScriptedUi::new();
        let recorder = BoolRecorder::new();
        let _ = telemetry(
            &state(),
            &Options::default(),
            &mut ui,
            &recorder.as_fn(),
            |_| async {},
        )
        .await;
        let t = ui.transcript();
        for phrase in [
            "command",
            "public",
            "private",
            "hash",
            "DO_NOT_TRACK",
            "telemetry.enabled",
        ] {
            assert!(
                t.contains(phrase),
                "consent text must mention {phrase:?}; got:\n{t}"
            );
        }
    }

    /// An explicit `--telemetry` answer is not re-asked.
    #[tokio::test]
    async fn an_explicit_telemetry_flag_skips_the_prompt() {
        let mut ui = ScriptedUi::new();
        let opts = Options {
            telemetry: Some(false),
            ..Options::default()
        };
        let recorder = BoolRecorder::new();
        let _ = telemetry(&state(), &opts, &mut ui, &recorder.as_fn(), |_| async {}).await;
        assert!(
            !ui.transcript().contains("Keep anonymous telemetry on?"),
            "an explicit --telemetry must not re-ask"
        );
    }

    /// Telemetry is on by default (`state().telemetry_enabled == true`), so
    /// accepting the default answer must not write `config.toml` at all —
    /// only a *change* from the effective value is worth persisting.
    #[tokio::test]
    async fn keeping_the_default_on_does_not_write() {
        let mut ui = ScriptedUi::new();
        let recorder = BoolRecorder::new();
        let outcome = telemetry(
            &state(),
            &Options::default(),
            &mut ui,
            &recorder.as_fn(),
            |_| async {},
        )
        .await;
        assert!(
            recorder.calls().is_empty(),
            "must not write the default back"
        );
        assert!(
            matches!(&outcome, StepOutcome::AlreadyDone(m) if m.contains("already on")),
            "got {outcome:?}"
        );
    }

    /// Declining writes `false` and says so plainly — no partial "some data
    /// still leaves" hedging.
    #[tokio::test]
    async fn declining_writes_false_and_says_nothing_will_be_sent() {
        let mut ui = ScriptedUi::new().with_confirms(&[false]);
        let recorder = BoolRecorder::new();
        let outcome = telemetry(
            &state(),
            &Options::default(),
            &mut ui,
            &recorder.as_fn(),
            |_| async {},
        )
        .await;
        assert_eq!(recorder.calls(), vec![false]);
        match &outcome {
            StepOutcome::Done(m) => assert!(m.contains("nothing will be sent"), "got {m}"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// `record_consent` must fire exactly once, and only after a successful
    /// write — never on an `AlreadyDone` no-op, and never before the write is
    /// confirmed to have landed.
    #[tokio::test]
    async fn record_consent_fires_once_and_only_after_a_successful_write() {
        let recorded = std::cell::RefCell::new(Vec::new());
        let mut ui = ScriptedUi::new().with_confirms(&[false]);
        let recorder = BoolRecorder::new();
        let outcome = telemetry(
            &state(),
            &Options::default(),
            &mut ui,
            &recorder.as_fn(),
            |enabled: bool| {
                recorded.borrow_mut().push(enabled);
                async {}
            },
        )
        .await;
        assert!(matches!(outcome, StepOutcome::Done(_)));
        assert_eq!(recorded.into_inner(), vec![false]);

        // Unchanged (already on): neither the write nor the consent event
        // fires.
        let recorded_noop = std::cell::RefCell::new(Vec::new());
        let mut ui = ScriptedUi::new();
        let recorder = BoolRecorder::new();
        let outcome = telemetry(
            &state(),
            &Options::default(),
            &mut ui,
            &recorder.as_fn(),
            |enabled: bool| {
                recorded_noop.borrow_mut().push(enabled);
                async {}
            },
        )
        .await;
        assert!(matches!(outcome, StepOutcome::AlreadyDone(_)));
        assert!(recorded_noop.into_inner().is_empty());
    }

    /// A `--yes`-shaped run (after `Options::resolve_defaults` folds it to
    /// `Some(true)`) must still show the disclosure, per the same rule the
    /// knowledge step's `a_yes_shaped_run_still_shows_the_disclosure` pins.
    #[tokio::test]
    async fn a_yes_shaped_telemetry_run_still_shows_the_disclosure() {
        let mut ui = ScriptedUi::new();
        let opts = Options {
            telemetry: Some(true),
            ..Options::default()
        };
        let recorder = BoolRecorder::new();
        let _ = telemetry(&state(), &opts, &mut ui, &recorder.as_fn(), |_| async {}).await;
        let t = ui.transcript();
        assert!(t.contains("Anonymous product analytics"), "got:\n{t}");
        assert!(
            !t.contains("Keep anonymous telemetry on?"),
            "an already-resolved answer must still not re-ask"
        );
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
}
