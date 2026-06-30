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

/// `dira init` (default) — wire Claude Code command hooks.
pub fn run(global: bool, print_only: bool) -> Result<()> {
    let command = format!("{} hook claude", dira_exe());
    let path = if global {
        PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".claude/settings.json")
    } else {
        PathBuf::from(".claude/settings.json")
    };
    apply_json_settings(path, print_only, "Claude Code", &command, |s| {
        inject_nested_hooks(s, &command, CLAUDE_EVENTS, "*");
    })
}

/// `dira init gemini` — wire Gemini CLI command hooks into `~/.gemini/settings.json`
/// (or `.gemini/settings.json` without `--global`).
pub fn run_gemini(global: bool, print_only: bool) -> Result<()> {
    let command = format!("{} hook gemini", dira_exe());
    let path = if global {
        PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".gemini/settings.json")
    } else {
        PathBuf::from(".gemini/settings.json")
    };
    // Gemini matches tool events by regex, so the catch-all is `.*` (not Claude's `*`).
    apply_json_settings(path, print_only, "Gemini CLI", &command, |s| {
        inject_nested_hooks(s, &command, GEMINI_EVENTS, ".*");
    })
}

/// `dira init cursor` — wire Cursor agent hooks into `~/.cursor/hooks.json`
/// (or `.cursor/hooks.json` without `--global`).
pub fn run_cursor(global: bool, print_only: bool) -> Result<()> {
    let command = format!("{} hook cursor", dira_exe());
    let path = if global {
        PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".cursor/hooks.json")
    } else {
        PathBuf::from(".cursor/hooks.json")
    };
    apply_json_settings(path, print_only, "Cursor", &command, |s| {
        inject_cursor_hooks(s, &command);
    })
}

/// Resolve the path to this `dira` executable (canonicalized when possible).
fn dira_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dira".to_string())
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
    let command = format!("{} hook codex", dira_exe());
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
        println!("command = \"{command}\"");
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

    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".config/opencode/plugin");
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
fn inject_nested_hooks(
    settings: &mut Value,
    command: &str,
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
                    hs.iter()
                        .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
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
fn inject_cursor_hooks(settings: &mut Value, command: &str) {
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
        let already = arr
            .iter()
            .any(|e| e.get("command").and_then(|c| c.as_str()) == Some(command));
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
        inject_nested_hooks(&mut s, "dira hook claude", CLAUDE_EVENTS, "*");
        assert!(s["hooks"]["UserPromptSubmit"].is_array());
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"].as_str(), Some("*"));
    }

    #[test]
    fn is_idempotent() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, "dira hook claude", CLAUDE_EVENTS, "*");
        inject_nested_hooks(&mut s, "dira hook claude", CLAUDE_EVENTS, "*");
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn preserves_existing_keys_and_hooks() {
        let mut s = json!({
            "model": "claude-opus-4-8",
            "hooks": { "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": "other" } ] } ] }
        });
        inject_nested_hooks(&mut s, "dira hook claude", CLAUDE_EVENTS, "*");
        assert_eq!(s["model"].as_str(), Some("claude-opus-4-8"));
        // both the pre-existing and our hook are present.
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn gemini_injects_lifecycle_and_regex_matcher() {
        let mut s = json!({});
        inject_nested_hooks(&mut s, "dira hook gemini", GEMINI_EVENTS, ".*");
        assert!(s["hooks"]["BeforeAgent"].is_array());
        assert!(s["hooks"]["AfterAgent"].is_array());
        assert_eq!(s["hooks"]["BeforeTool"][0]["matcher"].as_str(), Some(".*"));
    }

    #[test]
    fn cursor_injects_flat_command_entries() {
        let mut s = json!({});
        inject_cursor_hooks(&mut s, "dira hook cursor");
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
        inject_cursor_hooks(&mut s, "dira hook cursor");
        inject_cursor_hooks(&mut s, "dira hook cursor");
        // pre-existing + ours, ours added once.
        assert_eq!(s["hooks"]["stop"].as_array().unwrap().len(), 2);
    }
}
