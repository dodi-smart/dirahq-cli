//! Human-engaged time accounting — the honest core of Dira.
//!
//! The hard invariant: **no wall-clock minute is counted toward human time more
//! than once, regardless of how many sessions are open.** One person supervising
//! three agents is still one human-minute per minute.
//!
//! ## Model
//! Every human signal (a prompt, a permission decision, a manual tick) is a point
//! on the global timeline tagged with the project it belongs to. We merge signals
//! from *all* concurrent sessions into one sorted stream, then count the gap
//! between each consecutive pair **only if** it is within the idle threshold.
//! Gaps wider than the threshold are idle and excluded (idle-trim). Because the
//! counted gaps are consecutive and half-open `[a, b)`, they never overlap — that
//! is the no-double-count guarantee, by construction.
//!
//! Each counted gap is attributed to the project of the signal that *opens* it
//! (the v1 attribution policy; even/weighted splitting can layer on later). This
//! keeps per-project totals summing exactly to the de-duplicated grand total.

use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};

/// A human engagement signal on the global timeline.
#[derive(Debug, Clone)]
pub struct Signal {
    pub at: OffsetDateTime,
    /// Canonical project ref, or `None` for unresolved work.
    pub project: Option<String>,
}

/// A counted, non-overlapping slice of de-duplicated human time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountedGap {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub project: Option<String>,
}

impl CountedGap {
    pub fn seconds(&self) -> i64 {
        (self.end - self.start).whole_seconds()
    }
}

/// Compute the de-duplicated, idle-trimmed human-time gaps from a set of signals
/// drawn from any number of concurrent sessions.
///
/// `idle` is the threshold beyond which a gap between two signals is considered
/// idle and not counted (default 5 min).
pub fn counted_gaps(signals: &[Signal], idle: Duration) -> Vec<CountedGap> {
    if signals.len() < 2 {
        return Vec::new();
    }
    // Sort by time; ties keep input order (stable) so attribution is deterministic.
    let mut ordered: Vec<&Signal> = signals.iter().collect();
    ordered.sort_by_key(|s| s.at);

    let mut gaps = Vec::new();
    for pair in ordered.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let delta = b.at - a.at;
        if delta > Duration::ZERO && delta <= idle {
            gaps.push(CountedGap {
                start: a.at,
                end: b.at,
                project: a.project.clone(),
            });
        }
    }
    gaps
}

/// Total de-duplicated human seconds across all projects.
pub fn total_human_seconds(signals: &[Signal], idle: Duration) -> i64 {
    counted_gaps(signals, idle)
        .iter()
        .map(CountedGap::seconds)
        .sum()
}

/// Sum the idle-trimmed active seconds over a bare set of timestamps.
///
/// This is the same gap-counting logic as [`counted_gaps`] — sort, then sum each
/// consecutive gap `(a, b]` where `0 < delta <= idle` — but without any project
/// attribution. It is the no-double-count active-time measure for things that are
/// not human signals (e.g. an agent's whole event timeline): a timeline left open
/// for hours but only sporadically active reads as the sum of its active spans,
/// never the raw `last - first` wall span.
pub fn active_seconds(times: &[OffsetDateTime], idle: Duration) -> i64 {
    if times.len() < 2 {
        return 0;
    }
    let mut ordered: Vec<OffsetDateTime> = times.to_vec();
    ordered.sort_unstable();
    ordered
        .windows(2)
        .filter_map(|pair| {
            let delta = pair[1] - pair[0];
            (delta > Duration::ZERO && delta <= idle).then(|| delta.whole_seconds())
        })
        .sum()
}

/// How agent wall-clock treats the gap between two consecutive events.
///
/// Agent time needs its own thresholds because `idle_seconds` is a *human
/// attention* rule: "nobody has touched the keyboard for 5 minutes, so they
/// stopped working". That inference is invalid for an agent, which is most
/// likely mid-build. Applying it to agent wall-clock discarded whole tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPolicy {
    /// Ceiling for a gap **not** opened by a tool call — model inference between
    /// tools, or nobody home. Wider gaps clamp to this rather than being
    /// discarded, so the live timer can predict the settled value instead of
    /// promising time the accountant will refuse.
    pub idle: Duration,
    /// Ceiling for a gap opened by a tool call. A sanity bound, not a working
    /// limit: it exists so a laptop that sleeps mid-call, or a session abandoned
    /// with an unmatched `PreTool`, cannot bank days.
    pub max_span: Duration,
}

impl Default for AgentPolicy {
    /// The shipped defaults, and the single source of truth behind
    /// `Config::agent_idle_seconds` / `Config::agent_max_span_seconds`.
    fn default() -> Self {
        Self {
            // Deliberately the same 5 minutes as the human threshold, so short
            // gaps are credited exactly as they always were and this change is
            // confined to the two cases that were actually wrong: a tool call
            // (now credited in full) and a long quiet stretch (now clamped
            // instead of discarded). A larger value here would hand an hour of
            // genuine silence a big slice of phantom "agent work" and skew the
            // agent-vs-human ratio the report exists to show.
            idle: Duration::minutes(5),
            max_span: Duration::hours(8),
        }
    }
}

/// One point on a single agent session's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSample {
    pub at: OffsetDateTime,
    /// [`crate::model::EventKind::opens_agent_span`] for the event at `at`.
    pub opens_span: bool,
}

/// The idle-trimmed active seconds over one agent session's timeline.
///
/// This is the single accumulator behind every agent-time surface — the daemon's
/// live registry, the local report, and the sync rollup — so they cannot drift
/// from one another or from what the dashboard displays.
///
/// Replaces a plain [`active_seconds`] over the same timestamps, which *dropped*
/// any gap wider than `idle`. Because a harness emits nothing during a tool call,
/// that gap **is** the tool call: a 7-minute build, or a 2-hour test suite, banked
/// zero seconds. Here a gap opened by a `PreTool` is credited in full (bounded by
/// `max_span`) and every other gap is clamped to `idle` instead of discarded.
pub fn agent_active_seconds(samples: &[AgentSample], policy: AgentPolicy) -> i64 {
    if samples.len() < 2 {
        return 0;
    }
    let mut ordered: Vec<AgentSample> = samples.to_vec();
    // Sort by time only, and stably: two events sharing a timestamp keep input
    // order, so a `PreTool` colliding with another event still opens the span.
    ordered.sort_by_key(|s| s.at);

    ordered
        .windows(2)
        .map(|pair| {
            let delta = pair[1].at - pair[0].at;
            if delta <= Duration::ZERO {
                return 0;
            }
            let ceiling = if pair[0].opens_span {
                policy.max_span
            } else {
                policy.idle
            };
            delta.min(ceiling).whole_seconds()
        })
        .sum()
}

/// The credit a *single* gap earns under [`agent_active_seconds`].
///
/// Exposed so the `watch` dashboard's live tail can be computed with the exact
/// same rule that will settle it. The tail is a prediction of the settled value;
/// deriving both from this function is what stops them diverging — the display
/// previously clamped an over-idle gap while the accountant discarded it, which
/// is what made the timer climb and then visibly reset.
pub fn agent_gap_seconds(gap: Duration, opens_span: bool, policy: AgentPolicy) -> i64 {
    if gap <= Duration::ZERO {
        return 0;
    }
    let ceiling = if opens_span {
        policy.max_span
    } else {
        policy.idle
    };
    gap.min(ceiling).whole_seconds()
}

/// Per-project breakdown of de-duplicated human seconds. The values sum exactly
/// to [`total_human_seconds`].
pub fn per_project_seconds(signals: &[Signal], idle: Duration) -> BTreeMap<Option<String>, i64> {
    // Delegate to the generic keyed attribution so project and session breakdowns
    // can never drift from one another (or from the grand total).
    let keyed: Vec<(OffsetDateTime, Option<String>)> =
        signals.iter().map(|s| (s.at, s.project.clone())).collect();
    per_key_seconds(&keyed, idle)
}

/// Per-*key* breakdown of de-duplicated, idle-trimmed human seconds, attributing
/// each counted gap to the key of the signal that **opens** it — the same v1
/// policy as [`per_project_seconds`], generalised to any grouping key (a project,
/// a session id, an identity, …).
///
/// `keyed` is `(timestamp, key)` for every human signal, in any order. Because the
/// counted gaps are exactly those of [`counted_gaps`] over the same timestamps —
/// consecutive, half-open, idle-trimmed — the returned values sum **exactly** to
/// [`total_human_seconds`]. So a per-session breakdown reconciles to the very same
/// grand total a per-project breakdown does; no wall-minute is counted twice, and
/// none is dropped.
pub fn per_key_seconds<K: Ord + Clone>(
    keyed: &[(OffsetDateTime, K)],
    idle: Duration,
) -> BTreeMap<K, i64> {
    if keyed.len() < 2 {
        return BTreeMap::new();
    }
    // Sort by time; ties keep input order (stable), matching `counted_gaps`.
    let mut ordered: Vec<&(OffsetDateTime, K)> = keyed.iter().collect();
    ordered.sort_by_key(|(at, _)| *at);

    let mut out: BTreeMap<K, i64> = BTreeMap::new();
    for pair in ordered.windows(2) {
        let (a_at, a_key) = pair[0];
        let (b_at, _) = pair[1];
        let delta = *b_at - *a_at;
        if delta > Duration::ZERO && delta <= idle {
            *out.entry(a_key.clone()).or_insert(0) += delta.whole_seconds();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const IDLE: Duration = Duration::minutes(5);

    fn sig(secs: i64, project: Option<&str>) -> Signal {
        Signal {
            at: datetime!(2026-06-27 10:00:00 UTC) + Duration::seconds(secs),
            project: project.map(str::to_string),
        }
    }

    #[test]
    fn single_signal_counts_nothing() {
        assert_eq!(total_human_seconds(&[sig(0, None)], IDLE), 0);
    }

    #[test]
    fn consecutive_signals_within_idle_are_counted() {
        // 0s, 60s, 120s within idle -> 120s total.
        let s = [sig(0, Some("a")), sig(60, Some("a")), sig(120, Some("a"))];
        assert_eq!(total_human_seconds(&s, IDLE), 120);
    }

    #[test]
    fn gaps_wider_than_idle_are_trimmed() {
        // 0s, then 10min later, then +60s. The 10min gap is idle (excluded).
        let s = [sig(0, Some("a")), sig(600, Some("a")), sig(660, Some("a"))];
        assert_eq!(total_human_seconds(&s, IDLE), 60);
    }

    #[test]
    fn one_human_two_sessions_same_minute_counts_once() {
        // Two concurrent sessions on the same project firing interleaved every 30s.
        // Deduped: a continuous 0..120 = 120s, NOT 240s.
        let s = [
            sig(0, Some("a")),
            sig(30, Some("a")),
            sig(60, Some("a")),
            sig(90, Some("a")),
            sig(120, Some("a")),
        ];
        assert_eq!(total_human_seconds(&s, IDLE), 120);
    }

    /// Coalescing thinning preserves active time when coalesce < idle: keeping
    /// every Nth event (N spacing < idle) leaves the total unchanged.
    #[test]
    fn active_seconds_invariant_under_coalescing() {
        let base = datetime!(2026-06-27 10:00:00 UTC);
        // Dense: 601 events, 1s apart -> 600s span, all gaps within idle.
        let dense: Vec<_> = (0..=600).map(|s| base + Duration::seconds(s)).collect();
        let dense_total = active_seconds(&dense, IDLE);
        assert_eq!(dense_total, 600);
        // Coalesced at 45s (< 300s idle): keep first, then only events >= 45s
        // after the last kept one.
        let coalesce = Duration::seconds(45);
        let mut kept = Vec::new();
        let mut last: Option<OffsetDateTime> = None;
        for &t in &dense {
            if last.map(|l| t - l >= coalesce).unwrap_or(true) {
                kept.push(t);
                last = Some(t);
            }
        }
        let coalesced_total = active_seconds(&kept, IDLE);
        // Within the coalesce tolerance of the dense value (the last partial
        // gap may be dropped); never inflated.
        assert!(coalesced_total <= dense_total);
        assert!(dense_total - coalesced_total <= 45);
    }

    #[test]
    fn per_key_attributes_each_gap_to_its_opening_session() {
        // The parallel-supervision case: two sessions, ONE prompt each, interleaved
        // with a third. Per session in isolation every session has < 2 signals, so
        // the old per-session human read 0 — but the global opening-signal
        // attribution credits each opener its gap.
        let keyed = [
            (at(0), "s_a"),  // opens 0..30 (a) — within idle
            (at(30), "s_b"), // opens 30..60 (b) — within idle
            (at(60), "s_a"), // opens 60..? — the last signal, no successor
        ];
        let by = per_key_seconds(&keyed, IDLE);
        assert_eq!(by[&"s_a"], 30); // just the 0..30 gap it opened
        assert_eq!(by[&"s_b"], 30); // the 30..60 gap it opened
                                    // Sums to the deduped grand total over the same timestamps.
        let total: i64 = by.values().sum();
        assert_eq!(total, active_seconds(&[at(0), at(30), at(60)], IDLE));
        assert_eq!(total, 60);
    }

    #[test]
    fn per_key_matches_per_project_for_the_project_key() {
        // per_project_seconds now delegates to per_key_seconds; prove they agree on
        // the project grouping so the refactor can't silently drift.
        let signals = [
            sig(0, Some("a")),
            sig(30, Some("b")),
            sig(60, Some("a")),
            sig(90, Some("a")),
        ];
        let by_project = per_project_seconds(&signals, IDLE);
        let keyed: Vec<_> = signals.iter().map(|s| (s.at, s.project.clone())).collect();
        let by_key = per_key_seconds(&keyed, IDLE);
        assert_eq!(by_project, by_key);
    }

    #[test]
    fn split_across_projects_sums_to_total() {
        // Project a: 0,30 ; project b: 60,90. Gaps: 0-30(a),30-60(a),60-90(b).
        let s = [
            sig(0, Some("a")),
            sig(30, Some("a")),
            sig(60, Some("b")),
            sig(90, Some("b")),
        ];
        let total = total_human_seconds(&s, IDLE);
        let by = per_project_seconds(&s, IDLE);
        assert_eq!(total, 90);
        assert_eq!(by.values().sum::<i64>(), total);
        assert_eq!(by[&Some("a".to_string())], 60); // gaps 0-30, 30-60
        assert_eq!(by[&Some("b".to_string())], 30); // gap 60-90
    }

    fn at(secs: i64) -> OffsetDateTime {
        datetime!(2026-06-27 10:00:00 UTC) + Duration::seconds(secs)
    }

    // --- agent wall-clock ---------------------------------------------------

    const AGENT: AgentPolicy = AgentPolicy {
        idle: Duration::minutes(15),
        max_span: Duration::hours(8),
    };

    /// `opens` marks the sample as a `PreTool` — the agent is busy from here.
    fn tool(secs: i64) -> AgentSample {
        AgentSample {
            at: at(secs),
            opens_span: true,
        }
    }

    fn other(secs: i64) -> AgentSample {
        AgentSample {
            at: at(secs),
            opens_span: false,
        }
    }

    /// The reported bug. A harness emits nothing while a tool runs, so the gap
    /// after `PreTool` IS the tool call — it must be banked, not discarded.
    #[test]
    fn a_two_hour_tool_call_is_credited_in_full() {
        let two_hours = 2 * 60 * 60;
        let samples = [tool(0), other(two_hours)];

        assert_eq!(agent_active_seconds(&samples, AGENT), two_hours);

        // What the old rule did with the very same timeline: the gap exceeded
        // `idle`, so it was filtered out entirely and two hours of real work
        // banked nothing. This is the regression being fixed.
        assert_eq!(active_seconds(&[at(0), at(two_hours)], IDLE), 0);
    }

    /// A `PostToolUse` lost in transit (exactly what an unreachable daemon
    /// causes on windows) must not zero the tool call it was closing. This is
    /// why the rule keys on the opening event, not on a matched pair.
    #[test]
    fn a_tool_call_with_a_dropped_post_event_is_still_credited() {
        let ninety_min = 90 * 60;
        // PreTool, then the next thing we ever hear is a prompt: no PostTool.
        let samples = [tool(0), other(ninety_min)];
        assert_eq!(agent_active_seconds(&samples, AGENT), ninety_min);
    }

    /// The sanity ceiling: an abandoned session (or a laptop asleep mid-call)
    /// cannot bank days.
    #[test]
    fn an_abandoned_tool_call_is_capped_not_unbounded() {
        let three_days = 3 * 24 * 60 * 60;
        let samples = [tool(0), other(three_days)];
        assert_eq!(
            agent_active_seconds(&samples, AGENT),
            AGENT.max_span.whole_seconds()
        );
    }

    /// A gap *not* opened by a tool call is model inference or nobody home —
    /// clamped, not discarded. Clamping (rather than dropping) is what lets the
    /// dashboard's live tail predict the settled value instead of reverting it.
    #[test]
    fn an_unbracketed_gap_is_clamped_not_discarded() {
        let forty_min = 40 * 60;
        let samples = [other(0), other(forty_min)];

        assert_eq!(
            agent_active_seconds(&samples, AGENT),
            AGENT.idle.whole_seconds()
        );
        // The old rule discarded it outright.
        assert_eq!(active_seconds(&[at(0), at(forty_min)], IDLE), 0);
    }

    /// Short gaps are untouched by the new ceilings — the common case is
    /// unchanged, whichever kind of event opened it.
    #[test]
    fn gaps_within_the_thresholds_are_credited_exactly() {
        assert_eq!(agent_active_seconds(&[other(0), other(60)], AGENT), 60);
        assert_eq!(agent_active_seconds(&[tool(0), other(60)], AGENT), 60);
    }

    #[test]
    fn agent_active_seconds_needs_two_samples() {
        assert_eq!(agent_active_seconds(&[], AGENT), 0);
        assert_eq!(agent_active_seconds(&[tool(0)], AGENT), 0);
    }

    #[test]
    fn agent_active_seconds_is_order_independent() {
        let a = [tool(0), other(120), other(60)];
        let b = [other(60), tool(0), other(120)];
        assert_eq!(agent_active_seconds(&a, AGENT), 120);
        assert_eq!(
            agent_active_seconds(&a, AGENT),
            agent_active_seconds(&b, AGENT)
        );
    }

    /// Duplicate timestamps contribute nothing and cannot go negative.
    #[test]
    fn simultaneous_events_contribute_nothing() {
        assert_eq!(agent_active_seconds(&[tool(0), other(0)], AGENT), 0);
    }

    /// The per-gap helper the dashboard shares must agree with the accumulator,
    /// or the live tail and the settled value diverge again.
    #[test]
    fn the_per_gap_helper_matches_the_accumulator() {
        for &(secs, opens) in &[
            (30i64, false),
            (30, true),
            (40 * 60, false),
            (2 * 60 * 60, true),
            (3 * 24 * 60 * 60, true),
        ] {
            let samples = [
                AgentSample {
                    at: at(0),
                    opens_span: opens,
                },
                other(secs),
            ];
            assert_eq!(
                agent_gap_seconds(Duration::seconds(secs), opens, AGENT),
                agent_active_seconds(&samples, AGENT),
                "gap {secs}s opens_span={opens}",
            );
        }
    }

    /// **The critical guard.** The billing base must not move while the agent
    /// rules change next to it: human accounting is untouched by any of this.
    #[test]
    fn human_time_is_unchanged_by_the_agent_rules() {
        // A timeline whose gaps straddle the human idle threshold.
        let signals = [
            sig(0, Some("a")),
            sig(120, Some("a")),
            sig(600, Some("a")), // 8min gap — idle, still excluded
            sig(660, Some("b")),
        ];
        // 0→120 counted, 120→600 excluded (8min > idle), 600→660 counted.
        assert_eq!(total_human_seconds(&signals, IDLE), 180);
        // Both counted gaps are opened by a signal on project "a".
        let by_project = per_project_seconds(&signals, IDLE);
        assert_eq!(by_project[&Some("a".to_string())], 180);
        assert_eq!(by_project.get(&Some("b".to_string())), None);
        // And the human rule still *discards* over-idle gaps rather than
        // clamping them — the agent change must not have leaked across.
        assert_eq!(
            total_human_seconds(&[sig(0, None), sig(600, None)], IDLE),
            0
        );
    }

    #[test]
    fn active_seconds_single_timestamp_is_zero() {
        assert_eq!(active_seconds(&[at(0)], IDLE), 0);
        assert_eq!(active_seconds(&[], IDLE), 0);
    }

    #[test]
    fn active_seconds_sums_gaps_within_idle() {
        // 0s, 60s, 120s all within idle -> 120s.
        assert_eq!(active_seconds(&[at(0), at(60), at(120)], IDLE), 120);
    }

    #[test]
    fn active_seconds_trims_idle_gaps() {
        // 0s, +10min (idle, excluded), +60s -> only the trailing 60s counts.
        assert_eq!(active_seconds(&[at(0), at(600), at(660)], IDLE), 60);
    }

    #[test]
    fn active_seconds_is_order_independent() {
        // Same timestamps, shuffled, sort internally -> same result.
        assert_eq!(active_seconds(&[at(120), at(0), at(60)], IDLE), 120);
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;
    use time::{Duration, OffsetDateTime};

    // A signal is (offset_seconds, project_index).
    prop_compose! {
        fn arb_signal()(secs in 0i64..100_000, proj in 0u8..4) -> (i64, u8) {
            (secs, proj)
        }
    }

    fn build(raw: &[(i64, u8)]) -> Vec<Signal> {
        let base = OffsetDateTime::UNIX_EPOCH;
        raw.iter()
            .map(|(s, p)| Signal {
                at: base + Duration::seconds(*s),
                project: Some(format!("p{p}")),
            })
            .collect()
    }

    fn build_agent(raw: &[(i64, bool)]) -> Vec<AgentSample> {
        let base = OffsetDateTime::UNIX_EPOCH;
        let mut samples: Vec<AgentSample> = raw
            .iter()
            .map(|(s, opens)| AgentSample {
                at: base + Duration::seconds(*s),
                opens_span: *opens,
            })
            .collect();
        samples.sort_by_key(|s| s.at);
        samples
    }

    proptest! {
        /// The agent rules CLAMP, they never discard. Whatever the gap widths, the
        /// credited total is at least what a strict per-gap idle cap would give —
        /// which is the property the pre-fix code violated by dropping over-idle
        /// gaps to zero.
        #[test]
        fn agent_time_is_clamped_never_discarded(
            raw in prop::collection::vec((0i64..100_000, any::<bool>()), 0..200)
        ) {
            let samples = build_agent(&raw);
            let policy = AgentPolicy::default();
            let got = agent_active_seconds(&samples, policy);

            let floor: i64 = samples
                .windows(2)
                .map(|w| {
                    let d = (w[1].at - w[0].at).whole_seconds();
                    d.clamp(0, policy.idle.whole_seconds())
                })
                .sum();
            prop_assert!(
                got >= floor,
                "agent time must never fall below the per-gap idle clamp: {got} < {floor}"
            );
        }

        /// No single gap may contribute more than its own ceiling, so an abandoned
        /// tool call cannot run away with the clock.
        #[test]
        fn no_agent_gap_exceeds_its_ceiling(
            raw in prop::collection::vec((0i64..100_000, any::<bool>()), 0..200)
        ) {
            let samples = build_agent(&raw);
            let policy = AgentPolicy::default();
            let ceiling: i64 = samples
                .windows(2)
                .map(|w| {
                    if w[0].opens_span {
                        policy.max_span.whole_seconds()
                    } else {
                        policy.idle.whole_seconds()
                    }
                })
                .sum();
            prop_assert!(agent_active_seconds(&samples, policy) <= ceiling);
        }

        /// Agent accounting must never move human accounting.
        #[test]
        fn agent_rules_do_not_touch_human_time(
            raw in prop::collection::vec(arb_signal(), 0..200)
        ) {
            let signals = build(&raw);
            let idle = Duration::minutes(5);
            let before = counted_gaps(&signals, idle);
            // Same timeline read as agent samples — must not disturb the human sum.
            let _ = agent_active_seconds(
                &signals
                    .iter()
                    .map(|s| AgentSample { at: s.at, opens_span: false })
                    .collect::<Vec<_>>(),
                AgentPolicy::default(),
            );
            prop_assert_eq!(before.len(), counted_gaps(&signals, idle).len());
        }

        /// Counted gaps never overlap — the no-double-count guarantee.
        #[test]
        fn gaps_never_overlap(raw in prop::collection::vec(arb_signal(), 0..200)) {
            let signals = build(&raw);
            let gaps = counted_gaps(&signals, Duration::minutes(5));
            for w in gaps.windows(2) {
                prop_assert!(w[0].end <= w[1].start, "gaps overlap: {:?} then {:?}", w[0], w[1]);
            }
        }

        /// The deduped total never exceeds the wall-clock span of all signals.
        #[test]
        fn total_within_span(raw in prop::collection::vec(arb_signal(), 0..200)) {
            let signals = build(&raw);
            let total = total_human_seconds(&signals, Duration::minutes(5));
            if let (Some(min), Some(max)) =
                (signals.iter().map(|s| s.at).min(), signals.iter().map(|s| s.at).max())
            {
                prop_assert!(total <= (max - min).whole_seconds());
            } else {
                prop_assert_eq!(total, 0);
            }
        }

        /// Per-project breakdown always sums to the grand total.
        #[test]
        fn per_project_sums_to_total(raw in prop::collection::vec(arb_signal(), 0..200)) {
            let signals = build(&raw);
            let idle = Duration::minutes(5);
            let total = total_human_seconds(&signals, idle);
            let by: i64 = per_project_seconds(&signals, idle).values().sum();
            prop_assert_eq!(total, by);
        }

        /// No counted gap is ever wider than the idle threshold — idle-trim holds.
        #[test]
        fn no_gap_exceeds_idle(raw in prop::collection::vec(arb_signal(), 0..200)) {
            let idle = Duration::minutes(5);
            let gaps = counted_gaps(&build(&raw), idle);
            for g in &gaps {
                prop_assert!((g.end - g.start) <= idle);
            }
        }

        /// active_seconds never exceeds the wall-clock span of the timestamps.
        #[test]
        fn active_within_span(raw in prop::collection::vec(0i64..100_000, 0..200)) {
            let times: Vec<OffsetDateTime> = raw
                .iter()
                .map(|s| OffsetDateTime::UNIX_EPOCH + Duration::seconds(*s))
                .collect();
            let active = active_seconds(&times, Duration::minutes(5));
            if let (Some(min), Some(max)) = (times.iter().min(), times.iter().max()) {
                prop_assert!(active <= (*max - *min).whole_seconds());
            } else {
                prop_assert_eq!(active, 0);
            }
        }

        /// No single gap folded into active_seconds exceeds idle — so a sparse
        /// timeline (all gaps > idle) reads as 0, and the total is at most
        /// `(n-1) * idle`.
        #[test]
        fn active_bounded_by_idle_per_gap(raw in prop::collection::vec(0i64..100_000, 0..200)) {
            let times: Vec<OffsetDateTime> = raw
                .iter()
                .map(|s| OffsetDateTime::UNIX_EPOCH + Duration::seconds(*s))
                .collect();
            let idle = Duration::minutes(5);
            let active = active_seconds(&times, idle);
            let n = times.len();
            let max_possible = (n.saturating_sub(1) as i64) * idle.whole_seconds();
            prop_assert!(active <= max_possible);
        }

        /// A sparse timeline — every sample spaced wider than idle — reads as 0.
        #[test]
        fn active_sparse_reads_zero(n in 0usize..50, spacing in 301i64..10_000) {
            let idle = Duration::minutes(5); // 300s
            let times: Vec<OffsetDateTime> = (0..n)
                .map(|i| OffsetDateTime::UNIX_EPOCH + Duration::seconds(i as i64 * spacing))
                .collect();
            prop_assert_eq!(active_seconds(&times, idle), 0);
        }

        /// Densifying within idle is monotonic: inserting an extra sample inside an
        /// already-counted gap never decreases the active total (the split gaps
        /// stay within idle, so they still count and sum to the same or more).
        #[test]
        fn active_monotonic_with_denser_sampling(
            a in 0i64..1000, gap1 in 1i64..200, gap2 in 1i64..100,
        ) {
            let idle = Duration::minutes(5);
            let t0 = OffsetDateTime::UNIX_EPOCH + Duration::seconds(a);
            let t2 = t0 + Duration::seconds(gap1 + gap2);
            let sparse = [t0, t2];
            let t1 = t0 + Duration::seconds(gap1);
            let dense = [t0, t1, t2];
            prop_assert!(active_seconds(&dense, idle) >= active_seconds(&sparse, idle));
        }
    }
}
