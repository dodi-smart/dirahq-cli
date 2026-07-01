//! The control protocol spoken over the Unix domain socket between the thin
//! `dira` CLI and the resident `dirad` daemon.
//!
//! Framing (both directions): a 4-byte big-endian length prefix followed by that
//! many bytes of JSON. One request, one response, per connection.

use crate::report::Report;
use serde::{Deserialize, Serialize};

/// A command from the CLI to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Active sessions + today's per-project rollup.
    Status,
    /// List active + recent sessions.
    Sessions,
    /// Open a manual session.
    Start {
        project: Option<String>,
        label: Option<String>,
        activity: Option<String>,
        /// Free-text description for the manual session (`--note`).
        note: Option<String>,
        /// Working dir to resolve a project from when `project` is omitted.
        cwd: Option<String>,
    },
    /// Stop one or more manual sessions.
    Stop { selector: StopSelector },
    /// Retroactive manual entry.
    Log {
        duration_secs: u64,
        project: Option<String>,
        note: Option<String>,
        /// Activity classification (`--activity`), e.g. "meeting".
        activity: Option<String>,
        /// Operational tag (`--label`).
        label: Option<String>,
        cwd: Option<String>,
    },
    /// Local report for a scope.
    Report { scope: ReportScope },
    /// Forward a raw harness hook payload (from the `dira hook <harness>` shim)
    /// for normalization + accounting. Permission-gated by the socket itself, so
    /// no bearer is needed on this path.
    IngestHook {
        harness: String,
        payload: serde_json::Value,
    },
    /// Wipe all local statistics (events + token usage) for a fresh start. Keeps
    /// device identity; resets the sync cursor and the live-session registry.
    Nuke,
    /// Build + runtime info for the running daemon (`dira version`).
    DaemonInfo,
    /// Manual escape hatch (`dira device resync`): rewind the sync cursor so the
    /// daemon re-sends events to the cloud, then trigger a flush. `from = None`
    /// rewinds to the beginning (full re-send); `Some(id)` rewinds to that event
    /// id. Safe — the cloud dedups (no double counting).
    ResyncCursor { from: Option<String> },
}

/// Which manual session(s) to stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum StopSelector {
    /// A specific short handle returned by `start`.
    Handle { handle: String },
    /// All manual sessions sharing a label.
    Label { label: String },
    /// Every manual session.
    All,
    /// The single open manual session; ambiguous if several are open.
    Auto,
}

/// Reporting window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ReportScope {
    Today,
    Week,
    All,
    Project { project: String },
}

/// The daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Generic success with no payload.
    Ok,
    /// Failure with a human-readable message.
    Error { message: String },
    /// Daemon is alive (`Ping`).
    Pong,
    /// `Status`.
    Status(StatusView),
    /// `Sessions`.
    Sessions { sessions: Vec<SessionView> },
    /// `Start`.
    Started {
        handle: String,
        project: Option<String>,
    },
    /// `Stop`.
    Stopped { count: usize },
    /// `Log`.
    Logged { handle: String },
    /// `Report`.
    Report(Report),
    /// `Nuke`: how many rows were wiped from each stats table.
    Nuked { events: u64, tokens: u64 },
    /// `ResyncCursor`: the cursor was rewound and a flush was triggered; `pending`
    /// is how many events will now re-sync, from `from` (None = the beginning).
    ResyncQueued { pending: u64, from: Option<String> },
    /// `DaemonInfo`: the running daemon's build + runtime info.
    DaemonInfo {
        /// Daemon binary version (`CARGO_PKG_VERSION`).
        version: String,
        /// Wire contract schema version the daemon speaks.
        schema_version: String,
        /// Daemon process id.
        pid: u32,
        /// Seconds since the daemon started.
        uptime_seconds: u64,
    },
}

/// A live or recent session as shown by `status` / `sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    /// Short handle for manual sessions; harness id otherwise.
    pub handle: String,
    pub session_id: String,
    pub harness: String,
    pub kind: String,
    pub project: Option<String>,
    pub label: Option<String>,
    /// Activity classification + free-text note for manual sessions (display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub started_at: String,
    pub human_seconds: i64,
    pub agent_seconds: i64,
    /// `true` when no *human* signal (prompt / permission / manual tick) has
    /// arrived within the idle window — i.e. you are not currently driving this
    /// session. This is the human-engagement basis and is deliberately blind to
    /// agent activity: a session whose agent is churning away with no recent
    /// prompt is still `idle` here (but see [`SessionView::agent_active`]).
    pub idle: bool,
    /// `true` when this session saw activity of *any* kind — the agent's own tool
    /// calls included — within the idle window. Distinct from `idle`: it lets the
    /// live view surface an agent working on its own as `active` rather than
    /// misreporting it as `idle`, matching the cloud's activity-based "Right Now".
    /// Defaulted so a newer CLI stays wire-compatible with an older daemon (which
    /// omits it → `false`, i.e. degrades to the prior engaged/idle-only view).
    #[serde(default)]
    pub agent_active: bool,
    /// RFC3339 timestamp of this session's last event of any kind, if live. Lets
    /// the `watch` dashboard grow the agent timer by `now - last_activity_at`
    /// (clamped to idle) between polls so it ticks smoothly. Display-only;
    /// `human_seconds`/`agent_seconds` remain the settled snapshot values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
    /// RFC3339 timestamp of this session's last human signal, if any — the live
    /// engagement tail for the human timer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_human_at: Option<String>,
}

/// The live engagement state of a session for the STATE column, with precedence
/// `engaged > active > idle`. "Engaged" means *you* signalled it recently;
/// "active" means its agent is working but you haven't; "idle" means neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    /// A human signal arrived within the idle window — you are driving it.
    Engaged,
    /// No recent human signal, but the agent has been active within the window.
    Active,
    /// Nothing — human or agent — within the idle window.
    Idle,
}

impl SessionView {
    /// Fold [`SessionView::idle`] and [`SessionView::agent_active`] into the
    /// three-way [`LiveState`] the renderers display. Human engagement wins over
    /// agent activity, so a session you just prompted reads `Engaged` even while
    /// its agent runs.
    pub fn live_state(&self) -> LiveState {
        if !self.idle {
            LiveState::Engaged
        } else if self.agent_active {
            LiveState::Active
        } else {
            LiveState::Idle
        }
    }
}

/// `status` payload: what's live plus today's rollup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusView {
    pub active: Vec<SessionView>,
    pub today: Report,
    pub sync_pending: u64,
    /// `true` while the daemon is still replaying the recent log into its live
    /// registry at startup. The control socket answers immediately (before
    /// hydration), so a status issued during warm-up returns a valid but possibly
    /// sparse view; the CLI can surface "warming up" rather than implying zero
    /// activity. Defaulted so older CLIs stay wire-compatible.
    #[serde(default)]
    pub hydrating: bool,
}

/// True when the operator is in the loop — at least one of these sessions has a
/// recent *human* signal (`LiveState::Engaged`). Drives the "and you" marker in
/// the CLI renderers, mirroring the cloud's engaged badge. Deliberately keyed off
/// human engagement, not agent activity: an agent working alone is `active` but
/// does not put *you* in the loop.
pub fn any_engaged(sessions: &[SessionView]) -> bool {
    sessions
        .iter()
        .any(|s| s.live_state() == LiveState::Engaged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(idle: bool, agent_active: bool) -> SessionView {
        SessionView {
            handle: "h".into(),
            session_id: "s".into(),
            harness: "claude".into(),
            kind: "agent".into(),
            project: None,
            label: None,
            activity: None,
            note: None,
            started_at: "now".into(),
            human_seconds: 0,
            agent_seconds: 0,
            idle,
            agent_active,
            last_activity_at: None,
            last_human_at: None,
        }
    }

    #[test]
    fn live_state_prefers_engaged_then_active_then_idle() {
        // Recent human signal → engaged, regardless of agent activity.
        assert_eq!(view(false, false).live_state(), LiveState::Engaged);
        assert_eq!(view(false, true).live_state(), LiveState::Engaged);
        // No recent human signal but the agent is churning → active.
        assert_eq!(view(true, true).live_state(), LiveState::Active);
        // Nothing recent → idle.
        assert_eq!(view(true, false).live_state(), LiveState::Idle);
    }

    #[test]
    fn any_engaged_tracks_human_not_agent() {
        // An agent working alone (active, not engaged) does not put you in the loop.
        assert!(!any_engaged(&[view(true, true)]));
        // A recent human signal does.
        assert!(any_engaged(&[view(true, true), view(false, false)]));
    }

    #[test]
    fn agent_active_defaults_false_from_older_daemon() {
        // An older daemon omits `agent_active`; it must deserialize to `false` so a
        // busy-agent session degrades to the prior engaged/idle-only view.
        let json = r#"{
            "handle":"h","session_id":"s","harness":"claude","kind":"agent",
            "project":null,"started_at":"now","human_seconds":0,"agent_seconds":0,
            "idle":true
        }"#;
        let v: SessionView = serde_json::from_str(json).unwrap();
        assert!(!v.agent_active);
        assert_eq!(v.live_state(), LiveState::Idle);
    }
}
