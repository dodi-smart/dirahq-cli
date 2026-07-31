//! Harness normalization. Each AI coding harness emits hooks in its own shape;
//! this crate maps them all into Dira's [`dira_core::model::EventKind`] so the
//! daemon's accounting never has to know which harness it's talking to.
//!
//! Only metadata is read — `session_id`, `cwd`, `tool_name`, the event name.
//! Prompt text, tool arguments, and outputs are deliberately ignored.
//!
//! Sources are pluggable: each implements [`HarnessSource`] and is registered in
//! [`registry`]. The daemon dispatches a raw payload to a source by its string id
//! (the `:harness` path param on the HTTP ingress, or the `dira hook <harness>`
//! arg) via [`normalize_for`].

pub mod claude_code;
pub mod codex;
pub mod cursor;
pub mod gemini;
pub mod generic;
pub mod grok;
pub mod opencode;

use dira_contract::Harness;
use dira_core::model::EventKind;

/// A harness event after normalization, before the daemon enriches it with a
/// resolved project, identity, timestamp, and id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub session_id: String,
    pub kind: EventKind,
    pub cwd: Option<String>,
    /// Tool name for tool events (e.g. `Bash`, `Edit`); never tool arguments.
    pub tool: Option<String>,
    /// Path to the harness transcript, when the hook provides one. Used to
    /// capture token usage off the hot path; never read for prompt content.
    pub transcript_path: Option<String>,
    /// Why a session *began*, when the harness says so (Claude Code's
    /// `SessionStart.source`: `startup` / `resume` / `clear` / `compact` / `fork`).
    /// Diagnostics only — nothing accounts on it. Without it a launcher spawn and a
    /// real session are indistinguishable by the time they reach the daemon, which
    /// is what made issue #74 invisible for so long.
    pub source: Option<String>,
    /// Why a session *ended*, when the harness says so (Claude Code's
    /// `SessionEnd.reason`: `clear` / `resume` / `logout` / `prompt_input_exit` /
    /// `bypass_permissions_disabled` / `other`). Diagnostics only, as above.
    pub reason: Option<String>,
}

/// A pluggable harness ingress. Each source knows how to turn one harness's raw
/// hook payload into a harness-agnostic [`Normalized`] event.
pub trait HarnessSource: Send + Sync {
    /// The contract harness this source produces.
    fn harness(&self) -> Harness;
    /// The stable wire id this source answers to (e.g. `claude`, `codex`). The
    /// HTTP route `/hooks/{harness}` and `dira hook <harness>` match on this.
    fn id(&self) -> &'static str;
    /// Map a raw JSON payload to a normalized event, or `None` to safely ignore
    /// (unknown/uncounted hook). A malformed payload also yields `None`.
    fn normalize(&self, payload: serde_json::Value) -> Option<Normalized>;
}

/// All registered sources. Aliases (extra accepted ids) are matched in
/// [`source_for`]; this list is the canonical set.
pub fn registry() -> Vec<Box<dyn HarnessSource>> {
    vec![
        Box::new(claude_code::ClaudeCodeSource),
        Box::new(codex::CodexSource),
        Box::new(gemini::GeminiSource),
        Box::new(cursor::CursorSource),
        Box::new(grok::GrokSource),
        Box::new(opencode::OpenCodeSource),
        Box::new(generic::GenericSource),
    ]
}

/// Resolve a harness id (with a few friendly aliases) to its canonical wire id
/// (the `id()` a source answers to). The single source of truth for alias spelling
/// — both hook dispatch ([`source_for`]) and `dira init` rely on this so the
/// accepted spellings can never drift between the two.
pub fn canonical_harness_id(id: &str) -> Option<&'static str> {
    Some(match id.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" | "claudecode" | "claude-code" => "claude",
        "codex" | "codex-notify" | "codexnotify" => "codex",
        "gemini" | "gemini_cli" | "geminicli" | "gemini-cli" => "gemini",
        "cursor" => "cursor",
        "grok" | "grok-build" | "grok_build" | "grokbuild" => "grok",
        "opencode" | "open_code" | "open-code" => "opencode",
        "generic" => "generic",
        _ => return None,
    })
}

/// The canonical wire id for a [`Harness`], and the label rendered in the
/// HARNESS column.
///
/// These are the same ids `dira init <harness>`, [`canonical_harness_id`] and the
/// `/hooks/{harness}` route already use, so what a user reads in `dira status` is
/// exactly what they would type. Previously the column printed the Rust `Debug`
/// form (`ClaudeCode`), which no command accepted.
///
/// The match is exhaustive with no `_` arm on purpose: adding a harness fails to
/// compile until it is spelled here.
pub fn harness_id(h: Harness) -> &'static str {
    match h {
        Harness::ClaudeCode => "claude",
        Harness::Codex => "codex",
        Harness::Gemini => "gemini",
        Harness::Cursor => "cursor",
        Harness::OpenCode => "opencode",
        Harness::Grok => "grok",
        Harness::Generic => "generic",
        Harness::Manual => "manual",
    }
}

/// Resolve a harness id (with a few friendly aliases) to its source.
fn source_for(id: &str) -> Option<Box<dyn HarnessSource>> {
    let canonical = canonical_harness_id(id)?;
    registry().into_iter().find(|s| s.id() == canonical)
}

/// Dispatch a raw payload to the source registered under `id`. Returns the
/// normalized event paired with the harness that produced it, or `None` when the
/// id is unknown, the payload is malformed, or the hook is one we don't account
/// for. The caller (HTTP route / control socket) treats every `None` as a
/// silent ack so a harness never retries.
pub fn normalize_for(id: &str, payload: serde_json::Value) -> Option<(Normalized, Harness)> {
    let source = source_for(id)?;
    let harness = source.harness();
    let norm = source.normalize(payload)?;
    Some((norm, harness))
}

/// Whether `id` resolves to a known source (so the daemon can distinguish
/// "unknown harness" from "known harness, ignored hook").
pub fn is_known_harness(id: &str) -> bool {
    source_for(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_routes_claude_aliases() {
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "abc",
            "cwd": "/repo"
        });
        for alias in ["claude", "claude_code", "claudecode", "claude-code"] {
            let (n, h) = normalize_for(alias, payload.clone()).expect("known alias");
            assert_eq!(n.kind, EventKind::UserPrompt);
            assert_eq!(h, Harness::ClaudeCode);
        }
    }

    #[test]
    fn dispatch_routes_gemini_aliases() {
        let payload = serde_json::json!({
            "hook_event_name": "BeforeAgent",
            "session_id": "g1",
            "cwd": "/repo"
        });
        for alias in ["gemini", "gemini_cli", "geminicli", "gemini-cli"] {
            let (n, h) = normalize_for(alias, payload.clone()).expect("known alias");
            assert_eq!(n.kind, EventKind::UserPrompt);
            assert_eq!(h, Harness::Gemini);
        }
    }

    #[test]
    fn dispatch_routes_cursor() {
        let payload = serde_json::json!({
            "hook_event_name": "beforeSubmitPrompt",
            "conversation_id": "c1",
            "workspace_roots": ["/repo"]
        });
        let (n, h) = normalize_for("cursor", payload).expect("known harness");
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.session_id, "c1");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
        assert_eq!(h, Harness::Cursor);
    }

    #[test]
    fn dispatch_rejects_unknown_harness() {
        assert!(normalize_for("nope", serde_json::json!({})).is_none());
        assert!(!is_known_harness("nope"));
        assert!(is_known_harness("codex"));
    }

    #[test]
    fn dispatch_routes_grok_aliases() {
        let payload = serde_json::json!({
            "hookEventName": "user_prompt_submit",
            "sessionId": "g1",
            "cwd": "/repo"
        });
        for alias in ["grok", "grok-build", "grok_build", "grokbuild"] {
            let (n, h) = normalize_for(alias, payload.clone()).expect("known alias");
            assert_eq!(n.kind, EventKind::UserPrompt);
            assert_eq!(h, Harness::Grok);
        }
    }

    #[test]
    fn grok_payload_ignored_by_claude_and_cursor_sources() {
        let payload = serde_json::json!({
            "hookEventName": "user_prompt_submit",
            "sessionId": "g1",
            "cwd": "/repo"
        });
        assert!(normalize_for("claude", payload.clone()).is_none());
        assert!(normalize_for("cursor", payload).is_none());
    }
}
