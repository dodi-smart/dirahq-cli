//! Double-delivery guard for the portable hook.
//!
//! `dira cloud init` commits `.dira/hook.sh`, wired into the project's
//! `.claude/settings.json` / `.cursor/hooks.json`. Onboarding *also* wires
//! hooks at user (global) scope, with an absolute binary path — and Claude
//! Code runs both wirings for a project it recognises, so an onboarded laptop
//! opening a teleport-ready repo would forward every event twice.
//!
//! The fix is a yield, not a dedup: `.dira/hook.sh` marks its own invocation
//! (`DIRA_HOOK_VIA=portable`, see `templates/dira-hook.sh`), and `dira hook`
//! checks — only on a portable invocation, never on a direct one — whether
//! the *same event* for the *same harness* is *also* wired at user scope with
//! an executable that actually resolves. If so, the user-scope wiring is
//! live and will deliver this event on its own; the portable path exits 0
//! without forwarding. No cache, no time window: the check is cheap (one
//! file read) and re-derived on every invocation.
//!
//! Deliberately excluded from "live wiring": another portable wrapper. Two
//! portable entries yielding to each other would silently drop the event
//! everywhere — see [`crate::init::command_is_portable_wrapper`].
//!
//! The marker alone is not trusted, either: `DIRA_HOOK_VIA=portable` is an
//! ordinary env var, and one that leaked into a shell profile would be
//! inherited by a *direct* `dira hook` invocation that never went through
//! `.dira/hook.sh` at all. So the yield is structural on both sides — see
//! [`project_scope_wires_portable`] and DIRASH-0037.

use crate::doctor::checks::{resolve_exe, ExePath};
use crate::init;
use serde_json::Value;

/// Was this `dira hook` invocation launched by the repo-committed portable
/// wrapper (`.dira/hook.sh`)?
///
/// Env read at the call site, same shape as `hook_probe_mode` in `main.rs`:
/// a plain predicate over one process-lifetime variable, consulted exactly
/// once per invocation and passed down explicitly rather than re-read deep
/// in the plumbing.
pub(crate) fn via_portable() -> bool {
    std::env::var("DIRA_HOOK_VIA").is_ok_and(|v| v == "portable")
}

/// Is `event` for `harness` also wired at user (global) scope, live?
///
/// Reads the same file `dira init --global` writes — via
/// [`init::harness_config_paths`]'s global row for this harness, which
/// already honours `CLAUDE_CONFIG_DIR` the way Claude Code itself does — and
/// asks the pure [`decide`] with the real [`resolve_exe`]. A missing or
/// unparseable file reads as "not wired", the same tolerant default
/// [`init::event_is_wired`] uses: a diagnostic (or, here, a delivery
/// decision) that refuses to run on a hand-edited config is worse than one
/// that just forwards.
pub(crate) fn user_scope_wires(harness: &str, event: &str) -> bool {
    let Some(path) = user_scope_path(harness) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    decide(&settings, harness, event, resolve_exe)
}

/// The user-scope config path for `harness`, if this harness has one.
fn user_scope_path(harness: &str) -> Option<std::path::PathBuf> {
    init::harness_config_paths()
        .into_iter()
        .find(|c| c.scope == "global" && c.harness == harness)
        .map(|c| c.path)
}

/// Is `event` for `harness` wired at **project** scope through the
/// repo-committed portable wrapper — the structural half of the yield
/// condition (DIRASH-0037) that keeps a stray `DIRA_HOOK_VIA=portable` env
/// var from making a direct invocation yield?
///
/// Reads [`init::harness_config_paths`]'s project row for this harness (the
/// same relative path `dira cloud init` writes into, e.g.
/// `.claude/settings.json`), resolved against `CLAUDE_PROJECT_DIR` when set —
/// the same anchor the committed wrapper itself is invoked relative to
/// (`${CLAUDE_PROJECT_DIR:-.}`, see `cloud_init::hook_command`) — and falling
/// back to the current directory otherwise. A missing or unparseable file
/// reads as "not wired", the same tolerant default [`user_scope_wires`] uses.
pub(crate) fn project_scope_wires_portable(harness: &str, event: &str) -> bool {
    let Some(config) = init::harness_config_paths()
        .into_iter()
        .find(|c| c.scope == "project" && c.harness == harness)
    else {
        return false;
    };
    let base = std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let Ok(text) = std::fs::read_to_string(base.join(config.path)) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    init::event_wired_by_portable_wrapper(&settings, event, harness)
}

/// Should a portable-marked invocation actually yield? Both structural
/// signals are required — the marker on its own (already checked at the
/// call site before this runs) is not enough:
///
/// - `user_scope_live` — [`user_scope_wires`]: a live, resolvable, catch-all
///   user-scope entry will deliver this event on its own;
/// - `project_wraps_portable` — [`project_scope_wires_portable`]: the
///   project-scope config genuinely wires this event through the portable
///   wrapper, proving a real portable invocation exists to yield to rather
///   than a stray `DIRA_HOOK_VIA=portable` inherited from a shell profile.
///
/// Pure so the combination is testable without touching the filesystem.
pub(crate) fn should_yield(user_scope_live: bool, project_wraps_portable: bool) -> bool {
    user_scope_live && project_wraps_portable
}

/// Pure core of [`user_scope_wires`]: does `settings` wire `event` for
/// `harness` with a *live* (non-portable, resolvable) command?
///
/// `resolve` is injected so the exe-existence check is fakeable in tests
/// without touching the filesystem.
///
/// Three conditions, all required, matching the design decision verbatim:
/// 1. the entry's matcher is absent, `""`, `"*"`, or `".*"` — a tool-event
///    entry scoped to one matcher (e.g. `"Bash"`) does not cover the other
///    tool events the portable hook would otherwise carry, so it must not
///    cause a yield;
/// 2. the command is a *direct* `dira hook <harness>` invocation — another
///    portable wrapper does not count as live wiring (see the module doc);
/// 3. the command's executable resolves to `Exists` or `Unverifiable` (never
///    `Missing`) — a dangling user-scope entry must not silently swallow the
///    event the portable path would otherwise have delivered.
pub(crate) fn decide(
    settings: &Value,
    harness: &str,
    event: &str,
    resolve: impl Fn(&str) -> ExePath,
) -> bool {
    let Some(entries) = settings
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|a| a.as_array())
    else {
        return false;
    };
    entries
        .iter()
        .any(|entry| entry_wires_live(entry, harness, &resolve))
}

fn entry_wires_live(entry: &Value, harness: &str, resolve: &impl Fn(&str) -> ExePath) -> bool {
    if !matcher_is_catch_all(entry) {
        return false;
    }
    entry_commands(entry)
        .iter()
        .any(|command| command_wires_live(command, harness, resolve))
}

/// Every command string on this entry — Cursor's flat shape (`command`
/// directly on the entry) and the nested shape (`command` one level down,
/// under `hooks`) both, since a caller here does not know in advance which
/// shape it is looking at.
fn entry_commands(entry: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    if let Some(c) = entry.get("command").and_then(Value::as_str) {
        out.push(c);
    }
    if let Some(inner) = entry.get("hooks").and_then(Value::as_array) {
        for e in inner {
            if let Some(c) = e.get("command").and_then(Value::as_str) {
                out.push(c);
            }
        }
    }
    out
}

fn command_wires_live(command: &str, harness: &str, resolve: &impl Fn(&str) -> ExePath) -> bool {
    if !init::command_invokes_hook(command, harness) {
        return false;
    }
    // A live entry, not another portable wrapper yielding right back to us.
    if init::command_is_portable_wrapper(command, harness) {
        return false;
    }
    match init::hook_command_exe(command) {
        Some(exe) => matches!(resolve(&exe), ExePath::Exists(_) | ExePath::Unverifiable(_)),
        None => false,
    }
}

/// Absent, `""`, `"*"` (Claude's catch-all), or `".*"` (Gemini's regex
/// catch-all). Anything else — a real matcher like `"Bash"` — scopes the
/// entry to one tool and must not count as covering the whole event.
fn matcher_is_catch_all(entry: &Value) -> bool {
    match entry.get("matcher") {
        None => true,
        Some(Value::String(s)) => s.is_empty() || s == "*" || s == ".*",
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exists(_exe: &str) -> ExePath {
        ExePath::Exists("/usr/local/bin/dira".to_string())
    }
    fn missing(_exe: &str) -> ExePath {
        ExePath::Missing("/usr/local/bin/dira".to_string())
    }
    fn unverifiable(_exe: &str) -> ExePath {
        ExePath::Unverifiable("$HOME/.local/bin/dira".to_string())
    }

    fn nested(event: &str, matcher: Option<&str>, command: &str) -> Value {
        let mut group = json!({ "hooks": [ { "type": "command", "command": command } ] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        json!({ "hooks": { event: [ group ] } })
    }

    #[test]
    fn wired_and_exists_yields() {
        let settings = nested("Stop", None, "/usr/local/bin/dira hook claude");
        assert!(decide(&settings, "claude", "Stop", exists));
    }

    #[test]
    fn wired_and_missing_does_not_yield() {
        let settings = nested("Stop", None, "/usr/local/bin/dira hook claude");
        assert!(!decide(&settings, "claude", "Stop", missing));
    }

    #[test]
    fn wired_and_unverifiable_yields() {
        let settings = nested("Stop", None, "$HOME/.local/bin/dira hook claude");
        assert!(decide(&settings, "claude", "Stop", unverifiable));
    }

    #[test]
    fn other_harness_does_not_yield() {
        let settings = nested("Stop", None, "/usr/local/bin/dira hook claude");
        assert!(!decide(&settings, "gemini", "Stop", exists));
    }

    #[test]
    fn partial_wiring_yields_only_the_wired_event() {
        let settings = nested("Stop", None, "/usr/local/bin/dira hook claude");
        assert!(decide(&settings, "claude", "Stop", exists));
        assert!(!decide(&settings, "claude", "SessionStart", exists));
    }

    #[test]
    fn matcher_bash_does_not_yield_other_tool_events() {
        // Wired for Bash only — a PreToolUse on Edit is not covered by this
        // entry, so the portable path must still forward it.
        let settings = nested(
            "PreToolUse",
            Some("Bash"),
            "/usr/local/bin/dira hook claude",
        );
        assert!(!decide(&settings, "claude", "PreToolUse", exists));
    }

    #[test]
    fn matcher_catch_all_forms_all_yield() {
        for matcher in [None, Some(""), Some("*"), Some(".*")] {
            let settings = nested("PreToolUse", matcher, "/usr/local/bin/dira hook claude");
            assert!(
                decide(&settings, "claude", "PreToolUse", exists),
                "matcher {matcher:?} should count as catch-all"
            );
        }
    }

    #[test]
    fn cursor_flat_shape_yields() {
        let settings = json!({
            "hooks": {
                "stop": [
                    { "command": "/usr/local/bin/dira hook cursor" }
                ]
            }
        });
        assert!(decide(&settings, "cursor", "stop", exists));
    }

    #[test]
    fn another_portable_wrapper_does_not_yield() {
        let settings = nested("Stop", None, "sh /repo/.dira/hook.sh claude");
        assert!(!decide(&settings, "claude", "Stop", exists));
    }

    #[test]
    fn no_hooks_object_does_not_yield() {
        assert!(!decide(&json!({}), "claude", "Stop", exists));
    }

    // --- should_yield: the marker alone is never enough (DIRASH-0037) -----

    #[test]
    fn marker_plus_live_user_scope_plus_project_wrapper_yields() {
        assert!(should_yield(true, true));
    }

    #[test]
    fn marker_plus_live_user_scope_without_project_wrapper_forwards() {
        // A stray `DIRA_HOOK_VIA=portable` (e.g. leaked into a shell
        // profile) on a direct invocation must not be enough on its own to
        // yield, even when a live user-scope entry exists — there is no
        // real portable delivery to yield to.
        assert!(!should_yield(true, false));
    }

    #[test]
    fn project_wrapper_without_live_user_scope_forwards() {
        assert!(!should_yield(false, true));
    }

    #[test]
    fn neither_signal_forwards() {
        assert!(!should_yield(false, false));
    }
}
