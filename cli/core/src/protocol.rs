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
    pub started_at: String,
    pub human_seconds: i64,
    pub agent_seconds: i64,
    pub idle: bool,
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
