//! Contract 2.0 session stats derived from the stored event log (DIRASH-0052).
//!
//! Only what the event log already holds is measured here: the tool name of
//! each `PreTool` (bucketed into a category, never sent as a name), permission
//! requests, and the time from a permission request to the next activity. Files
//! touched, line counts, compactions and test runs need data the log does not
//! carry yet, so they stay `None` (omitted on the wire, never zero-filled).

use crate::model::{EventKind, RawEvent};
use dira_contract::{SessionStats, ToolCalls};
use time::OffsetDateTime;

/// A permission request with no follow-up for longer than this is a walk-away,
/// not a wait for approval, and is capped here.
const MAX_APPROVAL_WAIT_SECS: i64 = 30 * 60;

/// The category a tool name falls in. Names vary by harness (Claude Code's
/// `Read`, Gemini's `read_file`, Codex's `shell`), so this matches on the
/// lowercase name. MCP tools are `mcp` whatever the server; the server name is
/// never sent.
pub fn tool_category(tool: &str) -> &'static str {
    let t = tool.to_ascii_lowercase();
    if t.starts_with("mcp__") || t.starts_with("mcp_") || t.starts_with("mcp:") {
        return "mcp";
    }
    match t.as_str() {
        "task" | "agent" | "subagent" | "spawn_agent" => "task",
        "read" | "read_file" | "read_many_files" | "notebookread" | "ls" | "list_directory"
        | "view" | "cat" => "read",
        "edit" | "multiedit" | "notebookedit" | "replace" | "apply_patch" | "str_replace"
        | "edit_file" | "search_replace" => "edit",
        "write" | "write_file" | "create_file" => "write",
        "grep"
        | "glob"
        | "search"
        | "search_file_content"
        | "find"
        | "codebase_search"
        | "file_search"
        | "grep_search" => "search",
        "bash" | "bashoutput" | "killshell" | "killbash" | "shell" | "run_shell_command"
        | "run_terminal_cmd" | "exec" | "exec_command" | "local_shell" => "shell",
        "webfetch" | "websearch" | "web_fetch" | "web_search" | "google_web_search" => "web",
        _ => "other",
    }
}

/// Folds one session's events, oldest first, into its [`SessionStats`].
#[derive(Debug, Default)]
pub struct StatsAcc {
    tools: ToolCalls,
    permission_prompts: u32,
    approval_wait_seconds: u64,
    pending_permission: Option<OffsetDateTime>,
}

impl StatsAcc {
    pub fn observe(&mut self, e: &RawEvent) {
        match e.kind {
            EventKind::PermissionDecision => {
                self.permission_prompts += 1;
                // A second request while one is pending keeps the first start.
                self.pending_permission.get_or_insert(e.at);
                return;
            }
            EventKind::PreTool => {
                let t = &mut self.tools;
                let slot = match e.tool.as_deref().map(tool_category).unwrap_or("other") {
                    "read" => &mut t.read,
                    "edit" => &mut t.edit,
                    "write" => &mut t.write,
                    "search" => &mut t.search,
                    "shell" => &mut t.shell,
                    "web" => &mut t.web,
                    "mcp" => &mut t.mcp,
                    "task" => &mut t.task,
                    _ => &mut t.other,
                };
                *slot += 1;
            }
            _ => {}
        }
        if matches!(
            e.kind,
            EventKind::PreTool | EventKind::PostTool | EventKind::UserPrompt
        ) {
            if let Some(since) = self.pending_permission.take() {
                let waited = (e.at - since)
                    .whole_seconds()
                    .clamp(0, MAX_APPROVAL_WAIT_SECS);
                self.approval_wait_seconds += waited as u64;
            }
        }
    }

    pub fn finish(self) -> SessionStats {
        SessionStats {
            subagents: self.tools.task,
            tool_calls: self.tools,
            files_touched: None,
            agent_lines_added: None,
            agent_lines_removed: None,
            compactions: None,
            permission_prompts: self.permission_prompts,
            approval_wait_seconds: self.approval_wait_seconds,
            test_runs: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_contract::Harness;

    fn ev(kind: EventKind, tool: Option<&str>, secs: i64) -> RawEvent {
        RawEvent {
            id: format!("{secs}"),
            at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs),
            session_id: "s".into(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: None,
            identity_email: None,
            branch: None,
            tool: tool.map(str::to_string),
            label: None,
            activity: None,
            note: None,
            spec: None,
            lineage: Default::default(),
        }
    }

    #[test]
    fn categories_cover_each_harnesses_names_and_hide_mcp_servers() {
        assert_eq!(tool_category("Read"), "read");
        assert_eq!(tool_category("read_file"), "read");
        assert_eq!(tool_category("MultiEdit"), "edit");
        assert_eq!(tool_category("apply_patch"), "edit");
        assert_eq!(tool_category("run_shell_command"), "shell");
        assert_eq!(tool_category("mcp__github__create_issue"), "mcp");
        assert_eq!(tool_category("Task"), "task");
        assert_eq!(tool_category("SomethingNew"), "other");
    }

    #[test]
    fn counts_pre_tool_calls_once_and_measures_the_approval_wait() {
        let mut acc = StatsAcc::default();
        for e in [
            ev(EventKind::PreTool, Some("Read"), 0),
            ev(EventKind::PostTool, Some("Read"), 1),
            ev(EventKind::PermissionDecision, None, 10),
            ev(EventKind::PreTool, Some("Bash"), 25),
            ev(EventKind::PostTool, Some("Bash"), 30),
            ev(EventKind::PreTool, Some("Task"), 40),
            ev(EventKind::PermissionDecision, None, 100),
            ev(EventKind::UserPrompt, None, 100 + 3600),
        ] {
            acc.observe(&e);
        }
        let s = acc.finish();
        assert_eq!(
            (s.tool_calls.read, s.tool_calls.shell, s.tool_calls.task),
            (1, 1, 1)
        );
        assert_eq!(s.subagents, 1);
        assert_eq!(s.permission_prompts, 2);
        assert_eq!(
            s.approval_wait_seconds,
            15 + MAX_APPROVAL_WAIT_SECS as u64,
            "a walk-away is capped"
        );
        assert!(s.files_touched.is_none() && s.test_runs.is_none());
    }
}
