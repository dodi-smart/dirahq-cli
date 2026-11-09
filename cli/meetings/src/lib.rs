//! Bot-less meeting detection (DIRASH-0060, proposed).
//!
//! dira never joins a call. It reads what the OS already knows: which process
//! holds a microphone, a camera or an active audio stream, whether a known call
//! app has a call window, and whether a call app is running. An OS adapter turns
//! that into a [`Sample`]; [`fuse`] scores one sample; [`tracker`] turns a stream
//! of scored samples into meeting spans and suggestions.
//!
//! This crate is the pure, cross-platform core. It has no OS dependencies and
//! never sees a window title, a calendar entry or any call content. Process
//! identity is reduced to a catalogue [`Holder`] at the adapter boundary, and the
//! raw bundle id, executable name or path is dropped there.
//!
//! Design: `docs/superpowers/specs/2026-11-09-meeting-tracking-design.md`.

pub mod catalog;
pub mod fuse;
pub mod replay;
pub mod tracker;

pub use catalog::{AppKind, Catalog, Holder, HolderRole, ProviderClass};
pub use fuse::{Confidence, Fused};
pub use tracker::{Emitted, Live, Tracker, TrackerConfig};

use time::OffsetDateTime;

/// One signal's reading. `Unavailable` is never read as "no meeting": it only
/// removes that signal from the fusion (the OS cannot tell us, so we don't guess).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Reading<T> {
    On(T),
    #[default]
    Off,
    Unavailable(&'static str),
}

impl<T> Reading<T> {
    pub fn on(&self) -> Option<&T> {
        match self {
            Reading::On(v) => Some(v),
            _ => None,
        }
    }
}

/// One device adapter: an OS sampler, or [`replay::Replay`] for scripted runs.
/// `sample` is cheap, synchronous and side-effect free, and must never prompt
/// for a permission (DIRASH-0060): a signal the OS will not give without one is
/// reported `Unavailable`.
pub trait MeetingSignal: Send {
    /// Stable adapter id for `dira doctor` (`"macos.coreaudio"`, `"replay"`).
    fn id(&self) -> &'static str;
    fn sample(&mut self) -> Sample;
}

/// Everything one poll of an OS adapter observed, already reduced to catalogue
/// holders. Field letters follow the design: A = capture, B = output, C = call
/// window, D = process.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Wall-clock time of the sample. A gap between samples wider than the
    /// tracker's grace (sleep, lid close, a stalled daemon) ends a meeting at the
    /// last sample before it.
    pub at: Option<OffsetDateTime>,
    /// A: processes holding a microphone.
    pub mic: Reading<Vec<Holder>>,
    /// A: processes holding a camera.
    pub camera: Reading<Vec<Holder>>,
    /// B: processes with an active audio output stream.
    pub output: Reading<Vec<Holder>>,
    /// C: processes with a call window (class or state only, never a title).
    pub window: Reading<Vec<Holder>>,
    /// D: running call-app processes.
    pub process: Reading<Vec<Holder>>,
}
