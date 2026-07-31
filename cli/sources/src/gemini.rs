//! Gemini CLI hook normalization.
//!
//! Gemini CLI ships a first-class **hooks** system (stable since v0.26.0): each
//! hook is a `type = "command"` shell invocation that receives a single JSON
//! object on stdin, with snake_case `session_id`, `cwd`, `hook_event_name`,
//! `tool_name`, and `transcript_path` — the same wire shape as Codex/Claude. We
//! read only that metadata.
//!
//! Gemini's lifecycle names differ from Claude's: the human prompt is
//! `BeforeAgent`, the turn boundary ("agent waiting on the human") is
//! `AfterAgent`, and tool calls are `BeforeTool`/`AfterTool`. There is no single
//! approve/deny event; the observable permission signal is `Notification` with
//! `notification_type == "ToolPermission"`.

use crate::{HarnessSource, Normalized};
use dira_contract::Harness;
use dira_core::model::EventKind;
use serde::Deserialize;

/// The subset of a Gemini CLI hook payload we read (snake_case, like Codex) plus
/// `notification_type` (present on `Notification` events). Unknown fields ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct GeminiHook {
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Absolute path to the session transcript, when Gemini provides one. Read
    /// only to extract token usage counts.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// On `Notification` events, the kind of notification (e.g. `ToolPermission`).
    #[serde(default)]
    pub notification_type: Option<String>,
}

/// Map a Gemini hook to a normalized event. Returns `None` for hook names we
/// don't account for (`BeforeModel`/`AfterModel`/`BeforeToolSelection`/
/// `PreCompress`, plus anything new) and for non-permission `Notification`s.
pub fn normalize(hook: &GeminiHook) -> Option<Normalized> {
    let kind = match hook.hook_event_name.as_deref()? {
        "SessionStart" => EventKind::SessionStart,
        "SessionEnd" => EventKind::SessionEnd,
        "BeforeAgent" => EventKind::UserPrompt,
        "AfterAgent" => EventKind::Stop,
        "BeforeTool" => EventKind::PreTool,
        "AfterTool" => EventKind::PostTool,
        // The only permission-bearing notification we count; other notifications
        // (and notifications without a type) are ignored.
        "Notification" if hook.notification_type.as_deref() == Some("ToolPermission") => {
            EventKind::PermissionDecision
        }
        _ => return None,
    };
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

/// The Gemini CLI source.
pub struct GeminiSource;

impl HarnessSource for GeminiSource {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn normalize(&self, payload: serde_json::Value) -> Option<Normalized> {
        let hook: GeminiHook = serde_json::from_value(payload).ok()?;
        normalize(&hook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_before_agent_to_user_prompt() {
        let hook: GeminiHook = serde_json::from_str(
            r#"{"hook_event_name":"BeforeAgent","session_id":"abc","cwd":"/repo"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.session_id, "abc");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn maps_after_agent_to_stop() {
        let hook: GeminiHook =
            serde_json::from_str(r#"{"hook_event_name":"AfterAgent","session_id":"s"}"#).unwrap();
        assert_eq!(normalize(&hook).unwrap().kind, EventKind::Stop);
    }

    #[test]
    fn maps_tool_events_with_name() {
        let pre: GeminiHook = serde_json::from_str(
            r#"{"hook_event_name":"BeforeTool","session_id":"s","tool_name":"run_shell_command"}"#,
        )
        .unwrap();
        let n = normalize(&pre).unwrap();
        assert_eq!(n.kind, EventKind::PreTool);
        assert_eq!(n.tool.as_deref(), Some("run_shell_command"));

        let post: GeminiHook = serde_json::from_str(
            r#"{"hook_event_name":"AfterTool","session_id":"s","tool_name":"write_file","transcript_path":"/t.json"}"#,
        )
        .unwrap();
        let n = normalize(&post).unwrap();
        assert_eq!(n.kind, EventKind::PostTool);
        assert_eq!(n.transcript_path.as_deref(), Some("/t.json"));
    }

    #[test]
    fn session_lifecycle_maps() {
        for (name, want) in [
            ("SessionStart", EventKind::SessionStart),
            ("SessionEnd", EventKind::SessionEnd),
        ] {
            let hook: GeminiHook = serde_json::from_value(
                serde_json::json!({"hook_event_name": name, "session_id": "s"}),
            )
            .unwrap();
            assert_eq!(normalize(&hook).unwrap().kind, want, "{name}");
        }
    }

    #[test]
    fn tool_permission_notification_maps_to_permission_decision() {
        let hook: GeminiHook = serde_json::from_str(
            r#"{"hook_event_name":"Notification","session_id":"s","notification_type":"ToolPermission"}"#,
        )
        .unwrap();
        assert_eq!(
            normalize(&hook).unwrap().kind,
            EventKind::PermissionDecision
        );
    }

    #[test]
    fn other_notifications_are_ignored() {
        // A notification without the ToolPermission type is not a permission signal.
        let hook: GeminiHook = serde_json::from_str(
            r#"{"hook_event_name":"Notification","session_id":"s","notification_type":"Info"}"#,
        )
        .unwrap();
        assert!(normalize(&hook).is_none());
        let bare: GeminiHook =
            serde_json::from_str(r#"{"hook_event_name":"Notification","session_id":"s"}"#).unwrap();
        assert!(normalize(&bare).is_none());
    }

    #[test]
    fn unaccounted_events_are_ignored() {
        for name in [
            "BeforeModel",
            "AfterModel",
            "BeforeToolSelection",
            "PreCompress",
        ] {
            let hook: GeminiHook = serde_json::from_value(
                serde_json::json!({"hook_event_name": name, "session_id": "s"}),
            )
            .unwrap();
            assert!(normalize(&hook).is_none(), "{name} should be ignored");
        }
    }
}
