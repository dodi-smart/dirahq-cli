//! Work-unit assembly for the Sessions timeline — the offline twin of the cloud's
//! `assembleSessionGroups` + `pageFromFacts`.
//!
//! The cloud and the local daemon must agree about what a "work-unit" is. If they
//! don't, the desktop app and the web dashboard show different totals for the same
//! week, and there is no way for a user to tell which one is lying. So this module
//! is a **deliberate port**, not a re-derivation: the grouping key, the cluster
//! split, the head-selection rule, the sort, and the page-boundary filter all
//! mirror `cloud/src/lib/data/sessions.ts` line for line, and the shared fixture
//! `contract/testdata/session-grouping-vector.json` is asserted from both sides.
//!
//! The rules, per the cloud's D-0019:
//!
//! - Sessions are keyed by `(project, branch, identity)` and clustered in time,
//!   splitting whenever consecutive **starts** are more than [`CLUSTER_GAP`] apart.
//! - A unit's *head* is its **newest** member; the head's start is the unit's
//!   position on the timeline.
//! - A page fetches the padded window
//!   `[floor - SESSION_LOOKBACK, ceiling + SESSION_LOOKBACK)` but emits only units
//!   whose head lands in `[floor, ceiling)`. Both halves are required: pad one end
//!   only and a unit straddling the boundary is assembled twice, each time from
//!   half its sessions.
//!
//! What differs from the cloud, necessarily: there is no workspace and no second
//! user here, identity comes from `git config user.email` on the event rather than
//! a workspace membership, and repo identity is the canonical project ref rather
//! than a `repos.id`. None of that touches the grouping algebra.

use std::cmp::Reverse;
use std::collections::HashMap;

use dira_contract::{Harness, SessionKind};
use time::Duration;
use time::OffsetDateTime;

use crate::accounting::{self, AgentPolicy, AgentSample};
use crate::model::{EventKind, RawEvent};

/// Sessions on the same `(project, branch, identity)` further apart than this are
/// distinct work bursts, not one unit — keeps a week-old session off today's group.
///
/// Mirrors `CLUSTER_GAP_MS` in `cloud/src/lib/data/sessions.ts`.
pub const CLUSTER_GAP: Duration = Duration::hours(4);

/// How far beyond each end of a page's window the event query over-fetches, so a
/// unit straddling a page boundary is visible whole to both adjacent pages and can
/// therefore be claimed by exactly one of them.
///
/// 3× [`CLUSTER_GAP`]. Mirrors `SESSION_LOOKBACK_MS` in
/// `cloud/src/lib/data/paging.ts`. The residual imprecision is a unit whose own
/// chained span exceeds 12h; it renders as two units rather than one.
pub const SESSION_LOOKBACK: Duration = Duration::hours(12);

/// Days of history in one page of the timeline. Mirrors `SESSION_PAGE_DAYS`.
pub const PAGE_DAYS: i64 = 7;

/// One session, reconstructed from the event log and reduced to what grouping and
/// display need.
///
/// Deliberately rebuilt from events rather than read off the daemon's in-memory
/// session registry: the registry only holds live and recent sessions, so anything
/// older than the current run would silently vanish from the timeline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub harness: Harness,
    pub kind: SessionKind,
    /// Canonical repo ref (`github.com/org/repo`), or `None` for unresolved work.
    pub project: Option<String>,
    /// Branch at capture time, or `None` on a detached HEAD / non-git dir.
    pub branch: Option<String>,
    /// `git config user.email` for the project; empty when unresolved.
    pub identity: String,
    /// RFC3339 start — the session's first event.
    pub started_at: String,
    /// Epoch milliseconds of `started_at`, for grouping and sorting.
    pub started_at_ms: i64,
    /// Human prompts submitted in this session.
    pub prompts: i64,
    /// De-duplicated, idle-trimmed human seconds attributed to this session.
    pub human_seconds: i64,
    /// Idle-trimmed agent wall-clock seconds for this session.
    pub agent_seconds: i64,
}

/// A cluster of sessions on one `(project, branch, identity)`, displayed as a
/// single expandable row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkUnit {
    /// `project \0 branch \0 identity \0 head_started_at_ms` — stable across
    /// re-assembly, and the same shape the cloud emits.
    pub key: String,
    pub project: Option<String>,
    /// First member carrying a resolved branch, newest first. The head alone may
    /// be a branchless partial while an older member carries the real branch.
    pub branch: Option<String>,
    pub identity: String,
    /// The head (newest) member's harness — drives the unit's agent label.
    pub harness: Harness,
    /// RFC3339 start of the head member.
    pub started_at: String,
    /// Epoch milliseconds of the head member's start — the unit's timeline
    /// position, and what the page filter tests.
    pub started_at_ms: i64,
    pub count: usize,
    pub prompts: i64,
    pub human_seconds: i64,
    pub agent_seconds: i64,
    /// Members, newest first — empty unless the caller asked for them (see
    /// [`strip_sessions`] and `Request::Timeline.include_sessions`). Omitted from
    /// the wire entirely when empty, so a list-drawing client never carries it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionSummary>,
}

/// Drop every unit's member list.
///
/// The rollup a row draws (`count`, the summed seconds) is computed before this
/// runs, so stripping changes what is transmitted and nothing that is displayed.
/// This is the difference between a response that grows with a person's whole
/// history and one that grows with the number of units on a page.
pub fn strip_sessions(units: &mut [WorkUnit]) {
    for unit in units.iter_mut() {
        unit.sessions.clear();
    }
}

/// Reconstruct per-session summaries from a window of raw events.
///
/// `idle` and `agent` come from config and must be the same values the rest of the
/// engine uses — this function is not a second opinion about how time is counted,
/// only about how it is sliced per session.
///
/// Human time is attributed with [`accounting::per_key_seconds`] over the **whole**
/// window at once, not per session in isolation. That is what makes one person
/// supervising three agents cost one human-minute per minute instead of three: the
/// de-duplication is global, and a session that holds a single prompt inside the
/// idle window would otherwise read zero.
pub fn summarize_sessions(
    events: &[RawEvent],
    idle: Duration,
    agent: AgentPolicy,
) -> Vec<SessionSummary> {
    let human_signals: Vec<(OffsetDateTime, String)> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| (e.at, e.session_id.clone()))
        .collect();
    let human_by_session = accounting::per_key_seconds(&human_signals, idle);

    // First-seen order, so the output is deterministic without depending on hash
    // iteration order. Sessions are sorted by start before they are returned, but
    // ties must still resolve the same way on every run.
    let mut order: Vec<String> = Vec::new();
    let mut acc: HashMap<String, Acc> = HashMap::new();

    for e in events {
        let entry = acc.entry(e.session_id.clone()).or_insert_with(|| {
            order.push(e.session_id.clone());
            Acc::new(e)
        });
        entry.absorb(e);
    }

    let mut out: Vec<SessionSummary> = order
        .iter()
        .filter_map(|id| acc.get(id).map(|a| a.finish(id, &human_by_session, agent)))
        .collect();
    // Newest first is the display order everywhere else; grouping re-sorts anyway.
    out.sort_by_key(|s| Reverse(s.started_at_ms));
    out
}

/// Per-session accumulator used while folding the event window.
struct Acc {
    harness: Harness,
    /// Any manual lifecycle event marks the session manual — a `dira start` and a
    /// harness session are never the same session id, so this cannot mislabel.
    manual: bool,
    project: Option<String>,
    branch: Option<String>,
    identity: Option<String>,
    started_at: OffsetDateTime,
    prompts: i64,
    samples: Vec<AgentSample>,
    had_agent_activity: bool,
}

impl Acc {
    fn new(e: &RawEvent) -> Self {
        Self {
            harness: e.harness,
            manual: false,
            project: None,
            branch: None,
            identity: None,
            started_at: e.at,
            prompts: 0,
            samples: Vec::new(),
            had_agent_activity: false,
        }
    }

    fn absorb(&mut self, e: &RawEvent) {
        if e.at < self.started_at {
            self.started_at = e.at;
        }
        // Last non-empty wins: `cwd` can change mid-session (`CwdChanged`), and the
        // latest resolution is the one that describes where the work ended up.
        if e.project.is_some() {
            self.project = e.project.clone();
        }
        if e.branch.is_some() {
            self.branch = e.branch.clone();
        }
        if e.identity_email.is_some() {
            self.identity = e.identity_email.clone();
        }
        if matches!(
            e.kind,
            EventKind::ManualStart | EventKind::ManualStop | EventKind::ManualTick
        ) {
            self.manual = true;
        }
        if e.kind == EventKind::UserPrompt {
            self.prompts += 1;
        }
        self.samples.push(AgentSample {
            at: e.at,
            opens_span: e.kind.opens_agent_span(),
        });
        if e.kind.is_agent_activity() {
            self.had_agent_activity = true;
        }
    }

    fn finish(
        &self,
        session_id: &str,
        human_by_session: &std::collections::BTreeMap<String, i64>,
        agent: AgentPolicy,
    ) -> SessionSummary {
        // Mirrors `session_agent_evidence` in the daemon: no agent-activity event
        // means no wall-clock evidence, so a manual session cannot accrue phantom
        // agent time from its own keep-alive ticks.
        let agent_seconds = if self.had_agent_activity {
            accounting::agent_active_seconds(&self.samples, agent)
        } else {
            0
        };
        SessionSummary {
            session_id: session_id.to_string(),
            harness: self.harness,
            kind: if self.manual {
                SessionKind::Manual
            } else {
                SessionKind::Agent
            },
            project: self.project.clone(),
            branch: self.branch.clone(),
            identity: self.identity.clone().unwrap_or_default(),
            started_at: fmt_rfc3339(self.started_at),
            started_at_ms: epoch_ms(self.started_at),
            prompts: self.prompts,
            human_seconds: human_by_session.get(session_id).copied().unwrap_or(0),
            agent_seconds,
        }
    }
}

/// Group sessions into `(project, branch, identity)` work-units, time-clustered so
/// a burst of short same-branch sessions collapses into one row while distinct
/// bursts stay separate. Newest unit first.
///
/// The key uses `-` for a missing project or branch, exactly as the cloud does, so
/// unresolved work groups with unresolved work rather than with everything.
pub fn assemble_work_units(sessions: &[SessionSummary]) -> Vec<WorkUnit> {
    // Insertion-ordered buckets. The cloud groups into a JS `Map`, whose iteration
    // order is insertion order, and its final sort is stable — so units sharing a
    // head timestamp come out in first-seen key order. A `BTreeMap` here would
    // order them by key instead and silently diverge on ties.
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<SessionSummary>> = HashMap::new();

    for s in sessions {
        let key = group_key(s);
        buckets
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key.clone());
                Vec::new()
            })
            .push(s.clone());
    }

    let mut units: Vec<WorkUnit> = Vec::new();
    for key in &order {
        let Some(bucket) = buckets.get(key) else {
            continue;
        };
        // Oldest → newest, splitting whenever the gap between consecutive starts
        // exceeds the cluster window.
        let mut arr = bucket.clone();
        arr.sort_by_key(|s| s.started_at_ms);

        let gap_ms = CLUSTER_GAP.whole_milliseconds() as i64;
        let mut cluster: Vec<SessionSummary> = Vec::new();
        for s in arr {
            match cluster.last() {
                None => cluster.push(s),
                Some(prev) if s.started_at_ms - prev.started_at_ms <= gap_ms => cluster.push(s),
                Some(_) => {
                    units.push(build_unit(&cluster));
                    cluster = vec![s];
                }
            }
        }
        if !cluster.is_empty() {
            units.push(build_unit(&cluster));
        }
    }

    // Newest unit first, by its head's start. Stable, so ties keep the first-seen
    // key order established above.
    units.sort_by_key(|u| Reverse(u.started_at_ms));
    units
}

/// Filter assembled units down to the ones a page owns: heads in `[floor, ceiling)`.
///
/// `units` must have been assembled from events over the padded window
/// `[floor - SESSION_LOOKBACK, ceiling + SESSION_LOOKBACK)`. Given that, and since
/// consecutive pages tile the timeline with no gap (`ceiling(N+1) == floor(N)`),
/// every unit's head falls into exactly one page's tile — so this filter is both
/// necessary and sufficient to emit each unit exactly once.
pub fn page(units: Vec<WorkUnit>, floor_ms: i64, ceiling_ms: i64) -> Vec<WorkUnit> {
    units
        .into_iter()
        .filter(|u| u.started_at_ms >= floor_ms && u.started_at_ms < ceiling_ms)
        .collect()
}

/// Field separator inside [`WorkUnit::key`] and the grouping key.
///
/// ASCII Unit Separator, deliberately NOT the cloud's `\0`. The cloud's key never
/// leaves the process that built it; this one is serialized to JSON and parsed by
/// other languages, and a NUL inside a string is a live hazard for any C-adjacent
/// consumer (the desktop app's Zig core among them). US is equally impossible in
/// a repo ref, branch name or email, and survives a round trip through JSON,
/// logs, and a debugger intact.
///
/// The separator is an encoding detail, not part of the cross-language contract:
/// the grouping vector pins which sessions land in which unit, never how the key
/// spells itself. See D-0018.
const KEY_SEP: char = '\u{1f}';

/// `project ␟ branch ␟ identity` — the grouping key, missing parts as `-`.
fn group_key(s: &SessionSummary) -> String {
    format!(
        "{}{KEY_SEP}{}{KEY_SEP}{}",
        s.project.as_deref().unwrap_or("-"),
        s.branch.as_deref().unwrap_or("-"),
        s.identity.to_lowercase(),
    )
}

/// Aggregate a clustered set of sessions into one displayable work-unit.
fn build_unit(members: &[SessionSummary]) -> WorkUnit {
    // Newest first — the head drives the unit's labels and its timeline position.
    let mut ms = members.to_vec();
    ms.sort_by_key(|s| Reverse(s.started_at_ms));
    let head = ms[0].clone();

    WorkUnit {
        key: format!(
            "{}{KEY_SEP}{}{KEY_SEP}{}{KEY_SEP}{}",
            head.project.as_deref().unwrap_or("-"),
            head.branch.as_deref().unwrap_or("-"),
            head.identity.to_lowercase(),
            head.started_at_ms,
        ),
        project: head.project.clone(),
        branch: ms.iter().find_map(|m| m.branch.clone()),
        identity: head.identity.clone(),
        harness: head.harness,
        started_at: head.started_at.clone(),
        started_at_ms: head.started_at_ms,
        count: ms.len(),
        prompts: ms.iter().map(|m| m.prompts).sum(),
        human_seconds: ms.iter().map(|m| m.human_seconds).sum(),
        agent_seconds: ms.iter().map(|m| m.agent_seconds).sum(),
        sessions: ms,
    }
}

fn fmt_rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Epoch milliseconds — the cloud's `Date.getTime()`, and the unit of every
/// boundary comparison here.
pub fn epoch_ms(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Parse an RFC3339 timestamp to epoch milliseconds.
pub fn parse_epoch_ms(s: &str) -> Result<i64, crate::Error> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(epoch_ms)
        .map_err(crate::Error::parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The shared cross-language fixture. The cloud asserts the SAME file (vendored
    /// by `just contract-pull`) against `assembleSessionGroups`/`pageFromFacts`, so
    /// a change to the grouping rules on either side fails on both.
    const VECTOR: &str =
        include_str!("../../../contract/testdata/session-grouping-vector.json");

    fn vector() -> Value {
        serde_json::from_str(VECTOR).expect("grouping vector parses")
    }

    fn opt_str(v: &Value) -> Option<String> {
        v.as_str().map(str::to_string)
    }

    /// Build summaries straight from the fixture rows. Deliberately NOT via
    /// `summarize_sessions`: the fixture states engaged seconds as a given so both
    /// languages start from identical inputs and the assertion isolates *grouping*.
    /// Event-derived summarization is covered separately below.
    fn fixture_sessions(v: &Value) -> Vec<SessionSummary> {
        v["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .map(|s| {
                let started_at = s["startedAt"].as_str().expect("startedAt");
                SessionSummary {
                    session_id: s["id"].as_str().expect("id").to_string(),
                    harness: Harness::ClaudeCode,
                    kind: SessionKind::Agent,
                    project: opt_str(&s["project"]),
                    branch: opt_str(&s["branch"]),
                    identity: s["identity"].as_str().expect("identity").to_string(),
                    started_at: started_at.to_string(),
                    started_at_ms: parse_epoch_ms(started_at).expect("parses"),
                    prompts: 1,
                    human_seconds: s["engagedSeconds"].as_i64().expect("engagedSeconds"),
                    agent_seconds: 0,
                }
            })
            .collect()
    }

    #[test]
    fn matches_the_cross_language_grouping_vector() {
        let v = vector();
        let units = assemble_work_units(&fixture_sessions(&v));
        let expected = v["expectedUnits"].as_array().expect("expectedUnits");

        assert_eq!(
            units.len(),
            expected.len(),
            "unit count diverged from the vector"
        );

        for (got, want) in units.iter().zip(expected) {
            let want_head = want["headStartedAt"].as_str().expect("headStartedAt");
            assert_eq!(
                got.started_at_ms,
                parse_epoch_ms(want_head).expect("parses"),
                "unit head mismatch (want {want_head})"
            );
            assert_eq!(got.project, opt_str(&want["project"]), "project mismatch");
            assert_eq!(got.branch, opt_str(&want["branch"]), "branch mismatch");
            assert_eq!(
                got.identity,
                want["identity"].as_str().expect("identity"),
                "identity mismatch"
            );
            assert_eq!(
                got.count,
                want["count"].as_u64().expect("count") as usize,
                "member count mismatch at {want_head}"
            );
            assert_eq!(
                got.human_seconds,
                want["engagedSeconds"].as_i64().expect("engagedSeconds"),
                "summed engaged mismatch at {want_head}"
            );
            let want_members: Vec<&str> = want["members"]
                .as_array()
                .expect("members")
                .iter()
                .map(|m| m.as_str().expect("member id"))
                .collect();
            let got_members: Vec<&str> =
                got.sessions.iter().map(|s| s.session_id.as_str()).collect();
            assert_eq!(got_members, want_members, "members (newest first) mismatch");
        }
    }

    #[test]
    fn pages_tile_the_vector_without_gap_or_duplicate() {
        let v = vector();
        let sessions = fixture_sessions(&v);
        let mut seen: Vec<String> = Vec::new();

        for p in v["pages"].as_array().expect("pages") {
            let name = p["name"].as_str().expect("name");
            let floor = parse_epoch_ms(p["floor"].as_str().expect("floor")).expect("parses");
            let ceiling =
                parse_epoch_ms(p["ceiling"].as_str().expect("ceiling")).expect("parses");

            // Each page assembles from its own padded fetch, exactly as the daemon
            // will — the point of the fixture is that this reproduces one whole unit
            // per page, not a fragment on each side of the boundary.
            let windowed: Vec<SessionSummary> = sessions
                .iter()
                .filter(|s| {
                    let lookback = SESSION_LOOKBACK.whole_milliseconds() as i64;
                    s.started_at_ms >= floor - lookback && s.started_at_ms < ceiling + lookback
                })
                .cloned()
                .collect();

            let got = page(assemble_work_units(&windowed), floor, ceiling);
            let want: Vec<i64> = p["expectedHeads"]
                .as_array()
                .expect("expectedHeads")
                .iter()
                .map(|h| parse_epoch_ms(h.as_str().expect("head")).expect("parses"))
                .collect();

            assert_eq!(
                got.iter().map(|u| u.started_at_ms).collect::<Vec<_>>(),
                want,
                "page '{name}' emitted the wrong units"
            );
            seen.extend(got.into_iter().map(|u| u.key));
        }

        // Every unit claimed exactly once across the two pages — the double-count
        // D-0019 records would show up here as a repeated key.
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "a unit was claimed by two pages");
    }

    #[test]
    fn an_exactly_gap_wide_split_stays_one_unit_but_a_hair_more_splits() {
        let base = OffsetDateTime::from_unix_timestamp(1_767_600_000).expect("valid");
        let gap = CLUSTER_GAP;

        let mk = |id: &str, at: OffsetDateTime| SessionSummary {
            session_id: id.into(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            project: Some("github.com/acme/api".into()),
            branch: Some("main".into()),
            identity: "u1@example.com".into(),
            started_at: fmt_rfc3339(at),
            started_at_ms: epoch_ms(at),
            prompts: 0,
            human_seconds: 60,
            agent_seconds: 0,
        };

        let exact = assemble_work_units(&[mk("a", base), mk("b", base + gap)]);
        assert_eq!(exact.len(), 1, "a gap of exactly CLUSTER_GAP must not split");

        let over = assemble_work_units(&[
            mk("a", base),
            mk("b", base + gap + Duration::milliseconds(1)),
        ]);
        assert_eq!(over.len(), 2, "one millisecond past CLUSTER_GAP must split");
    }

    #[test]
    fn summarize_reconstructs_sessions_from_the_event_log() {
        let base = OffsetDateTime::from_unix_timestamp(1_767_600_000).expect("valid");
        let ev = |id: &str, session: &str, kind: EventKind, offset_secs: i64| RawEvent {
            id: id.into(),
            at: base + Duration::seconds(offset_secs),
            session_id: session.into(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some("github.com/acme/api".into()),
            identity_email: Some("u1@example.com".into()),
            branch: Some("main".into()),
            tool: None,
            label: None,
            activity: None,
            note: None,
        };

        let events = vec![
            ev("1", "sess-a", EventKind::SessionStart, 0),
            ev("2", "sess-a", EventKind::UserPrompt, 10),
            ev("3", "sess-a", EventKind::PreTool, 20),
            ev("4", "sess-a", EventKind::PostTool, 40),
            ev("5", "sess-a", EventKind::UserPrompt, 60),
        ];

        let out = summarize_sessions(&events, Duration::minutes(5), AgentPolicy::default());
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.session_id, "sess-a");
        assert_eq!(s.prompts, 2, "counts user prompts, nothing else");
        assert_eq!(
            s.started_at_ms,
            epoch_ms(base),
            "start is the session's FIRST event, not the first one seen"
        );
        assert_eq!(s.project.as_deref(), Some("github.com/acme/api"));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert!(s.agent_seconds > 0, "tool calls are agent evidence");
        assert!(s.human_seconds > 0, "two prompts inside idle accrue human time");
    }

    #[test]
    fn a_manual_session_accrues_no_phantom_agent_time() {
        let base = OffsetDateTime::from_unix_timestamp(1_767_600_000).expect("valid");
        let ev = |id: &str, kind: EventKind, offset_secs: i64| RawEvent {
            id: id.into(),
            at: base + Duration::seconds(offset_secs),
            session_id: "manual-1".into(),
            harness: Harness::Manual,
            kind,
            cwd: None,
            project: None,
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        };

        // Keep-alive ticks are human signals, never agent evidence — the sawtooth
        // this guards against was a manual session banking agent time from ticks.
        let events = vec![
            ev("1", EventKind::ManualStart, 0),
            ev("2", EventKind::ManualTick, 30),
            ev("3", EventKind::ManualTick, 60),
            ev("4", EventKind::ManualStop, 90),
        ];

        let out = summarize_sessions(&events, Duration::minutes(5), AgentPolicy::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, SessionKind::Manual);
        assert_eq!(out[0].agent_seconds, 0, "no agent-activity event, no agent time");
        assert!(out[0].human_seconds > 0, "ticks still accrue human time");
    }
}
