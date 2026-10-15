//! Claude Code http-hook normalization.
//!
//! Claude Code POSTs a JSON body per hook with at least `hook_event_name`,
//! `session_id`, and `cwd`; tool events add `tool_name`. We map the event name to
//! our internal [`EventKind`] and drop everything else.

use crate::{HarnessSource, Hints, Normalized};
use dira_contract::Harness;
use dira_core::model::EventKind;
use serde::Deserialize;

/// The Claude Code source: maps Claude's `hook_event_name` payloads.
pub struct ClaudeCodeSource;

impl HarnessSource for ClaudeCodeSource {
    fn harness(&self) -> Harness {
        Harness::ClaudeCode
    }
    fn id(&self) -> &'static str {
        "claude"
    }
    fn normalize(&self, mut payload: serde_json::Value) -> Option<Normalized> {
        // A tool's result can be megabytes (a Read, a Bash log) and only a
        // `start_session` result is ever read, so drop every other one before
        // it is rebuilt as a `Value` on this, the hottest ingest path.
        if let Some(obj) = payload.as_object_mut() {
            if obj.get("tool_name").and_then(|t| t.as_str())
                != Some(dira_core::orchestration::START_SESSION_TOOL)
            {
                obj.remove("tool_response");
            }
        }
        let hook: ClaudeHook = serde_json::from_value(payload).ok()?;
        normalize(&hook)
    }
}

/// The subset of a Claude Code hook payload we read. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeHook {
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Absolute path to the session transcript JSONL. Claude Code includes this
    /// on most hooks; we read it only to extract token usage counts.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// `SessionStart.source` — `startup`, `resume`, `clear`, `compact`, or `fork`.
    #[serde(default)]
    pub source: Option<String>,
    /// `SessionEnd.reason` — `clear`, `resume`, `logout`, `prompt_input_exit`,
    /// `bypass_permissions_disabled`, or `other`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Tool arguments. Read only to derive repo evidence for a cwd that names
    /// no repo (see [`dira_core::orchestration::repo_evidence`]); never kept.
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Tool result. Read only for a `start_session` call, to learn the child
    /// session's host id; never kept.
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    /// Env hints the `dira hook` shim forwarded (the harness never sets this).
    #[serde(default, rename = "dira_env")]
    pub dira_env: Option<DiraEnv>,
}

/// What the hook shim forwards from its own environment.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DiraEnv {
    #[serde(default)]
    pub host_session_id: Option<String>,
}

/// Map a Claude Code hook to a normalized event. Returns `None` for hook names we
/// don't account for (so new/unknown hooks are safely ignored).
pub fn normalize(hook: &ClaudeHook) -> Option<Normalized> {
    let kind = match hook.hook_event_name.as_deref()? {
        "SessionStart" => EventKind::SessionStart,
        "SessionEnd" => EventKind::SessionEnd,
        "UserPromptSubmit" => EventKind::UserPrompt,
        "Stop" | "SubagentStop" => EventKind::Stop,
        "PreToolUse" => EventKind::PreTool,
        "PostToolUse" => EventKind::PostTool,
        // Notifications are emitted when Claude needs the human (e.g. permission);
        // treat as a human-attention signal.
        "PermissionRequest" | "Notification" => EventKind::PermissionDecision,
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
        source: hook.source.clone(),
        reason: hook.reason.clone(),
        hints: Box::new(hints(hook, kind)),
    })
}

/// Orchestration hints for one hook. Evidence is only worth deriving for a
/// tool event, and the child id only from a *finished* `start_session` call.
fn hints(hook: &ClaudeHook, kind: EventKind) -> Hints {
    use dira_core::orchestration as orch;
    let host_session = hook
        .dira_env
        .as_ref()
        .and_then(|e| e.host_session_id.as_deref())
        .and_then(orch::host_session_id);
    let tool = hook.tool_name.as_deref().unwrap_or_default();
    let evidence = match (&hook.tool_input, kind) {
        (Some(input), EventKind::PreTool | EventKind::PostTool) => {
            let cwd = hook.cwd.as_deref().map(std::path::Path::new);
            orch::repo_evidence(tool, input, cwd)
        }
        _ => Vec::new(),
    };
    let spawned = match (&hook.tool_response, kind) {
        (Some(resp), EventKind::PostTool) if tool == orch::START_SESSION_TOOL => {
            orch::spawned_child(resp)
        }
        _ => None,
    };
    Hints {
        host_session,
        evidence,
        spawned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dira doctor --probe` sends a synthetic payload through this exact
    /// mapping. If a rename here stopped it normalizing, the probe would report
    /// "the daemon never stored the event" on every healthy machine — a
    /// diagnostic that always says broken is worse than no diagnostic.
    ///
    /// The specific properties the probe design depends on:
    /// - `UserPrompt` is a *human signal*, so the writer never coalesces it
    ///   (only `PostTool` coalesces) and it is never pruned as degenerate.
    /// - no `transcript_path`, so token capture is never entered and the probe
    ///   cannot touch `token_usage`.
    #[test]
    fn the_capture_probe_payload_still_normalizes() {
        let session_id = format!("{}01JQ", dira_core::model::PROBE_SESSION_PREFIX);
        let payload = dira_core::model::probe_hook_payload(&session_id, "/tmp");
        let (n, harness) =
            crate::normalize_for("claude", payload).expect("the probe payload must normalize");
        assert_eq!(harness, dira_contract::Harness::ClaudeCode);
        assert_eq!(n.session_id, session_id);
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.cwd.as_deref(), Some("/tmp"));
        assert!(n.transcript_path.is_none());
        assert!(n.tool.is_none());
        assert!(dira_core::model::is_probe_session(&n.session_id));
    }

    #[test]
    fn maps_user_prompt() {
        let hook: ClaudeHook = serde_json::from_str(
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
        let hook: ClaudeHook = serde_json::from_str(
            r#"{"hook_event_name":"PreToolUse","session_id":"abc","tool_name":"Bash"}"#,
        )
        .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::PreTool);
        assert_eq!(n.tool.as_deref(), Some("Bash"));
    }

    /// Claude Code sends `source` on SessionStart and `reason` on SessionEnd. Both
    /// used to be dropped on the floor, which is precisely why a launcher spawn and a
    /// real session were indistinguishable by the time they reached the daemon
    /// (issue #74). Nothing accounts on them — they exist to make that class of
    /// problem diagnosable instead of invisible.
    #[test]
    fn carries_session_start_source_and_session_end_reason() {
        let start: ClaudeHook = serde_json::from_str(
            r#"{"hook_event_name":"SessionStart","session_id":"abc","source":"startup"}"#,
        )
        .unwrap();
        let n = normalize(&start).unwrap();
        assert_eq!(n.kind, EventKind::SessionStart);
        assert_eq!(n.source.as_deref(), Some("startup"));
        assert_eq!(n.reason, None);

        let end: ClaudeHook = serde_json::from_str(
            r#"{"hook_event_name":"SessionEnd","session_id":"abc","reason":"clear"}"#,
        )
        .unwrap();
        let n = normalize(&end).unwrap();
        assert_eq!(n.kind, EventKind::SessionEnd);
        assert_eq!(n.reason.as_deref(), Some("clear"));
        assert_eq!(n.source, None);
    }

    /// An older Claude Code (or any other harness) omits both fields entirely —
    /// they must stay absent rather than fail the whole payload.
    #[test]
    fn missing_source_and_reason_are_absent_not_an_error() {
        let hook: ClaudeHook =
            serde_json::from_str(r#"{"hook_event_name":"SessionStart","session_id":"abc"}"#)
                .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.source, None);
        assert_eq!(n.reason, None);
    }

    #[test]
    fn unknown_hook_is_ignored() {
        let hook: ClaudeHook =
            serde_json::from_str(r#"{"hook_event_name":"PreCompact","session_id":"x"}"#).unwrap();
        assert!(normalize(&hook).is_none());
    }

    fn parse(json: &str) -> Normalized {
        normalize(&serde_json::from_str::<ClaudeHook>(json).unwrap()).unwrap()
    }

    #[test]
    fn tool_input_yields_evidence_but_never_survives_normalization() {
        let n = parse(
            r#"{"hook_event_name":"PostToolUse","session_id":"o","cwd":"/scratch",
                "tool_name":"Bash","tool_input":{"command":"cd repos/a && git push"},
                "tool_response":{"stdout":"secret output"}}"#,
        );
        use dira_core::orchestration::Evidence;
        assert_eq!(
            n.hints.evidence,
            vec![Evidence::Path("/scratch/repos/a".into())]
        );
        // Only derived facts cross into `Normalized`: no field holds the command
        // or its output.
        let dbg = format!("{n:?}");
        assert!(!dbg.contains("git push") && !dbg.contains("secret output"));
    }

    #[test]
    fn start_session_response_names_the_spawned_child() {
        let n = parse(
            r#"{"hook_event_name":"PostToolUse","session_id":"o",
                "tool_name":"mcp__ccd_session__start_session","tool_input":{"title":"t"},
                "tool_response":"started local_f42e16b4-7364-4ba7-966e-6d2f2eaa2147"}"#,
        );
        assert_eq!(
            n.hints.spawned.as_deref(),
            Some("local_f42e16b4-7364-4ba7-966e-6d2f2eaa2147")
        );
        // The pre-call hook has no response yet, and must not invent a child.
        let pre = parse(
            r#"{"hook_event_name":"PreToolUse","session_id":"o",
                "tool_name":"mcp__ccd_session__start_session","tool_input":{"title":"t"}}"#,
        );
        assert_eq!(pre.hints.spawned, None);
    }

    #[test]
    fn forwarded_host_session_is_validated() {
        let n = parse(
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"o",
                "dira_env":{"host_session_id":"local_684b6117-0abc-44f3-9ce0-44f092702849"}}"#,
        );
        assert_eq!(
            n.hints.host_session.as_deref(),
            Some("local_684b6117-0abc-44f3-9ce0-44f092702849")
        );
        let junk = parse(
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"o",
                "dira_env":{"host_session_id":"not-an-id"}}"#,
        );
        assert_eq!(junk.hints.host_session, None);
        // A payload with no shim hints (old shim, HTTP hook) is still accepted.
        assert_eq!(
            *parse(r#"{"hook_event_name":"Stop","session_id":"o"}"#).hints,
            Hints::default()
        );
    }
}
