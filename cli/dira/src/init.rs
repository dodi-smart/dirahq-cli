//! `dira init` — wire a harness's hooks so this machine reports to the daemon.
//!
//! - **Claude Code** (default): standard **command hooks**: each configured event
//!   runs `dira hook claude`, which reads the event JSON on stdin and forwards it
//!   to the daemon over the socket. Merged idempotently into `.claude/settings.json`
//!   (project) or `~/.claude/settings.json` (`--global`), preserving existing keys.
//! - **Gemini CLI** (`dira init gemini`): Gemini's hooks live under a `hooks` object
//!   in `~/.gemini/settings.json` (or `.gemini/settings.json`) with the same nested
//!   shape as Claude; each event runs `dira hook gemini` over the stdin→socket shim.
//! - **Cursor** (`dira init cursor`): Cursor reads `~/.cursor/hooks.json` (or
//!   `.cursor/hooks.json`); each event maps to an array of `{ command }` entries
//!   running `dira hook cursor`.
//! - **Codex** (`dira init codex`): Codex reads command hooks from
//!   `~/.codex/config.toml` `[[hooks.<Event>]]`; we print the snippet so events flow
//!   over the same stdin→socket shim.
//! - **OpenCode** (`dira init opencode`): OpenCode has no command hooks, so we
//!   write a tiny forwarder **plugin** to `~/.config/opencode/plugin/dira.js`
//!   that HTTP-POSTs to the daemon's `/hooks/opencode` route.
//! - **Grok Build** (`dira init grok`): grok-build merges every `*.json` in
//!   `~/.grok/hooks/`, so we write a dedicated hooks file
//!   `~/.grok/hooks/dira.json` (user scope is always trusted); each event runs
//!   `dira hook grok` over the stdin→socket shim.

use anyhow::{bail, Context, Result};
use dira_core::{Config, Store};
use serde_json::{json, Value};
use std::path::PathBuf;

/// What a single harness's wiring actually did.
///
/// The `run*` functions used to print their own three-line report and return
/// `Result<()>`, which made them unusable from anything that needs to
/// summarise several harnesses at once (`dira onboard` wires every detected
/// harness in one pass and prints one block at the end). Printing therefore
/// moved to the callers; `dira init`'s own output is unchanged, rendered by
/// [`Wired::print`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wired {
    /// Human label, as it appears in the report (`Claude Code`).
    pub label: &'static str,
    /// How this harness is wired, which is the whole reason the report has
    /// two wordings: five harnesses merge hook entries, OpenCode gets a
    /// forwarder plugin instead.
    pub kind: Kind,
    /// Where the config was written. `None` when nothing was written —
    /// `--print`, or codex, whose snippet is only ever printed.
    pub path: Option<PathBuf>,
    /// The hook command embedded in the config — or, for a [`Kind::Plugin`]
    /// harness, the URL its forwarder posts to.
    pub command: String,
    /// How many events this run newly added. Zero with a `Some(path)` means
    /// every event was already wired — the idempotent re-run case.
    pub events_added: usize,
    /// Harness-specific aside (grok's user-level-only note, codex's paste
    /// instruction).
    pub note: Option<String>,
}

/// How a harness receives its wiring.
///
/// The distinction is real, not cosmetic — it decides whether `events_added`
/// counts merged events or a single rewritten file — and it is what the two
/// report wordings are derived from, so neither wording has to be carried on
/// the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Hook entries merged into the harness's own JSON config.
    Hooks,
    /// A forwarder plugin file written whole (OpenCode).
    Plugin,
}

impl Wired {
    /// Nothing was added because everything was already there.
    pub fn already_wired(&self) -> bool {
        self.path.is_some() && self.events_added == 0
    }

    /// The report's first-line verb and second-line label, derived from
    /// [`Kind`] rather than carried per instance.
    ///
    /// Split out of [`print`] so it is assertable without capturing stdout —
    /// the wording is the part worth pinning, and it has no other guard.
    ///
    /// [`print`]: Wired::print
    fn headline(&self) -> (String, &'static str) {
        match self.kind {
            Kind::Hooks => (format!("wired {} hooks", self.label), "hook command"),
            Kind::Plugin => (format!("wrote {} forwarder plugin", self.label), "posts to"),
        }
    }

    /// The report `dira init` has always printed, byte for byte.
    pub fn print(&self) {
        let Some(path) = &self.path else {
            return; // --print / codex already emitted the payload itself
        };
        if let Some(note) = &self.note {
            println!("{note}");
        }
        let (written, command_label) = self.headline();
        println!(
            "{written} {} {}",
            crate::theme::glyphs().arrow,
            path.display()
        );
        println!("{command_label}: {}", self.command);
        println!("start the daemon with `dira daemon start`, then work as usual.");
    }
}

/// What to do when a harness's existing config file is not valid JSON.
///
/// The historical behaviour is [`OnUnparseable::Overwrite`]: parse failure
/// falls back to `{}`, so writing back silently discards whatever the file
/// held. That is defensible for `dira init`, where the user typed the
/// command at that exact file. It is not defensible for `dira onboard`,
/// which touches several files the user never named — hence
/// [`OnUnparseable::Refuse`], which reports the file and leaves it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUnparseable {
    /// Start from `{}` and overwrite (the `dira init` default).
    Overwrite,
    /// Error out, touching nothing.
    Refuse,
}

/// Claude Code events we hook and whether they need a tool matcher.
const CLAUDE_EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("SessionEnd", false),
    ("UserPromptSubmit", false),
    ("Stop", false),
    ("SubagentStop", false),
    ("Notification", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
];

/// Gemini CLI events we hook and whether they need a tool matcher. Gemini's
/// lifecycle names differ from Claude's (`BeforeAgent`/`AfterAgent`/`BeforeTool`/
/// `AfterTool`); see `dira_sources::gemini`.
const GEMINI_EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("SessionEnd", false),
    ("BeforeAgent", false),
    ("AfterAgent", false),
    ("BeforeTool", true),
    ("AfterTool", true),
    ("Notification", false),
];

/// Cursor hook event names (camelCase). Mapped in `dira_sources::cursor`.
const CURSOR_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "beforeSubmitPrompt",
    "beforeShellExecution",
    "afterShellExecution",
    "afterFileEdit",
    "stop",
];

/// Grok Build events we hook and whether they need a tool matcher. grok-build's
/// event vocabulary is a superset of Claude Code's (values are mapped in
/// `dira_sources::grok`); no matcher — grok hooks fire for all tools without one.
const GROK_EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("SessionEnd", false),
    ("UserPromptSubmit", false),
    ("Stop", false),
    ("StopFailure", false),
    ("SubagentStop", false),
    ("PreToolUse", false),
    ("PostToolUse", false),
    ("PostToolUseFailure", false),
    ("PermissionDenied", false),
    ("Notification", false),
];

/// Every harness `wire` can actually wire, in help/report order.
///
/// This is deliberately **not** `dira_sources::canonical_harness_id`'s domain:
/// that table also resolves `generic`, a wire id used by the hook ingest path
/// for payloads from an unrecognised harness. There is nothing to write for
/// it, so accepting it here would mean a caller validating against the alias
/// table and then failing at dispatch — which is exactly what `dira onboard
/// --harness generic` used to do, passing its up-front check and then failing
/// mid-run, five steps deep.
pub const WIRABLE: &[&str] = &["claude", "codex", "gemini", "cursor", "opencode", "grok"];

/// Whether [`wire`] accepts this canonical id.
pub fn is_wirable(id: &str) -> bool {
    WIRABLE.contains(&id)
}

/// Wire one harness by canonical id — the single dispatch point.
///
/// `dira init` and `dira onboard` differ only in the three policy arguments,
/// so they share this rather than each carrying a six-arm match that has to
/// be updated in lockstep when a harness is added.
pub async fn wire(
    id: &str,
    config: &Config,
    global: bool,
    print_only: bool,
    on_unparseable: OnUnparseable,
) -> Result<Wired> {
    match id {
        "claude" => run(global, print_only, on_unparseable),
        "codex" => run_codex(print_only),
        "gemini" => run_gemini(global, print_only, on_unparseable),
        "cursor" => run_cursor(global, print_only, on_unparseable),
        "opencode" => run_opencode(config, print_only).await,
        "grok" => run_grok(global, print_only, on_unparseable),
        other => bail!(
            "unknown harness '{other}' (expected: {})",
            WIRABLE.join(", ")
        ),
    }
}

/// `dira init` (default) — wire Claude Code command hooks.
pub fn run(global: bool, print_only: bool, on_unparseable: OnUnparseable) -> Result<Wired> {
    let (command, legacy_command) = hook_commands("claude");
    let path = if global {
        dira_core::config::home_dir()
            .context("resolve home directory")?
            .join(".claude/settings.json")
    } else {
        PathBuf::from(".claude/settings.json")
    };
    apply_json_settings(
        path,
        print_only,
        on_unparseable,
        "Claude Code",
        &command,
        |s| inject_nested_hooks(s, &command, &legacy_command, CLAUDE_EVENTS, "*"),
    )
}

/// `dira init gemini` — wire Gemini CLI command hooks into `~/.gemini/settings.json`
/// (or `.gemini/settings.json` without `--global`).
pub fn run_gemini(global: bool, print_only: bool, on_unparseable: OnUnparseable) -> Result<Wired> {
    let (command, legacy_command) = hook_commands("gemini");
    let path = if global {
        dira_core::config::home_dir()
            .context("resolve home directory")?
            .join(".gemini/settings.json")
    } else {
        PathBuf::from(".gemini/settings.json")
    };
    // Gemini matches tool events by regex, so the catch-all is `.*` (not Claude's `*`).
    apply_json_settings(
        path,
        print_only,
        on_unparseable,
        "Gemini CLI",
        &command,
        |s| inject_nested_hooks(s, &command, &legacy_command, GEMINI_EVENTS, ".*"),
    )
}

/// `dira init cursor` — wire Cursor agent hooks into `~/.cursor/hooks.json`
/// (or `.cursor/hooks.json` without `--global`).
pub fn run_cursor(global: bool, print_only: bool, on_unparseable: OnUnparseable) -> Result<Wired> {
    let (command, legacy_command) = hook_commands("cursor");
    let path = if global {
        dira_core::config::home_dir()
            .context("resolve home directory")?
            .join(".cursor/hooks.json")
    } else {
        PathBuf::from(".cursor/hooks.json")
    };
    apply_json_settings(path, print_only, on_unparseable, "Cursor", &command, |s| {
        inject_cursor_hooks(s, &command, &legacy_command)
    })
}

/// `dira init grok` — wire Grok Build hooks into `~/.grok/hooks/dira.json`.
/// grok-build has no equivalent project-local scope we can write without a
/// folder-trust prompt, so hooks are always written user-level regardless of
/// `--global`. Home resolves via `home_dir()` (USERPROFILE-aware) and the
/// command goes through `hook_commands` like every other harness — grok-build
/// runs natively on windows too, with the same `%USERPROFILE%\.grok` layout.
pub fn run_grok(global: bool, print_only: bool, on_unparseable: OnUnparseable) -> Result<Wired> {
    let (command, legacy_command) = hook_commands("grok");
    let path = dira_core::config::home_dir()
        .context("resolve home directory")?
        .join(".grok/hooks/dira.json");
    // Carried on the result rather than printed here, so the caller decides
    // ordering — `Wired::print` emits it above the wired line exactly as
    // before, and `dira onboard` folds it into its own summary.
    let note = (!global && !print_only).then(|| {
        format!(
            "note: grok hooks are user-level only (no trusted project scope); writing {}",
            path.display()
        )
    });
    let wired = apply_json_settings(
        path,
        print_only,
        on_unparseable,
        "Grok Build",
        &command,
        |s| inject_nested_hooks(s, &command, &legacy_command, GROK_EVENTS, "*"),
    )?;
    Ok(Wired { note, ..wired })
}

/// Resolve the path to this `dira` executable, canonicalized and normalized
/// for embedding into generated hook command strings.
///
/// Uses `dunce::canonicalize` instead of `std::fs::canonicalize`: on Windows,
/// std's canonicalize returns a `\\?\C:\...` verbatim-prefixed path, which —
/// once embedded into a hook config — breaks both Git Bash and PowerShell
/// (neither treats `\\?\` as an ordinary path). `dunce` strips the prefix
/// when it's safe to and is a no-op on unix.
pub(crate) fn dira_exe_path() -> String {
    let raw = std::env::current_exe()
        .ok()
        .and_then(|p| dunce::canonicalize(&p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dira".to_string());
    normalize_exe_path(raw)
}

/// On Windows, rewrite backslashes to forward slashes: `C:/Users/.../dira.exe`
/// is valid input to Win32, cmd.exe, PowerShell, and Git Bash alike, and it
/// keeps the printed Codex TOML snippet free of `\`-escape issues (TOML basic
/// strings treat `\` as an escape character, so a raw Windows path would need
/// every backslash doubled).
#[cfg(windows)]
fn normalize_exe_path(path: String) -> String {
    path.replace('\\', "/")
}

#[cfg(not(windows))]
fn normalize_exe_path(path: String) -> String {
    path
}

/// Quote `path` for embedding into a generated command string when it
/// contains whitespace (e.g. `/Users/John Doe/.local/bin/dira`). This was
/// broken on every platform before (not just Windows) — fixed once here for
/// every call site that builds a hook command string.
fn quote_if_needed(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// Build the `dira hook <harness>` command string in both forms an
/// idempotency check must recognize:
/// - `command` — the current, quoted-if-needed form actually written today.
/// - `legacy_command` — the always-unquoted form earlier `dira init` builds
///   wrote, so re-running `init` after an upgrade recognizes its own
///   previously-installed (unquoted) entry instead of adding a duplicate.
fn hook_commands(harness: &str) -> (String, String) {
    let exe = dira_exe_path();
    let command = format!("{} hook {harness}", quote_if_needed(&exe));
    let legacy_command = format!("{exe} hook {harness}");
    (command, legacy_command)
}

/// Does this hook group already invoke our command, in either form?
///
/// The single source of truth for "is dira wired here", shared by `dira init`'s
/// idempotency check and `dira doctor`'s reader — they cannot disagree about
/// what a wired entry looks like if they ask the same function.
fn group_has_command(group: &Value, command: &str, legacy_command: &str) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter().any(|h| {
                matches!(
                    h.get("command").and_then(|c| c.as_str()),
                    Some(c) if c == command || c == legacy_command
                )
            })
        })
        .unwrap_or(false)
}

/// Does this command string invoke *some* `dira hook <harness>`, whatever
/// binary it names?
///
/// This — not an exact string match — is the right question for "are hooks
/// wired". The path in a working config is routinely not this process's path:
/// the user may be running a dev build, a different install prefix, or a
/// command with a shell variable in it (`$HOME/.local/bin/dira hook claude`,
/// which the harness expands and we cannot). Whether the configured binary is
/// the right one is a separate question, and `hooks.exe_path` answers it.
/// Matched on the command's *suffix* rather than by parsing out the executable
/// first. The pre-upgrade unquoted form can itself contain spaces
/// (`/Users/John Doe/bin/dira hook claude`), so splitting on whitespace
/// mis-reads exactly the configs that most need recognising. Requiring a
/// non-empty prefix keeps a bare `hook claude` from matching, and anchoring
/// the end keeps `dira hook claude --dry-run` from doing so either.
pub(crate) fn command_invokes_hook(command: &str, harness: &str) -> bool {
    let command = command.trim();
    let suffix = format!(" hook {harness}");
    command.ends_with(&suffix) && command.len() > suffix.len()
}

/// Is `event` wired to a dira hook for `harness` in this settings tree?
///
/// Handles both shapes we write, because they cannot be confused with each
/// other: a nested group (Claude/Gemini/Grok) carries its commands one level
/// down under `hooks`, and a Cursor entry carries `command` directly. Accepting
/// either means the caller never has to know which shape it is looking at.
///
/// Deliberately tolerant: a missing `hooks` object or a non-array
/// `hooks.<Event>` reads as "not wired", never as an error. A diagnostic that
/// refuses to run on a hand-edited config is a diagnostic nobody can use.
fn event_is_wired(settings: &Value, event: &str, harness: &str) -> bool {
    let invokes = |v: &Value| {
        v.get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| command_invokes_hook(c, harness))
    };
    settings
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|a| a.as_array())
        .is_some_and(|entries| {
            entries.iter().any(|e| {
                invokes(e)
                    || e.get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|inner| inner.iter().any(invokes))
            })
        })
}

/// Which of `events` have no dira entry for `harness`. Empty ⇒ fully wired.
pub(crate) fn missing_hooks(settings: &Value, events: &[&str], harness: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| !event_is_wired(settings, e, harness))
        .map(|e| (*e).to_string())
        .collect()
}

/// Every dira-looking hook command string in a settings tree — including ones
/// pointing at a *different* binary, which is exactly what the "did you move or
/// reinstall dira" check needs and what a wired/not-wired predicate cannot see.
///
/// Walks both shapes: nested groups (`hooks.<Event>[].hooks[].command`) and
/// Cursor's flat entries (`hooks.<event>[].command`).
pub(crate) fn dira_hook_commands(settings: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(events) = settings.get("hooks").and_then(|h| h.as_object()) else {
        return out;
    };
    let mut push = |v: &Value| {
        if let Some(c) = v.get("command").and_then(|c| c.as_str()) {
            if c.contains(" hook ") && !out.iter().any(|s: &String| s == c) {
                out.push(c.to_string());
            }
        }
    };
    for entries in events.values().filter_map(|e| e.as_array()) {
        for entry in entries {
            // Cursor's flat form: the command sits on the entry itself.
            push(entry);
            // Nested form: one level deeper, under the group's `hooks`.
            for inner in entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .into_iter()
                .flatten()
            {
                push(inner);
            }
        }
    }
    out
}

/// Split the executable back out of a configured hook command string — the
/// inverse of [`quote_if_needed`].
///
/// Honours a leading double quote (a path with whitespace, the form `init`
/// writes today), otherwise splits on the first whitespace (the pre-upgrade
/// unquoted form). `None` for an empty or quote-only string.
pub(crate) fn hook_command_exe(command: &str) -> Option<String> {
    let command = command.trim();
    let exe = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default(),
        None => command.split_whitespace().next().unwrap_or_default(),
    };
    (!exe.is_empty()).then(|| exe.to_string())
}

/// A harness config file `dira doctor` inspects, and the events it should carry.
pub(crate) struct HarnessConfig {
    pub harness: &'static str,
    pub scope: &'static str,
    pub path: PathBuf,
    /// Event names only. The writer additionally needs to know which take a
    /// matcher; a reader only has to know what should be present.
    pub events: Vec<&'static str>,
}

/// Every harness config `dira init` can write: `(harness, relative path, event
/// names, project-scoped)`.
///
/// Codex is absent on purpose — `dira init codex` only prints a TOML snippet
/// for the user to paste, so there is no file we own and nothing to verify.
/// Grok is user-level only, matching `run_grok`.
type HarnessRow = (&'static str, &'static str, fn() -> Vec<&'static str>, bool);
const HARNESS_CONFIGS: &[HarnessRow] = &[
    ("claude", ".claude/settings.json", claude_events, true),
    ("gemini", ".gemini/settings.json", gemini_events, true),
    ("cursor", ".cursor/hooks.json", cursor_events, true),
    ("grok", ".grok/hooks/dira.json", grok_events, false),
];

fn claude_events() -> Vec<&'static str> {
    CLAUDE_EVENTS.iter().map(|(e, _)| *e).collect()
}
fn gemini_events() -> Vec<&'static str> {
    GEMINI_EVENTS.iter().map(|(e, _)| *e).collect()
}
fn cursor_events() -> Vec<&'static str> {
    CURSOR_EVENTS.to_vec()
}
fn grok_events() -> Vec<&'static str> {
    GROK_EVENTS.iter().map(|(e, _)| *e).collect()
}

/// Every harness config to inspect, **project scope first** — that is Claude
/// Code's own precedence, so the first match for a harness is the entry that
/// would actually run.
pub(crate) fn harness_config_paths() -> Vec<HarnessConfig> {
    let home = dira_core::config::home_dir();
    let mut out = Vec::new();
    for (harness, rel, events, project_scoped) in HARNESS_CONFIGS {
        if *project_scoped {
            out.push(HarnessConfig {
                harness,
                scope: "project",
                path: PathBuf::from(rel),
                events: events(),
            });
        }
    }
    if let Ok(home) = &home {
        for (harness, rel, events, _) in HARNESS_CONFIGS {
            out.push(HarnessConfig {
                harness,
                scope: "global",
                path: home.join(rel),
                events: events(),
            });
        }
    }
    out
}

/// Load a JSON settings file (or start empty), let `inject` merge our hooks in,
/// then either print or write it back — preserving existing keys. Shared by the
/// harnesses whose config is JSON (Claude, Gemini, Cursor).
///
/// `inject` returns how many events it newly added, which is what lets a
/// caller distinguish "wired it" from "was already wired" without re-reading
/// the file.
fn apply_json_settings(
    path: PathBuf,
    print_only: bool,
    on_unparseable: OnUnparseable,
    label: &'static str,
    command: &str,
    inject: impl FnOnce(&mut Value) -> usize,
) -> Result<Wired> {
    let mut settings: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => match on_unparseable {
                OnUnparseable::Overwrite => json!({}),
                OnUnparseable::Refuse => bail!(
                    "{} is not valid JSON ({e}) — refusing to overwrite it. \
                     Fix or move the file, then re-run.",
                    path.display()
                ),
            },
        }
    } else {
        json!({})
    };

    let events_added = inject(&mut settings);

    if print_only {
        println!("{}", serde_json::to_string_pretty(&settings)?);
        return Ok(Wired {
            label,
            kind: Kind::Hooks,
            path: None,
            command: command.to_string(),
            events_added,
            note: None,
        });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    Ok(Wired {
        label,
        kind: Kind::Hooks,
        path: Some(path),
        command: command.to_string(),
        events_added,
        note: None,
    })
}

/// `dira init codex` — emit the `~/.codex/config.toml` hook tables that wire
/// `dira hook codex` onto Codex's command hooks. The hook flows over the same
/// stdin→socket shim as Claude, so no HTTP bearer is needed.
///
/// We only ever **print** the snippet (TOML merging across arbitrary user config
/// is risky); the user pastes it into their `~/.codex/config.toml`.
///
/// Codex hooks use nested array-of-tables: `[[hooks.<Event>]]` (with an optional
/// regex `matcher`) then `[[hooks.<Event>.hooks]]` carrying `type`/`command`.
pub fn run_codex(_print_only: bool) -> Result<Wired> {
    let (command, _legacy_command) = hook_commands("codex");
    // The snippet is only ever printed (never merged into an existing file),
    // so there's no idempotency check here and no need for the legacy form.
    // TOML basic strings use `"` as the delimiter, so a quoted exe path (see
    // `quote_if_needed`) needs its inner `"` escaped to stay valid TOML.
    let toml_command = command.replace('"', "\\\"");
    // Codex's hook events mirror Claude Code's; each runs the same forwarder.
    let events: &[(&str, bool)] = &[
        ("SessionStart", false),
        ("UserPromptSubmit", false),
        ("Stop", false),
        ("SubagentStop", false),
        ("PreToolUse", true),
        ("PostToolUse", true),
        ("PermissionRequest", false),
    ];
    println!("# Add this to ~/.codex/config.toml to report Codex sessions to dira:");
    for (ev, needs_matcher) in events {
        println!("[[hooks.{ev}]]");
        if *needs_matcher {
            println!("matcher = \".*\"");
        }
        println!("[[hooks.{ev}.hooks]]");
        println!("type = \"command\"");
        println!("command = \"{toml_command}\"");
        println!();
    }
    // `dira daemon start` is exactly the trap DIRASH-0029 exists to delete:
    // a bare-started daemon holds the control socket, which then blocks
    // `dira daemon install` (D-0009). Point at the commands that actually
    // set the machine up instead.
    println!(
        "# Then run `dira onboard` (or `dira daemon install` to register it as a login service)."
    );
    // `path: None` — codex is print-only by construction, so there is nothing
    // for a caller to report as written. `dira onboard` renders this as
    // "snippet printed, paste it into ~/.codex/config.toml" rather than as a
    // completed step, because it isn't one.
    Ok(Wired {
        label: "Codex CLI",
        kind: Kind::Hooks,
        path: None,
        command,
        events_added: 0,
        note: Some("codex is print-only: paste the snippet above into ~/.codex/config.toml".into()),
    })
}

/// Whether `path` already holds exactly `plugin` — a missing or unreadable
/// file, or content that differs at all, is "not current" and must be
/// (re)written. Pulled out of [`run_opencode`] so the no-op decision is
/// testable against a real temp file without resolving `home_dir()` or a
/// daemon bearer token, neither of which this check needs.
fn opencode_plugin_is_current(path: &std::path::Path, plugin: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|existing| existing == plugin)
}

/// `dira init opencode` — write the forwarder plugin to
/// `~/.config/opencode/plugin/dira.js` (or print it with `--print`). The plugin
/// POSTs Dira-vocabulary JSON to the daemon's HTTP `/hooks/opencode` route, so it
/// needs the daemon's bearer token + port.
pub async fn run_opencode(config: &Config, print_only: bool) -> Result<Wired> {
    let bearer = resolve_bearer(config).await?;
    let url = format!("http://127.0.0.1:{}", config.http_port);
    let plugin = dira_sources::opencode::plugin_js(&url, &bearer);
    let command = format!("{url}/hooks/opencode");

    if print_only {
        println!("{plugin}");
        return Ok(Wired {
            label: "OpenCode",
            kind: Kind::Plugin,
            path: None,
            command,
            events_added: 0,
            note: None,
        });
    }

    let home = dira_core::config::home_dir().context("resolve home directory")?;
    // verify on native Windows — OpenCode's Windows plugin directory is unconfirmed.
    let dir = home.join(".config/opencode/plugin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("dira.js");

    // OpenCode is a written *plugin*, not merged events, so there is no
    // per-event count to report — one file, wholesale. `already_wired()`
    // is `path.is_some() && events_added == 0`, so an unconditional `1`
    // here meant it could never be true for OpenCode: every re-run —
    // including onboarding's second, nothing-should-change pass — rewrote
    // the file and reported it as newly wired. Read the existing file
    // first; identical content is a real no-op.
    let events_added = if opencode_plugin_is_current(&path, &plugin) {
        0
    } else {
        std::fs::write(&path, &plugin)?;
        1
    };
    Ok(Wired {
        label: "OpenCode",
        kind: Kind::Plugin,
        path: Some(path),
        command,
        events_added,
        note: None,
    })
}

/// Resolve the daemon's HTTP bearer the same way `dirad` does: `DIRA_BEARER`
/// env, else the value stored in the daemon's `meta` table. Errors if neither is
/// available (the daemon hasn't been started yet to mint one).
async fn resolve_bearer(config: &Config) -> Result<String> {
    if let Ok(env) = std::env::var("DIRA_BEARER") {
        if !env.is_empty() {
            return Ok(env);
        }
    }
    let store = Store::open(&config.db_path)
        .await
        .with_context(|| format!("open store at {}", config.db_path.display()))?;
    store
        .meta_get("bearer")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .context(
            "no bearer token yet — run `dira daemon start` once (it mints one), \
             or set DIRA_BEARER, then re-run `dira init opencode`",
        )
}

/// Ensure each event has our command hook under a Claude/Gemini-shaped `hooks`
/// object, without clobbering existing hooks. `matcher` is the catch-all value for
/// tool events (`*` for Claude, `.*` for Gemini's regex matcher).
///
/// `legacy_command` is the always-unquoted form of `command` a pre-upgrade
/// `dira init` may have already written (see [`hook_commands`]) — the
/// "already installed" check matches either form so re-running `init` after
/// an upgrade doesn't duplicate the entry.
fn inject_nested_hooks(
    settings: &mut Value,
    command: &str,
    legacy_command: &str,
    events: &[(&str, bool)],
    matcher: &str,
) -> usize {
    let hooks = settings
        .as_object_mut()
        .expect("settings is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().expect("hooks is an object");

    let mut added = 0;
    for (event, needs_matcher) in events {
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let arr = match arr.as_array_mut() {
            Some(a) => a,
            None => continue,
        };

        let already = arr
            .iter()
            .any(|group| group_has_command(group, command, legacy_command));
        if already {
            continue;
        }

        let mut group = json!({ "hooks": [ { "type": "command", "command": command } ] });
        if *needs_matcher {
            group["matcher"] = json!(matcher);
        }
        arr.push(group);
        added += 1;
    }
    added
}

/// Ensure each Cursor event has our `{ command }` entry under `hooks`, without
/// clobbering existing ones. Cursor's `hooks.json` is a flat `{ version, hooks }`.
///
/// `legacy_command` is the always-unquoted form of `command` a pre-upgrade
/// `dira init` may have already written (see [`hook_commands`]) — matched
/// alongside `command` so re-running `init` after an upgrade doesn't
/// duplicate the entry.
fn inject_cursor_hooks(settings: &mut Value, command: &str, legacy_command: &str) -> usize {
    let obj = settings.as_object_mut().expect("settings is an object");
    obj.entry("version".to_string()).or_insert_with(|| json!(1));
    let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().expect("hooks is an object");

    let mut added = 0;
    for event in CURSOR_EVENTS {
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let arr = match arr.as_array_mut() {
            Some(a) => a,
            None => continue,
        };
        let already = arr.iter().any(|e| {
            matches!(
                e.get("command").and_then(|c| c.as_str()),
                Some(c) if c == command || c == legacy_command
            )
        });
        if already {
            continue;
        }
        arr.push(json!({ "command": command }));
        added += 1;
    }
    added
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    const CMD: &str = "\"/Users/John Doe/.local/bin/dira\" hook claude";
    const LEGACY: &str = "/Users/John Doe/.local/bin/dira hook claude";

    /// The anti-drift test: whatever the writer injects, the reader must
    /// recognise. Without it the two halves are free to disagree and `doctor`
    /// reports a correctly-wired machine as broken.
    #[test]
    fn the_reader_recognises_everything_the_writer_injects() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, CMD, LEGACY, CLAUDE_EVENTS, "*");
        assert!(missing_hooks(&s, &claude_events(), "claude").is_empty());

        let cursor_cmd = "\"/Users/John Doe/.local/bin/dira\" hook cursor";
        let mut c = json!({});
        inject_cursor_hooks(&mut c, cursor_cmd, cursor_cmd);
        assert!(missing_hooks(&c, CURSOR_EVENTS, "cursor").is_empty());
    }

    /// The wiring question is "does a dira hook entry exist", NOT "does it
    /// name this exact binary". A correctly-wired machine reads as unwired
    /// otherwise, every time doctor runs from a different path than the config
    /// records — a dev build, a different install prefix, or a command with an
    /// unexpanded `$HOME` in it. Binary identity is `hooks.exe_path`'s job.
    #[test]
    fn wiring_is_recognised_whatever_binary_the_command_names() {
        for cmd in [
            "$HOME/.local/bin/dira hook claude",
            "/opt/homebrew/bin/dira hook claude",
            "~/bin/dira hook claude",
            "\"C:/Program Files/dira/dira.exe\" hook claude",
            "dira hook claude",
        ] {
            let mut s = json!({});
            inject_nested_hooks(&mut s, cmd, cmd, CLAUDE_EVENTS, "*");
            assert!(
                missing_hooks(&s, &claude_events(), "claude").is_empty(),
                "{cmd} should read as wired"
            );
        }
    }

    /// ...and it must not confuse one harness's hooks for another's.
    #[test]
    fn a_hook_for_another_harness_is_not_this_harness_wiring() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, "/bin/dira hook gemini", "x", CLAUDE_EVENTS, "*");
        assert_eq!(
            missing_hooks(&s, &claude_events(), "claude").len(),
            CLAUDE_EVENTS.len()
        );
        assert!(command_invokes_hook("/bin/dira hook gemini", "gemini"));
        // A command that merely starts the same way is not a hook entry.
        assert!(!command_invokes_hook(
            "/bin/dira hook claude --dry-run",
            "claude"
        ));
        assert!(!command_invokes_hook("/bin/dira status", "claude"));
    }

    /// Project scope before global, per harness — Claude Code's own precedence,
    /// so the first match is the entry that would actually run. The capture
    /// probe drives that entry, so the ordering is load-bearing, not cosmetic.
    #[test]
    fn harness_config_paths_put_project_scope_first() {
        let paths = harness_config_paths();
        let claude: Vec<&str> = paths
            .iter()
            .filter(|h| h.harness == "claude")
            .map(|h| h.scope)
            .collect();
        assert_eq!(claude.first(), Some(&"project"));
        assert!(
            claude.contains(&"global"),
            "global scope must be inspected too"
        );
        // Grok has no trusted project scope (see `run_grok`), so it is global only.
        assert!(paths
            .iter()
            .filter(|h| h.harness == "grok")
            .all(|h| h.scope == "global"));
        // Every entry carries the events its harness should have wired.
        assert!(paths.iter().all(|h| !h.events.is_empty()));
    }

    /// A pre-upgrade `init` wrote the unquoted form; it is still wired.
    #[test]
    fn legacy_unquoted_entries_are_recognised() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, LEGACY, LEGACY, CLAUDE_EVENTS, "*");
        assert!(missing_hooks(&s, &claude_events(), "claude").is_empty());
    }

    #[test]
    fn a_partially_wired_config_names_only_the_missing_events() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, CMD, LEGACY, &CLAUDE_EVENTS[..2], "*");
        let missing = missing_hooks(&s, &claude_events(), "claude");
        assert_eq!(missing.len(), CLAUDE_EVENTS.len() - 2);
        assert!(!missing.contains(&"SessionStart".to_string()));
        assert!(missing.contains(&"PostToolUse".to_string()));
    }

    /// A hand-edited config must read as "not wired", never blow up.
    #[test]
    fn malformed_settings_read_as_not_wired() {
        for s in [
            json!({}),
            json!({ "hooks": "nonsense" }),
            json!({ "hooks": { "SessionStart": 7 } }),
            json!({ "hooks": { "SessionStart": [ { "matcher": "*" } ] } }),
        ] {
            let missing = missing_hooks(&s, &claude_events(), "claude");
            assert_eq!(missing.len(), CLAUDE_EVENTS.len());
            assert!(dira_hook_commands(&s).is_empty());
        }
    }

    /// `dira_hook_commands` must surface entries pointing at a *different*
    /// binary — that is the moved/reinstalled-dira case, which the wired
    /// predicate is blind to by construction.
    #[test]
    fn hook_commands_surface_a_stale_binary_and_both_config_shapes() {
        let mut nested = json!({});
        inject_nested_hooks(
            &mut nested,
            "/old/path/dira hook claude",
            "x",
            CLAUDE_EVENTS,
            "*",
        );
        assert_eq!(
            dira_hook_commands(&nested),
            vec!["/old/path/dira hook claude"]
        );

        let cursor_cmd = "\"/Users/John Doe/.local/bin/dira\" hook cursor";
        let mut cursor = json!({});
        inject_cursor_hooks(&mut cursor, cursor_cmd, cursor_cmd);
        assert_eq!(dira_hook_commands(&cursor), vec![cursor_cmd]);

        // A non-dira hook someone else installed is not ours to report on.
        let foreign =
            json!({ "hooks": { "Stop": [ { "hooks": [ { "command": "make lint" } ] } ] } });
        assert!(dira_hook_commands(&foreign).is_empty());
    }

    #[test]
    fn hook_command_exe_inverts_quote_if_needed() {
        assert_eq!(
            hook_command_exe(CMD).as_deref(),
            Some("/Users/John Doe/.local/bin/dira")
        );
        assert_eq!(
            hook_command_exe("/usr/local/bin/dira hook claude").as_deref(),
            Some("/usr/local/bin/dira")
        );
        assert_eq!(
            hook_command_exe("dira hook claude").as_deref(),
            Some("dira")
        );
        // Windows: `init` writes forward slashes (see `normalize_exe_path`).
        assert_eq!(
            hook_command_exe("\"C:/Program Files/dira/dira.exe\" hook claude").as_deref(),
            Some("C:/Program Files/dira/dira.exe")
        );
        assert!(hook_command_exe("").is_none());
        assert!(hook_command_exe("\"").is_none());
    }

    /// Round-trip through the real quoting helper, so a change to
    /// `quote_if_needed` cannot silently break the reader.
    #[test]
    fn quote_then_split_round_trips_for_paths_with_spaces() {
        for path in [
            "/tmp/dira",
            "/Users/John Doe/bin/dira",
            "C:/Program Files/dira.exe",
        ] {
            let command = format!("{} hook claude", quote_if_needed(path));
            assert_eq!(hook_command_exe(&command).as_deref(), Some(path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_into_empty_settings() {
        let mut s = json!({});
        inject_nested_hooks(
            &mut s,
            "dira hook claude",
            "dira hook claude",
            CLAUDE_EVENTS,
            "*",
        );
        assert!(s["hooks"]["UserPromptSubmit"].is_array());
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"].as_str(), Some("*"));
    }

    #[test]
    fn is_idempotent() {
        let mut s = json!({});
        inject_nested_hooks(
            &mut s,
            "dira hook claude",
            "dira hook claude",
            CLAUDE_EVENTS,
            "*",
        );
        inject_nested_hooks(
            &mut s,
            "dira hook claude",
            "dira hook claude",
            CLAUDE_EVENTS,
            "*",
        );
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    }

    /// A pre-existing entry in the *legacy* (always-unquoted) form — as an
    /// earlier `dira init` would have written before quoting was added —
    /// must also be recognized as "already installed", so upgrading and
    /// re-running `init` doesn't duplicate the hook entry.
    #[test]
    fn is_idempotent_against_a_legacy_unquoted_entry() {
        let mut s = json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [ { "type": "command", "command": "/Users/John Doe/bin/dira hook claude" } ] }
                ]
            }
        });
        // The new quoted form differs from what's on disk...
        inject_nested_hooks(
            &mut s,
            "\"/Users/John Doe/bin/dira\" hook claude",
            "/Users/John Doe/bin/dira hook claude",
            CLAUDE_EVENTS,
            "*",
        );
        // ...but matching the legacy form must still suppress the duplicate.
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn preserves_existing_keys_and_hooks() {
        let mut s = json!({
            "model": "claude-opus-4-8",
            "hooks": { "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": "other" } ] } ] }
        });
        inject_nested_hooks(
            &mut s,
            "dira hook claude",
            "dira hook claude",
            CLAUDE_EVENTS,
            "*",
        );
        assert_eq!(s["model"].as_str(), Some("claude-opus-4-8"));
        // both the pre-existing and our hook are present.
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn gemini_injects_lifecycle_and_regex_matcher() {
        let mut s = json!({});
        inject_nested_hooks(
            &mut s,
            "dira hook gemini",
            "dira hook gemini",
            GEMINI_EVENTS,
            ".*",
        );
        assert!(s["hooks"]["BeforeAgent"].is_array());
        assert!(s["hooks"]["AfterAgent"].is_array());
        assert_eq!(s["hooks"]["BeforeTool"][0]["matcher"].as_str(), Some(".*"));
    }

    #[test]
    fn cursor_injects_flat_command_entries() {
        let mut s = json!({});
        inject_cursor_hooks(&mut s, "dira hook cursor", "dira hook cursor");
        assert_eq!(s["version"].as_i64(), Some(1));
        assert_eq!(
            s["hooks"]["beforeSubmitPrompt"][0]["command"].as_str(),
            Some("dira hook cursor")
        );
        assert!(s["hooks"]["stop"].is_array());
    }

    #[test]
    fn cursor_is_idempotent_and_preserves_keys() {
        let mut s = json!({ "version": 1, "hooks": { "stop": [ { "command": "other" } ] } });
        inject_cursor_hooks(&mut s, "dira hook cursor", "dira hook cursor");
        inject_cursor_hooks(&mut s, "dira hook cursor", "dira hook cursor");
        // pre-existing + ours, ours added once.
        assert_eq!(s["hooks"]["stop"].as_array().unwrap().len(), 2);
    }

    /// Cursor's flat entry form must also recognize a pre-existing legacy
    /// (unquoted) command string as already installed.
    #[test]
    fn cursor_is_idempotent_against_a_legacy_unquoted_entry() {
        let mut s = json!({
            "version": 1,
            "hooks": { "stop": [ { "command": "/Users/John Doe/bin/dira hook cursor" } ] }
        });
        inject_cursor_hooks(
            &mut s,
            "\"/Users/John Doe/bin/dira\" hook cursor",
            "/Users/John Doe/bin/dira hook cursor",
        );
        assert_eq!(s["hooks"]["stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn quote_if_needed_quotes_paths_with_whitespace() {
        assert_eq!(
            quote_if_needed("/Users/John Doe/.local/bin/dira"),
            "\"/Users/John Doe/.local/bin/dira\""
        );
        assert_eq!(
            quote_if_needed("/Users/jane/.local/bin/dira"),
            "/Users/jane/.local/bin/dira"
        );
    }

    #[test]
    fn grok_injects_events_without_matcher() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, "dira hook grok", "dira hook grok", GROK_EVENTS, "*");
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(s["hooks"]["PreToolUse"][0].get("matcher").is_none());
    }

    #[test]
    fn grok_inject_is_idempotent() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, "dira hook grok", "dira hook grok", GROK_EVENTS, "*");
        inject_nested_hooks(&mut s, "dira hook grok", "dira hook grok", GROK_EVENTS, "*");
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dira-init-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    fn wire(path: PathBuf, on_unparseable: OnUnparseable) -> Result<Wired> {
        let (command, legacy) = ("dira hook claude", "dira hook claude");
        apply_json_settings(path, false, on_unparseable, "Claude Code", command, |s| {
            inject_nested_hooks(s, command, legacy, CLAUDE_EVENTS, "*")
        })
    }

    /// The count is what lets a caller say "wired it" vs "already wired"
    /// without re-reading the file — so a fresh file reports every event, and
    /// an immediate re-run reports none.
    #[test]
    fn events_added_counts_the_first_run_and_zero_on_re_run() {
        let path = tmp("count");
        let first = wire(path.clone(), OnUnparseable::Overwrite).unwrap();
        assert_eq!(first.events_added, CLAUDE_EVENTS.len());
        assert!(!first.already_wired());

        let second = wire(path, OnUnparseable::Overwrite).unwrap();
        assert_eq!(second.events_added, 0);
        assert!(
            second.already_wired(),
            "a second run must be reportable as a no-op"
        );
    }

    /// `Refuse` exists because `dira onboard` writes files the user never
    /// named. It must leave the bytes untouched — an error that still
    /// clobbered the file would be worse than the fallback it replaces.
    #[test]
    fn refuse_leaves_an_unparseable_file_byte_identical() {
        let path = tmp("refuse");
        let original = "{ this is not json";
        std::fs::write(&path, original).unwrap();

        let err = wire(path.clone(), OnUnparseable::Refuse).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid JSON"), "got: {msg}");
        assert!(
            msg.contains(&path.display().to_string()),
            "the error must name the file: {msg}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// The historical `dira init` behaviour, pinned deliberately rather than
    /// left implicit: parse failure falls back to `{}` and the previous
    /// contents are lost.
    #[test]
    fn overwrite_discards_an_unparseable_file() {
        let path = tmp("overwrite");
        std::fs::write(&path, "{ this is not json").unwrap();

        let wired = wire(path.clone(), OnUnparseable::Overwrite).unwrap();
        assert_eq!(wired.events_added, CLAUDE_EVENTS.len());
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(after["hooks"]["SessionStart"].is_array());
    }

    /// `Kind` is what the two report wordings are derived from, so a wrong
    /// variant silently changes `dira init opencode`'s output. Pins both
    /// forms — the refactor that removed the per-instance `written`/
    /// `command_label` strings had no other guard.
    #[test]
    fn the_report_wording_follows_the_kind() {
        let hooks = Wired {
            label: "Claude Code",
            kind: Kind::Hooks,
            path: Some(PathBuf::from("/tmp/settings.json")),
            command: "dira hook claude".into(),
            events_added: 8,
            note: None,
        };
        let plugin = Wired {
            label: "OpenCode",
            kind: Kind::Plugin,
            ..hooks.clone()
        };
        assert_eq!(
            hooks.headline(),
            ("wired Claude Code hooks".to_string(), "hook command")
        );
        assert_eq!(
            plugin.headline(),
            ("wrote OpenCode forwarder plugin".to_string(), "posts to")
        );
    }

    /// A *valid* config is never discarded by either policy — the fallback
    /// only ever applies to unparseable bytes.
    #[test]
    fn existing_keys_survive_both_policies() {
        for policy in [OnUnparseable::Overwrite, OnUnparseable::Refuse] {
            let path = tmp("preserve");
            std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
            wire(path.clone(), policy).unwrap();
            let after: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(after["theme"], "dark");
            assert!(after["hooks"]["SessionStart"].is_array());
        }
    }

    /// A missing file is never "current" — the first run of `dira init
    /// opencode` must write, not silently no-op.
    #[test]
    fn opencode_plugin_is_current_is_false_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dira.js");
        assert!(!opencode_plugin_is_current(&path, "// plugin body"));
    }

    /// The re-run case this fix exists for: identical content on disk is a
    /// real no-op, so `run_opencode`'s `events_added` can be `0` and
    /// `already_wired()` can finally be true for OpenCode.
    #[test]
    fn opencode_plugin_is_current_matches_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dira.js");
        std::fs::write(&path, "// plugin body").unwrap();
        assert!(opencode_plugin_is_current(&path, "// plugin body"));
    }

    /// Any drift at all — a bearer rotation, a port change, an upgrade that
    /// changes the generated plugin — must still trigger a real rewrite.
    #[test]
    fn opencode_plugin_is_current_is_false_when_content_differs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dira.js");
        std::fs::write(&path, "// old plugin body").unwrap();
        assert!(!opencode_plugin_is_current(&path, "// new plugin body"));
    }
}
