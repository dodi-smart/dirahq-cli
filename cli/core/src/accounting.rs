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
//!
//! ## Meetings
//! A detected meeting is a wall-clock *span*, not a signal: a person listening in
//! a call emits nothing. [`overlay`] joins meeting spans with the counted gaps so
//! that every second is either work or meeting, never both, under a
//! [`Precedence`] decided per overlapping pair. With no spans it returns the gaps
//! unchanged. Once meetings are wired in, "human time" for a billing or reporting
//! surface means the overlay's total, not the bare sum of gaps (DIRASH-0063).

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
    // One gap loop for every caller (`counted_gaps_keyed`), so the plain,
    // per-key and keyed views can never drift apart.
    let keyed: Vec<(OffsetDateTime, Option<String>)> =
        signals.iter().map(|s| (s.at, s.project.clone())).collect();
    counted_gaps_keyed(&keyed, idle)
        .into_iter()
        .map(|g| CountedGap {
            start: g.start,
            end: g.end,
            project: g.key,
        })
        .collect()
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
    let mut out: BTreeMap<K, i64> = BTreeMap::new();
    for g in counted_gaps_keyed(keyed, idle) {
        let secs = g.seconds();
        *out.entry(g.key).or_insert(0) += secs;
    }
    out
}

// ---------------------------------------------------------------------------
// Meetings: spans overlaid on counted human time (DIRASH-0063, proposed)
// ---------------------------------------------------------------------------
//
// A detected meeting is a *span* of wall-clock time with no human signals in it:
// a person listening in a call touches nothing. It cannot enter the gap model
// above without synthesising signals, and synthetic ticks can express neither a
// precedence rule nor a retraction. So meetings stay spans, and [`overlay`]
// joins them with the counted gaps: every wall-clock second ends up in exactly
// one [`Slot`] — `Work` or `Meeting` — and the seconds where engaged work and a
// meeting coincide are reported separately as `engaged_during_meeting_seconds`,
// never added twice. Agent time is not involved: it stays per session and is
// never billed as human time.

/// Who owns a wall-clock second where a meeting and counted work overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precedence {
    /// The second is meeting time; the overlapping work is "engaged during meeting".
    MeetingWins,
    /// The second stays work; only the meeting's otherwise idle seconds are meeting time.
    WorkWins,
}

/// A counted gap carrying an arbitrary grouping key (a project, a line key, a
/// session id). The same gaps [`counted_gaps`] produces, keyed by the signal that
/// opens each one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedGap<K> {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub key: K,
}

impl<K> KeyedGap<K> {
    pub fn seconds(&self) -> i64 {
        (self.end - self.start).whole_seconds()
    }
}

/// [`counted_gaps`] for any key: the gaps whose per-key sums are exactly
/// [`per_key_seconds`]. Ties keep input order, matching both.
pub fn counted_gaps_keyed<K: Clone>(
    keyed: &[(OffsetDateTime, K)],
    idle: Duration,
) -> Vec<KeyedGap<K>> {
    if keyed.len() < 2 {
        return Vec::new();
    }
    let mut ordered: Vec<&(OffsetDateTime, K)> = keyed.iter().collect();
    ordered.sort_by_key(|(at, _)| *at);
    ordered
        .windows(2)
        .filter_map(|pair| {
            let (a_at, a_key) = pair[0];
            let (b_at, _) = pair[1];
            let delta = *b_at - *a_at;
            (delta > Duration::ZERO && delta <= idle).then(|| KeyedGap {
                start: *a_at,
                end: *b_at,
                key: a_key.clone(),
            })
        })
        .collect()
}

/// A meeting's wall-clock span `[start, end)`. Not idle-trimmed: its bounds are
/// the detector's (or the person's, for a confirmed suggestion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingSpan<M> {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub key: M,
}

/// The owner of a slice of wall-clock time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot<W, M> {
    Work(W),
    Meeting(M),
}

/// A non-overlapping piece of the overlaid timeline. `seconds` is its credited
/// whole seconds; it can differ from `end - start` by under a second, so that
/// cutting a gap never loses or invents seconds to truncation (see [`overlay`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice<W, M> {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub slot: Slot<W, M>,
    pub seconds: i64,
}

/// The result of [`overlay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay<W, M> {
    /// Sorted by start, never overlapping.
    pub slices: Vec<Slice<W, M>>,
    /// Seconds where counted work and a meeting coincided. Reported, never added.
    pub engaged_during_meeting_seconds: i64,
}

impl<W, M> Overlay<W, M> {
    /// The de-duplicated human total: work plus meeting time, each second once.
    pub fn total_seconds(&self) -> i64 {
        self.slices.iter().map(|s| s.seconds).sum()
    }

    pub fn work_seconds(&self) -> i64 {
        self.slices
            .iter()
            .filter(|s| matches!(s.slot, Slot::Work(_)))
            .map(|s| s.seconds)
            .sum()
    }

    pub fn meeting_seconds(&self) -> i64 {
        self.slices
            .iter()
            .filter(|s| matches!(s.slot, Slot::Meeting(_)))
            .map(|s| s.seconds)
            .sum()
    }
}

impl<W: Ord + Clone, M: Ord + Clone> Overlay<W, M> {
    /// Seconds per slot. Sums exactly to [`Overlay::total_seconds`].
    pub fn per_slot_seconds(&self) -> BTreeMap<Slot<W, M>, i64> {
        let mut out = BTreeMap::new();
        for s in &self.slices {
            *out.entry(s.slot.clone()).or_insert(0) += s.seconds;
        }
        out
    }
}

/// Make meetings disjoint. Bounds are snapped **inward** to whole seconds (start
/// up, end down; a detector's bounds are coarser than that anyway), so a snapped
/// meeting never claims a moment outside the real one; empty or inverted spans
/// are dropped. Where meetings overlap each other (a call and a huddle at once),
/// every second belongs to the meeting that **started first**; ties go to the
/// lower key, then to the longer span, so the result never depends on input
/// order. Meetings never double count among themselves. Returns sorted, disjoint
/// pieces, each inside the span it came from, with touching same-key pieces
/// merged.
pub fn normalize_spans<M: Ord + Clone>(spans: &[MeetingSpan<M>]) -> Vec<MeetingSpan<M>> {
    let mut ordered: Vec<MeetingSpan<M>> = spans
        .iter()
        .map(|s| MeetingSpan {
            start: ceil_second(s.start),
            end: floor_second(s.end),
            key: s.key.clone(),
        })
        .filter(|s| s.end > s.start)
        .collect();
    ordered.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| b.end.cmp(&a.end))
    });
    let mut out: Vec<MeetingSpan<M>> = Vec::with_capacity(ordered.len());
    let mut covered_until: Option<OffsetDateTime> = None;
    for s in ordered {
        // Every earlier span started at or before `s.start`, so what they cover
        // past `s.start` is the single run `[s.start, covered_until)`.
        let start = match covered_until {
            Some(c) if c > s.start => c,
            _ => s.start,
        };
        covered_until = Some(covered_until.map_or(s.end, |c| c.max(s.end)));
        if s.end <= start {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.key == s.key && last.end == start => last.end = s.end,
            _ => out.push(MeetingSpan {
                start,
                end: s.end,
                key: s.key,
            }),
        }
    }
    out
}

/// Round an instant down to its whole second.
fn floor_second(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_nanosecond(0).unwrap_or(t)
}

/// Round an instant up to its whole second.
fn ceil_second(t: OffsetDateTime) -> OffsetDateTime {
    let floor = floor_second(t);
    if floor == t {
        t
    } else {
        floor + Duration::SECOND
    }
}

/// Overlay meeting spans on counted work gaps.
///
/// `gaps` must be disjoint, as [`counted_gaps_keyed`] produces them (one person's
/// global timeline). `precedence` decides each overlap from the pair that meets
/// there, so a caller can let a workspace choose only for its own meetings and
/// work, and fix the rule across workspaces (DIRASH-0063: a meeting wins there).
///
/// Every wall-clock second covered by a gap or a meeting lands in exactly one
/// slice. Rounding never inflates: each owner keeps the floor of the time it
/// actually retains, and what it gives up is the rest of its whole seconds. So
/// `work + meeting + engaged_during_meeting == Σ gap seconds + Σ meeting seconds`
/// holds as an integer identity, the total never exceeds the floor of the
/// wall-clock union, and with no spans the output is exactly the gaps.
///
/// # Panics
/// If `gaps` overlap. Gaps from [`counted_gaps_keyed`] over one person's whole
/// timeline never do; concatenating per-session gap lists would, and would
/// double count, so that is refused loudly rather than billed.
pub fn overlay<W: Clone, M: Ord + Clone>(
    gaps: &[KeyedGap<W>],
    spans: &[MeetingSpan<M>],
    precedence: impl Fn(&W, &M) -> Precedence,
) -> Overlay<W, M> {
    let pieces = normalize_spans(spans);
    let mut ordered: Vec<&KeyedGap<W>> = gaps.iter().filter(|g| g.end > g.start).collect();
    ordered.sort_by_key(|g| g.start);
    assert!(
        ordered.windows(2).all(|w| w[0].end <= w[1].start),
        "overlay needs disjoint gaps: build them with counted_gaps_keyed over the whole timeline"
    );

    // Regions each side gives up. Gaps are iterated in order and pieces are
    // disjoint and sorted, so every list below comes out sorted and disjoint.
    let mut gap_cuts: Vec<Vec<(OffsetDateTime, OffsetDateTime)>> = vec![Vec::new(); ordered.len()];
    let mut piece_cuts: Vec<Vec<(OffsetDateTime, OffsetDateTime)>> = vec![Vec::new(); pieces.len()];
    for (gi, g) in ordered.iter().enumerate() {
        let first = pieces.partition_point(|p| p.end <= g.start);
        for (pi, p) in pieces.iter().enumerate().skip(first) {
            if p.start >= g.end {
                break;
            }
            let (s, e) = (g.start.max(p.start), g.end.min(p.end));
            if e <= s {
                continue;
            }
            match precedence(&g.key, &p.key) {
                Precedence::MeetingWins => gap_cuts[gi].push((s, e)),
                Precedence::WorkWins => piece_cuts[pi].push((s, e)),
            }
        }
    }

    let mut slices = Vec::with_capacity(ordered.len() + pieces.len());
    let mut overlap = 0i64;
    for (g, cuts) in ordered.iter().zip(&gap_cuts) {
        overlap += emit_remainder(g.start, g.end, cuts, Slot::Work(g.key.clone()), &mut slices);
    }
    for (p, cuts) in pieces.iter().zip(&piece_cuts) {
        overlap += emit_remainder(
            p.start,
            p.end,
            cuts,
            Slot::Meeting(p.key.clone()),
            &mut slices,
        );
    }
    slices.sort_by_key(|s| s.start);
    Overlay {
        slices,
        engaged_during_meeting_seconds: overlap,
    }
}

/// Push the parts of `[start, end)` not in `cuts` (sorted, disjoint, inside it)
/// as slices for `slot`. The slot keeps `floor(Σ parts)` seconds, spread over
/// its parts (each floored, the remainder on the last); it gives up the rest of
/// `whole_seconds(end - start)`, which is returned. Keeping the floor of what is
/// retained (never the floor of what is given up) is what stops rounding from
/// billing more than the wall clock.
fn emit_remainder<W: Clone, M: Clone>(
    start: OffsetDateTime,
    end: OffsetDateTime,
    cuts: &[(OffsetDateTime, OffsetDateTime)],
    slot: Slot<W, M>,
    out: &mut Vec<Slice<W, M>>,
) -> i64 {
    let whole = (end - start).whole_seconds();

    let mut cursor = start;
    let mut parts: Vec<(OffsetDateTime, OffsetDateTime)> = Vec::with_capacity(cuts.len() + 1);
    for (s, e) in cuts {
        if *s > cursor {
            parts.push((cursor, *s));
        }
        cursor = cursor.max(*e);
    }
    if end > cursor {
        parts.push((cursor, end));
    }

    let retained: Duration = parts.iter().map(|(s, e)| *e - *s).sum();
    let kept = retained.whole_seconds();
    let mut remainder = kept;
    let first_new = out.len();
    for (s, e) in parts {
        let secs = (e - s).whole_seconds();
        remainder -= secs;
        out.push(Slice {
            start: s,
            end: e,
            slot: slot.clone(),
            seconds: secs,
        });
    }
    // Flooring each part loses under a second per part; that remainder goes to
    // the last part so the slot keeps exactly `kept`. With no parts, kept is 0.
    if let Some(last) = out[first_new..].last_mut() {
        last.seconds += remainder;
    }
    whole - kept
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

#[cfg(test)]
mod overlay_tests {
    use super::*;
    use proptest::prelude::*;
    use time::{Duration, OffsetDateTime};

    type O = Overlay<&'static str, &'static str>;

    fn t(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs)
    }

    fn gap(a: i64, b: i64, k: &'static str) -> KeyedGap<&'static str> {
        KeyedGap {
            start: t(a),
            end: t(b),
            key: k,
        }
    }

    fn span(a: i64, b: i64, k: &'static str) -> MeetingSpan<&'static str> {
        MeetingSpan {
            start: t(a),
            end: t(b),
            key: k,
        }
    }

    fn meeting_wins(_: &&str, _: &&str) -> Precedence {
        Precedence::MeetingWins
    }

    fn work_wins(_: &&str, _: &&str) -> Precedence {
        Precedence::WorkWins
    }

    fn secs(o: &O, slot: Slot<&'static str, &'static str>) -> i64 {
        o.per_slot_seconds().get(&slot).copied().unwrap_or(0)
    }

    /// Reviewer scenario 1: two concurrent sessions (P at 0/60/120, Q at 30/90)
    /// and a meeting M over [45,100).
    #[test]
    fn two_sessions_and_a_meeting() {
        let keyed = [
            (t(0), "P"),
            (t(60), "P"),
            (t(120), "P"),
            (t(30), "Q"),
            (t(90), "Q"),
        ];
        let gaps = counted_gaps_keyed(&keyed, Duration::minutes(5));
        let spans = [span(45, 100, "M")];

        let o = overlay(&gaps, &spans, meeting_wins);
        assert_eq!(o.total_seconds(), 120);
        assert_eq!(secs(&o, Slot::Work("P")), 30);
        assert_eq!(secs(&o, Slot::Work("Q")), 35);
        assert_eq!(secs(&o, Slot::Meeting("M")), 55);
        assert_eq!(o.engaged_during_meeting_seconds, 55);

        let o = overlay(&gaps, &spans, work_wins);
        assert_eq!(o.total_seconds(), 120);
        assert_eq!(o.work_seconds(), 120);
        assert_eq!(o.meeting_seconds(), 0);
        assert_eq!(o.engaged_during_meeting_seconds, 55);
    }

    /// Reviewer scenario 3: a manual session (P) crossing a meeting.
    #[test]
    fn manual_session_crossing_a_meeting() {
        let gaps = [
            gap(0, 240, "P"),
            gap(240, 480, "P"),
            gap(480, 720, "P"),
            gap(720, 960, "P"),
            gap(960, 1200, "P"),
        ];
        let spans = [span(600, 2400, "M")];
        let mw = overlay(&gaps, &spans, meeting_wins);
        assert_eq!((mw.work_seconds(), mw.meeting_seconds()), (600, 1800));
        let ww = overlay(&gaps, &spans, work_wins);
        assert_eq!((ww.work_seconds(), ww.meeting_seconds()), (1200, 1200));
        assert_eq!(mw.total_seconds(), ww.total_seconds());
    }

    /// Reviewer scenario 4 (H1): the precedence is decided per pair, so a meeting
    /// in one workspace and work routed to another are never both counted.
    #[test]
    fn cross_workspace_overlap_counts_once() {
        // Meeting for W1 (meeting wins there), work for W2 (work wins there).
        let gaps = [
            gap(900, 1200, "w2:oss"),
            gap(1200, 1500, "w2:oss"),
            gap(1500, 1800, "w2:oss"),
        ];
        let spans = [span(0, 3600, "w1:acme")];
        let rule = |w: &&str, m: &&str| {
            if w.split(':').next() == m.split(':').next() {
                Precedence::WorkWins
            } else {
                Precedence::MeetingWins
            }
        };
        let o = overlay(&gaps, &spans, rule);
        assert_eq!(o.total_seconds(), 3600);
        assert_eq!(o.meeting_seconds(), 3600);
        assert_eq!(o.engaged_during_meeting_seconds, 900);
    }

    #[test]
    fn no_spans_is_the_identity() {
        let gaps = [gap(0, 60, "P"), gap(60, 90, "Q")];
        let o = overlay::<_, &str>(&gaps, &[], meeting_wins);
        assert_eq!(o.engaged_during_meeting_seconds, 0);
        let got: Vec<_> = o
            .slices
            .iter()
            .map(|s| (s.start, s.end, s.slot.clone(), s.seconds))
            .collect();
        assert_eq!(
            got,
            vec![
                (t(0), t(60), Slot::Work("P"), 60),
                (t(60), t(90), Slot::Work("Q"), 30)
            ]
        );
    }

    #[test]
    fn overlapping_meetings_go_to_the_one_that_started_first() {
        let pieces = normalize_spans(&[
            span(100, 200, "B"),
            span(0, 150, "A"),
            span(300, 290, "bad"),
        ]);
        assert_eq!(pieces, vec![span(0, 150, "A"), span(150, 200, "B")]);
        // Same start: the lower key wins.
        let pieces = normalize_spans(&[span(0, 100, "Z"), span(0, 50, "A")]);
        assert_eq!(pieces, vec![span(0, 50, "A"), span(50, 100, "Z")]);
        // Fully contained later meeting gets nothing.
        assert_eq!(
            normalize_spans(&[span(0, 100, "A"), span(10, 20, "B")]),
            vec![span(0, 100, "A")]
        );
    }

    /// Cutting a gap in sub-second pieces never loses or invents seconds, and
    /// meeting bounds snap inward to whole seconds.
    #[test]
    fn sub_second_cuts_keep_whole_seconds() {
        let ms = |m: i64| OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(m);
        let gaps = [KeyedGap {
            start: ms(0),
            end: ms(10_900),
            key: "P",
        }]; // 10 s credited
        let spans = [
            MeetingSpan {
                start: ms(1_500),
                end: ms(3_200),
                key: "A",
            }, // -> [2, 3)
            MeetingSpan {
                start: ms(5_300),
                end: ms(7_900),
                key: "B",
            }, // -> [6, 7)
        ];
        let o = overlay(&gaps, &spans, meeting_wins);
        // The gap retains 8.9 s -> keeps 8 and gives up 2 of its 10.
        assert_eq!(o.work_seconds(), 8);
        assert_eq!(o.engaged_during_meeting_seconds, 2);
        assert_eq!(o.meeting_seconds(), 2);
        assert_eq!(
            o.work_seconds() + o.meeting_seconds() + o.engaged_during_meeting_seconds,
            10 + 2
        );
    }

    /// Reviewer case: rounding the loss down used to bill 2 s for 1.5 s of wall clock.
    #[test]
    fn rounding_never_bills_more_than_the_wall_clock() {
        let ms = |m: i64| OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(m);
        let gaps = [KeyedGap {
            start: ms(0),
            end: ms(1_000),
            key: "P",
        }];
        let spans = [MeetingSpan {
            start: ms(500),
            end: ms(1_500),
            key: "M",
        }]; // -> [0, 1)
        for rule in [meeting_wins as fn(&&str, &&str) -> Precedence, work_wins] {
            assert!(overlay(&gaps, &spans, rule).total_seconds() <= 1);
        }
    }

    #[test]
    fn same_start_same_key_is_order_independent() {
        let ms = |m: i64| OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(m);
        let a = MeetingSpan {
            start: ms(0),
            end: ms(101_000),
            key: "x",
        };
        let b = MeetingSpan {
            start: ms(0),
            end: ms(50_500),
            key: "x",
        };
        assert_eq!(
            normalize_spans(&[a.clone(), b.clone()]),
            normalize_spans(&[b, a])
        );
    }

    #[test]
    fn touching_same_key_meetings_merge() {
        assert_eq!(
            normalize_spans(&[span(0, 10, "a"), span(10, 20, "a")]),
            vec![span(0, 20, "a")]
        );
    }

    /// Reviewer probes E/E2/C: inward snapping never bills time outside the meeting.
    #[test]
    fn meetings_snap_inward() {
        let ms = |m: i64| OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(m);
        let m = |a, b| MeetingSpan {
            start: ms(a),
            end: ms(b),
            key: "M",
        };
        assert_eq!(normalize_spans(&[m(750, 1_250)]), vec![]);
        assert_eq!(normalize_spans(&[m(-200, 3_000)])[0].start, ms(0));
        let gaps = [KeyedGap {
            start: ms(0),
            end: ms(10_400),
            key: "P",
        }];
        let o = overlay(&gaps, &[m(10_600, 20_000)], meeting_wins);
        assert!(o.total_seconds() <= 19);
        assert_eq!(o.engaged_during_meeting_seconds, 0);
    }

    // ---- properties ---------------------------------------------------------

    const KEYS: [&str; 4] = ["a", "b", "c", "d"];

    fn arb_gaps() -> impl Strategy<Value = Vec<KeyedGap<&'static str>>> {
        // Signals on a global timeline, ms resolution, keyed; gaps as counted_gaps makes them.
        prop::collection::vec((0i64..200_000_000, 0usize..4), 0..120).prop_map(|raw| {
            let keyed: Vec<_> = raw
                .iter()
                .map(|(ms, k)| {
                    (
                        OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(*ms),
                        KEYS[*k],
                    )
                })
                .collect();
            counted_gaps_keyed(&keyed, Duration::minutes(5))
        })
    }

    fn arb_spans() -> impl Strategy<Value = Vec<MeetingSpan<&'static str>>> {
        prop::collection::vec(
            (0i64..200_000_000, -50_000i64..20_000_000, 0usize..4),
            0..12,
        )
        .prop_map(|raw| {
            raw.iter()
                .map(|(s, len, k)| MeetingSpan {
                    start: OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(*s),
                    end: OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(*s + *len),
                    key: KEYS[*k],
                })
                .collect()
        })
    }

    /// Whole-second variant, where the union measure is exact.
    fn arb_gaps_s() -> impl Strategy<Value = Vec<KeyedGap<&'static str>>> {
        prop::collection::vec((0i64..200_000, 0usize..4), 0..120).prop_map(|raw| {
            let keyed: Vec<_> = raw.iter().map(|(s, k)| (t(*s), KEYS[*k])).collect();
            counted_gaps_keyed(&keyed, Duration::minutes(5))
        })
    }

    fn arb_spans_s() -> impl Strategy<Value = Vec<MeetingSpan<&'static str>>> {
        prop::collection::vec((0i64..200_000, -50i64..20_000, 0usize..4), 0..12).prop_map(|raw| {
            raw.iter()
                .map(|(s, len, k)| span_owned(*s, *s + *len, KEYS[*k]))
                .collect()
        })
    }

    fn span_owned(a: i64, b: i64, k: &'static str) -> MeetingSpan<&'static str> {
        MeetingSpan {
            start: t(a),
            end: t(b),
            key: k,
        }
    }

    /// Measure of the union of intervals, in whole seconds (inputs whole-second).
    fn union_seconds(mut v: Vec<(OffsetDateTime, OffsetDateTime)>) -> i64 {
        v.retain(|(s, e)| e > s);
        v.sort();
        let mut total = 0;
        let mut cur: Option<(OffsetDateTime, OffsetDateTime)> = None;
        for (s, e) in v {
            match cur {
                Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
                Some((cs, ce)) => {
                    total += (ce - cs).whole_seconds();
                    cur = Some((s, e));
                }
                None => cur = Some((s, e)),
            }
        }
        if let Some((cs, ce)) = cur {
            total += (ce - cs).whole_seconds();
        }
        total
    }

    /// Dense generators: a 0–20 s window at 250 ms steps, so equal starts,
    /// touching edges and spans inside one gap are common (§2.5 #14).
    fn ms250(steps: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(steps * 250)
    }

    fn arb_dense_gaps() -> impl Strategy<Value = Vec<KeyedGap<&'static str>>> {
        prop::collection::vec((0i64..80, 0usize..3), 0..30).prop_map(|raw| {
            let keyed: Vec<_> = raw.iter().map(|(s, k)| (ms250(*s), KEYS[*k])).collect();
            counted_gaps_keyed(&keyed, Duration::seconds(4))
        })
    }

    fn arb_dense_spans() -> impl Strategy<Value = Vec<MeetingSpan<&'static str>>> {
        prop::collection::vec((0i64..80, -4i64..40, 0usize..3), 0..6).prop_map(|raw| {
            raw.iter()
                .map(|(s, len, k)| MeetingSpan {
                    start: ms250(*s),
                    end: ms250(*s + *len),
                    key: KEYS[*k],
                })
                .collect()
        })
    }

    fn union_nanos(mut v: Vec<(OffsetDateTime, OffsetDateTime)>) -> i128 {
        v.retain(|(s, e)| e > s);
        v.sort();
        let mut total = 0i128;
        let mut cur: Option<(OffsetDateTime, OffsetDateTime)> = None;
        for (s, e) in v {
            match cur {
                Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
                Some((cs, ce)) => {
                    total += (ce - cs).whole_nanoseconds();
                    cur = Some((s, e));
                }
                None => cur = Some((s, e)),
            }
        }
        if let Some((cs, ce)) = cur {
            total += (ce - cs).whole_nanoseconds();
        }
        total
    }

    fn mixed(w: &&str, m: &&str) -> Precedence {
        // A deterministic per-pair rule, standing in for "workspace decides its own".
        if (w.as_bytes()[0] + m.as_bytes()[0]).is_multiple_of(2) {
            Precedence::MeetingWins
        } else {
            Precedence::WorkWins
        }
    }

    proptest! {
        /// Slices never overlap and are never empty — the no-double-count guarantee.
        #[test]
        fn slices_are_disjoint(gaps in arb_gaps(), spans in arb_spans()) {
            for rule in [meeting_wins as fn(&&str, &&str) -> Precedence, work_wins, mixed] {
                let o = overlay(&gaps, &spans, rule);
                for s in &o.slices {
                    prop_assert!(s.end > s.start);
                    prop_assert!(s.seconds >= 0);
                }
                for w in o.slices.windows(2) {
                    prop_assert!(w[0].end <= w[1].start, "overlap: {:?} / {:?}", w[0], w[1]);
                }
            }
        }

        /// Work + meeting + engaged-during-meeting is exactly the gaps plus the
        /// normalized meetings, even with sub-second cuts.
        #[test]
        fn accounting_identity_is_exact(gaps in arb_gaps(), spans in arb_spans()) {
            let g: i64 = gaps.iter().map(KeyedGap::seconds).sum();
            let m: i64 = normalize_spans(&spans).iter().map(|p| (p.end - p.start).whole_seconds()).sum();
            for rule in [meeting_wins as fn(&&str, &&str) -> Precedence, work_wins, mixed] {
                let o = overlay(&gaps, &spans, rule);
                prop_assert_eq!(o.work_seconds() + o.meeting_seconds() + o.engaged_during_meeting_seconds, g + m);
            }
        }

        /// The total is the union of work and meetings, whatever the precedence.
        #[test]
        fn total_is_the_union(gaps in arb_gaps_s(), spans in arb_spans_s()) {
            let mut all: Vec<_> = gaps.iter().map(|g| (g.start, g.end)).collect();
            all.extend(spans.iter().map(|s| (s.start, s.end)));
            let union = union_seconds(all);
            for rule in [meeting_wins as fn(&&str, &&str) -> Precedence, work_wins, mixed] {
                prop_assert_eq!(overlay(&gaps, &spans, rule).total_seconds(), union);
            }
        }

        /// MeetingWins keeps every meeting second; WorkWins keeps every work second.
        #[test]
        fn each_precedence_keeps_its_side(gaps in arb_gaps(), spans in arb_spans()) {
            let g: i64 = gaps.iter().map(KeyedGap::seconds).sum();
            let m: i64 = normalize_spans(&spans).iter().map(|p| (p.end - p.start).whole_seconds()).sum();
            prop_assert_eq!(overlay(&gaps, &spans, meeting_wins).meeting_seconds(), m);
            prop_assert_eq!(overlay(&gaps, &spans, work_wins).work_seconds(), g);
        }

        /// The overlay never moves time between work keys: each key keeps at most
        /// what per_key_seconds gives it, exactly that under WorkWins.
        #[test]
        fn never_moves_time_between_projects(raw in prop::collection::vec((0i64..200_000, 0usize..4), 0..120), spans in arb_spans_s()) {
            let keyed: Vec<_> = raw.iter().map(|(s, k)| (t(*s), KEYS[*k])).collect();
            let idle = Duration::minutes(5);
            let base = per_key_seconds(&keyed, idle);
            let gaps = counted_gaps_keyed(&keyed, idle);
            let gap_sum: BTreeMap<&str, i64> = gaps.iter().fold(BTreeMap::new(), |mut acc, g| {
                *acc.entry(g.key).or_insert(0) += g.seconds();
                acc
            });
            prop_assert_eq!(&gap_sum, &base);
            for (rule, exact) in [(meeting_wins as fn(&&str, &&str) -> Precedence, false), (work_wins, true), (mixed, false)] {
                let per = overlay(&gaps, &spans, rule).per_slot_seconds();
                for (k, v) in &base {
                    let got = per.get(&Slot::Work(*k)).copied().unwrap_or(0);
                    if exact { prop_assert_eq!(got, *v); } else { prop_assert!(got <= *v); }
                }
            }
        }

        /// No spans: the slices are the gaps, as work, and nothing overlaps.
        #[test]
        fn no_spans_returns_the_gaps(gaps in arb_gaps()) {
            let o = overlay::<_, &str>(&gaps, &[], meeting_wins);
            prop_assert_eq!(o.engaged_during_meeting_seconds, 0);
            prop_assert_eq!(o.slices.len(), gaps.len());
            for (s, g) in o.slices.iter().zip(&gaps) {
                prop_assert_eq!((s.start, s.end, &s.slot, s.seconds), (g.start, g.end, &Slot::Work(g.key), g.seconds()));
            }
        }

        /// Adding a meeting never shrinks the total, never grows work, and under
        /// MeetingWins never shrinks meeting time.
        #[test]
        fn adding_a_meeting_is_monotonic(gaps in arb_gaps_s(), spans in arb_spans_s(), a in 0i64..200_000, len in 1i64..20_000) {
            let before = overlay(&gaps, &spans, meeting_wins);
            let mut more = spans.clone();
            more.push(span_owned(a, a + len, "z"));
            let after = overlay(&gaps, &more, meeting_wins);
            prop_assert!(after.total_seconds() >= before.total_seconds());
            prop_assert!(after.work_seconds() <= before.work_seconds());
            prop_assert!(after.meeting_seconds() >= before.meeting_seconds());
        }

        /// The order meetings arrive in never changes the result.
        #[test]
        fn input_order_does_not_matter(gaps in arb_gaps(), spans in arb_spans()) {
            let mut rev = spans.clone();
            rev.reverse();
            prop_assert_eq!(overlay(&gaps, &spans, mixed), overlay(&gaps, &rev, mixed));
        }

        /// Normalized meetings are disjoint, inside spans of the same key, and
        /// cover exactly the union of the (snapped) spans.
        #[test]
        fn normalized_meetings_partition_the_union(spans in arb_spans_s()) {
            let pieces = normalize_spans(&spans);
            for w in pieces.windows(2) {
                prop_assert!(w[0].end <= w[1].start);
            }
            for p in &pieces {
                let inside: Vec<_> = spans
                    .iter()
                    .filter(|s| s.key == p.key)
                    .map(|s| (s.start.max(p.start), s.end.min(p.end)))
                    .collect();
                prop_assert_eq!(union_seconds(inside), (p.end - p.start).whole_seconds());
            }
            let covered: i64 = pieces.iter().map(|p| (p.end - p.start).whole_seconds()).sum();
            prop_assert_eq!(covered, union_seconds(spans.iter().map(|s| (s.start, s.end)).collect()));
        }

        /// Sub-second inputs: the total never exceeds the floor of the wall-clock
        /// union, and loses at most a second per slice to rounding.
        #[test]
        fn rounding_never_inflates(gaps in arb_dense_gaps(), spans in arb_dense_spans()) {
            // Against the RAW spans: snapping inward means a meeting never bills
            // a moment outside the real one.
            let mut all: Vec<_> = gaps.iter().map(|g| (g.start, g.end)).collect();
            all.extend(spans.iter().map(|s| (s.start, s.end)));
            let union_ns = union_nanos(all);
            for rule in [meeting_wins as fn(&&str, &&str) -> Precedence, work_wins, mixed] {
                let o = overlay(&gaps, &spans, rule);
                let total = o.total_seconds();
                prop_assert!(i128::from(total) * 1_000_000_000 <= union_ns, "total {total}s > union {union_ns}ns");
                // Each slice floors once, each snapped span loses under 2 s.
                let slack = o.slices.len() as i64 + gaps.len() as i64 + 2 * spans.len() as i64;
                prop_assert!(i128::from(total + slack) * 1_000_000_000 >= union_ns);
            }
        }

        /// Sub-second inputs: under MeetingWins adding a meeting never lowers
        /// meeting time and never raises work. The TOTAL is deliberately not
        /// asserted: a whole-second meeting can cut sub-second slivers off two
        /// gaps and lower each gap's floor, so the total may drop by about a
        /// second. Rounding only ever goes down, never up; that is acceptable.
        #[test]
        fn adding_a_meeting_is_monotonic_dense(gaps in arb_dense_gaps(), spans in arb_dense_spans(), a in 0i64..80, len in 1i64..40) {
            let before = overlay(&gaps, &spans, meeting_wins);
            let mut more = spans.clone();
            more.push(MeetingSpan { start: ms250(a), end: ms250(a + len), key: "z" });
            let after = overlay(&gaps, &more, meeting_wins);
            prop_assert!(after.meeting_seconds() >= before.meeting_seconds());
            prop_assert!(after.work_seconds() <= before.work_seconds());
        }

        /// Ties included: any permutation of the meetings gives the same overlay.
        #[test]
        fn meeting_permutations_do_not_matter(gaps in arb_dense_gaps(), spans in arb_dense_spans().prop_shuffle()) {
            let mut sorted = spans.clone();
            sorted.sort_by(|a, b| (a.start, a.end, a.key).cmp(&(b.start, b.end, b.key)));
            prop_assert_eq!(overlay(&gaps, &spans, mixed), overlay(&gaps, &sorted, mixed));
        }

        /// Through the overlay, no meeting is ever credited more than its own span.
        #[test]
        fn a_meeting_never_exceeds_its_span(gaps in arb_dense_gaps(), spans in arb_dense_spans()) {
            let per = overlay(&gaps, &spans, meeting_wins).per_slot_seconds();
            for k in KEYS {
                let own: Vec<_> = spans.iter().filter(|s| s.key == k).map(|s| (s.start, s.end)).collect();
                let got = per.get(&Slot::Meeting(k)).copied().unwrap_or(0);
                prop_assert!(i128::from(got) * 1_000_000_000 <= union_nanos(own));
            }
        }

    }
}
