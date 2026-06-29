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
pub mod generic;
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
        Box::new(opencode::OpenCodeSource),
        Box::new(generic::GenericSource),
    ]
}

/// Resolve a harness id (with a few friendly aliases) to its source.
fn source_for(id: &str) -> Option<Box<dyn HarnessSource>> {
    let id = id.trim().to_ascii_lowercase();
    let canonical = match id.as_str() {
        "claude" | "claude_code" | "claudecode" | "claude-code" => "claude",
        "codex" | "codex-notify" | "codexnotify" => "codex",
        "opencode" | "open_code" | "open-code" => "opencode",
        "generic" => "generic",
        _ => return None,
    };
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
    fn dispatch_rejects_unknown_harness() {
        assert!(normalize_for("nope", serde_json::json!({})).is_none());
        assert!(!is_known_harness("nope"));
        assert!(is_known_harness("codex"));
    }
}
