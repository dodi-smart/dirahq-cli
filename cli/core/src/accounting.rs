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

/// Per-project breakdown of de-duplicated human seconds. The values sum exactly
/// to [`total_human_seconds`].
pub fn per_project_seconds(signals: &[Signal], idle: Duration) -> BTreeMap<Option<String>, i64> {
    let mut out: BTreeMap<Option<String>, i64> = BTreeMap::new();
    for gap in counted_gaps(signals, idle) {
        *out.entry(gap.project.clone()).or_insert(0) += gap.seconds();
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

    proptest! {
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
