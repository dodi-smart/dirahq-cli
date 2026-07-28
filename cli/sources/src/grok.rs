//! grok-build (xAI) hook normalization.
//!
//! grok-build invokes hook commands with a JSON envelope on stdin, but unlike
//! Claude Code/Cursor its field names are **camelCase**: `hookEventName`,
//! `sessionId`, `cwd`, `workspaceRoot`, `timestamp`, `transcriptPath`, plus
//! flattened event-specific fields like `toolName`, `toolUseId`, `toolInput`,
//! `toolResult`. The `hookEventName` *values* are, confusingly, snake_case
//! strings (`user_prompt_submit`, `pre_tool_use`, …) rather than camelCase.
//!
//! Compat gotcha: grok-build can replay hooks configured for other harnesses
//! (`~/.claude/settings.json`, `~/.cursor/hooks.json`), so a grok payload may
//! reach the claude/cursor sources too. Those sources look for their own field
//! names (`hook_event_name`, `conversation_id`, …), which a grok envelope
//! doesn't carry, so it maps to no event there and is safely ignored — see the
//! cross-source test in `lib.rs`.
//!
//! Only metadata is read — never prompt text, tool arguments, or outputs.

use crate::{HarnessSource, Normalized};
use dira_contract::Harness;
use dira_core::model::EventKind;
use serde::Deserialize;

/// The subset of a grok-build hook payload we read. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokHook {
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Absolute path to the session's ACP `updates.jsonl`, when present.
    /// Forwarded as-is — see [`normalize`].
    #[serde(default)]
    pub transcript_path: Option<String>,
}

/// Map a grok-build `hookEventName` value to our [`EventKind`].
fn map_event(name: &str) -> Option<EventKind> {
    Some(match name {
        "session_start" => EventKind::SessionStart,
        "session_end" => EventKind::SessionEnd,
        "user_prompt_submit" => EventKind::UserPrompt,
        "pre_tool_use" => EventKind::PreTool,
        "post_tool_use" | "post_tool_use_failure" => EventKind::PostTool,
        "stop" | "stop_failure" | "subagent_stop" => EventKind::Stop,
        "permission_denied" | "notification" => EventKind::PermissionDecision,
        _ => return None,
    })
}

/// Map a grok-build hook to a normalized event. Returns `None` for unmapped names.
pub fn normalize(hook: &GrokHook) -> Option<Normalized> {
    let kind = map_event(hook.hook_event_name.as_deref()?)?;
    Some(Normalized {
        session_id: hook
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        kind,
        cwd: hook.cwd.clone().or_else(|| hook.workspace_root.clone()),
        tool: hook.tool_name.clone(),
        // grok's transcriptPath points at its ACP `updates.jsonl`, whose
        // `turn_completed` records are parsed by
        // `dira_core::tokens::parse_grok_updates_usage` in the daemon's
        // token capture — see `cli/dirad/src/writer.rs::capture_tokens`.
        transcript_path: hook.transcript_path.clone(),
    })
}

/// The grok-build source.
pub struct GrokSource;

impl HarnessSource for GrokSource {
    fn harness(&self) -> Harness {
        Harness::Grok
    }
    fn id(&self) -> &'static str {
        "grok"
    }
    fn normalize(&self, payload: serde_json::Value) -> Option<Normalized> {
        let hook: GrokHook = serde_json::from_value(payload).ok()?;
        normalize(&hook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_user_prompt_from_camel_case() {
        let hook: GrokHook = serde_json::from_str(
            r#"{"hookEventName":"user_prompt_submit","sessionId":"abc","cwd":"/repo"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.session_id, "abc");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn maps_tool_events_with_name() {
        let pre: GrokHook = serde_json::from_str(
            r#"{"hookEventName":"pre_tool_use","sessionId":"abc","toolName":"Bash"}"#,
        )
        .unwrap();
        let n = normalize(&pre).unwrap();
        assert_eq!(n.kind, EventKind::PreTool);
        assert_eq!(n.tool.as_deref(), Some("Bash"));

        let post_fail: GrokHook = serde_json::from_str(
            r#"{"hookEventName":"post_tool_use_failure","sessionId":"abc","toolName":"Bash"}"#,
        )
        .unwrap();
        assert_eq!(normalize(&post_fail).unwrap().kind, EventKind::PostTool);
    }

    #[test]
    fn session_lifecycle_and_stop_map() {
        for (name, want) in [
            ("session_start", EventKind::SessionStart),
            ("session_end", EventKind::SessionEnd),
            ("stop", EventKind::Stop),
            ("stop_failure", EventKind::Stop),
            ("subagent_stop", EventKind::Stop),
        ] {
            let hook: GrokHook = serde_json::from_value(
                serde_json::json!({"hookEventName": name, "sessionId": "c"}),
            )
            .unwrap();
            assert_eq!(normalize(&hook).unwrap().kind, want, "{name}");
        }
    }

    #[test]
    fn permission_denied_and_notification_map_to_permission_decision() {
        for name in ["permission_denied", "notification"] {
            let hook: GrokHook = serde_json::from_value(
                serde_json::json!({"hookEventName": name, "sessionId": "c"}),
            )
            .unwrap();
            assert_eq!(
                normalize(&hook).unwrap().kind,
                EventKind::PermissionDecision,
                "{name}"
            );
        }
    }

    #[test]
    fn cwd_falls_back_to_workspace_root() {
        let hook: GrokHook = serde_json::from_str(
            r#"{"hookEventName":"user_prompt_submit","sessionId":"c","workspaceRoot":"/ws"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.cwd.as_deref(), Some("/ws"));
    }

    #[test]
    fn transcript_path_is_forwarded() {
        let hook: GrokHook = serde_json::from_str(
            r#"{"hookEventName":"session_start","sessionId":"c","transcriptPath":"/tmp/updates.jsonl"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.transcript_path.as_deref(), Some("/tmp/updates.jsonl"));
    }

    #[test]
    fn unmapped_events_are_ignored() {
        for name in ["subagent_start", "pre_compact", "post_compact"] {
            let hook: GrokHook = serde_json::from_value(
                serde_json::json!({"hookEventName": name, "sessionId": "c"}),
            )
            .unwrap();
            assert!(normalize(&hook).is_none(), "{name} should be ignored");
        }
    }

    #[test]
    fn claude_shaped_snake_case_payload_is_ignored() {
        let hook: GrokHook =
            serde_json::from_str(r#"{"hook_event_name":"UserPromptSubmit","session_id":"x"}"#)
                .unwrap();
        assert!(normalize(&hook).is_none());
    }
}
