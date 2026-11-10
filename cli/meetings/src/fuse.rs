//! Score one [`Sample`]: which call app, if any, is in a meeting, and how sure.
//!
//! The device-family table from the design (§1.3):
//!
//! | Evidence | Confidence |
//! |---|---|
//! | `platform`: mic or camera (A) | high |
//! | `platform`: output (B) + call window (C) + process (D) | high (joined muted, listen-only) |
//! | `platform`: a call-only process (Zoom `CptHost`) + output (B) | high |
//! | `messaging`: mic (A) + output (B) | high (after the longer confirm, see tracker) |
//! | `browser`: mic (A) + output (B) in the same browser | high |
//! | anything else with A or B | low (a local suggestion) |
//! | only C or only D | nothing |
//!
//! Deny and relay holders are never evidence. `off_by_default` apps are ignored.

use crate::catalog::{AppKind, Holder, HolderRole, ProviderClass};
use crate::Sample;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    Low,
    High,
}

/// The best meeting candidate in one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fused {
    pub class: &'static str,
    pub kind: AppKind,
    pub wire: ProviderClass,
    pub confidence: Confidence,
    /// B was on for this class: the stuck-holder rule needs to know.
    pub has_output: bool,
}

#[derive(Default)]
struct Evidence {
    mic: bool,
    camera: bool,
    output: bool,
    window: bool,
    process: bool,
    call_only: bool,
}

/// Score a sample. `None` means no meeting evidence at all.
pub fn fuse(sample: &Sample) -> Option<Fused> {
    let mut by_class: BTreeMap<&'static str, (Holder, Evidence)> = BTreeMap::new();
    let mut note = |reading: &crate::Reading<Vec<Holder>>, set: fn(&mut Evidence, &Holder)| {
        if let Some(holders) = reading.on() {
            for h in holders {
                if matches!(h.role, HolderRole::Deny | HolderRole::Relay)
                    || h.kind == AppKind::OffByDefault
                {
                    continue;
                }
                let entry = by_class
                    .entry(h.class)
                    .or_insert_with(|| (h.clone(), Evidence::default()));
                set(&mut entry.1, h);
            }
        }
    };
    note(&sample.mic, |e, _| e.mic = true);
    note(&sample.camera, |e, _| e.camera = true);
    note(&sample.output, |e, _| e.output = true);
    note(&sample.window, |e, _| e.window = true);
    note(&sample.process, |e, h| {
        e.process = true;
        if h.role == HolderRole::CallOnly {
            e.call_only = true;
        }
    });

    by_class
        .into_iter()
        .filter_map(|(class, (h, e))| {
            score(h.kind, &e).map(|confidence| Fused {
                class,
                kind: h.kind,
                wire: h.wire,
                confidence,
                has_output: e.output,
            })
        })
        // Highest confidence, then the kind that needs the least corroboration,
        // then the class name: deterministic whatever the sample order.
        .max_by(|a, b| {
            a.confidence
                .cmp(&b.confidence)
                .then_with(|| b.kind.cmp(&a.kind))
                .then_with(|| b.class.cmp(a.class))
        })
}

fn score(kind: AppKind, e: &Evidence) -> Option<Confidence> {
    use Confidence::*;
    let high = match kind {
        AppKind::Platform => {
            e.mic || e.camera || (e.output && e.window && e.process) || (e.call_only && e.output)
        }
        AppKind::Messaging | AppKind::Browser => e.mic && e.output,
        AppKind::OffByDefault => return None,
    };
    if high {
        return Some(High);
    }
    let low = match kind {
        AppKind::Platform => e.output || e.call_only,
        AppKind::Messaging => e.mic,
        AppKind::Browser => e.mic || e.output || e.camera,
        AppKind::OffByDefault => false,
    };
    low.then_some(Low)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, Reading};

    fn h(id: &str) -> Holder {
        Catalog::embedded().classify(&[id]).expect(id)
    }

    fn on(ids: &[&str]) -> Reading<Vec<Holder>> {
        Reading::On(ids.iter().map(|i| h(i)).collect())
    }

    fn conf(s: &Sample) -> Option<(&'static str, Confidence)> {
        fuse(s).map(|f| (f.class, f.confidence))
    }

    #[test]
    fn a_platform_app_holding_the_mic_is_high() {
        let s = Sample {
            mic: on(&["us.zoom.xos"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("zoom", Confidence::High)));
    }

    #[test]
    fn joined_muted_needs_output_window_and_process() {
        let mut s = Sample {
            output: on(&["com.microsoft.teams2"]),
            process: on(&["com.microsoft.teams2"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("teams", Confidence::Low)));
        s.window = on(&["com.microsoft.teams2"]);
        assert_eq!(conf(&s), Some(("teams", Confidence::High)));
    }

    #[test]
    fn zoom_call_only_process_plus_output_is_high() {
        let s = Sample {
            output: on(&["us.zoom.xos"]),
            process: on(&["us.zoom.xos", "CptHost"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("zoom", Confidence::High)));
    }

    #[test]
    fn messaging_needs_mic_and_output() {
        let mut s = Sample {
            mic: on(&["com.tinyspeck.slackmacgap"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("slack", Confidence::Low)));
        s.output = on(&["com.tinyspeck.slackmacgap.helper"]);
        assert_eq!(conf(&s), Some(("slack", Confidence::High)));
        // Output alone (a voice note playing) is nothing.
        let s = Sample {
            output: on(&["com.tinyspeck.slackmacgap"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), None);
    }

    #[test]
    fn a_browser_needs_duplex_audio() {
        let s = Sample {
            output: on(&["com.google.Chrome.helper"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("chrome", Confidence::Low)));
        let s = Sample {
            mic: on(&["com.google.Chrome.helper"]),
            output: on(&["com.google.Chrome.helper"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("chrome", Confidence::High)));
    }

    #[test]
    fn window_or_process_alone_is_nothing() {
        let s = Sample {
            process: on(&["us.zoom.xos"]),
            window: on(&["us.zoom.xos"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), None);
    }

    #[test]
    fn deny_relay_and_off_by_default_are_never_evidence() {
        let s = Sample {
            mic: on(&["com.apple.VoiceMemos", "ai.krisp.krispMac", "mumble"]),
            output: on(&["mumble"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), None);
        // A relay next to a real call app: the call app counts.
        let s = Sample {
            mic: on(&["ai.krisp.krispMac", "us.zoom.xos"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("zoom", Confidence::High)));
    }

    #[test]
    fn unavailable_is_not_off_and_not_evidence() {
        let s = Sample {
            mic: Reading::Unavailable("bluetooth"),
            output: on(&["us.zoom.xos"]),
            process: on(&["us.zoom.xos"]),
            window: Reading::Unavailable("wayland"),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("zoom", Confidence::Low)));
    }

    #[test]
    fn the_strongest_candidate_wins_deterministically() {
        let s = Sample {
            mic: on(&["com.google.Chrome.helper", "us.zoom.xos"]),
            ..Default::default()
        };
        assert_eq!(conf(&s), Some(("zoom", Confidence::High)));
    }
}
