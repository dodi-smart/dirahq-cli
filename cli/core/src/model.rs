//! The internal event model. Every harness hook and manual command is normalized
//! into a [`RawEvent`] at ingress and appended to the event log before any
//! further processing. The event log is the source of truth; all derived state
//! (sessions, intervals) can be rebuilt by replaying it.

use dira_contract::Harness;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A normalized capture event. Content (prompts, diffs, file bodies) is **never**
/// part of this struct — redaction happens at ingress, before the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    /// ULID, monotonic; also the event-log primary key.
    pub id: String,
    /// When the daemon received the event (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Stable session id: the harness's own id, or a daemon ULID for manual sessions.
    pub session_id: String,
    pub harness: Harness,
    pub kind: EventKind,
    /// Working directory reported by the harness, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Canonical repo ref resolved from `cwd` (`github.com/org/repo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// `git config user.email` resolved for the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_email: Option<String>,
    /// Session branch (`git rev-parse --abbrev-ref HEAD`) at capture time, or
    /// `None` on a detached HEAD / non-git dir. Used to set the session branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Tool name for `PreTool`/`PostTool` (e.g. `Bash`, `Edit`). Never tool args.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Optional label for a manual session (`meeting`, `manual-testing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional activity classification, surfaced on intervals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// Optional free-text description for a manual session (`--note`/comment),
    /// surfaced on the session rollup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What happened. Split into human signals, agent activity, and lifecycle so the
/// accounting engine can treat them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    // --- lifecycle ---
    /// A harness session began.
    SessionStart,
    /// A harness session ended.
    SessionEnd,
    /// A manual `dira start` opened a session.
    ManualStart,
    /// A manual `dira stop` closed a session.
    ManualStop,

    // --- human signals (drive human-engaged time) ---
    /// The human submitted a prompt.
    UserPrompt,
    /// The human approved or denied a permission request.
    PermissionDecision,
    /// A manual keep-alive tick (the CLI/daemon emits these while a manual dira runs).
    ManualTick,

    // --- agent activity (drives agent wall-clock) ---
    /// A tool call is about to run.
    PreTool,
    /// A tool call finished.
    PostTool,
    /// The agent finished a turn and is waiting on the human.
    Stop,

    /// The working directory changed; triggers project re-resolution.
    CwdChanged,
}

impl EventKind {
    /// Does this event count as a *human* engagement signal? These drive
    /// de-duplicated human-engaged time. `ManualStop` is included as a terminal
    /// signal so a manual dira accrues right up to the moment it's stopped.
    pub fn is_human_signal(self) -> bool {
        matches!(
            self,
            EventKind::UserPrompt
                | EventKind::PermissionDecision
                | EventKind::ManualStart
                | EventKind::ManualTick
                | EventKind::ManualStop
        )
    }

    /// Does this event indicate the agent is doing work (wall-clock evidence)?
    pub fn is_agent_activity(self) -> bool {
        matches!(
            self,
            EventKind::PreTool | EventKind::PostTool | EventKind::Stop
        )
    }
}
