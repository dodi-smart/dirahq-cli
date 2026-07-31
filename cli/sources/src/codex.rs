//! Codex hook normalization.
//!
//! Codex ships a hooks system nearly 1:1 with Claude Code: each hook is a stdin
//! JSON object with snake_case `session_id`, `cwd`, `hook_event_name`,
//! `tool_name`, and `transcript_path`. We read only that metadata.
//!
//! There is deliberately **no SessionEnd hook** in Codex — we do not synthesize
//! one here; the daemon's idle machinery closes a session that goes quiet.
//!
//! The hook event names (`SessionStart`, `Stop`/`SubagentStop`, `UserPromptSubmit`,
//! `PreToolUse`, `PostToolUse`, `PermissionRequest`, plus the ignored
//! `Pre/PostCompact`/`SubagentStart`) are confirmed against Codex's documented
//! hooks system (stdin JSON, snake_case fields).
//!
//! The legacy `notify` mechanism is a separate, narrower channel: it emits exactly
//! one notification, `type: "agent-turn-complete"`, as a kebab-case JSON object
//! with `thread-id`. (Note: real Codex `notify` delivers that JSON as an argv
//! argument, not on stdin, so the stdin→socket shim only ever sees the hooks
//! shape; we still accept the notify shape here for completeness / HTTP posters.)

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

/// The alternate kebab-case `notify` shape. The notification kind is tagged by a
/// `type` field; the only kind Codex emits is `agent-turn-complete`. Carries
/// `thread-id` (the session id) and `cwd`.
#[derive(Debug, Clone, Deserialize)]
struct CodexNotify {
    #[serde(rename = "thread-id", default)]
    thread_id: Option<String>,
    /// The notify kind, e.g. `agent-turn-complete`.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
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
        source: None,
        reason: None,
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

/// Map a `notify` kebab-case kind to our [`EventKind`]. Codex emits only
/// `agent-turn-complete` (the agent finished a turn and is waiting on the human).
fn map_notify(name: &str) -> Option<EventKind> {
    match name {
        "agent-turn-complete" => Some(EventKind::Stop),
        _ => None,
    }
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
        // Fallback: the kebab-case notify shape.
        let n: CodexNotify = serde_json::from_value(payload).ok()?;
        let kind = map_notify(n.kind.as_deref()?)?;
        Some(Normalized {
            session_id: n.thread_id.unwrap_or_else(|| "unknown".to_string()),
            kind,
            cwd: n.cwd,
            tool: None,
            transcript_path: None,
            source: None,
            reason: None,
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
            "type": "agent-turn-complete",
            "thread-id": "t-1",
            "cwd": "/repo"
        });
        let n = src.normalize(payload).unwrap();
        assert_eq!(n.kind, EventKind::Stop);
        assert_eq!(n.session_id, "t-1");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn notify_only_maps_agent_turn_complete() {
        // The fictional notify kinds we used to map no longer resolve.
        for name in ["session-start", "user-prompt", "approval-requested"] {
            assert!(map_notify(name).is_none(), "{name} should not map");
        }
        assert_eq!(map_notify("agent-turn-complete"), Some(EventKind::Stop));
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
