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

use anyhow::{Context, Result};
use dira_core::{Config, Store};
use serde_json::{json, Value};
use std::path::PathBuf;

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

/// `dira init` (default) — wire Claude Code command hooks.
pub fn run(global: bool, print_only: bool) -> Result<()> {
    let (command, legacy_command) = hook_commands("claude");
    let path = if global {
        dira_core::config::home_dir()
            .context("resolve home directory")?
            .join(".claude/settings.json")
    } else {
        PathBuf::from(".claude/settings.json")
    };
    apply_json_settings(path, print_only, "Claude Code", &command, |s| {
        inject_nested_hooks(s, &command, &legacy_command, CLAUDE_EVENTS, "*");
    })
}

/// `dira init gemini` — wire Gemini CLI command hooks into `~/.gemini/settings.json`
/// (or `.gemini/settings.json` without `--global`).
pub fn run_gemini(global: bool, print_only: bool) -> Result<()> {
    let (command, legacy_command) = hook_commands("gemini");
    let path = if global {
        dira_core::config::home_dir()
            .context("resolve home directory")?
            .join(".gemini/settings.json")
    } else {
        PathBuf::from(".gemini/settings.json")
    };
    // Gemini matches tool events by regex, so the catch-all is `.*` (not Claude's `*`).
    apply_json_settings(path, print_only, "Gemini CLI", &command, |s| {
        inject_nested_hooks(s, &command, &legacy_command, GEMINI_EVENTS, ".*");
    })
}

/// `dira init cursor` — wire Cursor agent hooks into `~/.cursor/hooks.json`
/// (or `.cursor/hooks.json` without `--global`).
pub fn run_cursor(global: bool, print_only: bool) -> Result<()> {
    let (command, legacy_command) = hook_commands("cursor");
    let path = if global {
        dira_core::config::home_dir()
            .context("resolve home directory")?
            .join(".cursor/hooks.json")
    } else {
        PathBuf::from(".cursor/hooks.json")
    };
    apply_json_settings(path, print_only, "Cursor", &command, |s| {
        inject_cursor_hooks(s, &command, &legacy_command);
    })
}

/// `dira init grok` — wire Grok Build hooks into `~/.grok/hooks/dira.json`.
/// grok-build has no equivalent project-local scope we can write without a
/// folder-trust prompt, so hooks are always written user-level regardless of
/// `--global`. Home resolves via `home_dir()` (USERPROFILE-aware) and the
/// command goes through `hook_commands` like every other harness — grok-build
/// runs natively on windows too, with the same `%USERPROFILE%\.grok` layout.
pub fn run_grok(global: bool, print_only: bool) -> Result<()> {
    let (command, legacy_command) = hook_commands("grok");
    let path = dira_core::config::home_dir()
        .context("resolve home directory")?
        .join(".grok/hooks/dira.json");
    if !global && !print_only {
        println!(
            "note: grok hooks are user-level only (no trusted project scope); writing {}",
            path.display()
        );
    }
    apply_json_settings(path, print_only, "Grok Build", &command, |s| {
        inject_nested_hooks(s, &command, &legacy_command, GROK_EVENTS, "*");
    })
}

/// Resolve the path to this `dira` executable, canonicalized and normalized
/// for embedding into generated hook command strings.
///
/// Uses `dunce::canonicalize` instead of `std::fs::canonicalize`: on Windows,
/// std's canonicalize returns a `\\?\C:\...` verbatim-prefixed path, which —
/// once embedded into a hook config — breaks both Git Bash and PowerShell
/// (neither treats `\\?\` as an ordinary path). `dunce` strips the prefix
/// when it's safe to and is a no-op on unix.
fn dira_exe_path() -> String {
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

/// Load a JSON settings file (or start empty), let `inject` merge our hooks in,
/// then either print or write it back — preserving existing keys. Shared by the
/// harnesses whose config is JSON (Claude, Gemini, Cursor).
fn apply_json_settings(
    path: PathBuf,
    print_only: bool,
    label: &str,
    command: &str,
    inject: impl FnOnce(&mut Value),
) -> Result<()> {
    let mut settings: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    inject(&mut settings);

    if print_only {
        println!("{}", serde_json::to_string_pretty(&settings)?);
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    println!("wired {label} hooks → {}", path.display());
    println!("hook command: {command}");
    println!("start the daemon with `dira daemon start`, then work as usual.");
    Ok(())
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
pub fn run_codex(_print_only: bool) -> Result<()> {
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
    println!("# Then start the daemon with `dira daemon start` and work as usual.");
    Ok(())
}

/// `dira init opencode` — write the forwarder plugin to
/// `~/.config/opencode/plugin/dira.js` (or print it with `--print`). The plugin
/// POSTs Dira-vocabulary JSON to the daemon's HTTP `/hooks/opencode` route, so it
/// needs the daemon's bearer token + port.
pub async fn run_opencode(config: &Config, print_only: bool) -> Result<()> {
    let bearer = resolve_bearer(config).await?;
    let url = format!("http://127.0.0.1:{}", config.http_port);
    let plugin = dira_sources::opencode::plugin_js(&url, &bearer);

    if print_only {
        println!("{plugin}");
        return Ok(());
    }

    let home = dira_core::config::home_dir().context("resolve home directory")?;
    // verify on native Windows — OpenCode's Windows plugin directory is unconfirmed.
    let dir = home.join(".config/opencode/plugin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("dira.js");
    std::fs::write(&path, plugin)?;
    println!("wrote OpenCode forwarder plugin → {}", path.display());
    println!("posts to {url}/hooks/opencode");
    println!("start the daemon with `dira daemon start`, then work as usual.");
    Ok(())
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
) {
    let hooks = settings
        .as_object_mut()
        .expect("settings is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().expect("hooks is an object");

    for (event, needs_matcher) in events {
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let arr = match arr.as_array_mut() {
            Some(a) => a,
            None => continue,
        };

        let already = arr.iter().any(|group| {
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
        });
        if already {
            continue;
        }

        let mut group = json!({ "hooks": [ { "type": "command", "command": command } ] });
        if *needs_matcher {
            group["matcher"] = json!(matcher);
        }
        arr.push(group);
    }
}

/// Ensure each Cursor event has our `{ command }` entry under `hooks`, without
/// clobbering existing ones. Cursor's `hooks.json` is a flat `{ version, hooks }`.
///
/// `legacy_command` is the always-unquoted form of `command` a pre-upgrade
/// `dira init` may have already written (see [`hook_commands`]) — matched
/// alongside `command` so re-running `init` after an upgrade doesn't
/// duplicate the entry.
fn inject_cursor_hooks(settings: &mut Value, command: &str, legacy_command: &str) {
    let obj = settings.as_object_mut().expect("settings is an object");
    obj.entry("version".to_string()).or_insert_with(|| json!(1));
    let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().expect("hooks is an object");

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
