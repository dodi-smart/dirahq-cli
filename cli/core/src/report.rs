//! Local reporting — computed on demand from the event log. Human time is
//! de-duplicated and idle-trimmed via [`crate::accounting`]; agent wall-clock is
//! summed freely per session (it is evidence, never a billing base).

use crate::accounting;
use crate::model::RawEvent;
use crate::store::RollupLine;
use std::collections::BTreeMap;
use time::Duration;

/// Per-project rollup line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectReport {
    pub project: Option<String>,
    pub human_seconds: i64,
    pub agent_wall_seconds: i64,
    /// The repo-less work of orchestrating sessions — coordination that no
    /// single repo describes, kept apart from genuinely unresolved work
    /// (DIRASH-0041). Only ever set on a `project: None` line.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orchestration: bool,
}

/// A report line's key: the project, and whether a repo-less line is the
/// orchestration bucket.
type LineKey = (Option<String>, bool);

/// A computed report over a slice of events.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub projects: Vec<ProjectReport>,
    pub total_human_seconds: i64,
    pub total_agent_seconds: i64,
    pub session_count: usize,
}

/// Build a report from events, de-duplicating human time across all sessions.
///
/// `idle` governs human time; `agent` governs agent wall-clock. They are separate
/// because a gap that means "the human walked away" means "the agent is running a
/// build" — see [`accounting::AgentPolicy`].
pub fn build(events: &[RawEvent], idle: Duration, agent: accounting::AgentPolicy) -> Report {
    // Sessions whose lineage makes them orchestrators — the same rule the
    // rollup and the live registry fold. Their repo-less time is the
    // orchestration bucket, not "unresolved".
    let mut lineages: std::collections::HashMap<&str, crate::orchestration::SessionLineage> =
        std::collections::HashMap::new();
    for e in events {
        lineages
            .entry(e.session_id.as_str())
            .or_default()
            .observe(e);
    }
    let orchestrators: std::collections::HashSet<&str> = lineages
        .into_iter()
        .filter_map(|(s, l)| l.orchestrator.then_some(s))
        .collect();
    let key = |project: &Option<String>, session: &str| -> LineKey {
        (
            project.clone(),
            project.is_none() && orchestrators.contains(session),
        )
    };

    // --- human time: dedup across the whole concurrent set ---
    // Keyed per opening signal exactly as `per_project_seconds` would, with
    // the orchestration flag riding along on repo-less signals.
    let signals: Vec<(time::OffsetDateTime, LineKey)> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| (e.at, key(&e.project, &e.session_id)))
        .collect();
    let human_by_project = accounting::per_key_seconds(&signals, idle);

    // --- agent wall-clock: clamped active time per session, summed freely ---
    // The wall-clock is the clamped active span over each session's own event
    // timestamps ([`accounting::agent_active_seconds`]), NOT the raw `last - first`
    // lifetime: a session left open for hours between bursts of work reads as the
    // time it was actually active, so dead spans can't inflate it. This is the same
    // measure the sync/rollup path (`sync::batch`) and the cloud already use, so the
    // local report, the historical rollups folded in by [`build_merged`], and the
    // synced totals all agree.
    //
    // "Clamped", not "idle-trimmed": a gap opened by a `PreTool` is a tool call in
    // flight and is credited in FULL up to `agent_max_span_seconds`; any other gap
    // is capped at `agent_idle_seconds`. Neither is discarded. Trimming over-idle
    // gaps the way human time does is what used to bank zero seconds for a
    // two-hour build — human silence means absence, agent silence usually means
    // work.
    let mut sessions: BTreeMap<&str, (Option<String>, Vec<accounting::AgentSample>, bool)> =
        BTreeMap::new();
    for e in events {
        let entry = sessions
            .entry(e.session_id.as_str())
            .or_insert_with(|| (e.project.clone(), Vec::new(), false));
        entry.1.push(accounting::AgentSample {
            at: e.at,
            opens_span: e.kind.opens_agent_span(),
        });
        if e.kind.is_agent_activity() {
            entry.2 = true;
        }
        if entry.0.is_none() && e.project.is_some() {
            entry.0 = e.project.clone();
        }
    }

    let mut agent_by_project: BTreeMap<LineKey, i64> = BTreeMap::new();
    for (session, (project, samples, had_activity)) in &sessions {
        if *had_activity {
            // An orchestrator's agent time is coordination as a whole: no
            // single repo describes the session, whatever it touched first.
            let k = if orchestrators.contains(session) {
                (None, true)
            } else {
                (project.clone(), false)
            };
            *agent_by_project.entry(k).or_insert(0) +=
                accounting::agent_active_seconds(samples, agent);
        }
    }

    // --- merge into per-project lines ---
    let mut keys: Vec<LineKey> = human_by_project
        .keys()
        .chain(agent_by_project.keys())
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();

    let projects: Vec<ProjectReport> = keys
        .into_iter()
        .map(|k| ProjectReport {
            human_seconds: human_by_project.get(&k).copied().unwrap_or(0),
            agent_wall_seconds: agent_by_project.get(&k).copied().unwrap_or(0),
            orchestration: k.1,
            project: k.0,
        })
        .collect();

    Report {
        total_human_seconds: projects.iter().map(|p| p.human_seconds).sum(),
        total_agent_seconds: projects.iter().map(|p| p.agent_wall_seconds).sum(),
        session_count: sessions.len(),
        projects,
    }
}

/// Build a report over the *recent* raw events, then fold in the compacted
/// historical rollup so totals survive retention/compaction.
///
/// Compaction (`Store::compact`) only ever removes events older than the
/// retention window and replaces them with per-project daily rollup lines holding
/// the *same* human/active seconds (computed with the same accounting code). So
/// `raw report + rollup lines` reconstructs the totals the raw log would have
/// produced before compaction — no data is lost from `report --all` after a
/// sweep. The rollup window must match the report range: callers pass only the
/// rollup lines for days inside the report's lower bound, so `--today/--week`
/// (inside retention, so normally no matching rollups) stay exactly raw.
///
/// Rollups are summed *into* the matching per-project line (additive), keeping
/// the per-project breakdown summing to the grand total. `rollup_sessions` is the
/// distinct session count captured in those rollups, added to the live count.
pub fn build_merged(
    events: &[RawEvent],
    idle: Duration,
    agent: accounting::AgentPolicy,
    rollups: &[RollupLine],
    rollup_sessions: usize,
) -> Report {
    let mut report = build(events, idle, agent);
    if rollups.is_empty() {
        return report;
    }

    let mut by_project: BTreeMap<LineKey, ProjectReport> = report
        .projects
        .into_iter()
        .map(|p| ((p.project.clone(), p.orchestration), p))
        .collect();
    // Compacted rollups predate the orchestration split and carry none: their
    // repo-less time stays on the unresolved line.
    for line in rollups {
        let entry = by_project
            .entry((line.project.clone(), false))
            .or_insert_with(|| ProjectReport {
                project: line.project.clone(),
                human_seconds: 0,
                agent_wall_seconds: 0,
                orchestration: false,
            });
        entry.human_seconds += line.human_seconds;
        entry.agent_wall_seconds += line.agent_wall_seconds;
    }

    let projects: Vec<ProjectReport> = by_project.into_values().collect();
    report.total_human_seconds = projects.iter().map(|p| p.human_seconds).sum();
    report.total_agent_seconds = projects.iter().map(|p| p.agent_wall_seconds).sum();
    report.session_count += rollup_sessions;
    report.projects = projects;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventKind, RawEvent};
    use dira_contract::Harness;
    use time::OffsetDateTime;

    fn ev(session: &str, secs: i64, kind: EventKind, project: &str) -> RawEvent {
        RawEvent {
            id: format!("{session}-{secs}"),
            at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs),
            session_id: session.to_string(),
            harness: Harness::ClaudeCode,
            kind,
            cwd: None,
            project: Some(project.to_string()),
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
            spec: None,
            lineage: Default::default(),
        }
    }

    #[test]
    fn orchestrator_repo_less_time_is_its_own_line_not_unresolved() {
        let orch = |secs: i64, kind: EventKind, project: Option<&str>| {
            let mut e = ev("orch", secs, kind, "x");
            e.project = project.map(str::to_string);
            e.lineage.workspace = Some("0123456789abcdef".into());
            e
        };
        let stray = |secs: i64| {
            let mut e = ev("stray", secs, EventKind::UserPrompt, "x");
            e.project = None;
            e
        };
        let events = vec![
            orch(0, EventKind::UserPrompt, None),
            orch(60, EventKind::UserPrompt, Some("github.com/acme/api")),
            orch(120, EventKind::UserPrompt, None),
            stray(180),
            stray(240),
        ];
        let r = build(&events, Duration::minutes(5), Default::default());
        let line = |project: Option<&str>, orchestration: bool| {
            r.projects
                .iter()
                .find(|p| p.project.as_deref() == project && p.orchestration == orchestration)
                .map(|p| p.human_seconds)
        };
        // Gaps are attributed to their opening signal.
        assert_eq!(line(None, true), Some(60 + 60), "orchestration bucket");
        assert_eq!(line(Some("github.com/acme/api"), false), Some(60));
        assert_eq!(
            line(None, false),
            Some(60),
            "the stray session stays unresolved"
        );
        assert_eq!(
            r.total_human_seconds, 240,
            "the split never changes the total"
        );
    }

    #[test]
    fn a_repo_less_scratch_session_is_unresolved_not_orchestration() {
        // A scheduled-task session: scratch workspace, no spawn, no repo.
        let mut a = ev("cron", 0, EventKind::UserPrompt, "x");
        a.project = None;
        a.lineage.workspace = Some("0123456789abcdef".into());
        let mut b = a.clone();
        b.at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(60);
        let r = build(&[a, b], Duration::minutes(5), Default::default());
        assert!(
            r.projects.iter().all(|p| !p.orchestration),
            "no orchestration line: {:#?}",
            r.projects
        );
        let unresolved = r
            .projects
            .iter()
            .find(|p| p.project.is_none() && !p.orchestration)
            .map(|p| p.human_seconds);
        assert_eq!(unresolved, Some(60));
    }

    #[test]
    fn human_dedups_but_agent_sums() {
        // Two concurrent sessions on the same project, human prompts interleaved.
        let events = vec![
            ev("s1", 0, EventKind::SessionStart, "p"),
            ev("s2", 0, EventKind::SessionStart, "p"),
            ev("s1", 10, EventKind::UserPrompt, "p"),
            ev("s2", 20, EventKind::UserPrompt, "p"),
            ev("s1", 30, EventKind::PreTool, "p"),
            ev("s2", 40, EventKind::PreTool, "p"),
            ev("s1", 60, EventKind::UserPrompt, "p"),
        ];
        let r = build(
            &events,
            Duration::minutes(5),
            accounting::AgentPolicy::default(),
        );
        // Human: gaps 10-20, 20-60 -> 50s, deduped across both sessions.
        assert_eq!(r.total_human_seconds, 50);
        // Agent wall: all gaps are within idle, so idle-trimmed active == the span:
        // s1 [0,10,30,60] = 60s, s2 [0,20,40] = 40s -> 100s summed.
        assert_eq!(r.total_agent_seconds, 100);
        assert_eq!(r.session_count, 2);
    }

    #[test]
    fn agent_wall_is_idle_trimmed_not_raw_span() {
        // One session: a burst of activity, a long quiet gap (>5min) that is NOT a
        // tool call, then a final burst. The point being protected is that the raw
        // last-first span (~1h) is never what gets counted.
        let events = vec![
            ev("s", 0, EventKind::SessionStart, "p"),
            ev("s", 10, EventKind::PreTool, "p"),
            ev("s", 20, EventKind::PostTool, "p"), // burst 1: 0..20 = 20s
            ev("s", 3600, EventKind::PreTool, "p"), // +1h quiet, opened by PostTool
            ev("s", 3630, EventKind::PostTool, "p"), // burst 2: 3600..3630 = 30s
        ];
        let policy = accounting::AgentPolicy::default();
        let r = build(&events, Duration::minutes(5), policy);

        // 10 (start→pre) + 10 (the tool call) + 300 (the hour of quiet, CLAMPED to
        // the agent idle ceiling) + 30 (the second tool call) = 350s.
        //
        // The quiet gap is clamped rather than discarded so the `watch` dashboard's
        // live tail can predict this number instead of displaying time that is
        // later revoked — the clamp-vs-discard asymmetry was the reset users saw.
        // It is still nothing like the ~3630s raw span, which is what this test
        // has always existed to rule out.
        assert_eq!(r.total_agent_seconds, 350);
        assert!(r.total_agent_seconds < 3630 / 10);
    }

    /// The regression that motivated the agent policy: a harness emits nothing
    /// while a tool runs, so the gap after a `PreTool` IS the tool call. Under the
    /// old shared-idle rule a two-hour build banked zero.
    #[test]
    fn a_long_tool_call_is_credited_not_discarded() {
        let two_hours = 2 * 60 * 60;
        let events = vec![
            ev("s", 0, EventKind::PreTool, "p"),
            ev("s", two_hours, EventKind::PostTool, "p"),
        ];
        let r = build(
            &events,
            Duration::minutes(5),
            accounting::AgentPolicy::default(),
        );
        assert_eq!(r.total_agent_seconds, two_hours);
    }

    /// Compaction is lossless for reports: the full raw report equals the merged
    /// report after the old portion is summarized into rollup lines and removed.
    #[tokio::test]
    async fn merged_report_matches_raw_before_compaction() {
        use crate::store::Store;
        use time::OffsetDateTime;
        let idle = Duration::minutes(5);
        let store = Store::open_in_memory().await.unwrap();
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(30);

        // Old session (synced + past retention) + a recent one.
        let mut all = Vec::new();
        for i in 0..4 {
            all.push(RawEvent {
                id: format!("01OLD{i}"),
                at: now - Duration::days(20) + Duration::seconds(60 * i),
                session_id: "s_old".into(),
                harness: Harness::ClaudeCode,
                kind: EventKind::UserPrompt,
                cwd: None,
                project: Some("p".into()),
                identity_email: None,
                branch: None,
                tool: None,
                label: None,
                activity: None,
                note: None,
                spec: None,
                lineage: Default::default(),
            });
        }
        for i in 0..3 {
            all.push(RawEvent {
                id: format!("01NEW{i}"),
                at: now + Duration::seconds(30 * i),
                session_id: "s_new".into(),
                harness: Harness::ClaudeCode,
                kind: EventKind::UserPrompt,
                cwd: None,
                project: Some("p".into()),
                identity_email: None,
                branch: None,
                tool: None,
                label: None,
                activity: None,
                note: None,
                spec: None,
                lineage: Default::default(),
            });
        }
        for e in &all {
            store.append(e).await.unwrap();
        }
        let raw_report = build(&all, idle, accounting::AgentPolicy::default());

        // Compact the old, synced portion, then merge rollups with the survivors.
        store
            .compact(Some("01OLD3"), now - Duration::days(14), idle)
            .await
            .unwrap();
        let survivors = store.events_since(None).await.unwrap();
        let rollups = store.rollup_totals_since(None).await.unwrap();
        let rollup_sessions = store.rollup_session_count(None).await.unwrap();
        let merged = build_merged(
            &survivors,
            idle,
            accounting::AgentPolicy::default(),
            &rollups,
            rollup_sessions,
        );

        assert_eq!(merged.total_human_seconds, raw_report.total_human_seconds);
        assert_eq!(merged.total_agent_seconds, raw_report.total_agent_seconds);
        assert_eq!(merged.session_count, raw_report.session_count);
        assert_eq!(merged.projects, raw_report.projects);
    }
}
