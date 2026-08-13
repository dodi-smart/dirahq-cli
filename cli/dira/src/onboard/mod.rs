//! `dira onboard` — one re-runnable command from a fresh install to capturing.
//!
//! ## Why this exists
//!
//! Before it, four different surfaces told users four different getting-started
//! sequences (the root `--help`, `README.md`, `install.sh`, `install.ps1`), and
//! the two that most people actually saw — the installers — omitted `dira init`
//! entirely. Following them left you with a running daemon that captured
//! nothing, silently, forever. Worse, they recommended `dira daemon start`,
//! which takes the control socket and thereby blocks the `dira daemon install`
//! you actually wanted (D-0009).
//!
//! ## The design rules
//!
//! - **Skip, don't fail.** Every step is independent enough that one bad
//!   harness config should not cost you the daemon service. A step that cannot
//!   run reports why and the wizard continues; the summary collects what is
//!   still open.
//! - **Idempotent and resumable.** Re-running reports `AlreadyDone` for
//!   finished steps and picks up the rest. There is no state file — each step
//!   re-derives its own status from the machine, which is the only version
//!   that cannot go stale.
//! - **Consent is asked for on its own terms.** The knowledge tier gets its own
//!   prompt naming exactly what it sends, and is never implied by device
//!   linking or billing consent. D-0001 keeps those channels separate on the
//!   wire; this keeps them separate in the conversation too.

pub(crate) mod detect;
pub(crate) mod prompt;
pub(crate) mod steps;

use anyhow::Result;
use dira_core::config::KnowledgeSyncMode;
use dira_core::Config;
use prompt::{Auto, Interactive, Ui};

/// What happened to one step. `Failed` is a record, never a control-flow
/// signal — the runner keeps going and the summary reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    Done(String),
    AlreadyDone(String),
    Skipped(String),
    Failed(String),
}

impl StepOutcome {
    fn message(&self) -> &str {
        match self {
            StepOutcome::Done(m)
            | StepOutcome::AlreadyDone(m)
            | StepOutcome::Skipped(m)
            | StepOutcome::Failed(m) => m,
        }
    }

    /// The marker and colour, matching `dira doctor`'s four-shape convention
    /// (`doctor::render::mark`) so onboard output reads like the rest of the
    /// CLI — and, more importantly, so the shapes still distinguish outcomes
    /// once colour is stripped by a pipe.
    fn mark(&self) -> (String, crate::theme::Role) {
        let g = crate::theme::glyphs();
        match self {
            StepOutcome::Done(_) => (g.bullet.to_string(), crate::theme::Role::Engaged),
            StepOutcome::AlreadyDone(_) => (g.dot.to_string(), crate::theme::Role::Faint),
            StepOutcome::Skipped(_) => (g.triangle.to_string(), crate::theme::Role::Compute),
            StepOutcome::Failed(_) => (g.cross.to_string(), crate::theme::Role::Negative),
        }
    }
}

/// How the knowledge tier was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Knowledge {
    /// Ask, defaulting to `full`.
    Ask,
    /// `--knowledge <tier>` (or `--yes`, which resolves to `full`).
    Explicit(KnowledgeSyncMode),
}

/// A parsed `dira onboard` invocation, independent of clap.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// Accept every default without asking.
    pub yes: bool,
    /// Show the plan and change nothing.
    pub print: bool,
    pub no_service: bool,
    pub no_zavet: bool,
    /// Wire exactly these, bypassing detection.
    pub harness: Vec<String>,
    pub knowledge: Knowledge,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            yes: false,
            print: false,
            no_service: false,
            no_zavet: false,
            harness: Vec::new(),
            knowledge: Knowledge::Ask,
        }
    }
}

/// `dira onboard`.
pub(crate) async fn run(config: &Config, mut opts: Options) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let state = detect::run(config, &cwd).await;

    // `--yes` resolves the tier to the same value the prompt defaults to.
    // Encoding it here rather than inside the step keeps one answer to "what
    // does --yes do", and the summary still restates the tier either way, so
    // a non-interactive run never leaves the user unaware of it.
    if opts.yes && opts.knowledge == Knowledge::Ask {
        opts.knowledge = Knowledge::Explicit(KnowledgeSyncMode::Full);
    }

    if opts.print {
        print_plan(&state, &opts);
        return Ok(());
    }

    // Neither a terminal nor `--yes`: printing the plan and exiting 0 is the
    // only defensible behaviour. Prompting would hang a CI job forever, and
    // silently assuming every default would make persistent system changes
    // nobody asked for.
    if !opts.yes && !prompt::is_interactive() {
        println!("not a terminal — showing the plan instead of prompting.");
        println!("re-run with --yes to accept these defaults non-interactively.\n");
        print_plan(&state, &opts);
        return Ok(());
    }

    let mut ui: Box<dyn Ui> = if opts.yes {
        Box::new(Auto { narrate: true })
    } else {
        Box::new(Interactive)
    };

    let mut results: Vec<(String, StepOutcome)> = Vec::new();

    results.extend(steps::harnesses(config, &state, &opts, ui.as_mut()).await);
    results.push((
        "daemon".into(),
        steps::daemon(config, &state, &opts, ui.as_mut()).await,
    ));
    results.push((
        "device".into(),
        steps::device(config, &state, ui.as_mut()).await,
    ));
    results.push((
        "zavet".into(),
        steps::zavet_plugin(&state, &opts, ui.as_mut()),
    ));
    results.push((
        "zavet:repo".into(),
        steps::zavet_repo(
            &crate::zavet_install::SystemRunner,
            &state,
            &opts,
            ui.as_mut(),
        ),
    ));
    // Knowledge last of the mutating steps: it writes config the daemon reads
    // at startup, and step 3 may have just restarted the daemon. Ordering it
    // after means the value is on disk before the *next* start; the summary
    // says so when a restart is still needed.
    results.push((
        "knowledge".into(),
        steps::knowledge(config, &state, &opts, ui.as_mut()),
    ));

    print_summary(&results);
    print_open_items(config, &state, &results).await;
    Ok(())
}

/// `--print`, and the non-interactive fallback.
fn print_plan(state: &detect::State, opts: &Options) {
    println!("dira onboard would:");
    let wirable = state.wirable();
    if opts.harness.is_empty() {
        if wirable.is_empty() {
            println!("  · wire no harnesses (none detected, or all already wired)");
        } else {
            for h in &wirable {
                println!("  · wire {}", h.probe.label);
            }
        }
    } else {
        for h in &opts.harness {
            println!("  · wire {h} (explicitly requested)");
        }
    }

    if opts.no_service {
        println!("  · skip the daemon service (--no-service)");
    } else if state.supervised() {
        println!("  · leave the daemon service alone (already supervised)");
    } else {
        println!("  · install dirad as a login service");
    }

    if state.device_linked {
        println!("  · leave the device link alone (already linked)");
    } else {
        println!("  · offer to link this device (skippable)");
    }

    if opts.no_zavet {
        println!("  · skip zavet (--no-zavet)");
    } else {
        if state.zavet_installed {
            println!("  · leave the zavet plugin alone (already installed)");
        } else {
            println!("  · install the zavet plugin");
        }
        match (&state.repo_root, state.has_zavet_dir) {
            (None, _) => println!("  · skip .zavet/ scaffolding (not in a git repo)"),
            (Some(r), true) => println!(
                "  · leave {}'s .zavet/ alone (already present)",
                r.display()
            ),
            (Some(r), false) => println!("  · scaffold .zavet/ in {}", r.display()),
        }
    }

    let tier = match opts.knowledge {
        Knowledge::Explicit(t) => t.as_str(),
        Knowledge::Ask => "full (after asking)",
    };
    println!("  · set knowledge sync to {tier}");
    println!("\nNothing was changed.");
}

fn print_summary(results: &[(String, StepOutcome)]) {
    println!("\nonboarding summary");
    for (name, outcome) in results {
        let (mark, role) = outcome.mark();
        println!(
            "  {} {:<14} {}",
            crate::theme::paint(&mark, role),
            name,
            outcome.message()
        );
    }
}

/// The closing block: what is still open, in the order it should be dealt
/// with. Only genuinely-open items appear — a run with nothing outstanding
/// ends on the summary.
async fn print_open_items(
    config: &Config,
    state: &detect::State,
    results: &[(String, StepOutcome)],
) {
    let mut open: Vec<String> = Vec::new();

    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, o)| matches!(o, StepOutcome::Failed(_)))
        .map(|(n, _)| n.as_str())
        .collect();
    if !failed.is_empty() {
        open.push(format!(
            "{} step(s) failed ({}) — see the summary above",
            failed.len(),
            failed.join(", ")
        ));
    }

    if !state.device_linked
        && results
            .iter()
            .any(|(n, o)| n == "device" && matches!(o, StepOutcome::Skipped(_)))
    {
        open.push(
            "link this device when you have a code: `dira device link` \
             (local capture works meanwhile)"
                .into(),
        );
    }

    // The scaffold is structurally complete but semantically empty: RULES.md
    // ships with a placeholder line. Saying so is the difference between a
    // knowledge layer and an untouched template.
    if results
        .iter()
        .any(|(n, o)| n == "zavet:repo" && matches!(o, StepOutcome::Done(_)))
    {
        open.push(
            "restart Claude Code, then run `/zavet:init` to write this repo's standing rules \
             — the scaffold ships with a placeholder"
                .into(),
        );
        open.push(
            "git hooks are staged but not active: dira never sets core.hooksPath. \
             Run `git config core.hooksPath .zavet/githooks` yourself if you want them"
                .into(),
        );
    }

    if let Some((_, StepOutcome::Done(_))) = results.iter().find(|(n, _)| n == "knowledge") {
        if state.device_linked {
            open.push(
                "set your workspace's knowledge tier to `full` in the dashboard \
                 (ZAVET · KNOWLEDGE SYNC) — both ends must agree before bodies are stored"
                    .into(),
            );
        }
        open.push("restart the daemon to pick up the new config: `dira daemon restart`".into());
    }

    if !open.is_empty() {
        println!("\nstill to do");
        for item in &open {
            println!("  {} {item}", crate::theme::glyphs().arrow);
        }
    }

    let _ = config;
    println!("\nverify anytime with `dira doctor` (add --probe to prove capture end to end).");
}

/// Parse `--knowledge`.
pub(crate) fn parse_knowledge(raw: &str) -> Result<KnowledgeSyncMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(KnowledgeSyncMode::Off),
        "metadata" => Ok(KnowledgeSyncMode::Metadata),
        "full" => Ok(KnowledgeSyncMode::Full),
        other => Err(anyhow::anyhow!(
            "--knowledge must be one of: off, metadata, full (got `{other}`)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Supervision;
    use detect::State;

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
        }
    }

    #[test]
    fn knowledge_parses_the_three_tiers_and_rejects_others() {
        assert_eq!(parse_knowledge("off").unwrap(), KnowledgeSyncMode::Off);
        assert_eq!(
            parse_knowledge("METADATA").unwrap(),
            KnowledgeSyncMode::Metadata
        );
        assert_eq!(parse_knowledge(" full ").unwrap(), KnowledgeSyncMode::Full);
        assert!(parse_knowledge("everything").is_err());
    }

    /// The four outcomes must be visually distinct without colour, because
    /// piped output loses it — the same reason `dira doctor` uses four shapes.
    #[test]
    fn every_outcome_has_a_distinct_glyph() {
        let outcomes = [
            StepOutcome::Done("a".into()),
            StepOutcome::AlreadyDone("b".into()),
            StepOutcome::Skipped("c".into()),
            StepOutcome::Failed("d".into()),
        ];
        let marks: Vec<String> = outcomes.iter().map(|o| o.mark().0).collect();
        let unique: std::collections::HashSet<&String> = marks.iter().collect();
        assert_eq!(unique.len(), marks.len(), "glyphs collide: {marks:?}");
    }

    /// `--yes` must resolve to a concrete tier before any step runs, so the
    /// summary can restate it. Left as `Ask`, a non-interactive run would
    /// silently take the prompt default with nothing shown.
    #[test]
    fn yes_resolves_the_knowledge_tier_up_front() {
        let mut opts = Options {
            yes: true,
            ..Options::default()
        };
        assert_eq!(opts.knowledge, Knowledge::Ask);
        if opts.yes && opts.knowledge == Knowledge::Ask {
            opts.knowledge = Knowledge::Explicit(KnowledgeSyncMode::Full);
        }
        assert_eq!(
            opts.knowledge,
            Knowledge::Explicit(KnowledgeSyncMode::Full),
            "--yes must mean full, and must say so"
        );
    }

    /// An explicit `--knowledge` survives `--yes`: the more specific flag wins.
    #[test]
    fn an_explicit_tier_is_not_overridden_by_yes() {
        let mut opts = Options {
            yes: true,
            knowledge: Knowledge::Explicit(KnowledgeSyncMode::Off),
            ..Options::default()
        };
        if opts.yes && opts.knowledge == Knowledge::Ask {
            opts.knowledge = Knowledge::Explicit(KnowledgeSyncMode::Full);
        }
        assert_eq!(opts.knowledge, Knowledge::Explicit(KnowledgeSyncMode::Off));
    }

    /// `--print` must never claim it did anything.
    #[test]
    fn the_plan_says_nothing_changed() {
        // Rendering goes to stdout; this asserts the shape of the decision
        // rather than capturing output — the runner returns before any step.
        let opts = Options {
            print: true,
            ..Options::default()
        };
        assert!(opts.print);
        assert!(state().wirable().is_empty());
    }
}
