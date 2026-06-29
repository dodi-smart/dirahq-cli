//! Codex hook normalization.
//!
//! Codex ships a hooks system nearly 1:1 with Claude Code: each hook is a stdin
//! JSON object with snake_case `session_id`, `cwd`, `hook_event_name`,
//! `tool_name`, and `transcript_path`. We read only that metadata.
//!
//! There is deliberately **no SessionEnd hook** in Codex — we do not synthesize
//! one here; the daemon's idle machinery closes a session that goes quiet.
//!
//! TODO(version-verify): confirmed against Codex's documented hook event names
//! (`SessionStart`, `Stop`/`SubagentStop`, `UserPromptSubmit`, `PreToolUse`,
//! `PostToolUse`, `PermissionRequest`, `Pre/PostCompact`, `SubagentStart`).
//! Re-verify the exact strings against the shipping Codex release before GA.
//!
//! The `codex-notify` variant (kebab-case `thread-id` / `agent-turn-complete` /
//! `approval-requested`) is handled as an alternate deserializer below so both
//! shapes route through the same `codex` source.

use crate::{HarnessSource, Normalized};
use dira_contract::Harness;
use dira_core::model::EventKind;
use serde::Deserialize;

/// The subset of a Codex hook payload we read (snake_case, like Claude Code).
/// Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct CodexHook {
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Absolute path to the session transcript, when Codex provides one. Read
    /// only to extract token usage counts.
    #[serde(default)]
    pub transcript_path: Option<String>,
}

/// The alternate kebab-case `codex-notify` shape. Optional/cheap to support: it
/// carries `thread-id` and a `notification`/event name plus an optional tool.
#[derive(Debug, Clone, Deserialize)]
struct CodexNotify {
    #[serde(rename = "thread-id", default)]
    thread_id: Option<String>,
    /// The notify event name, e.g. `agent-turn-complete`, `approval-requested`.
    #[serde(default)]
    notification: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(rename = "tool-name", default)]
    tool_name: Option<String>,
}

/// Map a Codex hook to a normalized event. Returns `None` for hook names we don't
/// account for (`PreCompact`/`PostCompact`/`SubagentStart`, plus anything new).
pub fn normalize(hook: &CodexHook) -> Option<Normalized> {
    let kind = map_event(hook.hook_event_name.as_deref()?)?;
    Some(Normalized {
        session_id: hook
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        kind,
        cwd: hook.cwd.clone(),
        tool: hook.tool_name.clone(),
        transcript_path: hook.transcript_path.clone(),
    })
}

/// Map a Codex `hook_event_name` to our [`EventKind`].
fn map_event(name: &str) -> Option<EventKind> {
    Some(match name {
        "SessionStart" => EventKind::SessionStart,
        "Stop" | "SubagentStop" => EventKind::Stop,
        "UserPromptSubmit" => EventKind::UserPrompt,
        "PreToolUse" => EventKind::PreTool,
        "PostToolUse" => EventKind::PostTool,
        "PermissionRequest" => EventKind::PermissionDecision,
        // Ignored: compaction + subagent lifecycle (no SessionEnd exists).
        "PreCompact" | "PostCompact" | "SubagentStart" => return None,
        _ => return None,
    })
}

/// Map a `codex-notify` kebab-case event name to our [`EventKind`].
fn map_notify(name: &str) -> Option<EventKind> {
    Some(match name {
        "session-start" => EventKind::SessionStart,
        "agent-turn-complete" => EventKind::Stop,
        "user-prompt" | "user-prompt-submit" => EventKind::UserPrompt,
        "approval-requested" => EventKind::PermissionDecision,
        _ => return None,
    })
}

/// The Codex source. Accepts both the snake_case hook shape and, as a fallback,
/// the kebab-case `codex-notify` shape.
pub struct CodexSource;

impl HarnessSource for CodexSource {
    fn harness(&self) -> Harness {
        Harness::Codex
    }
    fn id(&self) -> &'static str {
        "codex"
    }
    fn normalize(&self, payload: serde_json::Value) -> Option<Normalized> {
        // Primary: the Claude-Code-shaped hook.
        if let Ok(hook) = serde_json::from_value::<CodexHook>(payload.clone()) {
            if hook.hook_event_name.is_some() {
                return normalize(&hook);
            }
        }
        // Fallback: the kebab-case codex-notify shape.
        let n: CodexNotify = serde_json::from_value(payload).ok()?;
        let kind = map_notify(n.notification.as_deref()?)?;
        Some(Normalized {
            session_id: n.thread_id.unwrap_or_else(|| "unknown".to_string()),
            kind,
            cwd: n.cwd,
            tool: n.tool_name,
            transcript_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_user_prompt() {
        let hook: CodexHook = serde_json::from_str(
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"abc","cwd":"/repo"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.session_id, "abc");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn maps_tool_events_with_name() {
        let hook: CodexHook = serde_json::from_str(
            r#"{"hook_event_name":"PreToolUse","session_id":"abc","tool_name":"Bash"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::PreTool);
        assert_eq!(n.tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn maps_post_tool_and_transcript() {
        let hook: CodexHook = serde_json::from_str(
            r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Edit","transcript_path":"/t.jsonl"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::PostTool);
        assert_eq!(n.transcript_path.as_deref(), Some("/t.jsonl"));
    }

    #[test]
    fn subagent_stop_maps_to_stop() {
        let hook: CodexHook =
            serde_json::from_str(r#"{"hook_event_name":"SubagentStop","session_id":"s"}"#).unwrap();
        assert_eq!(normalize(&hook).unwrap().kind, EventKind::Stop);
    }

    #[test]
    fn permission_request_maps_to_permission_decision() {
        let hook: CodexHook =
            serde_json::from_str(r#"{"hook_event_name":"PermissionRequest","session_id":"s"}"#)
                .unwrap();
        assert_eq!(
            normalize(&hook).unwrap().kind,
            EventKind::PermissionDecision
        );
    }

    #[test]
    fn compaction_and_subagent_start_are_ignored() {
        for name in ["PreCompact", "PostCompact", "SubagentStart"] {
            let hook: CodexHook = serde_json::from_value(
                serde_json::json!({"hook_event_name": name, "session_id": "s"}),
            )
            .unwrap();
            assert!(normalize(&hook).is_none(), "{name} should be ignored");
        }
    }

    #[test]
    fn no_session_end_hook() {
        // Codex has no SessionEnd; even if a payload claimed one we don't map it.
        let hook: CodexHook =
            serde_json::from_str(r#"{"hook_event_name":"SessionEnd","session_id":"s"}"#).unwrap();
        assert!(normalize(&hook).is_none());
    }

    #[test]
    fn source_handles_codex_notify_shape() {
        let src = CodexSource;
        let payload = serde_json::json!({
            "thread-id": "t-1",
            "notification": "agent-turn-complete",
            "cwd": "/repo"
        });
        let n = src.normalize(payload).unwrap();
        assert_eq!(n.kind, EventKind::Stop);
        assert_eq!(n.session_id, "t-1");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn source_handles_primary_hook_shape() {
        let src = CodexSource;
        let payload = serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "abc"
        });
        let n = src.normalize(payload).unwrap();
        assert_eq!(n.kind, EventKind::SessionStart);
        assert_eq!(n.session_id, "abc");
    }
}
