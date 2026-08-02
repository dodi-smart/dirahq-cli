//! Local reporting — computed on demand from the event log. Human time is
//! de-duplicated and idle-trimmed via [`crate::accounting`]; agent wall-clock is
//! summed freely per session (it is evidence, never a billing base).

use crate::accounting::{self, Signal};
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
}

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
    // --- human time: dedup across the whole concurrent set ---
    let signals: Vec<Signal> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| Signal {
            at: e.at,
            project: e.project.clone(),
        })
        .collect();
    let human_by_project = accounting::per_project_seconds(&signals, idle);

    // --- agent wall-clock: idle-trimmed active time per session, summed freely ---
    // The wall-clock is the idle-trimmed active span over each session's own event
    // timestamps ([`accounting::active_seconds`]), NOT the raw `last - first`
    // lifetime: a session left open for hours between bursts of work reads as the
    // time it was actually active, so dead spans can't inflate it. This is the same
    // measure the sync/rollup path (`sync::batch`) and the cloud already use, so the
    // local report, the historical rollups folded in by [`build_merged`], and the
    // synced totals all agree.
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

    let mut agent_by_project: BTreeMap<Option<String>, i64> = BTreeMap::new();
    for (project, samples, had_activity) in sessions.values() {
        if *had_activity {
            *agent_by_project.entry(project.clone()).or_insert(0) +=
                accounting::agent_active_seconds(samples, agent);
        }
    }

    // --- merge into per-project lines ---
    let mut keys: Vec<Option<String>> = human_by_project
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
            project: k,
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

    let mut by_project: BTreeMap<Option<String>, ProjectReport> = report
        .projects
        .into_iter()
        .map(|p| (p.project.clone(), p))
        .collect();
    for line in rollups {
        let entry = by_project
            .entry(line.project.clone())
            .or_insert_with(|| ProjectReport {
                project: line.project.clone(),
                human_seconds: 0,
                agent_wall_seconds: 0,
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
        }
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
