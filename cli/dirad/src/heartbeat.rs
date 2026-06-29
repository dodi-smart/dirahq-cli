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

/// What a single beat decided to do, so the loop can pace the next tick and the
/// observability stays honest about sends vs dedup-skips.
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
    let client = match reqwest::Client::builder()
        .timeout(SHUTDOWN_BEAT_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
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
    let _ = client.post(&url).json(&envelope).send().await;
    tracing::debug!("heartbeat: sent final offline presence beat");
}

/// The heartbeat loop: build one client, then POST a presence snapshot each tick
/// at an **adaptive cadence** (Phase 6a). The loop carries the dedup state — the
/// last sent `sessions` JSON and when it was sent — across ticks so `beat` can
/// skip a redundant POST while still guaranteeing one keepalive per TTL.
async fn run(state: AppState) {
    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("heartbeat: failed to build http client, presence disabled: {e}");
            return;
        }
    };

    // Dedup state: the JSON of the last `sessions` we actually sent, and the
    // instant of that send. `None` forces the first beat to send.
    let mut last_sent_sessions: Option<String> = None;
    let mut last_sent_at: Option<Instant> = None;
    // Whether any session was active (recent activity) on the last beat, used to
    // pick the next cadence even when we skipped the POST.
    let mut last_any_active = false;

    loop {
        let result = beat(
            &state,
            &client,
            &mut last_sent_sessions,
            &mut last_sent_at,
            &mut last_any_active,
        )
        .await;

        // Pace the next tick adaptively. Active sessions ⇒ fast cadence; all idle
        // (or none) ⇒ slow cadence. A cloud `next_beat_hint_secs` overrides both.
        // Either way we never let the gap reach the TTL: a beat must land before
        // the cloud would expire the device.
        let next = next_cadence(&state, last_any_active, matches!(result, BeatResult::Noop));
        tokio::time::sleep(next).await;
    }
}

/// Decide how long to wait before the next beat.
///
/// Priority:
/// 1. A live cloud hint (`PresenceAck.next_beat_hint_secs`, stashed in
///    `presence_hints`) — the cloud knows its own load/expiry best.
/// 2. Otherwise the configured adaptive band: `heartbeat_active` when any session
///    is active, `heartbeat_idle` when all are idle / none.
///
/// The result is always capped strictly below the effective TTL (config or the
/// cloud-advertised `ttl_secs`, whichever is known) minus a small safety margin,
/// so the keepalive renews presence before it expires. When we couldn't beat at
/// all (unconfigured/unlinked), fall back to the idle cadence — there's nothing to
/// keep alive yet, so there's no point spinning fast.
fn next_cadence(state: &AppState, any_active: bool, was_noop: bool) -> StdDuration {
    let idle_cadence = state.config.heartbeat_idle();
    let active_cadence = state.config.heartbeat_active();

    // Effective TTL: prefer the cloud's advertised value when it has answered,
    // else our configured TTL. Used only to bound the cadence ceiling.
    let cloud_ttl = state.presence_hints.ttl_secs.load(Ordering::Relaxed);
    let ttl_secs = if cloud_ttl > 0 {
        cloud_ttl
    } else {
        state.config.presence_ttl_secs
    };
    // Leave headroom so a beat lands before expiry: ttl - 5s (or ttl/2 for tiny TTLs).
    let ttl_ceiling = Duration::seconds(ttl_secs.saturating_sub(5).max(ttl_secs / 2).max(1) as i64);

    let base = if was_noop {
        idle_cadence
    } else if let Some(hint) = nonzero(
        state
            .presence_hints
            .next_beat_hint_secs
            .load(Ordering::Relaxed),
    ) {
        Duration::seconds(hint as i64)
    } else if any_active {
        active_cadence
    } else {
        idle_cadence
    };

    let capped = base.min(ttl_ceiling).max(Duration::seconds(1));
    StdDuration::from_secs(capped.whole_seconds().max(1) as u64)
}

/// `0` is the sentinel for "no hint" in the presence atomics.
fn nonzero(v: u64) -> Option<u64> {
    (v > 0).then_some(v)
}

/// One heartbeat attempt. Re-reads config + linkage each call so `dira device
/// link` / a config change takes effect without a daemon restart. Holds no lock
/// across the HTTP await.
///
/// Reads the per-session `engaged_seconds` / `active_seconds` straight off the
/// live registry (Phase 6b) — the writer maintains them incrementally, so there
/// is no per-tick SQLite scan any more.
///
/// **Dedup (Phase 6a):** the `sessions` snapshot is serialized and compared to
/// `last_sent_sessions`. If it is byte-identical AND we are still within the TTL
/// of the last successful send, the POST is skipped entirely. We always send when
/// the snapshot changes, and at least once per TTL (the keepalive) so a quiet
/// device never silently expires.
async fn beat(
    state: &AppState,
    client: &reqwest::Client,
    last_sent_sessions: &mut Option<String>,
    last_sent_at: &mut Option<Instant>,
    last_any_active: &mut bool,
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

    // Snapshot the live registry (with its rolling counters), then release the
    // lock before any await. `engaged_seconds` / `agent_wall_seconds` come
    // straight off `LiveSession` — no SQLite scan.
    let active = match state.sessions.lock() {
        Ok(reg) => reg.active(),
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

    // Dedup key: the JSON of the `sessions` list. `sent_at` and the signature are
    // deliberately excluded — they change every tick and would defeat dedup. A
    // serialize failure simply forces a send (treated as "changed").
    let sessions_json = serde_json::to_string(&sessions).ok();
    let within_ttl = last_sent_at
        .map(|t| t.elapsed() < StdDuration::from_secs(state.config.presence_ttl_secs.max(1)))
        .unwrap_or(false);
    let unchanged = match (sessions_json.as_ref(), last_sent_sessions.as_ref()) {
        (Some(cur), Some(prev)) => cur == prev,
        _ => false,
    };
    if unchanged && within_ttl {
        tracing::debug!(
            sessions = sessions.len(),
            "heartbeat: snapshot unchanged within TTL — skipping POST (dedup)"
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
        // here from config.
        presence_ttl_secs: Some(state.config.presence_ttl_secs),
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
    match client.post(&url).json(&envelope).send().await {
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
                    "heartbeat: sent presence snapshot"
                );
                BeatResult::Sent
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
        engaged_seconds: s.engaged_seconds,
        agent_wall_seconds: s.active_seconds,
        // Activity-based idle for the live view: a session is "idle" when there has
        // been NO activity (any event — agent tool calls included) within the idle
        // window. We deliberately do NOT use `is_idle` (last *human* signal): an agent
        // churning away with no recent human prompt is active in "Right now", and the
        // cloud hides idle sessions.
        idle: (now - s.last_event_at) > idle,
    }
}

/// RFC 3339 timestamp, matching the formatting used in `sync.rs` / `batch.rs`.
fn fmt_rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}
