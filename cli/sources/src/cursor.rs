//! Cursor hook normalization.
//!
//! Cursor (1.7+) ships an agent-lifecycle **Hooks** system: each hook runs an
//! external command that receives a JSON object on stdin and may reply on stdout.
//! We act as a passive observer — read the metadata, forward it, never block the
//! agent (the `dira hook cursor` shim exits 0 immediately).
//!
//! Cursor's payload uses different field names than Claude/Codex/Gemini:
//!   - `conversation_id` is the session identifier (→ `session_id`),
//!   - `workspace_roots` is an array of roots (→ `cwd` = the first one),
//!   - `hook_event_name` values are camelCase (`beforeSubmitPrompt`, `stop`, …).
//!
//! We map both the stable core hooks (`beforeSubmitPrompt`, `beforeShellExecution`,
//! `afterFileEdit`, `stop`) and the expanded set (`sessionStart`/`sessionEnd`/
//! `preToolUse`/`postToolUse`/`afterShellExecution`); unmapped names are ignored.
//!
//! There is no clean cross-tool approve/deny event in Cursor (permission gating is
//! only exposed for shell/MCP execution), so we deliberately emit no
//! `PermissionDecision` for Cursor.

use crate::{HarnessSource, Normalized};
use dira_contract::Harness;
use dira_core::model::EventKind;
use serde::Deserialize;

/// The subset of a Cursor hook payload we read. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct CursorHook {
    #[serde(default)]
    pub hook_event_name: Option<String>,
    /// Cursor's session identifier.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Workspace roots; we attribute to the first.
    #[serde(default)]
    pub workspace_roots: Option<Vec<String>>,
    /// Tool name on generic tool hooks (`preToolUse`/`postToolUse`).
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Per-hook working directory on shell hooks, when present.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Path to the session transcript, when Cursor provides one (often null).
    #[serde(default)]
    pub transcript_path: Option<String>,
}

/// Map a Cursor `hook_event_name` to our [`EventKind`].
fn map_event(name: &str) -> Option<EventKind> {
    Some(match name {
        "sessionStart" => EventKind::SessionStart,
        "sessionEnd" => EventKind::SessionEnd,
        "beforeSubmitPrompt" => EventKind::UserPrompt,
        "beforeShellExecution" | "preToolUse" | "beforeMCPExecution" | "beforeReadFile" => {
            EventKind::PreTool
        }
        "afterShellExecution" | "postToolUse" | "afterMCPExecution" | "afterFileEdit" => {
            EventKind::PostTool
        }
        "stop" => EventKind::Stop,
        _ => return None,
    })
}

/// Map a Cursor hook to a normalized event. Returns `None` for unmapped names.
pub fn normalize(hook: &CursorHook) -> Option<Normalized> {
    let kind = map_event(hook.hook_event_name.as_deref()?)?;
    // Prefer the workspace root; fall back to a per-hook cwd when given.
    let cwd = hook
        .workspace_roots
        .as_ref()
        .and_then(|r| r.first().cloned())
        .or_else(|| hook.cwd.clone());
    // Shell hooks carry the command, not a tool_name; label them `shell`.
    let tool = hook.tool_name.clone().or_else(|| {
        matches!(
            hook.hook_event_name.as_deref(),
            Some("beforeShellExecution" | "afterShellExecution")
        )
        .then(|| "shell".to_string())
    });
    Some(Normalized {
        session_id: hook
            .conversation_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        kind,
        cwd,
        tool,
        transcript_path: hook.transcript_path.clone(),
    })
}

/// The Cursor source.
pub struct CursorSource;

impl HarnessSource for CursorSource {
    fn harness(&self) -> Harness {
        Harness::Cursor
    }
    fn id(&self) -> &'static str {
        "cursor"
    }
    fn normalize(&self, payload: serde_json::Value) -> Option<Normalized> {
        let hook: CursorHook = serde_json::from_value(payload).ok()?;
        normalize(&hook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_prompt_and_remaps_session_and_cwd() {
        let hook: CursorHook = serde_json::from_str(
            r#"{"hook_event_name":"beforeSubmitPrompt","conversation_id":"c1","workspace_roots":["/repo","/other"]}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.session_id, "c1");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn shell_execution_maps_to_tool_events_labelled_shell() {
        let pre: CursorHook = serde_json::from_str(
            r#"{"hook_event_name":"beforeShellExecution","conversation_id":"c"}"#,
        )
        .unwrap();
        let n = normalize(&pre).unwrap();
        assert_eq!(n.kind, EventKind::PreTool);
        assert_eq!(n.tool.as_deref(), Some("shell"));

        let post: CursorHook = serde_json::from_str(
            r#"{"hook_event_name":"afterShellExecution","conversation_id":"c"}"#,
        )
        .unwrap();
        assert_eq!(normalize(&post).unwrap().kind, EventKind::PostTool);
    }

    #[test]
    fn generic_tool_hooks_carry_tool_name() {
        let hook: CursorHook = serde_json::from_str(
            r#"{"hook_event_name":"preToolUse","conversation_id":"c","tool_name":"edit_file"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::PreTool);
        assert_eq!(n.tool.as_deref(), Some("edit_file"));
    }

    #[test]
    fn session_lifecycle_and_stop_map() {
        for (name, want) in [
            ("sessionStart", EventKind::SessionStart),
            ("sessionEnd", EventKind::SessionEnd),
            ("stop", EventKind::Stop),
            ("afterFileEdit", EventKind::PostTool),
        ] {
            let hook: CursorHook = serde_json::from_value(
                serde_json::json!({"hook_event_name": name, "conversation_id": "c"}),
            )
            .unwrap();
            assert_eq!(normalize(&hook).unwrap().kind, want, "{name}");
        }
    }

    #[test]
    fn before_read_file_maps_to_pretool() {
        let hook: CursorHook =
            serde_json::from_str(r#"{"hook_event_name":"beforeReadFile","conversation_id":"c"}"#)
                .unwrap();
        assert_eq!(normalize(&hook).unwrap().kind, EventKind::PreTool);
    }

    #[test]
    fn unmapped_events_are_ignored() {
        for name in ["afterAgentThought", "workspaceOpen", "preCompact"] {
            let hook: CursorHook = serde_json::from_value(
                serde_json::json!({"hook_event_name": name, "conversation_id": "c"}),
            )
            .unwrap();
            assert!(normalize(&hook).is_none(), "{name} should be ignored");
        }
    }
}
