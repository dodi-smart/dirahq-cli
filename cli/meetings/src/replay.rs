//! A scripted adapter: replays device readings from a JSON script instead of
//! reading the OS. It drives the end-to-end smoke test (`DIRA_MEETINGS_FAKE`)
//! and the tests, so the whole pipeline (catalogue, fuse, tracker, daemon,
//! sync) can run without a real call.
//!
//! Script (identities go through the real catalogue, so the reduction to a
//! class is exercised too):
//!
//! ```json
//! { "steps": [
//!     { "for_s": 600, "mic": ["us.zoom.xos"], "output": ["us.zoom.xos"],
//!       "process": ["us.zoom.xos", "CptHost"] },
//!     { "for_s": 120 },
//!     { "for_s": 300, "output": ["com.microsoft.teams2"], "process": ["com.microsoft.teams2"],
//!       "unavailable": { "window": "wayland" } }
//! ],
//!   "repeat": false }
//! ```
//!
//! A step lists, per signal, the identities that hold it. A signal left out is
//! `Off`; a signal named in `unavailable` is `Unavailable`. Past the last step
//! everything is `Off`, unless `repeat` loops the script.

use crate::{Catalog, Holder, MeetingSignal, Reading, Sample};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub steps: Vec<Step>,
    #[serde(default)]
    pub repeat: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub for_s: u64,
    #[serde(default)]
    pub mic: Vec<String>,
    #[serde(default)]
    pub camera: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
    #[serde(default)]
    pub window: Vec<String>,
    #[serde(default)]
    pub process: Vec<String>,
    /// Signal name -> reason, e.g. `{"window": "wayland"}`.
    #[serde(default)]
    pub unavailable: BTreeMap<String, String>,
}

pub struct Replay {
    script: Script,
    catalog: &'static Catalog,
    started: Instant,
    /// Reasons must be `'static` for `Reading::Unavailable`; the script lives
    /// for the daemon's lifetime, so they are leaked once at load.
    reasons: Vec<BTreeMap<String, &'static str>>,
}

impl Replay {
    pub fn from_json(json: &str, catalog: &'static Catalog) -> Result<Self, serde_json::Error> {
        let script: Script = serde_json::from_str(json)?;
        let reasons = script
            .steps
            .iter()
            .map(|s| {
                s.unavailable
                    .iter()
                    .map(|(k, v)| (k.clone(), &*Box::leak(v.clone().into_boxed_str())))
                    .collect()
            })
            .collect();
        Ok(Self {
            script,
            catalog,
            started: Instant::now(),
            reasons,
        })
    }

    /// The sample `elapsed` into the script. Deterministic, for tests.
    pub fn sample_at(&self, elapsed: Duration) -> Sample {
        let Some(i) = self.step_index(elapsed) else {
            return Sample::default();
        };
        let step = &self.script.steps[i];
        let reasons = &self.reasons[i];
        let read = |name: &str, ids: &[String]| -> Reading<Vec<Holder>> {
            if let Some(r) = reasons.get(name) {
                return Reading::Unavailable(r);
            }
            let holders: Vec<Holder> = ids
                .iter()
                .filter_map(|id| self.catalog.classify(&[id.as_str()]))
                .collect();
            if holders.is_empty() {
                Reading::Off
            } else {
                Reading::On(holders)
            }
        };
        Sample {
            at: None,
            mic: read("mic", &step.mic),
            camera: read("camera", &step.camera),
            output: read("output", &step.output),
            window: read("window", &step.window),
            process: read("process", &step.process),
        }
    }

    fn step_index(&self, elapsed: Duration) -> Option<usize> {
        let total: u64 = self.script.steps.iter().map(|s| s.for_s).sum();
        if total == 0 {
            return None;
        }
        let mut t = elapsed.as_secs();
        if t >= total {
            if !self.script.repeat {
                return None;
            }
            t %= total;
        }
        let mut acc = 0;
        for (i, s) in self.script.steps.iter().enumerate() {
            acc += s.for_s;
            if t < acc {
                return Some(i);
            }
        }
        None
    }
}

impl MeetingSignal for Replay {
    fn id(&self) -> &'static str {
        "replay"
    }

    fn sample(&mut self) -> Sample {
        self.sample_at(self.started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{fuse, Confidence};

    const SCRIPT: &str = r#"{ "steps": [
        { "for_s": 600, "mic": ["us.zoom.xos"], "output": ["us.zoom.xos"], "process": ["us.zoom.xos", "CptHost"] },
        { "for_s": 120 },
        { "for_s": 300, "output": ["com.microsoft.teams2"], "process": ["com.microsoft.teams2"],
          "unavailable": { "window": "wayland" } }
    ] }"#;

    fn replay() -> Replay {
        Replay::from_json(SCRIPT, Catalog::embedded()).unwrap()
    }

    #[test]
    fn steps_follow_the_script() {
        let r = replay();
        let s =
            |secs| fuse(&r.sample_at(Duration::from_secs(secs))).map(|f| (f.class, f.confidence));
        assert_eq!(s(0), Some(("zoom", Confidence::High)));
        assert_eq!(s(599), Some(("zoom", Confidence::High)));
        assert_eq!(s(600), None);
        // Joined muted with no window signal on Wayland: low.
        assert_eq!(s(720), Some(("teams", Confidence::Low)));
        assert_eq!(
            r.sample_at(Duration::from_secs(720)).window,
            Reading::Unavailable("wayland")
        );
        assert_eq!(s(1020), None);
    }

    #[test]
    fn repeat_loops_the_script() {
        let r = Replay::from_json(
            r#"{ "steps": [ { "for_s": 10, "mic": ["us.zoom.xos"] }, { "for_s": 10 } ], "repeat": true }"#,
            Catalog::embedded(),
        )
        .unwrap();
        assert!(fuse(&r.sample_at(Duration::from_secs(25))).is_some());
        assert!(fuse(&r.sample_at(Duration::from_secs(35))).is_none());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(Replay::from_json(
            r#"{ "steps": [ { "for_s": 1, "title": "x" } ] }"#,
            Catalog::embedded()
        )
        .is_err());
    }

    #[test]
    fn unknown_identities_are_ignored() {
        let r = Replay::from_json(
            r#"{ "steps": [ { "for_s": 5, "mic": ["com.apple.Terminal"] } ] }"#,
            Catalog::embedded(),
        )
        .unwrap();
        assert_eq!(r.sample_at(Duration::ZERO).mic, Reading::Off);
    }
}
