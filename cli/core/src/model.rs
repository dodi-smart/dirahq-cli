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

/// Session-id prefix reserved for `dira doctor --probe`'s end-to-end capture
/// probe.
///
/// Rows carrying it are invisible to every read the store serves (see the
/// `NOT LIKE` guards in `Store`), so a probe row can never be synced, counted,
/// rolled up, or billed — and the daemon refuses to create one at all unless
/// it minted the id itself moments earlier.
///
/// Nothing outside the probe path can produce one: `dira start` mints a bare
/// ULID and every harness session id comes from the harness.
pub const PROBE_SESSION_PREFIX: &str = "dira-probe-";

/// Is this a capture-probe session id?
///
/// Requires a non-empty suffix, so the bare literal `dira-probe-` — or a real
/// session that merely starts the same way — is not mistaken for one.
pub fn is_probe_session(session_id: &str) -> bool {
    session_id.len() > PROBE_SESSION_PREFIX.len() && session_id.starts_with(PROBE_SESSION_PREFIX)
}

/// The SQL `LIKE` pattern matching probe session ids.
///
/// Bound as a parameter, never interpolated. The prefix contains no `LIKE`
/// metacharacter (`%`/`_`), so no `ESCAPE` clause is needed. A `const` rather
/// than a `format!` because it is bound on every filtered read; the test below
/// keeps it from drifting from [`PROBE_SESSION_PREFIX`].
pub const PROBE_LIKE_PATTERN: &str = "dira-probe-%";

/// The synthetic Claude Code hook payload the capture probe sends.
///
/// Metadata only, and deliberately minimal:
/// - `UserPromptSubmit` maps to [`EventKind::UserPrompt`], a *human signal*, so
///   it is never coalesced (only `PostTool` is) and would survive the writer's
///   degenerate-session pruning even without the probe's own short path.
/// - exactly one event, so "did a row land" has an unambiguous answer.
/// - no `transcript_path`, so the writer's token-capture path is never entered
///   and the probe cannot touch `token_usage`.
/// - nothing drawn from the user's machine beyond the temp dir it runs in.
pub fn probe_hook_payload(session_id: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session_id,
        "cwd": cwd,
    })
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

    /// Does this event open a span in which the agent is *definitionally* busy?
    ///
    /// A `PreTool` is emitted immediately before a tool call runs, and no harness
    /// emits anything else until it returns — so the gap following a `PreTool`
    /// **is** the tool call, however long it takes. That is what lets agent
    /// wall-clock credit a two-hour build instead of discarding it as idle
    /// (see [`crate::accounting::agent_active_seconds`]).
    ///
    /// Deliberately keyed on the *opening* event rather than on a matched
    /// `PreTool`/`PostTool` pair: a `PostToolUse` hook lost in transit must not
    /// zero the work it was closing.
    pub fn opens_agent_span(self) -> bool {
        matches!(self, EventKind::PreTool)
    }
}
