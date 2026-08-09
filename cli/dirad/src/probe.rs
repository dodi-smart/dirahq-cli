//! `dira doctor --probe`'s end-to-end capture probe, daemon half.
//!
//! The probe answers the one question nothing else in the product could: does a
//! hook, launched the way the harness launches it, actually reach the daemon
//! and land a row? A machine once ran for days where the answer was no while
//! every other signal — daemon status, ingress, commit capture, cloud sync —
//! reported healthy, because the only broken link was the hook shim's connect
//! to the control channel and nothing exercised it.
//!
//! The daemon owns three things the CLI is deliberately not trusted with:
//!
//! 1. It **mints** the reserved session id, so the CLI can never aim a probe at
//!    a real session.
//! 2. It **admits** that id only while its own arm is live and unexpired, so a
//!    stale replay or a hand-crafted payload can never create a probe row.
//! 3. It **deletes** the row in the same request that verifies it.
//!
//! What the daemon must NOT do is spawn the hook child. It may itself be the
//! elevated process, and a child it forks would inherit that token and open the
//! elevated control channel happily — the probe would pass on precisely the
//! machine the bug is on. Spawning belongs to `dira doctor`, under the user's
//! own ordinary token. See `Request::CaptureProbe`.

use crate::control::lock_recover;
use crate::AppState;
use dira_core::protocol::{CaptureProbeView, Response};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// How long an arm stays valid.
///
/// Comfortably longer than the CLI's child deadline plus its verify wait, and
/// short enough that a `doctor` killed mid-probe cannot leave a usable id
/// behind for long. This bounds the *write* window: once it lapses, [`admit`]
/// refuses the id and no row can be created late.
pub const ARM_TTL: Duration = Duration::from_secs(30);

/// The single in-flight probe.
pub struct ProbeSlot {
    pub session_id: String,
    /// When [`arm`] minted the id. The arm lapses at `armed_at + ARM_TTL`;
    /// one timestamp rather than two so they cannot be set inconsistently.
    pub armed_at: Instant,
    /// Fired by the writer the moment the probe row is appended.
    pub landed: Arc<Notify>,
}

impl ProbeSlot {
    fn live(&self, now: Instant) -> bool {
        now.duration_since(self.armed_at) < ARM_TTL
    }
}

/// Mint a reserved session id and register the landing watch.
///
/// The watch is installed *before* this responds, and the CLI cannot spawn the
/// child until it has the response — so there is no window in which the row
/// could land unwatched.
pub async fn arm(state: &AppState) -> Response {
    let now = Instant::now();
    {
        let mut slot = lock_recover(&state.probe);
        if let Some(existing) = slot.as_ref() {
            if existing.live(now) {
                return Response::Error {
                    message: "a capture probe is already in flight".into(),
                };
            }
        }
        let session_id = format!(
            "{}{}",
            dira_core::model::PROBE_SESSION_PREFIX,
            ulid::Ulid::generate()
        );
        *slot = Some(ProbeSlot {
            session_id: session_id.clone(),
            armed_at: now,
            landed: Arc::new(Notify::new()),
        });
        Response::CaptureProbe(Box::new(CaptureProbeView {
            session_id: Some(session_id),
            daemon_elevated: dira_ipc::elevation::is_elevated(),
            control_channel_warning: lock_recover(&state.control_channel_warning).clone(),
            ..Default::default()
        }))
    }
}

/// Wait for the probe row, then reap it and disarm.
pub async fn verify(state: &AppState, session_id: String, wait_ms: u64) -> Response {
    let (notify, armed_at) = {
        let slot = lock_recover(&state.probe);
        match slot.as_ref() {
            Some(s) if s.session_id == session_id => (s.landed.clone(), s.armed_at),
            // Either nothing is armed or a different probe is. Either way this
            // id is not ours to verify; reap defensively and report honestly.
            _ => (Arc::new(Notify::new()), Instant::now()),
        }
    };

    // Subscribe BEFORE the first store read: a `Notified` created here also
    // catches a notify that fires during the read itself.
    let fut = notify.notified();
    tokio::pin!(fut);

    let mut landed = count_of(state, &session_id).await > 0;
    if !landed {
        landed = tokio::time::timeout(Duration::from_millis(wait_ms), fut)
            .await
            .is_ok();
        // The store is the authority; the notify is only the low-latency path.
        // Re-check once so correctness never depends on the notify firing — a
        // daemon restart drops the slot entirely, and the writer's per-message
        // panic guard can drop a message after the append.
        if !landed {
            landed = count_of(state, &session_id).await > 0;
        }
    }
    let waited_ms = armed_at.elapsed().as_millis() as u64;

    // Reap UNCONDITIONALLY. On the failure path too: otherwise a round-trip
    // slower than the CLI's wait leaves a row behind forever.
    let deleted = state
        .store
        .delete_session_events(&session_id)
        .await
        .unwrap_or(0);
    disarm(state, &session_id);

    Response::CaptureProbe(Box::new(CaptureProbeView {
        landed: Some(landed),
        waited_ms: Some(waited_ms),
        deleted,
        // Elevation and the control-channel warning ride the `Arm` reply; the
        // CLI already has them by the time it verifies.
        ..Default::default()
    }))
}

async fn count_of(state: &AppState, session_id: &str) -> u64 {
    state
        .store
        .session_event_count(session_id)
        .await
        .unwrap_or(0)
}

fn disarm(state: &AppState, session_id: &str) {
    let mut slot = lock_recover(&state.probe);
    if slot.as_ref().is_some_and(|s| s.session_id == session_id) {
        *slot = None;
    }
}

/// The ingress guard.
///
/// `Ok(())` only for the live, unexpired, armed id. Everything else under the
/// reserved prefix is refused, which is what makes "a probe row can only exist
/// because this daemon just asked for one" true rather than hoped for.
pub fn admit(state: &AppState, session_id: &str) -> Result<(), String> {
    let slot = lock_recover(&state.probe);
    match slot.as_ref() {
        Some(s) if s.session_id == session_id && s.live(Instant::now()) => Ok(()),
        Some(s) if s.session_id == session_id => {
            Err("the capture probe expired before the hook arrived".into())
        }
        _ => Err("the reserved capture-probe session prefix is not accepted here".into()),
    }
}

/// Called by the writer once a probe row is appended.
pub fn note_landed(state: &AppState, session_id: &str) {
    let slot = lock_recover(&state.probe);
    if let Some(s) = slot.as_ref() {
        if s.session_id == session_id {
            s.landed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install a slot armed `age` ago, so a test can make one lapse without
    /// waiting out the real TTL.
    fn slot(state: &AppState, id: &str, age: Duration) {
        *lock_recover(&state.probe) = Some(ProbeSlot {
            session_id: id.to_string(),
            armed_at: Instant::now() - age,
            landed: Arc::new(Notify::new()),
        });
    }

    async fn state() -> AppState {
        let store = dira_core::Store::open_in_memory().await.expect("store");
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, dira_core::Config::default())
                .await
                .expect("build_state");
        state
    }

    /// The containment guarantee: an id this daemon did not just mint is
    /// refused, so nothing outside the probe path can create a probe row.
    #[tokio::test]
    async fn only_the_live_armed_id_is_admitted() {
        let state = state().await;
        let id = format!("{}01ARMED", dira_core::model::PROBE_SESSION_PREFIX);

        // Nothing armed at all.
        assert!(admit(&state, &id).is_err());

        slot(&state, &id, Duration::ZERO);
        assert!(admit(&state, &id).is_ok());
        // A different probe id, while one IS armed.
        let other = format!("{}01OTHER", dira_core::model::PROBE_SESSION_PREFIX);
        assert!(admit(&state, &other).is_err());

        // An expired arm bounds the write window.
        slot(&state, &id, ARM_TTL + Duration::from_secs(1));
        let err = admit(&state, &id).expect_err("an expired arm must refuse");
        assert!(err.contains("expired"), "{err}");
    }

    #[tokio::test]
    async fn a_second_arm_is_refused_while_one_is_live() {
        let state = state().await;
        let first = arm(&state).await;
        let id = match &first {
            Response::CaptureProbe(v) => v.session_id.clone().expect("an armed id"),
            other => panic!("expected CaptureProbe, got {other:?}"),
        };
        assert!(dira_core::model::is_probe_session(&id));

        match arm(&state).await {
            Response::Error { message } => assert!(message.contains("already in flight")),
            other => panic!("a second arm must be refused, got {other:?}"),
        }

        // Verifying releases the slot, so the next probe can arm.
        verify(&state, id, 0).await;
        assert!(matches!(arm(&state).await, Response::CaptureProbe(_)));
    }

    /// The CLI never chooses the id — two arms never produce the same one.
    #[tokio::test]
    async fn each_arm_mints_a_fresh_id() {
        let state = state().await;
        let mut seen = Vec::new();
        for _ in 0..3 {
            let id = match arm(&state).await {
                Response::CaptureProbe(v) => v.session_id.expect("id"),
                other => panic!("{other:?}"),
            };
            assert!(!seen.contains(&id), "arm reused {id}");
            verify(&state, id.clone(), 0).await;
            seen.push(id);
        }
    }

    /// A verify for a row that never arrives must return within its budget
    /// rather than hanging — a guard against the notify path regressing.
    #[tokio::test]
    async fn verify_gives_up_at_the_deadline_and_reports_not_landed() {
        let state = state().await;
        let id = match arm(&state).await {
            Response::CaptureProbe(v) => v.session_id.expect("id"),
            other => panic!("{other:?}"),
        };
        let started = Instant::now();
        match verify(&state, id, 50).await {
            Response::CaptureProbe(v) => {
                assert_eq!(v.landed, Some(false));
                assert_eq!(v.deleted, 0);
            }
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5), "verify hung");
    }
}
