//! Live-presence heartbeat.
//!
//! A single background task periodically POSTs a signed [`PresenceEnvelope`] of
//! the daemon's currently-active sessions to `{cloud_url}/api/v1/presence`. It
//! mirrors the sync task's shape (one `reqwest::Client`, per-run gating on
//! `cloud_url` + `device_id`, sign-the-payload, POST the envelope) but is
//! deliberately simpler: presence is **ephemeral**. There is no cursor, no
//! idempotency key, and no backoff — a heartbeat is a stateless snapshot, so a
//! failed tick is simply retried on the next interval.
//!
//! Unlike sync, a heartbeat is sent on *every* tick even when no sessions are
//! live: an empty `sessions` list is itself the "device is online" signal the
//! cloud uses to keep the device fresh.

use crate::state::{AppState, LiveSession};
use dira_contract::{PresenceAck, PresenceEnvelope, PresencePing, PresenceSession, SCHEMA_VERSION};
use dira_core::identity;
use std::sync::atomic::Ordering;
use std::time::{Duration as StdDuration, Instant};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// HTTP timeout for a single heartbeat POST. Short — presence is best-effort and
/// the next tick covers any miss.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// Timeout for the single best-effort shutdown beat. Short — we must not delay
/// the daemon's exit waiting on the network.
const SHUTDOWN_BEAT_TIMEOUT: StdDuration = StdDuration::from_secs(3);

/// Safety margin the dedup-skip check (see `beat`) adds on top of the
/// worst-case NEXT-tick cadence before comparing against the effective dedup
/// TTL. Covers `HTTP_TIMEOUT` (the POST itself can take up to 10s before we'd
/// even know it failed) plus a few seconds of slack for the daemon's own
/// scheduling slop (`tokio::time::sleep` overshoot, a busy executor, etc.), so
/// "the next real send lands before the TTL" holds even under load, not just
/// in the ideal case.
const DEDUP_SKIP_MARGIN: StdDuration = StdDuration::from_secs(15); // HTTP_TIMEOUT (10s) + 5s slack

/// What a single beat decided to do, so the loop can pace the next tick and the
/// observability stays honest about sends vs dedup-skips.
#[cfg_attr(test, derive(Debug, PartialEq))]
enum BeatResult {
    /// Posted a fresh snapshot (something changed, or the TTL keepalive was due),
    /// and the cloud accepted it. Carries the time it was sent.
    Sent,
    /// The snapshot was byte-identical to the last sent one and we're still within
    /// the TTL — skipped the POST entirely (dedup).
    SkippedDuplicate,
    /// Could not beat (unconfigured / unlinked / sign or post failed). Treated like
    /// a no-op for pacing — the next tick retries.
    Noop,
}

/// Spawn the background heartbeat task. No-ops each tick until the device is both
/// linked (`device_id`) and configured (`cloud_url`); never panics, never blocks
/// startup.
pub fn spawn(state: AppState) {
    tokio::spawn(run(state));
}

/// Send ONE best-effort "going offline" presence beat with an empty `sessions`
/// list, on daemon shutdown (Phase 3c). Short timeout, all errors swallowed — it
/// must never delay or fail the daemon's exit. No-ops when unconfigured/unlinked,
/// exactly like a normal tick. An empty-sessions ping is the cloud's signal that
/// this device has no live sessions; combined with the (short) presence TTL it
/// lets the cloud mark the device offline promptly instead of waiting out the TTL.
pub async fn send_offline_beat(state: &AppState) {
    let Some(cloud_url) = state.config.cloud_url.clone() else {
        return; // presence off
    };
    let device_id = match identity::device_id(&state.store).await {
        Ok(Some(id)) => id,
        _ => return, // not linked / read failed — nothing to send
    };

    let ping = PresencePing {
        device_id: device_id.clone(),
        sent_at: fmt_rfc3339(OffsetDateTime::now_utc()),
        sessions: Vec::new(), // explicitly empty: the device is going offline
        presence_ttl_secs: Some(state.config.presence_ttl_secs),
    };
    let Some(device_key) = state.device_key().await else {
        return; // signing key unavailable — nothing to send
    };
    let sig = match device_key.sign_payload(&ping) {
        Ok(s) => s,
        Err(_) => return,
    };
    let envelope = PresenceEnvelope {
        schema_version: SCHEMA_VERSION.to_string(),
        device_id,
        payload: ping,
        sig,
    };
    let url = format!("{}/api/v1/presence", cloud_url.trim_end_matches('/'));
    // Fully best-effort: ignore the outcome entirely.
    let _ = state
        .http
        .post(&url)
        .timeout(SHUTDOWN_BEAT_TIMEOUT)
        .json(&envelope)
        .send()
        .await;
    tracing::debug!("heartbeat: sent final offline presence beat");
}

/// The heartbeat loop: build one client, then POST a presence snapshot each tick
/// at an **adaptive cadence** (Phase 6a). The loop carries the dedup state — the
/// last sent `sessions` JSON and when it was sent — across ticks so `beat` can
/// skip a redundant POST while still guaranteeing one keepalive per TTL.
///
/// WP-A3: the sleep between ticks races against `state.presence_wake` — an event
/// at one of the sync-trigger sites (writer/control/capture) wakes the loop
/// immediately, so a long deep-idle sleep never delays presence catching up with
/// real activity.
async fn run(state: AppState) {
    // Dedup state: the JSON of the last `sessions` we actually sent, and the
    // instant of that send. `None` forces the first beat to send.
    let mut last_sent_sessions: Option<String> = None;
    let mut last_sent_at: Option<Instant> = None;
    // Whether any session was active (recent activity) on the last beat, used to
    // pick the next cadence even when we skipped the POST.
    let mut last_any_active = false;
    // WP-A3: newest-activity fallback for the deep-idle predicate, used only when
    // the registry has never observed a single event (see `beat`'s doc comment).
    let mut last_nonempty_at = OffsetDateTime::now_utc();
    // Whether the just-completed beat considered the device deep idle, so the
    // cadence selection (below) can pace off the deep-idle TTL instead of the
    // ordinary active/idle bands.
    let mut deep_idle = false;

    loop {
        let result = beat(
            &state,
            &mut last_sent_sessions,
            &mut last_sent_at,
            &mut last_any_active,
            &mut last_nonempty_at,
            &mut deep_idle,
        )
        .await;

        // Pace the next tick adaptively. Active sessions ⇒ fast cadence; all idle
        // (or none) ⇒ slow cadence; deep idle ⇒ the slow deep-idle TTL ceiling. A
        // cloud `next_beat_hint_secs` overrides the ordinary bands. Either way we
        // never let the gap reach the TTL: a beat must land before the cloud would
        // expire the device.
        let next = next_cadence(
            &state,
            last_any_active,
            matches!(result, BeatResult::Noop),
            deep_idle,
        );
        tokio::select! {
            _ = tokio::time::sleep(next) => {}
            _ = state.presence_wake.notified() => {
                tracing::debug!("heartbeat: woken by activity, beating early");
            }
        }
    }
}

/// Decide how long to wait before the next beat.
///
/// Priority:
/// 1. Deep idle (WP-A3): pace directly off the deep-idle TTL's own headroomed
///    ceiling (skipping the hint/active/idle bands below) — there's nothing to
///    keep fresh but the device's bare "still online" signal, and the instant
///    wake (`presence_wake`) covers the moment that stops being true.
/// 2. A live cloud hint (`PresenceAck.next_beat_hint_secs`, stashed in
///    `presence_hints`) — the cloud knows its own load/expiry best.
/// 3. Otherwise the configured adaptive band: `heartbeat_active` when any session
///    is active, `heartbeat_idle` when all are idle / none.
///
/// The result is jittered by ±10% (so many daemons don't beat the cloud in
/// lockstep) and then always capped strictly below the effective TTL (deep-idle
/// TTL when deep idle, else the cloud-advertised `ttl_secs` or our configured
/// TTL) minus a small safety margin, so the keepalive renews presence before it
/// expires — jitter is applied BEFORE the TTL clamp, so it can only ever pull
/// the cadence in, never push it past the ceiling. When we couldn't beat at all
/// (unconfigured/unlinked), fall back to the idle cadence — there's nothing to
/// keep alive yet, so there's no point spinning fast.
fn next_cadence(
    state: &AppState,
    any_active: bool,
    was_noop: bool,
    deep_idle: bool,
) -> StdDuration {
    let ttl_ceiling_std = ttl_ceiling_std(effective_ttl_secs(state, deep_idle));
    let base_std = next_cadence_base(state, any_active, was_noop, deep_idle, ttl_ceiling_std);
    jittered_cadence(base_std, ttl_ceiling_std)
}

/// Effective TTL basis (seconds) for the ceiling used both to pace the next
/// tick ([`next_cadence`]) and to bound the dedup-skip safety check in
/// [`beat`] — factored out so the two can never drift apart.
///
/// Deep idle takes `min(deep-idle TTL, cloud-acked TTL)` (via
/// [`dedup_ttl_secs`], same rule the dedup window uses): a self-hosted cloud
/// clamping presence TTL below our 600s advertisement acks the smaller value,
/// and pacing off the unclamped 600 would let deep-idle devices expire between
/// beats (issue #23). The cost is one conservative tick per deep-idle entry —
/// the last ack from the active band (~75s) bounds the first deep-idle
/// cadence, that beat's ack then carries the cloud's real deep-idle clamp and
/// the cadence stretches to it. Outside deep idle, prefer the cloud's
/// advertised value when it has answered, else our configured TTL.
fn effective_ttl_secs(state: &AppState, deep_idle: bool) -> u64 {
    let cloud_ttl = state.presence_hints.ttl_secs.load(Ordering::Relaxed);
    if deep_idle {
        dedup_ttl_secs(state.config.presence_ttl_deep_idle(), cloud_ttl)
    } else if cloud_ttl > 0 {
        cloud_ttl
    } else {
        state.config.presence_ttl_secs
    }
}

/// The pre-jitter cadence [`next_cadence`] bases its pick on — thin `AppState`
/// glue over the pure [`select_cadence_base`], which does the actual band
/// selection.
fn next_cadence_base(
    state: &AppState,
    any_active: bool,
    was_noop: bool,
    deep_idle: bool,
    ttl_ceiling_std: StdDuration,
) -> StdDuration {
    let hint = nonzero(
        state
            .presence_hints
            .next_beat_hint_secs
            .load(Ordering::Relaxed),
    );
    select_cadence_base(
        std_secs(state.config.heartbeat_idle()),
        std_secs(state.config.heartbeat_active()),
        hint,
        any_active,
        was_noop,
        deep_idle,
        ttl_ceiling_std,
    )
}

/// Pure band-selection step underlying [`next_cadence_base`] — decoupled from
/// `AppState` so both [`beat`]'s dedup-skip check and the property test below
/// can exercise the exact same priority order the real cadence loop uses. See
/// [`next_cadence`]'s doc comment for the priority rationale.
fn select_cadence_base(
    idle_cadence: StdDuration,
    active_cadence: StdDuration,
    hint_secs: Option<u64>,
    any_active: bool,
    was_noop: bool,
    deep_idle: bool,
    ttl_ceiling_std: StdDuration,
) -> StdDuration {
    if deep_idle {
        // Pace directly off the ceiling itself — see priority (1) above.
        ttl_ceiling_std
    } else if let Some(hint) = hint_secs {
        // Priority (2) above, checked BEFORE `was_noop`: a 429 beat itself
        // returns `Noop` (see `beat`'s 429 arm), so the very next tick after
        // stashing a Retry-After hint is exactly the tick where `was_noop`
        // would otherwise be true. Checking `was_noop` first would silently
        // discard the hint we just stashed in favor of the plain idle band,
        // defeating the whole point of honoring Retry-After. `hint_secs` is
        // `None` for every OTHER kind of "couldn't beat" (unconfigured,
        // unlinked, sign failure, network error) — those fall through to the
        // `was_noop` branch below exactly as before.
        StdDuration::from_secs(hint)
    } else if was_noop {
        idle_cadence
    } else if any_active {
        active_cadence
    } else {
        idle_cadence
    }
}

/// Floor a `time::Duration` down to whole seconds as a `StdDuration`, never
/// negative. Shared conversion used wherever a `Config` cadence needs to
/// cross into the `std::time` types the jitter/dedup math is written in.
fn std_secs(d: Duration) -> StdDuration {
    StdDuration::from_secs(d.whole_seconds().max(0) as u64)
}

/// Headroomed TTL ceiling as a `StdDuration`: `ttl - 5s` (or `ttl/2` for tiny
/// TTLs, floored at 1s) — leaves margin so a beat lands before the cloud expires
/// the device. Shared by the normal and deep-idle cadence bases in
/// [`next_cadence`].
fn ttl_ceiling_std(ttl_secs: u64) -> StdDuration {
    StdDuration::from_secs(ttl_secs.saturating_sub(5).max(ttl_secs / 2).max(1))
}

/// Pure jitter-then-clamp step, factored out of [`next_cadence`] so it's
/// testable without an `AppState`: jitter `base` by ±10%, then clamp to
/// `[1s, ttl_ceiling]`. The clamp runs AFTER jitter, so the result can never
/// reach or exceed `ttl_ceiling` regardless of how jitter perturbed `base`.
fn jittered_cadence(base: StdDuration, ttl_ceiling: StdDuration) -> StdDuration {
    let spread = crate::jitter::jittered(base, crate::jitter::DEFAULT_FRAC);
    spread.min(ttl_ceiling).max(StdDuration::from_secs(1))
}

/// Deterministic (non-random) upper bound on what [`jittered_cadence`] can
/// return for a given `base`/`ttl_ceiling` pair: `base` inflated by the
/// worst-case jitter spread (`1 + DEFAULT_FRAC`), then clamped to the
/// ceiling exactly like the real jitter step. Used by the dedup-skip safety
/// check in [`beat`] so it can prove a skip is safe against the NEXT tick's
/// worst case without depending on which way the actual jitter draw rolls
/// (jitter is sampled fresh per tick, so the value used to pace THIS sleep
/// isn't known yet when the skip decision is made).
fn jittered_cadence_upper_bound(base: StdDuration, ttl_ceiling: StdDuration) -> StdDuration {
    base.mul_f64(1.0 + crate::jitter::DEFAULT_FRAC)
        .min(ttl_ceiling)
}

/// `0` is the sentinel for "no hint" in the presence atomics.
fn nonzero(v: u64) -> Option<u64> {
    (v > 0).then_some(v)
}

/// Deep-idle predicate (WP-A3): true when the registry currently has zero active
/// (non-ended) sessions AND the newest known activity is at least
/// `deep_idle_after` in the past. Pure so the fast→idle→deep-idle→wake
/// transitions are unit-testable without an `AppState`. Shared with the idle
/// ticker's sweep decimation (`crate::sweep_this_tick`) so both tasks agree on
/// what "deep idle" means.
pub(crate) fn is_deep_idle(
    active_sessions: usize,
    idle_for: Duration,
    deep_idle_after: Duration,
) -> bool {
    active_sessions == 0 && idle_for >= deep_idle_after
}

/// Dedup-TTL fix (WP-A3): the window used to gate the byte-identical-snapshot
/// skip must never outlive the cloud's actual acked TTL. Before this fix the
/// dedup window used only our own configured `presence_ttl_secs`; if the cloud's
/// last-acked `ttl_secs` was SHORTER (e.g. a cloud-side clamp), dedup could keep
/// skipping the POST past the point the cloud actually expires the device,
/// flapping it offline. `0` is the "no ack yet" sentinel, so we fall back to
/// whatever we're advertising this tick.
fn dedup_ttl_secs(advertise_ttl_secs: u64, cloud_acked_ttl_secs: u64) -> u64 {
    if cloud_acked_ttl_secs > 0 {
        advertise_ttl_secs.min(cloud_acked_ttl_secs)
    } else {
        advertise_ttl_secs
    }
}

/// The expiry-aware dedup-skip rule: true when skipping a byte-identical
/// payload right now is provably safe. Pure and shared verbatim by [`beat`]
/// and the property test — see the inline comment at the call site in
/// [`beat`] for the inductive argument for why this bounds the max gap
/// between real sends to strictly under `dedup_window_secs`.
///
/// `elapsed_since_last_send` is how long it's been since the last REAL send;
/// `upcoming` is the jitter-max-bounded cadence the NEXT tick will pick (see
/// [`jittered_cadence_upper_bound`]); `dedup_window_secs` is
/// [`dedup_ttl_secs`] — the effective TTL this skip must not outlive.
fn dedup_skip_is_safe(
    elapsed_since_last_send: StdDuration,
    upcoming: StdDuration,
    dedup_window_secs: u64,
) -> bool {
    elapsed_since_last_send + upcoming + DEDUP_SKIP_MARGIN
        < StdDuration::from_secs(dedup_window_secs.max(1))
}

/// One heartbeat attempt. Re-reads config + linkage each call so `dira device
/// link` / a config change takes effect without a daemon restart. Holds no lock
/// across the HTTP await.
///
/// Reads the per-session `engaged_seconds` / `active_seconds` straight off the
/// live registry (Phase 6b) — the writer maintains them incrementally, so there
/// is no per-tick SQLite scan any more.
///
/// **Dedup (Phase 6a, fixed in WP-A3, made expiry-aware in a later fix):** the
/// `sessions` snapshot is serialized and compared to `last_sent_sessions`. If
/// it is byte-identical AND skipping is provably safe, the POST is skipped
/// entirely. "Provably safe" means the NEXT tick's worst-case cadence still
/// lands before [`dedup_ttl_secs`] — the shorter of what we're about to
/// advertise this tick and the cloud's last-acked `ttl_secs` — expires (see
/// the inline comment at the skip check for the inductive argument). Cadence
/// and dedup window are no longer tuned independently against each other:
/// whatever the pacing bands are, the skip rule can only fire when it
/// structurally cannot outlive the TTL. In the deep-idle band this means
/// dedup skips essentially never fire (the ~570-595s cadence is already
/// nearly as long as the 600s TTL) — the request reduction there comes from
/// the slow cadence itself, not from dedup. We always send when the snapshot
/// changes, and the skip rule guarantees at least one real send per TTL
/// window (the keepalive) so a quiet device never silently expires.
///
/// **Deep idle (WP-A3):** `last_nonempty_at` is the loop-carried fallback for
/// "newest known activity" used only when the registry has never observed a
/// single event (a brand new, never-active device) — once at least one event
/// has been observed, [`crate::state::SessionRegistry::last_activity_at`]
/// (which persists across sessions ending, unlike the live `active` snapshot)
/// is authoritative.
async fn beat(
    state: &AppState,
    last_sent_sessions: &mut Option<String>,
    last_sent_at: &mut Option<Instant>,
    last_any_active: &mut bool,
    last_nonempty_at: &mut OffsetDateTime,
    last_deep_idle: &mut bool,
) -> BeatResult {
    // Gate on configuration + linkage, both re-read every tick.
    let Some(cloud_url) = state.config.cloud_url.clone() else {
        return BeatResult::Noop; // presence off until a cloud_url is configured
    };
    let device_id = match identity::device_id(&state.store).await {
        Ok(Some(id)) => id,
        Ok(None) => return BeatResult::Noop, // not linked yet
        Err(e) => {
            tracing::warn!("heartbeat: read device_id failed: {e}");
            return BeatResult::Noop;
        }
    };

    let now = OffsetDateTime::now_utc();
    let idle = state.config.idle();

    // Snapshot the live registry (with its rolling counters) plus the newest
    // activity across ALL known sessions (active or ended), then release the
    // lock before any await. `engaged_seconds` / `agent_wall_seconds` come
    // straight off `LiveSession` — no SQLite scan.
    let (active, last_activity) = match state.sessions.lock() {
        Ok(reg) => (reg.active(), reg.last_activity_at()),
        Err(e) => {
            tracing::warn!("heartbeat: sessions lock poisoned: {e}");
            return BeatResult::Noop;
        }
    };
    let sessions: Vec<PresenceSession> = active
        .iter()
        .map(|s| to_presence_session(s, now, idle))
        .collect();

    // Track whether any session is active (recent activity) for the next cadence.
    *last_any_active = sessions.iter().any(|s| !s.idle);

    // Deep-idle gating (WP-A3): zero active sessions AND the newest known
    // activity is at least `deep_idle_after` in the past. `last_nonempty_at`
    // only matters as a fallback when the registry has literally never seen an
    // event (see the doc comment above); once it has, `last_activity` wins.
    if !active.is_empty() {
        *last_nonempty_at = now;
    }
    let newest_activity = last_activity.unwrap_or(*last_nonempty_at);
    let idle_for = (now - newest_activity).max(Duration::ZERO);
    let deep_idle = is_deep_idle(active.len(), idle_for, state.config.deep_idle_after());
    *last_deep_idle = deep_idle;

    // The TTL we're about to tell the cloud to honor this tick: the deep-idle
    // TTL while deep idle, else the normal configured TTL. Activity resuming
    // (deep_idle flips back to false) re-advertises the normal TTL on the very
    // next beat — no extra state needed, it falls out of recomputing this fresh
    // every tick.
    let advertise_ttl_secs = if deep_idle {
        state.config.presence_ttl_deep_idle()
    } else {
        state.config.presence_ttl_secs
    };

    // Dedup key: the JSON of the `sessions` list. `sent_at` and the signature are
    // deliberately excluded — they change every tick and would defeat dedup. A
    // serialize failure simply forces a send (treated as "changed").
    let sessions_json = serde_json::to_string(&sessions).ok();
    let cloud_acked_ttl = state.presence_hints.ttl_secs.load(Ordering::Relaxed);
    let dedup_window_secs = dedup_ttl_secs(advertise_ttl_secs, cloud_acked_ttl);
    let unchanged = match (sessions_json.as_ref(), last_sent_sessions.as_ref()) {
        (Some(cur), Some(prev)) => cur == prev,
        _ => false,
    };
    // Expiry-aware skip rule (fixes the deep-idle/idle offline-flap bug): a
    // byte-identical payload may be skipped ONLY when the send that would
    // follow it is guaranteed to still land before `dedup_window_secs`
    // expires. `upcoming` is the worst case (jitter-max bounded, see
    // `jittered_cadence_upper_bound`) cadence the loop will pick for the NEXT
    // tick given what's already known this tick (`*last_any_active`,
    // `deep_idle`; `was_noop` is false because this tick isn't a Noop on the
    // skip path). `DEDUP_SKIP_MARGIN` covers the in-flight POST latency
    // (`HTTP_TIMEOUT`) plus scheduling slack. This is inductively safe: if the
    // check passes, `elapsed_since_last_send + upcoming + MARGIN <
    // dedup_window_secs` holds, so the tick `upcoming` from now — the
    // earliest the next real send can happen — still lands with `MARGIN` to
    // spare before expiry; if it fails, we send now instead, resetting
    // `elapsed_since_last_send` to ~0. Either way the gap between two REAL
    // sends can never reach `dedup_window_secs`.
    let ttl_ceiling = ttl_ceiling_std(effective_ttl_secs(state, deep_idle));
    let upcoming = jittered_cadence_upper_bound(
        next_cadence_base(state, *last_any_active, false, deep_idle, ttl_ceiling),
        ttl_ceiling,
    );
    let safe_to_skip = last_sent_at
        .map(|t| dedup_skip_is_safe(t.elapsed(), upcoming, dedup_window_secs))
        .unwrap_or(false);
    if unchanged && safe_to_skip {
        tracing::debug!(
            sessions = sessions.len(),
            deep_idle,
            upcoming_secs = upcoming.as_secs(),
            "heartbeat: snapshot unchanged and next tick still lands within TTL — skipping POST (dedup)"
        );
        return BeatResult::SkippedDuplicate;
    }

    let ping = PresencePing {
        device_id: device_id.clone(),
        sent_at: fmt_rfc3339(now),
        sessions,
        // Send the device's intended TTL in-band so the cloud knows it even
        // before answering with a PresenceAck. Omitted-when-None keeps older
        // payloads (and the signing vector) byte-identical; we always populate it
        // here — the deep-idle TTL while deep idle, else the normal configured one.
        presence_ttl_secs: Some(advertise_ttl_secs),
    };
    let Some(device_key) = state.device_key().await else {
        tracing::warn!("heartbeat: signing key unavailable");
        return BeatResult::Noop;
    };
    let sig = match device_key.sign_payload(&ping) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("heartbeat: sign failed: {e}");
            return BeatResult::Noop;
        }
    };
    let session_count = ping.sessions.len();
    let envelope = PresenceEnvelope {
        schema_version: SCHEMA_VERSION.to_string(),
        device_id,
        payload: ping,
        sig,
    };

    let url = format!("{}/api/v1/presence", cloud_url.trim_end_matches('/'));
    match state
        .http
        .post(&url)
        .timeout(HTTP_TIMEOUT)
        .json(&envelope)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                // Record the sent snapshot for dedup, then parse + stash the cloud's
                // pacing hints for the adaptive cadence (Phase 6a).
                *last_sent_sessions = sessions_json;
                *last_sent_at = Some(Instant::now());
                let body = resp.text().await.unwrap_or_default();
                let ack = parse_presence_ack(&body);
                stash_hints(state, &ack);
                tracing::debug!(
                    sessions = session_count,
                    any_active = *last_any_active,
                    deep_idle,
                    ttl_secs = advertise_ttl_secs,
                    "heartbeat: sent presence snapshot"
                );
                BeatResult::Sent
            } else if status.as_u16() == 429 {
                // WP-B6: stash the cloud's Retry-After into the EXISTING
                // `next_beat_hint_secs` pacing hint — `select_cadence_base`
                // checks a live hint BEFORE `was_noop`/the active-idle bands
                // (see its doc comment above), so a hint stashed by THIS
                // Noop tick still wins on the very next tick's cadence pick,
                // and this is the only line needed to make it wait the hint
                // out; no new pacing plumbing. Header form first, typed body
                // as fallback. Deliberately does NOT touch
                // `last_sent_sessions` / `last_sent_at` — the expiry-aware
                // dedup-skip logic above this function is entirely
                // unaffected by a rejected send.
                let header_hint = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(dira_core::sync::parse_retry_after_secs);
                let body = resp.text().await.unwrap_or_default();
                let hint = header_hint.or_else(|| dira_core::sync::parse_retry_after_body(&body));
                if let Some(secs) = hint {
                    state
                        .presence_hints
                        .next_beat_hint_secs
                        .store(secs, Ordering::Relaxed);
                }
                tracing::debug!("heartbeat: presence 429 (rate limited), retry_after={hint:?}");
                BeatResult::Noop
            } else {
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!("heartbeat: presence {status}: {body}");
                BeatResult::Noop
            }
        }
        Err(e) => {
            // Network blip — ephemeral, the next tick retries. Keep it quiet.
            tracing::debug!("heartbeat: post presence failed: {e}");
            BeatResult::Noop
        }
    }
}

/// Parse a 2xx presence response body into a typed [`PresenceAck`]. Tolerant:
/// an empty body or one the current schema can't fully read both degrade to a
/// default ack, so presence never errors over a logging/hint detail.
fn parse_presence_ack(body: &str) -> PresenceAck {
    if body.trim().is_empty() {
        return PresenceAck::default();
    }
    serde_json::from_str(body).unwrap_or_default()
}

/// Stash the cloud's pacing hints into shared atomics for the future adaptive
/// heartbeat (Phase 6). `0` is the "unset" sentinel for `next_beat_hint_secs`
/// (the ack uses `None`). Nothing reads these yet — this is wiring only.
fn stash_hints(state: &AppState, ack: &PresenceAck) {
    state
        .presence_hints
        .ttl_secs
        .store(ack.ttl_secs, Ordering::Relaxed);
    state
        .presence_hints
        .next_beat_hint_secs
        .store(ack.next_beat_hint_secs.unwrap_or(0), Ordering::Relaxed);
}

/// Map a [`LiveSession`] to a wire [`PresenceSession`].
///
/// `repo_canonical` is the session's resolved project. `branch` is always `None`
/// here — the live registry does not track a per-session branch (it is captured
/// on events, not folded into `LiveSession`). `engaged_seconds` and
/// `agent_wall_seconds` are read straight off the live session's incrementally-
/// maintained rolling counters (Phase 6b): `engaged_seconds` is de-duplicated,
/// idle-trimmed human time and `active_seconds` is the idle-trimmed active span
/// over all of the session's events — each equal to the accounting-core scan over
/// the same event sequence, with no per-tick SQLite read.
fn to_presence_session(s: &LiveSession, now: OffsetDateTime, idle: Duration) -> PresenceSession {
    PresenceSession {
        session_id: s.session_id.clone(),
        harness: s.harness,
        kind: s.kind,
        repo_canonical: s.project.clone(),
        branch: None,
        identity_email: s.identity_email.clone().unwrap_or_default(),
        started_at: fmt_rfc3339(s.started_at),
        last_signal_at: s.last_signal_at.map(fmt_rfc3339),
        // WP-A4: quantized to 15s buckets in the PRESENCE PAYLOAD ONLY — the
        // accounting/sync/billing paths (`sync::batch`, `report`) read the exact
        // `LiveSession` counters untouched. These counters advance on effectively
        // every tick while a session is open, which defeats the byte-identical
        // dedup above almost always; rounding down to a 15s grid makes the
        // snapshot byte-identical across ticks until a counter actually crosses a
        // bucket boundary, so dedup fires for roughly half of an active session's
        // otherwise-redundant beats.
        engaged_seconds: quantize_15s(s.engaged_seconds),
        agent_wall_seconds: quantize_15s(s.active_seconds),
        // Activity-based idle for the live view: a session is "idle" when there has
        // been NO activity (any event — agent tool calls included) within the idle
        // window. We deliberately do NOT use `is_idle` (last *human* signal): an agent
        // churning away with no recent human prompt is active in "Right now", and the
        // cloud hides idle sessions.
        idle: (now - s.last_event_at) > idle,
    }
}

/// Round `v` down to the nearest 15-second bucket (WP-A4). Presence-payload-only
/// quantization: evidence for "is this session still alive", not a billing base,
/// so losing sub-15s precision in the live snapshot is fine.
fn quantize_15s(v: u64) -> u64 {
    (v / 15) * 15
}

/// RFC 3339 timestamp, matching the formatting used in `sync.rs` / `batch.rs`.
/// Crate-visible so sibling cloud tasks (`billing.rs`) share the one fallback.
pub(crate) fn fmt_rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockCloud, MockResp};
    use dira_contract::{Harness, SessionKind};

    /// A linked, cloud-configured `AppState` with no live sessions — enough for
    /// `beat` to attempt a presence POST (an empty `sessions` list is itself a
    /// valid, sendable snapshot).
    async fn linked_state(cloud: &MockCloud) -> AppState {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();
        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        state
    }

    /// Issue #23: deep-idle pacing must honor a cloud that clamps the presence
    /// TTL below our deep-idle advertisement — `effective_ttl_secs` takes
    /// `min(deep-idle TTL, acked TTL)`, falling back to the deep-idle TTL only
    /// while the cloud has never answered.
    #[tokio::test]
    async fn deep_idle_effective_ttl_respects_a_smaller_cloud_ack() {
        let cloud = MockCloud::start(&["/api/v1/presence"]).await;
        let state = linked_state(&cloud).await;
        let deep = state.config.presence_ttl_deep_idle();

        // No ack yet: pace off the configured deep-idle TTL (fast start).
        state.presence_hints.ttl_secs.store(0, Ordering::Relaxed);
        assert_eq!(effective_ttl_secs(&state, true), deep);

        // Self-host clamp below the advertisement: the ack must win.
        state.presence_hints.ttl_secs.store(300, Ordering::Relaxed);
        assert_eq!(effective_ttl_secs(&state, true), 300);

        // An ack larger than the deep-idle TTL never stretches the ceiling.
        state
            .presence_hints
            .ttl_secs
            .store(deep + 300, Ordering::Relaxed);
        assert_eq!(effective_ttl_secs(&state, true), deep);

        // Outside deep idle the acked value is used verbatim, as before.
        state.presence_hints.ttl_secs.store(42, Ordering::Relaxed);
        assert_eq!(effective_ttl_secs(&state, false), 42);
    }

    /// WP-B6: a 429 with a `Retry-After` header must stash it into
    /// `next_beat_hint_secs` (the cadence loop already honors that hint —
    /// nothing else about pacing changes) and must not disturb the dedup state.
    #[tokio::test]
    async fn beat_429_with_retry_after_header_stashes_the_hint() {
        let cloud = MockCloud::start(&["/api/v1/presence"]).await;
        cloud.push(
            "/api/v1/presence",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":99}"#)
                .with_header("Retry-After", "13"),
        );
        let state = linked_state(&cloud).await;

        let mut last_sent_sessions = None;
        let mut last_sent_at = None;
        let mut last_any_active = false;
        let mut last_nonempty_at = OffsetDateTime::now_utc();
        let mut last_deep_idle = false;

        let result = beat(
            &state,
            &mut last_sent_sessions,
            &mut last_sent_at,
            &mut last_any_active,
            &mut last_nonempty_at,
            &mut last_deep_idle,
        )
        .await;

        assert_eq!(result, BeatResult::Noop);
        assert_eq!(
            state
                .presence_hints
                .next_beat_hint_secs
                .load(Ordering::Relaxed),
            13,
            "header (13) must win over the body's retryAfterSecs (99)"
        );
        // Dedup state untouched by a rejected send.
        assert!(last_sent_sessions.is_none());
        assert!(last_sent_at.is_none());

        // Regression coverage for the priority-order fix: the NEXT tick's
        // cadence must actually equal the just-stashed hint, not fall back to
        // the idle band just because THIS tick (the one that stashed it) was
        // itself a `Noop`. Exercises the exact inputs the real `run` loop
        // feeds `next_cadence`/`next_cadence_base` from this tick's outcome.
        let ttl_ceiling = ttl_ceiling_std(effective_ttl_secs(&state, last_deep_idle));
        let next_base = next_cadence_base(
            &state,
            last_any_active,
            matches!(result, BeatResult::Noop),
            last_deep_idle,
            ttl_ceiling,
        );
        assert_eq!(
            next_base,
            StdDuration::from_secs(13).min(ttl_ceiling),
            "the next tick's cadence must equal the stashed Retry-After hint \
             (clamped by the TTL ceiling), not the idle band"
        );
    }

    /// WP-B6: without a header, the typed body's `retryAfterSecs` is the fallback.
    #[tokio::test]
    async fn beat_429_without_header_falls_back_to_typed_body() {
        let cloud = MockCloud::start(&["/api/v1/presence"]).await;
        cloud.push(
            "/api/v1/presence",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":21}"#),
        );
        let state = linked_state(&cloud).await;

        let mut last_sent_sessions = None;
        let mut last_sent_at = None;
        let mut last_any_active = false;
        let mut last_nonempty_at = OffsetDateTime::now_utc();
        let mut last_deep_idle = false;

        beat(
            &state,
            &mut last_sent_sessions,
            &mut last_sent_at,
            &mut last_any_active,
            &mut last_nonempty_at,
            &mut last_deep_idle,
        )
        .await;

        assert_eq!(
            state
                .presence_hints
                .next_beat_hint_secs
                .load(Ordering::Relaxed),
            21
        );
    }

    /// Property: whatever `base` jitter draws, the clamped cadence never reaches
    /// or exceeds the TTL ceiling — the invariant the adaptive heartbeat relies on
    /// to guarantee a keepalive lands before the cloud expires the device.
    #[test]
    fn jittered_cadence_never_reaches_the_ttl_ceiling() {
        let ceiling = StdDuration::from_secs(70); // e.g. default TTL 75 - 5 headroom
        for base_secs in [1u64, 5, 10, 50, 69, 70, 71, 90, 600, 10_000] {
            for _ in 0..200 {
                let c = jittered_cadence(StdDuration::from_secs(base_secs), ceiling);
                assert!(c <= ceiling, "cadence {c:?} exceeded ceiling {ceiling:?}");
                assert!(
                    c >= StdDuration::from_secs(1),
                    "cadence {c:?} below the 1s floor"
                );
            }
        }
    }

    /// A base comfortably under the ceiling still varies by jitter (not pinned to
    /// a single value), so the lockstep-avoidance actually does something.
    #[test]
    fn jittered_cadence_varies_when_headroom_allows() {
        let ceiling = StdDuration::from_secs(1000);
        let base = StdDuration::from_secs(100);
        let samples: std::collections::HashSet<StdDuration> =
            (0..200).map(|_| jittered_cadence(base, ceiling)).collect();
        assert!(
            samples.len() > 1,
            "expected jitter to produce varying cadences, got {samples:?}"
        );
    }

    /// Existing heartbeat idle/active bands must stay green under jitter: the
    /// active cadence's jittered value never exceeds the TTL ceiling either, even
    /// at the tight default config (active=10s, idle=90s clamped to 70s, ttl=75s).
    #[test]
    fn default_bands_stay_under_ttl_ceiling_after_jitter() {
        let config = dira_core::Config::default();
        let ttl_ceiling = StdDuration::from_secs(
            config
                .presence_ttl_secs
                .saturating_sub(5)
                .max(config.presence_ttl_secs / 2)
                .max(1),
        );
        let active = StdDuration::from_secs(config.heartbeat_active_secs.max(1));
        let idle = StdDuration::from_secs(config.heartbeat_idle().whole_seconds().max(1) as u64);
        for base in [active, idle] {
            for _ in 0..200 {
                let c = jittered_cadence(base, ttl_ceiling);
                assert!(c <= ttl_ceiling);
            }
        }
    }

    /// WP-A3: the deep-idle predicate across a fast→idle→deep-idle→wake sequence.
    #[test]
    fn deep_idle_predicate_covers_fast_idle_deep_idle_wake_transitions() {
        let deep_idle_after = Duration::seconds(900);

        // "Fast": an active session ⇒ never deep idle, no matter how stale
        // `idle_for` looks (it wouldn't be, in practice, but the predicate must
        // not depend on that — zero active sessions is the hard gate).
        assert!(!is_deep_idle(1, Duration::seconds(10_000), deep_idle_after));

        // "Idle" (zero active sessions) but the quiet window hasn't elapsed yet.
        assert!(!is_deep_idle(0, Duration::ZERO, deep_idle_after));
        assert!(!is_deep_idle(0, Duration::seconds(899), deep_idle_after));

        // "Deep idle": zero active sessions AND the quiet window has fully
        // elapsed (inclusive at the boundary, and beyond it).
        assert!(is_deep_idle(0, Duration::seconds(900), deep_idle_after));
        assert!(is_deep_idle(0, Duration::seconds(10_000), deep_idle_after));

        // "Wake": activity resumes ⇒ `idle_for` collapses back to ~0 and the
        // session count goes non-zero ⇒ immediately leaves deep idle.
        assert!(!is_deep_idle(1, Duration::ZERO, deep_idle_after));
    }

    /// The deep-idle cadence bases off the deep-idle TTL's own headroomed
    /// ceiling (WP-A3) — not the ordinary active/idle bands — and jitter still
    /// respects that ceiling.
    #[test]
    fn deep_idle_cadence_bases_off_the_deep_idle_ttl_ceiling() {
        let ceiling = ttl_ceiling_std(600); // 600 - 5 = 595s headroom
        assert_eq!(ceiling, StdDuration::from_secs(595));
        for _ in 0..200 {
            let c = jittered_cadence(ceiling, ceiling);
            assert!(c <= ceiling, "deep-idle cadence {c:?} exceeded {ceiling:?}");
            assert!(c >= StdDuration::from_secs(1));
        }
    }

    /// WP-A3 dedup-TTL bug fix: the dedup window must be the SHORTER of what
    /// we're advertising this tick and the cloud's last-acked TTL, never just our
    /// own configured/advertised value in isolation.
    #[test]
    fn dedup_ttl_uses_the_shorter_of_advertised_and_cloud_acked() {
        // No ack yet (0 sentinel) ⇒ use what we're advertising this tick.
        assert_eq!(dedup_ttl_secs(75, 0), 75);
        assert_eq!(dedup_ttl_secs(600, 0), 600);

        // The bug this fixes: a cloud-acked TTL SHORTER than what we're
        // advertising must win, or dedup could outlive the cloud's actual expiry
        // and flap the device offline.
        assert_eq!(dedup_ttl_secs(600, 75), 75);

        // A cloud-acked TTL longer than what we're advertising must NOT extend
        // the dedup window past what we told the cloud we intend this tick.
        assert_eq!(dedup_ttl_secs(75, 600), 75);

        assert_eq!(dedup_ttl_secs(75, 75), 75);
    }

    /// End-to-end property test for the expiry-aware dedup-skip fix: simulate
    /// the tick loop with the same pure building blocks `beat`/`next_cadence`
    /// use (`ttl_ceiling_std`, `select_cadence_base`, `jittered_cadence`,
    /// `jittered_cadence_upper_bound`, `dedup_ttl_secs`, `dedup_skip_is_safe`)
    /// across all three bands — active, idle, and deep idle — under real
    /// (freshly-drawn-per-tick, i.e. adversarial) jitter.
    ///
    /// This is the regression test for the bug the fix addresses: before it,
    /// deep idle's skip-then-send arithmetic let two REAL sends land
    /// ~1071-1190s apart against a 600s TTL. The expiry-aware rule makes that
    /// structurally impossible — this test proves it by construction rather
    /// than by picking specific numbers.
    #[test]
    fn simulated_tick_loop_never_lets_real_sends_gap_past_the_ttl() {
        #[derive(Clone, Copy)]
        struct Band {
            name: &'static str,
            any_active: bool,
            deep_idle: bool,
            ttl_secs: u64,
        }

        // Real, clamped `Config` cadences (active=10s; idle clamped to just
        // under the normal TTL's ceiling; ttl=75s / deep-idle ttl=600s) —
        // exactly what the production loop would compute.
        let config = dira_core::Config::default();
        let idle_cadence = std_secs(config.heartbeat_idle());
        let active_cadence = std_secs(config.heartbeat_active());

        let bands = [
            Band {
                name: "active",
                any_active: true,
                deep_idle: false,
                ttl_secs: config.presence_ttl_secs,
            },
            Band {
                name: "idle",
                any_active: false,
                deep_idle: false,
                ttl_secs: config.presence_ttl_secs,
            },
            Band {
                name: "deep_idle",
                any_active: false,
                deep_idle: true,
                ttl_secs: config.presence_ttl_deep_idle(),
            },
        ];

        const TICKS: usize = 500;
        const TRIALS: usize = 5;

        for band in bands {
            let ttl_ceiling = ttl_ceiling_std(band.ttl_secs);
            let mut max_gap = StdDuration::ZERO;
            let mut active_skip_count = 0usize;

            for _ in 0..TRIALS {
                let mut now = StdDuration::ZERO;
                let mut last_real_send_at: Option<StdDuration> = None;

                for _ in 0..TICKS {
                    let base = select_cadence_base(
                        idle_cadence,
                        active_cadence,
                        None, // no cloud `next_beat_hint_secs` in this simulation
                        band.any_active,
                        false, // was_noop: never true on the dedup-skip path
                        band.deep_idle,
                        ttl_ceiling,
                    );
                    // No cloud ack in this simulation, so the dedup window is
                    // exactly the band's advertised TTL — the tightest case.
                    let dedup_window_secs = dedup_ttl_secs(band.ttl_secs, 0);
                    let upcoming = jittered_cadence_upper_bound(base, ttl_ceiling);

                    // Every tick after the first reproduces a byte-identical
                    // payload — the worst case for exercising the skip rule.
                    let unchanged = last_real_send_at.is_some();
                    let safe_to_skip = last_real_send_at
                        .map(|t| dedup_skip_is_safe(now - t, upcoming, dedup_window_secs))
                        .unwrap_or(false);

                    if unchanged && safe_to_skip {
                        if band.any_active {
                            active_skip_count += 1;
                        }
                    } else {
                        if let Some(t) = last_real_send_at {
                            max_gap = max_gap.max(now - t);
                        }
                        last_real_send_at = Some(now);
                    }

                    // Advance time by the ACTUAL (freshly, adversarially
                    // jittered every tick) cadence the real loop sleeps for.
                    now += jittered_cadence(base, ttl_ceiling);
                }
            }

            let ttl = StdDuration::from_secs(band.ttl_secs);
            assert!(
                max_gap < ttl,
                "{}: max gap between real sends {max_gap:?} reached the TTL {ttl:?}",
                band.name
            );
            // Tighter, provable bound: every real-send gap is either the
            // TTL-ceiling-bounded pacing cadence itself (when dedup can't
            // safely skip at all, e.g. deep idle) or a skip-induced gap that
            // `dedup_skip_is_safe` keeps strictly under `ttl - MARGIN` — both
            // are bounded by the headroomed TTL ceiling.
            assert!(
                max_gap <= ttl_ceiling,
                "{}: max gap {max_gap:?} exceeded the TTL ceiling {ttl_ceiling:?}",
                band.name
            );

            if band.any_active {
                assert!(
                    active_skip_count > 0,
                    "active band got zero dedup skips across {TRIALS} trials x {TICKS} ticks \
                     — the expiry-aware rule should still preserve dedup where it's provably safe"
                );
            }
        }
    }

    /// WP-A4: `quantize_15s` rounds DOWN to the 15-second bucket.
    #[test]
    fn quantize_15s_rounds_down_to_the_bucket() {
        assert_eq!(quantize_15s(0), 0);
        assert_eq!(quantize_15s(14), 0);
        assert_eq!(quantize_15s(15), 15);
        assert_eq!(quantize_15s(29), 15);
        assert_eq!(quantize_15s(30), 30);
        assert_eq!(quantize_15s(104), 90);
        assert_eq!(quantize_15s(119), 105);
    }

    /// A `LiveSession` snapshot for an agent session that's still open with no new
    /// *human* signal (`last_signal_at` pinned) but ticking `active_seconds` up
    /// via ordinary agent activity — the case WP-A4 targets.
    fn quiet_active_session(active_seconds: u64, last_event_at: OffsetDateTime) -> LiveSession {
        LiveSession {
            session_id: "s1".to_string(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            project: Some("github.com/acme/api".to_string()),
            identity_email: None,
            label: None,
            activity: None,
            note: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            last_event_at,
            last_signal_at: Some(OffsetDateTime::UNIX_EPOCH),
            ended: false,
            engaged_seconds: 42, // unchanged across ticks: no new human signal
            active_seconds,
            last_human_signal_at: None,
            last_active_at: None,
            last_partial_active_seconds: None,
        }
    }

    /// WP-A4: two heartbeat ticks of an active-but-human-quiet session, with
    /// `active_seconds` advancing but staying inside the same 15s bucket, must
    /// serialize to byte-identical `PresenceSession` JSON — the whole point of
    /// quantizing is to make dedup actually fire during an open session instead
    /// of the raw per-tick counter always defeating it.
    #[test]
    fn quantized_presence_payload_is_json_stable_across_ticks_within_a_bucket() {
        let idle = Duration::seconds(300);
        let now = OffsetDateTime::UNIX_EPOCH + Duration::seconds(1000);

        // 100s and 104s both quantize to the 90s bucket.
        let tick1 = quiet_active_session(100, OffsetDateTime::UNIX_EPOCH + Duration::seconds(900));
        let tick2 = quiet_active_session(104, OffsetDateTime::UNIX_EPOCH + Duration::seconds(904));

        let p1 = to_presence_session(&tick1, now, idle);
        let p2 = to_presence_session(&tick2, now, idle);

        assert_eq!(p1.agent_wall_seconds, 90);
        assert_eq!(p2.agent_wall_seconds, 90);
        assert_eq!(
            p1.engaged_seconds, 30,
            "engaged_seconds (42) quantizes down too"
        );
        assert_eq!(
            p1.idle, p2.idle,
            "idle bool must not flap between quiet ticks"
        );
        assert_eq!(
            serde_json::to_string(&p1).unwrap(),
            serde_json::to_string(&p2).unwrap(),
            "same-bucket ticks must be byte-identical so the heartbeat's dedup fires"
        );

        // Crossing into the next bucket (105s) breaks the tie, as expected — the
        // snapshot legitimately changed.
        let tick3 = quiet_active_session(105, OffsetDateTime::UNIX_EPOCH + Duration::seconds(905));
        let p3 = to_presence_session(&tick3, now, idle);
        assert_eq!(p3.agent_wall_seconds, 105);
        assert_ne!(
            serde_json::to_string(&p1).unwrap(),
            serde_json::to_string(&p3).unwrap(),
            "crossing a bucket boundary must change the serialized snapshot"
        );
    }
}
