//! Turn a stream of scored samples into meeting spans (design §1.3).
//!
//! - **Start**: a run starts at the first sample with evidence, so every span is
//!   back-dated to it. High confidence only counts once it has held for
//!   `confirm` (`confirm_messaging` for chat apps, which also hold the mic for
//!   voice notes); a shorter high stretch is a suggestion.
//! - **Continue**: while the same app keeps any evidence.
//! - **End**: after `grace` with no evidence for it, at the last sample that had
//!   some. A wall-clock gap between samples wider than `grace` (sleep, lid close,
//!   a stalled daemon) or a clock going backwards ends the run at the last sample
//!   before it.
//! - **Minimum length**: a run shorter than `min_len` is dropped entirely: no
//!   meeting and no suggestion (a dictation burst, a voice-note test).
//! - **Stuck holder**: a high stretch with no audio output for longer than
//!   `stuck_after` becomes a suggestion from the last output on (a settings pane
//!   that held the mic for hours is not a meeting).
//! - A run whose confidence changes is split: high spans become meetings, low
//!   spans become suggestions.
//!
//! Thresholds are provisional until the P0 validation spike.

use crate::catalog::{AppKind, ProviderClass};
use crate::fuse::{Confidence, Fused};
use time::{Duration, OffsetDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackerConfig {
    pub confirm: Duration,
    pub confirm_messaging: Duration,
    pub grace: Duration,
    pub min_len: Duration,
    pub stuck_after: Duration,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            confirm: Duration::seconds(30),
            confirm_messaging: Duration::seconds(90),
            grace: Duration::seconds(60),
            min_len: Duration::minutes(2),
            stuck_after: Duration::hours(3),
        }
    }
}

/// A finished span. `High` spans are meeting time; `Low` spans are suggestions
/// that are never counted until a person confirms them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emitted {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub class: &'static str,
    pub wire: ProviderClass,
    pub confidence: Confidence,
}

/// What is happening right now, for `dira status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub class: &'static str,
    pub wire: ProviderClass,
    pub since: OffsetDateTime,
    /// High only once the current high stretch has held for its confirm window.
    pub confidence: Confidence,
}

#[derive(Debug, Clone)]
struct Run {
    class: &'static str,
    kind: AppKind,
    wire: ProviderClass,
    /// Confidence changes: (from, confidence), first entry at the run start.
    segments: Vec<(OffsetDateTime, Confidence)>,
    /// Last sample with evidence for this class: where the run ends.
    last_seen: OffsetDateTime,
    /// Last sample with audio output, for the stuck-holder rule.
    last_output: Option<OffsetDateTime>,
}

/// The per-daemon tracker. Feed it every sample, in time order.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    cfg: TrackerConfig,
    run: Option<Run>,
    last_sample: Option<OffsetDateTime>,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self {
            cfg,
            run: None,
            last_sample: None,
        }
    }

    /// Feed one scored sample taken at `at`. Returns the spans that finished.
    pub fn observe(&mut self, at: OffsetDateTime, fused: Option<&Fused>) -> Vec<Emitted> {
        let mut out = Vec::new();
        if let Some(prev) = self.last_sample {
            if at < prev || at - prev > self.cfg.grace {
                // Sleep, a stall, or the clock moved back: close at the last sample.
                self.finish(&mut out);
            }
        }
        self.last_sample = Some(at);

        match (fused, self.run.as_mut()) {
            (Some(f), Some(run)) if run.class == f.class => {
                if run.segments.last().map(|(_, c)| *c) != Some(f.confidence) {
                    run.segments.push((at, f.confidence));
                }
                run.last_seen = at;
                if f.has_output {
                    run.last_output = Some(at);
                }
            }
            (Some(f), _) => {
                // A different app took over (back-to-back calls), or a new run.
                self.finish(&mut out);
                self.run = Some(Run {
                    class: f.class,
                    kind: f.kind,
                    wire: f.wire,
                    segments: vec![(at, f.confidence)],
                    last_seen: at,
                    last_output: f.has_output.then_some(at),
                });
            }
            (None, Some(run)) => {
                if at - run.last_seen > self.cfg.grace {
                    self.finish(&mut out);
                }
            }
            (None, None) => {}
        }
        out
    }

    /// Close any open run (daemon shutdown, module turned off).
    pub fn flush(&mut self) -> Vec<Emitted> {
        let mut out = Vec::new();
        self.finish(&mut out);
        self.last_sample = None;
        out
    }

    pub fn live(&self) -> Option<Live> {
        let run = self.run.as_ref()?;
        let since = run.segments.first()?.0;
        let (seg_start, conf) = *run.segments.last()?;
        let confirmed = run.last_seen - seg_start >= self.confirm_for(run.kind);
        let stuck = run.last_seen - run.last_output.unwrap_or(since) > self.cfg.stuck_after;
        let confidence = if conf == Confidence::High && confirmed && !stuck {
            Confidence::High
        } else {
            Confidence::Low
        };
        Some(Live {
            class: run.class,
            wire: run.wire,
            since,
            confidence,
        })
    }

    fn confirm_for(&self, kind: AppKind) -> Duration {
        if kind == AppKind::Messaging {
            self.cfg.confirm_messaging
        } else {
            self.cfg.confirm
        }
    }

    fn finish(&mut self, out: &mut Vec<Emitted>) {
        let Some(run) = self.run.take() else { return };
        let start = run.segments[0].0;
        let end = run.last_seen;
        if end - start < self.cfg.min_len {
            return;
        }
        let confirm = self.confirm_for(run.kind);

        // Segment bounds, clipped to the run end.
        let mut spans: Vec<(OffsetDateTime, OffsetDateTime, Confidence)> = Vec::new();
        for (i, (from, conf)) in run.segments.iter().enumerate() {
            let to = run.segments.get(i + 1).map_or(end, |(t, _)| *t).min(end);
            if to > *from {
                // A high stretch that never held for its confirm window is a suggestion.
                let conf = if *conf == Confidence::High && to - *from < confirm {
                    Confidence::Low
                } else {
                    *conf
                };
                spans.push((*from, to, conf));
            }
        }

        // Stuck holder: no output for longer than `stuck_after` -> low from the
        // last output (or the start, if there never was any) on.
        let quiet_from = run.last_output.unwrap_or(start);
        if end - quiet_from > self.cfg.stuck_after {
            let mut split = Vec::with_capacity(spans.len() + 1);
            for (s, e, c) in spans {
                if e <= quiet_from || c == Confidence::Low {
                    split.push((s, e, c));
                } else if s >= quiet_from {
                    split.push((s, e, Confidence::Low));
                } else {
                    split.push((s, quiet_from, c));
                    split.push((quiet_from, e, Confidence::Low));
                }
            }
            spans = split;
        }

        // Merge neighbours with the same confidence.
        let mut merged: Vec<(OffsetDateTime, OffsetDateTime, Confidence)> = Vec::new();
        for (s, e, c) in spans {
            match merged.last_mut() {
                Some(last) if last.2 == c && last.1 == s => last.1 = e,
                _ => merged.push((s, e, c)),
            }
        }
        out.extend(merged.into_iter().map(|(start, end, confidence)| Emitted {
            start,
            end,
            class: run.class,
            wire: run.wire,
            confidence,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(s)
    }

    fn f(class: &'static str, kind: AppKind, c: Confidence, out: bool) -> Fused {
        Fused {
            class,
            kind,
            wire: ProviderClass::Other,
            confidence: c,
            has_output: out,
        }
    }

    fn zoom(c: Confidence) -> Fused {
        f("zoom", AppKind::Platform, c, true)
    }

    /// Feed `(second, fused)` samples, then flush.
    fn run(samples: &[(i64, Option<Fused>)]) -> Vec<(i64, i64, Confidence)> {
        let mut tr = Tracker::new(TrackerConfig::default());
        let mut out = Vec::new();
        for (s, fu) in samples {
            out.extend(tr.observe(t(*s), fu.as_ref()));
        }
        out.extend(tr.flush());
        out.iter()
            .map(|e| {
                let base = OffsetDateTime::UNIX_EPOCH;
                (
                    (e.start - base).whole_seconds(),
                    (e.end - base).whole_seconds(),
                    e.confidence,
                )
            })
            .collect()
    }

    fn every(from: i64, to: i64, step: i64, fu: Fused) -> Vec<(i64, Option<Fused>)> {
        (from..=to)
            .step_by(step as usize)
            .map(|s| (s, Some(fu.clone())))
            .collect()
    }

    #[test]
    fn a_call_is_back_dated_and_ends_at_the_last_evidence() {
        let mut s = every(0, 600, 5, zoom(Confidence::High));
        s.extend((605..=700).step_by(5).map(|x| (x, None)));
        assert_eq!(run(&s), vec![(0, 600, Confidence::High)]);
    }

    #[test]
    fn a_short_blip_is_dropped_entirely() {
        let s = every(0, 60, 5, zoom(Confidence::High));
        assert_eq!(run(&s), vec![]);
    }

    #[test]
    fn a_dip_inside_the_grace_keeps_one_meeting() {
        let mut s = every(0, 300, 5, zoom(Confidence::High));
        s.extend((305..=340).step_by(5).map(|x| (x, None)));
        s.extend(every(345, 600, 5, zoom(Confidence::High)));
        assert_eq!(run(&s), vec![(0, 600, Confidence::High)]);
    }

    #[test]
    fn a_gap_beyond_the_grace_splits_the_call() {
        let mut s = every(0, 300, 5, zoom(Confidence::High));
        s.extend((305..=400).step_by(5).map(|x| (x, None)));
        s.extend(every(405, 700, 5, zoom(Confidence::High)));
        assert_eq!(
            run(&s),
            vec![(0, 300, Confidence::High), (405, 700, Confidence::High)]
        );
    }

    #[test]
    fn sleep_ends_the_meeting_at_the_last_sample() {
        let mut s = every(0, 300, 5, zoom(Confidence::High));
        // Lid closed for an hour; the call app still "holds" the mic on wake.
        s.extend(every(3900, 4200, 5, zoom(Confidence::High)));
        assert_eq!(
            run(&s),
            vec![(0, 300, Confidence::High), (3900, 4200, Confidence::High)]
        );
    }

    #[test]
    fn confidence_changes_split_the_run() {
        let mut s = every(0, 300, 5, zoom(Confidence::Low));
        s.extend(every(305, 900, 5, zoom(Confidence::High)));
        assert_eq!(
            run(&s),
            vec![(0, 305, Confidence::Low), (305, 900, Confidence::High)]
        );
    }

    #[test]
    fn messaging_needs_the_longer_confirm() {
        let slack = |c| f("slack", AppKind::Messaging, c, true);
        let mut s = every(0, 200, 5, slack(Confidence::Low));
        s.extend(every(205, 265, 5, slack(Confidence::High))); // 60 s high < 90 s confirm
        s.extend(every(270, 400, 5, slack(Confidence::Low)));
        assert_eq!(run(&s), vec![(0, 400, Confidence::Low)]);
    }

    #[test]
    fn a_stuck_holder_becomes_a_suggestion() {
        // Mic held for 4 h by a platform app, output only in the first 10 minutes.
        let mut s = every(0, 600, 30, zoom(Confidence::High));
        s.extend(every(
            630,
            4 * 3600,
            30,
            f("zoom", AppKind::Platform, Confidence::High, false),
        ));
        assert_eq!(
            run(&s),
            vec![(0, 600, Confidence::High), (600, 4 * 3600, Confidence::Low)]
        );
    }

    #[test]
    fn back_to_back_calls_in_different_apps_are_two_meetings() {
        let teams = f("teams", AppKind::Platform, Confidence::High, true);
        let mut s = every(0, 600, 5, zoom(Confidence::High));
        s.extend(every(605, 1200, 5, teams));
        assert_eq!(
            run(&s),
            vec![(0, 600, Confidence::High), (605, 1200, Confidence::High)]
        );
    }

    #[test]
    fn live_reports_low_until_confirmed() {
        let mut tr = Tracker::new(TrackerConfig::default());
        tr.observe(t(0), Some(&zoom(Confidence::High)));
        tr.observe(t(10), Some(&zoom(Confidence::High)));
        assert_eq!(tr.live().unwrap().confidence, Confidence::Low);
        tr.observe(t(40), Some(&zoom(Confidence::High)));
        let live = tr.live().unwrap();
        assert_eq!(
            (live.class, live.since, live.confidence),
            ("zoom", t(0), Confidence::High)
        );
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;

    fn t(s: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(s)
    }

    // A sample: (step seconds since the previous one, evidence: 0 none, 1 low, 2 high,
    // class index, output on).
    fn arb_stream() -> impl Strategy<Value = Vec<(i64, u8, usize, bool)>> {
        prop::collection::vec((1i64..120, 0u8..3, 0usize..2, any::<bool>()), 0..400)
    }

    proptest! {
        /// Emitted spans never overlap, never run backwards, stay inside the
        /// sampled range, and every one is at least `min_len` long or part of a
        /// run that was.
        #[test]
        fn emitted_spans_are_sane(stream in arb_stream()) {
            let cfg = TrackerConfig::default();
            let mut tr = Tracker::new(cfg);
            let classes = ["zoom", "slack"];
            let kinds = [AppKind::Platform, AppKind::Messaging];
            let mut now = 0i64;
            let mut out = Vec::new();
            for (step, ev, ci, output) in &stream {
                now += step;
                let fused = (*ev > 0).then(|| Fused {
                    class: classes[*ci],
                    kind: kinds[*ci],
                    wire: ProviderClass::Other,
                    confidence: if *ev == 2 { Confidence::High } else { Confidence::Low },
                    has_output: *output,
                });
                out.extend(tr.observe(t(now), fused.as_ref()));
            }
            out.extend(tr.flush());
            for e in &out {
                prop_assert!(e.end > e.start);
                prop_assert!(e.start >= t(0) && e.end <= t(now));
            }
            for w in out.windows(2) {
                prop_assert!(w[0].end <= w[1].start, "overlap: {:?} / {:?}", w[0], w[1]);
            }
            // A high span from a messaging app has held for its longer confirm.
            for e in out.iter().filter(|e| e.confidence == Confidence::High && e.class == "slack") {
                prop_assert!(e.end - e.start >= cfg.confirm_messaging);
            }
        }
    }
}
