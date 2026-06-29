//! `dira init` — wire a harness's hooks so this machine reports to the daemon.
//!
//! - **Claude Code** (default): standard **command hooks**: each configured event
//!   runs `dira hook claude`, which reads the event JSON on stdin and forwards it
//!   to the daemon over the socket. Merged idempotently into `.claude/settings.json`
//!   (project) or `~/.claude/settings.json` (`--global`), preserving existing keys.
//! - **Codex** (`dira init codex`): Codex reads command hooks from
//!   `~/.codex/config.toml` `[hooks]`; we emit the `command = "dira hook codex"`
//!   snippet so events flow over the same stdin→socket shim.
//! - **OpenCode** (`dira init opencode`): OpenCode has no command hooks, so we
//!   write a tiny forwarder **plugin** to `~/.config/opencode/plugin/dira.js`
//!   that HTTP-POSTs to the daemon's `/hooks/opencode` route.

use anyhow::{Context, Result};
use dira_core::{Config, Store};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Events we hook and whether they need a tool matcher.
const EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("SessionEnd", false),
    ("UserPromptSubmit", false),
    ("Stop", false),
    ("SubagentStop", false),
    ("Notification", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
];

pub fn run(global: bool, print_only: bool) -> Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dira".to_string());
    let command = format!("{exe} hook claude");

    let settings_path = if global {
        let home = std::env::var("HOME").context("HOME not set")?;
        PathBuf::from(home).join(".claude/settings.json")
    } else {
        PathBuf::from(".claude/settings.json")
    };

    let mut settings: Value = if settings_path.exists() {
        let text = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    inject_hooks(&mut settings, &command);

    if print_only {
        println!("{}", serde_json::to_string_pretty(&settings)?);
        return Ok(());
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings)? + "\n",
    )?;
    println!("wired Claude Code hooks → {}", settings_path.display());
    println!("hook command: {command}");
    println!("start the daemon with `dira daemon start`, then work as usual.");
    Ok(())
}

/// Resolve the path to this `dira` executable (canonicalized when possible).
fn dira_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dira".to_string())
}

/// `dira init codex` — emit the `~/.codex/config.toml [hooks]` snippet that wires
/// `dira hook codex` onto Codex's command hooks. The hook flows over the same
/// stdin→socket shim as Claude, so no HTTP bearer is needed.
///
/// We only ever **print** the snippet (TOML merging across arbitrary user config
/// is risky); the user pastes it into their `~/.codex/config.toml`.
pub fn run_codex(_print_only: bool) -> Result<()> {
    let exe = dira_exe();
    let command = format!("{exe} hook codex");
    // Codex's hook events mirror Claude Code's; each runs the same forwarder.
    let events = [
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "SubagentStop",
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
    ];
    println!("# Add this to ~/.codex/config.toml to report Codex sessions to dira:");
    println!("[hooks]");
    for ev in events {
        println!("{ev} = {{ command = \"{command}\" }}");
    }
    println!();
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

/// Ensure each event has our command hook, without clobbering existing hooks.
fn inject_hooks(settings: &mut Value, command: &str) {
    let hooks = settings
        .as_object_mut()
        .expect("settings is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().expect("hooks is an object");

    for (event, needs_matcher) in EVENTS {
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
            group["matcher"] = json!("*");
        }
        arr.push(group);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_into_empty_settings() {
        let mut s = json!({});
        inject_hooks(&mut s, "dira hook claude");
        assert!(s["hooks"]["UserPromptSubmit"].is_array());
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"].as_str(), Some("*"));
    }

    #[test]
    fn is_idempotent() {
        let mut s = json!({});
        inject_hooks(&mut s, "dira hook claude");
        inject_hooks(&mut s, "dira hook claude");
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn preserves_existing_keys_and_hooks() {
        let mut s = json!({
            "model": "claude-opus-4-8",
            "hooks": { "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": "other" } ] } ] }
        });
        inject_hooks(&mut s, "dira hook claude");
        assert_eq!(s["model"].as_str(), Some("claude-opus-4-8"));
        // both the pre-existing and our hook are present.
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 2);
    }
}
