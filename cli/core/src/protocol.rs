//! The control protocol spoken over the Unix domain socket between the thin
//! `dira` CLI and the resident `dirad` daemon.
//!
//! Framing (both directions): a 4-byte big-endian length prefix followed by that
//! many bytes of JSON. One request, one response, per connection.

use crate::report::Report;
use dira_contract::{Harness, SessionKind};
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
    /// Forward a raw zavet guard event (from the `dira zavet emit` shim — the
    /// plugin's fire-and-forget channel). Parsed permissively daemon-side so
    /// the event schema can evolve without protocol churn; the daemon resolves
    /// the repo from the payload's `cwd` and never trusts a caller-supplied
    /// repo identity.
    IngestZavet { payload: serde_json::Value },
    /// Zavet activation + capture health for a repo (resolved from `repo`, or
    /// `cwd`, or the daemon's own cwd — same ladder as `Start`).
    ZavetStatus {
        cwd: Option<String>,
        repo: Option<String>,
    },
    /// Answer "why?" from recorded knowledge (`dira zavet why`). `query` is a
    /// decision id (`D-0042`) or free text; free text is searched across
    /// titles, slugs, guards, bodies, and trailers — a single confident hit
    /// answers in full, several return ranked matches.
    ZavetWhy {
        query: String,
        cwd: Option<String>,
        repo: Option<String>,
    },
    /// Browse the knowledge base (`dira zavet wiki`): overview without a
    /// topic, ranked matches with one.
    ZavetWiki {
        topic: Option<String>,
        cwd: Option<String>,
        repo: Option<String>,
    },
    /// List a repo's captured decisions.
    ZavetDecisions {
        cwd: Option<String>,
        repo: Option<String>,
    },
    /// Sweep one repo's knowledge NOW instead of waiting for the idle ticker
    /// (`dira zavet sync`), and register its directory so the ticker keeps
    /// sweeping it.
    ///
    /// Local control protocol only — like [`Self::Shutdown`], this does not
    /// touch the cloud wire contract under `/contract`.
    ///
    /// This closes a latency hole, not a scope one: capture reads decision
    /// records out of git objects, so a sync only picks up what is already
    /// COMMITTED. Records on disk stay reported, never ingested.
    ZavetSync {
        cwd: Option<String>,
        repo: Option<String>,
    },
    /// Rebuild a repo's knowledge index from git history (`dira zavet reindex`).
    ///
    /// The ambient poll only sweeps `COMMIT_BACKFILL_LIMIT` commits on a repo's
    /// first sight and then records a baseline, so a fresh clone indexes only
    /// the decisions and specs that fall in that window, and no later sweep
    /// revisits the rest. This walks the full `.zavet/`-scoped history instead.
    /// Explicit and user-initiated — never the ambient path. `all_trailers`
    /// lifts the bound on the (unscoped, therefore costlier) trailer pass.
    ///
    /// Distinct from [`Self::ZavetSync`], which runs the same bounded capture
    /// the ticker does, just sooner: sync cannot reach behind the baseline,
    /// which is precisely what this exists for.
    ///
    /// Takes no `repo`, unlike its sibling `Zavet*` queries: this one reads git
    /// history off a working tree, so a repo the caller is not standing in has
    /// nothing to walk. The daemon resolves both the toplevel and the canonical
    /// repo from `cwd`.
    ZavetReindex {
        cwd: Option<String>,
        all_trailers: bool,
    },
    /// Set or clear the per-repo zavet override (`dira zavet enable|disable`).
    /// `mode` is `on`, `off`, or `clear`.
    ZavetSetMode {
        cwd: Option<String>,
        repo: Option<String>,
        mode: String,
    },
    /// Ask the daemon to shut down gracefully over the control channel — the
    /// platform-neutral SIGTERM equivalent. Unix already has SIGTERM (and
    /// keeps it; see `dirad::wait_for_shutdown_signal`), but windows has no
    /// signal of that shape at all, so `dira daemon stop` and self-restarts
    /// (`dira update`) need an in-band way to ask the resident daemon to wind
    /// down instead of being hard-killed. This is the internal CLI↔daemon
    /// control protocol — distinct from the cloud wire contract under
    /// `/contract`, which this does not touch.
    Shutdown,
    /// Drive `dira doctor --probe`'s end-to-end capture probe.
    ///
    /// Local control protocol only — like [`Self::Shutdown`], this does not
    /// touch the cloud wire contract under `/contract`, so `just contract`
    /// output is unchanged.
    ///
    /// Two phases over one variant because the daemon must mint and arm the
    /// reserved session id *before* the CLI spawns the hook child: the CLI
    /// never chooses a probe id, and the landing watch is registered before the
    /// row can exist.
    ///
    /// The daemon deliberately does **not** spawn the child itself. It may be
    /// the elevated process, and a child it forks would inherit that token and
    /// open the elevated control channel happily — so the probe would pass on
    /// exactly the machine the bug is on. The spawning process must be
    /// `dira doctor`, running under the user's own ordinary token.
    CaptureProbe { phase: ProbePhase },
}

/// Which half of the capture probe a [`Request::CaptureProbe`] drives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ProbePhase {
    /// Mint a reserved session id, register a landing watch, start the TTL.
    Arm,
    /// Wait up to `wait_ms` for the row, then delete every row for
    /// `session_id` regardless of the outcome, and disarm.
    Verify { session_id: String, wait_ms: u64 },
}

/// The daemon's half of the capture probe.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureProbeView {
    /// `Arm`: the reserved session id the CLI must put in the payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `Verify`: did a row actually reach the store?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed: Option<bool>,
    /// `Verify`: milliseconds from the start of the wait to the row landing,
    /// or to the deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waited_ms: Option<u64>,
    /// `Verify`: rows removed by the reap. 1 on success, 0 otherwise; anything
    /// higher would mean something else wrote under the reserved prefix.
    #[serde(default)]
    pub deleted: u64,
    /// Is the *daemon* elevated? A probe that passes while this is true and the
    /// doctor process is not is worth saying out loud: it works today only
    /// because both sides happen to match.
    #[serde(default)]
    pub daemon_elevated: bool,
    /// The daemon's `control_channel_warning`, repeated here so the probe can
    /// report it without a second round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_channel_warning: Option<String>,
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
    /// `Status`. Boxed (WP-B9): `StatusView` grew past clippy's large-variant
    /// threshold against the small `Ok`/`Pong`/etc. arms once `sync_health`
    /// (WP-B9) joined `writer_health` (WP-B7) — the same fix `tui::Live`/
    /// `PollResult` already applied for the same reason. One allocation per
    /// `status` response, nowhere near a hot path.
    Status(Box<StatusView>),
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
    /// `pending_tokens` is the token-usage backlog the rewind re-sends — its own
    /// number, never folded into `pending`: conflating events and token rows in one
    /// counter is worse than the under-count it replaces. Always 0 for a `--from`
    /// rewind, which leaves the token cursor put by design (D-0018/D-0020).
    ResyncQueued {
        pending: u64,
        #[serde(default)]
        pending_tokens: u64,
        from: Option<String>,
    },
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
        /// Why the loopback hook ingress is not serving, or `None` when it is
        /// healthy. A daemon whose ingress port is taken keeps answering here
        /// but captures nothing, so it must not be indistinguishable from a
        /// healthy one (D-0009). `default` so a newer `dira` can still read an
        /// older daemon's reply during a partial update.
        #[serde(default)]
        http_ingress_error: Option<String>,
        /// Why the *control channel itself* is not in its intended state, or
        /// `None`. Covers the windows pipe's security descriptor falling back to
        /// a weaker rung, and/or the daemon running elevated.
        ///
        /// Same rationale as `http_ingress_error` (D-0009: a daemon that cannot
        /// do its job must never report as plainly healthy) — and the same skew
        /// posture: `default` so a newer `dira` can still read an older daemon.
        #[serde(default)]
        control_channel_warning: Option<String>,
        /// The store the daemon actually opened.
        ///
        /// Reported because the CLI cannot otherwise tell that it and the daemon
        /// resolved DIFFERENT databases — the elevated/service-account case,
        /// where `project_dirs()` succeeds on both sides but lands in two
        /// profiles. No amount of work inside the daemon can detect that; only
        /// comparing the two answers can. `default` for skew: an older daemon
        /// omits it and the CLI simply says nothing.
        #[serde(default)]
        db_path: Option<String>,
        /// Why the store is not anchored to a durable per-user location, or
        /// `None`. Same D-0009 posture as the two warnings above: a daemon
        /// writing a whole capture history into `$TMPDIR` must not report as
        /// plainly healthy.
        #[serde(default)]
        storage_warning: Option<String>,
    },
    /// `ZavetStatus`. Boxed like `Status` to keep small arms small.
    ZavetStatus(Box<ZavetStatusView>),
    /// `ZavetWhy`.
    ZavetWhy(Box<ZavetWhyView>),
    /// `ZavetDecisions`.
    ZavetDecisions(Box<ZavetDecisionsView>),
    /// `ZavetWhy` with an ambiguous free-text query: ranked matches instead
    /// of a single answer. Also `ZavetWiki` with a topic. `trailers` are
    /// matching orphan commit trailers — micro-decisions that never got a
    /// record; they can be the whole answer when `hits` is empty.
    ZavetSearch {
        query: String,
        hits: Vec<ZavetSearchHit>,
        /// Matching living specs, ranked with the same weights.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        specs: Vec<ZavetSpecHit>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        trailers: Vec<ZavetTrailerHit>,
    },
    /// `ZavetWiki` without a topic: the knowledge-base overview.
    ZavetWiki(Box<ZavetWikiView>),
    /// `ZavetWhy` when the confident winner is a living spec. Skew note:
    /// unlike the additive `specs` fields elsewhere, a NEW variant makes an
    /// older `dira` error (not degrade) when a newer daemon answers with a
    /// spec — accepted deliberately: the shapes genuinely differ, both
    /// binaries ship from one workspace, and the failure is a clean error on
    /// a query an old CLI couldn't render anyway.
    ZavetSpec(Box<ZavetSpecWhyView>),
    /// `ZavetSetMode`: the applied override (`on`/`off`) or `clear`.
    ZavetModeSet { repo: String, mode: String },
    /// `ZavetSync`. Boxed like the other `Zavet*` views so the small arms stay
    /// small. A new variant does not degrade across skew, which is harmless
    /// here for the same reason as `ZavetSpec`: only a CLI new enough to send
    /// `ZavetSync` can receive it, and an older daemon answers with an error
    /// the CLI reports as version skew rather than as a failure.
    ZavetSync(Box<ZavetSyncView>),
    /// `ZavetReindex`: what the walk saw and what it actually wrote. Same
    /// new-variant skew posture as `ZavetSpec` — only a CLI new enough to send
    /// `ZavetReindex` can receive it.
    ZavetReindex(Box<ZavetReindexView>),
    /// `CaptureProbe`. Boxed like `Status`/`Zavet*` so the small arms stay small.
    ///
    /// Same new-variant skew posture as `ZavetSpec`, and harmless here: only a
    /// CLI new enough to send `CaptureProbe` can ever receive this.
    CaptureProbe(Box<CaptureProbeView>),
}

/// A live or recent session as shown by `status` / `sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    /// Short handle for manual sessions; harness id otherwise.
    pub handle: String,
    pub session_id: String,
    /// Typed, not stringly. These were `String`s built with `format!("{:?}", …)`,
    /// whose `Debug` form ignores the enums' `rename_all = "snake_case"` — so the
    /// wire carried `"Manual"` while every consumer compared against `"manual"`.
    /// Those comparisons silently never matched, which is what let a manual
    /// session grow phantom agent time. Typing them makes the compiler reject the
    /// whole class of mistake.
    #[serde(deserialize_with = "compat::harness")]
    pub harness: Harness,
    #[serde(deserialize_with = "compat::session_kind")]
    pub kind: SessionKind,
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
    /// `true` when this session has produced at least one agent-activity event
    /// (`PreTool`/`PostTool`/`Stop`) inside the reported window — i.e. the daemon
    /// has real wall-clock evidence for it.
    ///
    /// The **only** gate a renderer may use to grow a live agent tail.
    /// Deliberately not `agent_seconds > 0`: a genuine agent session in its first
    /// second has evidence but no settled seconds yet. Deliberately not `kind`
    /// either — belt and braces, so a session mislabelled by some future path
    /// still cannot accrue agent time it never earned.
    ///
    /// Defaulted (`false`) so an older daemon degrades to "no tail", never to a
    /// phantom one.
    #[serde(default)]
    pub has_agent_activity: bool,
}

/// Lenient deserializers for the two typed enums above.
///
/// Every other field this protocol has gained is `#[serde(default)]`, so a newer
/// `dira` against an older `dirad` degrades rather than fails. A bare typed field
/// would break that: a pre-fix daemon still sends `"Manual"`/`"ClaudeCode"`, and
/// strict parsing would make `dira status` and `dira watch` error outright
/// against it. Accepting both spellings keeps the skew behaviour this protocol
/// has always had.
///
/// The reverse direction needs nothing: an *older* CLI reads these as `String`,
/// receives `"manual"`, and its `== "manual"` comparison — the one it always
/// intended — starts working. The old binary is fixed by the daemon change.
mod compat {
    use super::{Harness, SessionKind};
    use serde::{Deserialize, Deserializer};

    /// `"ClaudeCode"` and `"claude_code"` both normalise to `claudecode`.
    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect()
    }

    pub(super) fn harness<'de, D: Deserializer<'de>>(d: D) -> Result<Harness, D::Error> {
        let raw = String::deserialize(d)?;
        match normalize(&raw).as_str() {
            "claudecode" | "claude" => Ok(Harness::ClaudeCode),
            "codex" => Ok(Harness::Codex),
            "gemini" => Ok(Harness::Gemini),
            "cursor" => Ok(Harness::Cursor),
            "opencode" => Ok(Harness::OpenCode),
            "grok" => Ok(Harness::Grok),
            "generic" => Ok(Harness::Generic),
            "manual" => Ok(Harness::Manual),
            _ => Err(serde::de::Error::custom(format!("unknown harness: {raw}"))),
        }
    }

    pub(super) fn session_kind<'de, D: Deserializer<'de>>(d: D) -> Result<SessionKind, D::Error> {
        let raw = String::deserialize(d)?;
        match normalize(&raw).as_str() {
            "agent" => Ok(SessionKind::Agent),
            "manual" => Ok(SessionKind::Manual),
            _ => Err(serde::de::Error::custom(format!(
                "unknown session kind: {raw}"
            ))),
        }
    }
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
    /// A manual `dira start` session rather than a harness one.
    pub fn is_manual(&self) -> bool {
        matches!(self.kind, SessionKind::Manual)
    }

    /// The single gate for growing *displayed* agent time.
    ///
    /// Both conditions matter. `is_manual` is the intent ("a stopwatch a human
    /// started is not agent work"); `has_agent_activity` is the evidence, computed
    /// by the daemon from the same predicate that produces `agent_seconds`. A
    /// renderer that consults either one alone can drift; this is the one place
    /// that decides.
    pub fn accrues_agent_time(&self) -> bool {
        !self.is_manual() && self.has_agent_activity
    }

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
    /// Un-synced `token_usage` rows past the confirmed token cursor. Reported
    /// separately from `sync_pending` because the two backlogs drain on separate
    /// cursors: status could show a full day of local compute alongside
    /// "0 event(s) pending sync" while none of those tokens had ever reached the
    /// cloud, and nothing anywhere revealed the difference. Defaulted so an older
    /// daemon (which cannot report it) reads as zero rather than failing the parse.
    #[serde(default)]
    pub tokens_pending: u64,
    /// `true` while the daemon is still replaying the recent log into its live
    /// registry at startup. The control socket answers immediately (before
    /// hydration), so a status issued during warm-up returns a valid but possibly
    /// sparse view; the CLI can surface "warming up" rather than implying zero
    /// activity. Defaulted so older CLIs stay wire-compatible.
    #[serde(default)]
    pub hydrating: bool,
    /// Today's token totals + local cost estimate, for the status compute row.
    /// `None` from an older daemon (skew) or when the read fails — the renderer
    /// omits the row. Defaulted + omitted-when-None so both directions of
    /// dira↔dirad skew stay wire-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<ComputeView>,
    /// The cloud-computed billable summary (last successful fetch, possibly
    /// stale — see `fetched_at`). `None` when the device is unlinked, offline
    /// since startup with no cache, or the daemon is older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<BillingView>,
    /// Writer-task health (WP-B7): panics caught + dropped at the message
    /// grain, watchdog stall count, and whether it looks wedged right now.
    /// `None` from an older daemon (skew) — the renderer omits the health
    /// line then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_health: Option<WriterHealthView>,
    /// Sync-task health (WP-B9): the last flush attempt's outcome, consecutive
    /// failure count, current backoff, and cursor/watermark, plus process-wide
    /// flush counters — the daemon's own honest view of "is sync actually
    /// working" (a stalled sync and a quiet because-nothing-changed sync both
    /// otherwise look identical from `sync_pending` alone). `None` from an
    /// older daemon (skew) — the renderer omits the health line then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthView>,
}

/// The writer task's self-reported health, attached to `status` (WP-B7). The
/// writer catches and drops any panic tripped by a single message rather than
/// dying, so `panics > 0` is an operator signal worth investigating — not
/// itself an outage. `wedged` is the same definition the watchdog uses: no
/// progress past the stall threshold while messages are backed up.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct WriterHealthView {
    /// Messages whose processing panicked and were dropped since daemon start.
    pub panics: u64,
    /// How many times the watchdog has observed the writer stalled.
    pub stalls: u64,
    /// Seconds since the writer last drained a message, or `None` if it never
    /// has (a fresh, still-hydrating daemon).
    pub idle_secs: Option<i64>,
    /// True when the writer currently looks wedged (no progress past the
    /// stall threshold while messages are backed up).
    pub wedged: bool,
    /// Token turns stored with no repo since daemon start. Repo-less compute is
    /// neither counted nor shown (D-0026), so a nonzero count is usage that has
    /// gone invisible — an operator signal, not an outage. `0` from an older
    /// daemon (issue #93).
    #[serde(default)]
    pub unattributed_token_rows: u64,
}

/// The sync task's self-reported health, attached to `status` (WP-B9). Mirrors
/// `dira_core::sync::SyncHealth` (the persisted snapshot `sync.rs` writes
/// after every flush attempt) plus process-wide flush counters from
/// `ProgressTracker` — defined here rather than reusing the sync-module type
/// directly so the protocol doesn't depend on sync internals; the daemon maps
/// between them. Every field defaults, so an old/short-written snapshot (or a
/// daemon that has never yet attempted a flush) still renders sensibly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncHealthView {
    /// RFC 3339 timestamp of the most recent flush attempt, or `None` before
    /// the first one.
    pub last_attempt_at: Option<String>,
    /// RFC 3339 timestamp of the most recent flush that fully succeeded.
    pub last_success_at: Option<String>,
    /// A stable, short code for the most recent failure kind (e.g.
    /// `"signature_rejected"`, `"unknown_device"`, `"transient"`), or `None`
    /// right after a success / before the first attempt.
    pub last_error_kind: Option<String>,
    /// Consecutive failed flush attempts since the last success.
    pub consecutive_failures: u32,
    /// The backoff the sync loop is currently sleeping (or just slept) for,
    /// in seconds. `0` in steady state.
    pub backoff_secs: u64,
    /// The sync cursor (last confirmed-synced event id) as of the snapshot.
    pub cursor: Option<String>,
    /// The cloud's last-reported persisted watermark, as of the snapshot.
    pub cloud_watermark: Option<String>,
    /// Total flush attempts since daemon start.
    pub flush_attempts: u64,
    /// Total flush attempts that fully succeeded since daemon start.
    pub flush_successes: u64,
    /// Total flush attempts that failed since daemon start.
    pub flush_failures: u64,
}

/// Today's compute totals for the status summary. Defined here (not reusing the
/// store's row type) so the wire protocol never depends on storage shapes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ComputeView {
    /// All tokens through the pipe today: input + output + cache read/create.
    pub total_tokens: u64,
    /// Estimated USD cost from the bundled pricing table. A local estimate and
    /// always a label — never a billing base (that's the cloud's job).
    pub est_cost_usd: f64,
}

/// The cloud's billable rollup as attached to `status`. Mirrors
/// [`crate::sync::BillingSummary`] but is defined here so the protocol doesn't
/// depend on sync types; the daemon maps between them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BillingView {
    /// Engaged hours of billable intervals in the period (raw, not rounded).
    pub billable_hours: f64,
    /// Policy-priced value of those intervals, in `currency`.
    pub unbilled_amount: f64,
    /// Currency symbol from the workspace policy, e.g. `"€"`.
    pub currency: String,
    /// The period the summary covers, e.g. `"week"`.
    pub period: String,
    /// RFC 3339 timestamp of the daemon's successful fetch — lets the renderer
    /// flag a stale value ("as of 32m ago") instead of presenting it as live.
    pub fetched_at: String,
}

/// Zavet activation + capture health for one repo (`dira zavet status`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetStatusView {
    /// Canonical repo the view describes.
    pub repo: String,
    /// The resolved activation verdict.
    pub active: bool,
    /// The global `modules.zavet` knob (`auto`/`on`/`off`).
    pub knob: String,
    /// Per-repo override, if set (`on`/`off`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_mode: Option<String>,
    /// Whether the repo carries `.zavet/` at its toplevel (the `auto` probe);
    /// `None` when the daemon has no working dir for the repo yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zavet_dir: Option<bool>,
    pub decisions_total: u64,
    pub decisions_active: u64,
    pub trailers: u64,
    pub guard_events: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_stats: Vec<ZavetGuardStatView>,
}

/// What one `dira zavet reindex` walk saw and what it actually wrote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetReindexView {
    pub repo: String,
    /// False when the repo is not zavet-active — nothing was walked.
    pub active: bool,
    /// Commits returned by the `.zavet/`-scoped history walk.
    pub commits_scanned: u64,
    /// Commits returned by the trailer walk, and whether its bound was lifted.
    pub trailer_commits_scanned: u64,
    pub trailers_bounded: bool,
    /// Records the walk parsed, split by what the store actually did. `skipped`
    /// counts records whose content hash already matched — the measure of the
    /// command being idempotent rather than re-stamping every row.
    pub decisions_indexed: u64,
    pub decisions_skipped: u64,
    pub specs_indexed: u64,
    pub specs_skipped: u64,
    pub trailer_commits_recorded: u64,
}

/// Per-kind guard-event tallies with the honest unattributed count.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetGuardStatView {
    pub kind: String,
    pub total: u64,
    pub unattributed: u64,
}

/// Whether a captured record's file is on the branch the caller is standing on.
///
/// The store keys knowledge by repo alone, so a decision recorded on another
/// branch keeps listing forever — correct for an append-only knowledge model
/// (ids are minted repo-wide, and a row is never deleted), but it means the
/// list cannot be read as "what governs the code in front of me" unless the
/// distinction is shown. This is a *display* state computed per query; nothing
/// in the store changes.
///
/// The absent case is deliberate and is spelled `Option::None` at every use
/// site: with no working directory to ask git in (`--project <repo>` from
/// elsewhere, or a daemon that has never seen the repo), presence is
/// **unknown** and renders as nothing. It is never guessed — the same honesty
/// rule [`ZavetSpecView::stale_commits`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ZavetPresence {
    /// The record's path is in `HEAD`'s tree.
    OnBranch,
    /// Captured, but its path is not in `HEAD`'s tree — another branch's record.
    OffBranch,
}

/// A record file found in the working tree that the store has never captured.
///
/// Capture reads decision records out of git objects, never the working tree,
/// so a record written but not yet committed is invisible to every `dira zavet`
/// query. Surfacing it as its own state is what keeps "I can see the file in my
/// editor" and "dira does not list it" from reading as a bug.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetUncapturedView {
    /// Parsed from the file's frontmatter; `None` when it does not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Repo-relative path of the file on disk.
    pub path: String,
    /// `uncommitted` (not in `HEAD` either) or `awaiting sweep` (committed, but
    /// the daemon has not walked that commit yet). The two need different
    /// remedies, so they are not collapsed.
    pub reason: String,
    /// `decision` | `spec`.
    pub kind: String,
}

/// `dira zavet decisions` — the captured decisions plus what the working tree
/// says about them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetDecisionsView {
    pub repo: String,
    /// The checked-out branch, when a working directory was available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub decisions: Vec<ZavetDecisionView>,
    /// Records on disk with no store row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncaptured: Vec<ZavetUncapturedView>,
}

/// The result of one on-demand knowledge sweep (`dira zavet sync`).
///
/// The counts are a before/after delta around the ordinary capture path, not a
/// separate ingest: sync reuses `capture_commits` rather than forcing a
/// re-read, so "unchanged HEAD captures nothing" stays as true here as it is
/// for the idle ticker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSyncView {
    /// Canonical repo that was swept.
    pub repo: String,
    /// Whether zavet is active for it. An inactive repo is swept for commits
    /// like any other but yields no knowledge — worth saying rather than
    /// reporting a bare zero.
    pub active: bool,
    /// Net new decision rows this sweep added. A sweep that re-captured an
    /// AMENDED record upserts in place and does not move this — the delta
    /// counts records the store had never seen, not writes performed.
    pub decisions_captured: u64,
    /// Net new commit trailers this sweep added.
    pub trailers_captured: u64,
    /// Decisions in the store for this repo after the sweep.
    pub decisions_total: u64,
    /// Whether this sweep is what first gave the daemon a directory for the
    /// repo. That set is empty after a restart and is otherwise filled only by
    /// session registration and agent events, so a repo nobody has opened a
    /// session in is never swept at all.
    pub registered: bool,
    /// Records still on disk with no store row after the sweep — the ones a
    /// sweep structurally cannot help with, because capture reads git objects
    /// and never the working tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncaptured: Vec<ZavetUncapturedView>,
}

/// One captured decision (list row; `zavet why` carries the body separately).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetDecisionView {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// `recorded` | `reverse-engineered` (absent = recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Explicit verification flag; `Some(false)` renders as an unverified
    /// hypothesis, never as fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// A later decision that corrects one claim in this one. The record stays
    /// `active` and keeps its body — this is the pointer that makes an
    /// append-only record safe to leave standing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected_by: Option<String>,
    /// How this record's invariants are verified. Empty means nobody has said
    /// — which is a finding, not a pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ZavetCheckView>,
    /// Whether this record's file is on the caller's branch; `None` = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<ZavetPresence>,
    /// Per-kind guard-event tallies for THIS decision — how often the guard
    /// actually fired. Empty means no guard event was ever recorded against it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_stats: Vec<ZavetGuardStatView>,
}

/// One verification binding as shown by `why` / `wiki`.
///
/// The command is displayed, never run: dira only reports what a record claims
/// about its own verification. Executing it is `zavet verify`'s job, which a
/// human invokes explicitly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetCheckView {
    pub label: String,
    pub command: String,
}

/// One ranked hit for a free-text `why`/`wiki` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSearchHit {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// First plain-prose sentence of the record body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub score: u32,
}

/// One matching orphan commit trailer — a micro-decision with no record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetTrailerHit {
    pub sha: String,
    /// Normalized trailer key (`why`, `constraint`, …).
    pub key: String,
    pub value: String,
    pub score: u32,
}

/// One captured living spec (`.zavet/specs/<slug>.md`), with its computed
/// staleness when the daemon had a working directory to ask git in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSpecView {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: i64,
    /// `designed` | `session` | `reverse-engineered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// `true` only after a human confirmed the spec matches the code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// `low` | `med` | `high`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    pub path: String,
    /// Git pathspecs the spec covers — the staleness domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Linked decision ids (spec-side links; decisions stay append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    /// Feature-level scenarios proving this spec's behavior still holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ZavetCheckView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// Commits touching `paths` after `last_commit` — computed at query time
    /// from git. `None` when no working directory was available to ask in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_commits: Option<u64>,
    /// Whether this spec's file is on the caller's branch; `None` = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<ZavetPresence>,
}

/// One ranked spec hit for a free-text `why`/`wiki` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSpecHit {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// First plain-prose sentence of the spec body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub score: u32,
}

/// A `(slug, title)` pointer to a spec that links a decision — the reverse
/// direction shown on `zavet why D-NNNN`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSpecRef {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `dira zavet wiki` — the knowledge-base overview for a repo.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetWikiView {
    pub repo: String,
    /// The checked-out branch, when a working directory was available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub decisions_total: u64,
    pub trailers: u64,
    pub guard_events: u64,
    #[serde(default)]
    pub specs_total: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active: Vec<ZavetDecisionView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<ZavetDecisionView>,
    /// Living specs with staleness + confidence badges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<ZavetSpecView>,
    /// Latest captured trailers, newest first: `(sha, key, value)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<(String, String, String)>,
    /// Decision records and specs on disk with no store row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncaptured: Vec<ZavetUncapturedView>,
}

/// A commit linked to a decision.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetCommitView {
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Per-session cost line for a decision (`zavet why`'s time panel).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSessionCostView {
    pub session_id: String,
    pub human_seconds: i64,
    pub agent_seconds: i64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// `dira zavet why <id>` — the recorded knowledge plus what it cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetWhyView {
    pub repo: String,
    /// When a free-text query resolved to this decision, what it matched on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_query: Option<String>,
    pub decision: ZavetDecisionView,
    /// Full record body (local-only data; shown, never synced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
    /// The decision that replaced this one, if any (reverse `supersedes` link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Records THIS decision corrects (reverse `corrected-by` link) — the
    /// forward direction lives on `decision.corrected_by`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub corrects: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<ZavetCommitView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_stats: Vec<ZavetGuardStatView>,
    /// Priced sessions evidencing this decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<ZavetSessionCostView>,
    /// Summed cost over `sessions`.
    pub total_human_seconds: i64,
    pub total_agent_seconds: i64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Evidence that could not be attributed to a session — reported so the
    /// cost reads as an honest lower bound.
    pub unattributed_commits: u64,
    pub unattributed_guard_events: u64,
    /// Specs that link this decision (the reverse of the spec-side links).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<ZavetSpecRef>,
}

/// `dira zavet why <spec query>` — a living spec plus what it cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZavetSpecWhyView {
    pub repo: String,
    /// When a free-text query resolved to this spec, what it matched on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_query: Option<String>,
    pub spec: ZavetSpecView,
    /// Full spec body (local-only data; shown, never synced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
    /// Commits linked via `Spec:` trailers plus the spec's own first/last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<ZavetCommitView>,
    /// Priced sessions evidencing this spec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<ZavetSessionCostView>,
    /// Summed cost over `sessions`.
    pub total_human_seconds: i64,
    pub total_agent_seconds: i64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Commits that could not be attributed to a session — the cost is an
    /// honest lower bound.
    pub unattributed_commits: u64,
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
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
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
            has_agent_activity: true,
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
    fn status_view_tolerates_older_daemon_without_tokens_or_billing() {
        // An older daemon omits `tokens`/`billing`; both must deserialize to
        // `None` so the new CLI simply omits the compute row and billable footer.
        let json = r#"{
            "active": [],
            "today": {"projects":[],"total_human_seconds":0,"total_agent_seconds":0,"session_count":0},
            "sync_pending": 0
        }"#;
        let v: StatusView = serde_json::from_str(json).unwrap();
        assert!(v.tokens.is_none());
        assert!(v.billing.is_none());
        assert!(!v.hydrating);
        assert!(v.writer_health.is_none());
        assert!(v.sync_health.is_none());
    }

    #[test]
    fn status_view_omits_absent_tokens_and_billing_on_the_wire() {
        // `None` must not serialize its key, so an older CLI (serde: unknown
        // fields ignored, but keep the payload minimal) sees the pre-field shape.
        let v = StatusView {
            active: vec![],
            today: Report {
                projects: vec![],
                total_human_seconds: 0,
                total_agent_seconds: 0,
                session_count: 0,
            },
            sync_pending: 0,
            tokens_pending: 0,
            hydrating: false,
            tokens: None,
            billing: None,
            writer_health: None,
            sync_health: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        // Match the exact key, not a substring: `tokens_pending` is a distinct,
        // always-present counter, so a bare `contains("tokens")` would conflate
        // "the optional compute view was omitted" with "the word appears anywhere".
        assert!(!json.contains("\"tokens\":"));
        assert!(!json.contains("\"billing\":"));
        assert!(!json.contains("\"writer_health\":"));
        assert!(!json.contains("\"sync_health\":"));
        // The pending counters are NOT optional — they always ride.
        assert!(json.contains("\"tokens_pending\":0"));

        let v = StatusView {
            tokens: Some(ComputeView {
                total_tokens: 2_060_000,
                est_cost_usd: 15.2,
            }),
            billing: Some(BillingView {
                billable_hours: 10.4,
                unbilled_amount: 1064.0,
                currency: "€".into(),
                period: "week".into(),
                fetched_at: "2026-07-02T09:00:00Z".into(),
            }),
            writer_health: Some(WriterHealthView {
                panics: 2,
                stalls: 0,
                idle_secs: Some(5),
                wedged: false,
                unattributed_token_rows: 142,
            }),
            sync_health: Some(SyncHealthView {
                last_attempt_at: Some("2026-07-09T10:00:00Z".into()),
                last_success_at: Some("2026-07-09T09:55:00Z".into()),
                last_error_kind: None,
                consecutive_failures: 0,
                backoff_secs: 0,
                cursor: Some("01J0EVENT".into()),
                cloud_watermark: Some("01J0WATERMARK".into()),
                flush_attempts: 10,
                flush_successes: 9,
                flush_failures: 1,
            }),
            ..v
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: StatusView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tokens.unwrap().total_tokens, 2_060_000);
        assert_eq!(back.billing.unwrap().currency, "€");
        assert_eq!(back.writer_health.unwrap().panics, 2);
        assert_eq!(back.writer_health.unwrap().unattributed_token_rows, 142);
        assert_eq!(back.sync_health.unwrap().flush_attempts, 10);
    }

    /// An older daemon's `WriterHealthView` predates issue #93 and omits
    /// `unattributed_token_rows` entirely — it must deserialize to `0`, not fail.
    #[test]
    fn writer_health_unattributed_token_rows_defaults_zero_from_older_daemon() {
        let json = r#"{"panics":1,"stalls":0,"idle_secs":null,"wedged":false}"#;
        let v: WriterHealthView = serde_json::from_str(json).unwrap();
        assert_eq!(v.unattributed_token_rows, 0);
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
