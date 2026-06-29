//! Off-hot-path cloud sync.
//!
//! A single background task owns all sync work. The capture hot path only ever
//! does a non-blocking `try_send(())` on the trigger channel (see `main::writer`);
//! everything heavy — building the batch, signing, and the HTTP round-trip —
//! happens here, never holding the sessions lock across an `await`.
//!
//! The task wakes on two sources:
//! - **triggers**, coalesced through a short debounce so a burst of lifecycle
//!   events (SessionStart → tool calls → SessionEnd) collapses into one flush;
//! - a **periodic backstop** so a dropped trigger (the channel is lossy by
//!   design) or a transient network failure still drains eventually.
//!
//! ### Cursor & idempotency
//! The sync cursor (`meta` key [`META_SYNC_CURSOR`]) is the last event id we have
//! confirmed the cloud accepted. A flush snapshots `until = max_event_id()`,
//! builds the window `(cursor, until]`, and advances the cursor to `until` **only
//! after** a 2xx (accepted *or* duplicate). The batch id is deterministic over
//! the window, so a crash-retry re-sends the identical batch and the cloud
//! no-ops on it. Nothing is ever lost and nothing is double-counted.

use crate::state::AppState;
use dira_contract::{Envelope, IngestAck, IngestError, ServerMeta, SCHEMA_VERSION};
use dira_core::identity;
use dira_core::signing::DeviceKey;
use std::time::Duration as StdDuration;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};

/// `meta` key: the last event id confirmed-synced to the cloud. Defined in
/// `dira_core::sync` so the store (`nuke`) and CLI share one definition; re-
/// exported here for the daemon's existing references.
pub use dira_core::sync::{META_ARTIFACTS_CURSOR, META_SYNC_CURSOR};

/// Debounce window: coalesce a burst of triggers into one flush.
const DEBOUNCE: StdDuration = StdDuration::from_secs(3);
/// Backstop cadence: flush even with no triggers (and retry after failures).
const BACKSTOP: StdDuration = StdDuration::from_secs(90);
/// Cap for exponential backoff after a network/5xx failure.
const MAX_BACKOFF: StdDuration = StdDuration::from_secs(300);

/// Handle to the sync task. Cloneable; cloning shares the same trigger channel.
#[derive(Clone)]
pub struct SyncHandle {
    /// Non-blocking trigger. The hot path `try_send(())`s here after a durable
    /// append; a full channel is fine (the backstop covers a missed nudge).
    pub trigger: mpsc::Sender<()>,
}

/// Create the trigger channel + handle *before* `AppState` exists (the handle is
/// a field of `AppState`). The returned receiver is handed to [`spawn`] once the
/// state is assembled.
pub fn channel() -> (SyncHandle, mpsc::Receiver<()>) {
    // Depth 1 is enough: many triggers coalesce into one flush, so we only need
    // to know "something happened since the last flush".
    let (trigger, rx) = mpsc::channel::<()>(1);
    (SyncHandle { trigger }, rx)
}

/// Spawn the background sync task with the assembled state + the trigger receiver.
/// The signing key is read from `state.device_key`.
pub fn spawn(state: AppState, rx: mpsc::Receiver<()>) {
    tokio::spawn(run(state, rx));
}

/// The task loop: select over coalesced triggers and the periodic backstop.
///
/// The device signing key is fetched lazily per flush via [`AppState::device_key`]
/// (loaded on first use, then cached) rather than at task start, so this task can
/// spin up before the key — which may block on a keychain prompt — is ready, and
/// the control socket stays responsive throughout startup.
async fn run(state: AppState, mut rx: mpsc::Receiver<()>) {
    let client = match reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("sync: failed to build http client, sync disabled: {e}");
            return;
        }
    };

    let mut backstop = interval(BACKSTOP);
    backstop.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Skip the immediate first tick so startup doesn't double-flush with hydrate.
    backstop.tick().await;

    let mut backoff = StdDuration::ZERO;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                if recv.is_none() {
                    // Sender dropped (daemon shutting down).
                    break;
                }
                // Debounce: let a burst of triggers settle into one flush.
                sleep(DEBOUNCE).await;
                // Drain any triggers that arrived during the debounce.
                while rx.try_recv().is_ok() {}
            }
            _ = backstop.tick() => {}
        }

        // Fetch the signing key lazily (loaded-on-first-use, then cached). If it
        // can't load yet there is nothing to sign, so skip this flush; the next
        // trigger/backstop retries once the key is available.
        let Some(device_key) = state.device_key().await else {
            continue;
        };
        match flush(&state, device_key, &client).await {
            Ok(FlushOutcome::Synced) | Ok(FlushOutcome::Nothing) => {
                backoff = StdDuration::ZERO;
            }
            Ok(FlushOutcome::Skipped) => {
                // Not configured / not linked — nothing to back off on.
                backoff = StdDuration::ZERO;
            }
            Err(SyncError::Transient(e)) => {
                backoff = next_backoff(backoff);
                tracing::warn!("sync: transient failure, backing off {backoff:?}: {e}");
                sleep(backoff).await;
            }
            Err(SyncError::ReLinkRequired) => {
                // Cursor intentionally not advanced; a re-link clears this.
                backoff = StdDuration::ZERO;
                tracing::error!(
                    "sync: cloud rejected device (unknown_device) — re-link required \
                     (`dira device link`); pausing sync until then"
                );
            }
            Err(SyncError::Fatal(e)) => {
                backoff = next_backoff(backoff);
                tracing::warn!("sync: error, backing off {backoff:?}: {e}");
                sleep(backoff).await;
            }
        }
    }
}

fn next_backoff(current: StdDuration) -> StdDuration {
    let next = if current.is_zero() {
        StdDuration::from_secs(2)
    } else {
        current * 2
    };
    next.min(MAX_BACKOFF)
}

/// What a flush did, for backoff bookkeeping.
enum FlushOutcome {
    /// A batch was accepted (or was a duplicate) and the cursor advanced.
    Synced,
    /// The window was empty; nothing to do.
    Nothing,
    /// Sync is not configured (no cloud_url) or the device is not linked.
    Skipped,
}

/// Errors that distinguish "retry later" from "stop until a human acts".
enum SyncError {
    /// Network / 5xx — leave the cursor, back off, the backstop will retry.
    Transient(String),
    /// 401 unknown_device — the device isn't linked cloud-side; needs a re-link.
    ReLinkRequired,
    /// Local error (DB, signing) — log and back off; not expected in steady state.
    Fatal(String),
}

/// One sync attempt. Re-reads config/linkage each call so `dira device link`
/// takes effect without a daemon restart. Holds no lock across the HTTP await.
async fn flush(
    state: &AppState,
    device_key: &DeviceKey,
    client: &reqwest::Client,
) -> Result<FlushOutcome, SyncError> {
    // 1. Gate on configuration + linkage, both re-read every run.
    let Some(cloud_url) = state.config.cloud_url.clone() else {
        return Ok(FlushOutcome::Skipped);
    };
    let device_id = match identity::device_id(&state.store).await {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(FlushOutcome::Skipped),
        Err(e) => return Err(SyncError::Fatal(format!("read device_id: {e}"))),
    };

    // 2. Snapshot two independent windows — the event log and the captured-commit
    //    backlog — and flush when *either* has something new. Each rides its own
    //    cursor; `until`/`art_until` are the snapshot upper bounds we advance to.
    let cursor = state
        .store
        .meta_get(META_SYNC_CURSOR)
        .await
        .map_err(|e| SyncError::Fatal(format!("read cursor: {e}")))?
        .filter(|s| !s.is_empty());

    let until = state
        .store
        .max_event_id()
        .await
        .map_err(|e| SyncError::Fatal(format!("read max id: {e}")))?;

    // Events in `(cursor, until]` — empty when the log is empty or already caught up.
    let events = match &until {
        Some(u) if cursor.as_deref() != Some(u.as_str()) => state
            .store
            .events_between(cursor.as_deref(), u)
            .await
            .map_err(|e| SyncError::Fatal(format!("load events: {e}")))?,
        _ => Vec::new(),
    };

    // Artifacts in `(art_cursor, art_until]` — they aren't event-id ordered, so
    // they ship on their own rowid cursor. The cloud dedups artifacts by sha.
    let art_cursor = state
        .store
        .meta_get(META_ARTIFACTS_CURSOR)
        .await
        .map_err(|e| SyncError::Fatal(format!("read artifacts cursor: {e}")))?
        .and_then(|s| s.parse::<i64>().ok());
    let art_until = state
        .store
        .max_artifact_rowid()
        .await
        .map_err(|e| SyncError::Fatal(format!("read max artifact rowid: {e}")))?;
    let artifact_rows = match art_until {
        Some(u) if art_cursor != Some(u) => state
            .store
            .unsynced_artifacts(art_cursor, u)
            .await
            .map_err(|e| SyncError::Fatal(format!("load artifacts: {e}")))?,
        _ => Vec::new(),
    };

    if events.is_empty() && artifact_rows.is_empty() {
        return Ok(FlushOutcome::Nothing);
    }

    // Token rows: bound by the at-range of this event window (empty when there are
    // no new events). The cloud dedups token_usage by id, so over-inclusion is fine.
    let token_rows = if events.is_empty() {
        Vec::new()
    } else {
        let since_at = events.first().map(fmt_at);
        let until_at = events.last().map(fmt_at).unwrap_or_default();
        state
            .store
            .token_usage_between(since_at.as_deref(), &until_at)
            .await
            .map_err(|e| SyncError::Fatal(format!("load token rows: {e}")))?
    };

    // 3. Build → sign → wrap. Never post-process the JCS payload after signing.
    //
    // Phase 6c: also gather partial rollups for long-running un-ended sessions
    // from the live registry, so a multi-day session contributes settled-ish wall
    // time before its SessionEnd. Read-only here; the watermark is advanced only
    // after the cloud accepts the batch (below) so a failed flush re-offers them.
    let now = time::OffsetDateTime::now_utc();
    let partials = partial_rollups(state, now);
    let partial_ids: Vec<String> = partials.iter().map(|p| p.session_id.clone()).collect();
    let batch = dira_core::sync::build_batch_with_partials(
        &events,
        &token_rows,
        &artifact_rows,
        &partials,
        &device_id,
        state.config.idle(),
        now,
    );
    let sig = device_key
        .sign_payload(&batch)
        .map_err(|e| SyncError::Fatal(format!("sign: {e}")))?;
    let envelope = Envelope {
        schema_version: SCHEMA_VERSION.to_string(),
        device_id: device_id.clone(),
        payload: batch,
        sig,
    };

    // 4. POST. Advance the cursor only on a 2xx ack.
    let url = format!("{}/api/v1/ingest", cloud_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&envelope)
        .send()
        .await
        .map_err(|e| SyncError::Transient(format!("post ingest: {e}")))?;

    let status = resp.status();
    if status.is_success() {
        // Advance both cursors only after the ack. Each is independent: an
        // artifact-only flush still advances the event cursor to the current head
        // (a no-op when unchanged) and vice-versa.
        if let Some(u) = &until {
            state
                .store
                .meta_set(META_SYNC_CURSOR, u)
                .await
                .map_err(|e| SyncError::Fatal(format!("advance cursor: {e}")))?;
        }
        if let Some(u) = art_until {
            state
                .store
                .meta_set(META_ARTIFACTS_CURSOR, &u.to_string())
                .await
                .map_err(|e| SyncError::Fatal(format!("advance artifacts cursor: {e}")))?;
        }
        // Advance the partial-rollup watermarks now that the cloud accepted the
        // batch, so an idle long session doesn't re-ship an identical partial every
        // flush (it only re-ships once its active_seconds grows again). Best-effort:
        // a poisoned lock just means the next flush re-offers the same partials.
        if !partial_ids.is_empty() {
            if let Ok(mut reg) = state.sessions.lock() {
                reg.mark_partials_sent(&partial_ids);
            }
        }
        // Parse the typed ack so we can log what the cloud actually did. Tolerant:
        // an older cloud that returns an empty/absent body still yields a default
        // ack (all zeros), so logging degrades gracefully rather than erroring.
        let body = resp.text().await.unwrap_or_default();
        let ack = parse_ingest_ack(&body);
        tracing::info!(
            events = events.len(),
            artifacts = artifact_rows.len(),
            partial_rollups = partial_ids.len(),
            accepted = ack.accepted,
            duplicates = ack.duplicates,
            cursor = until.as_deref().unwrap_or(""),
            "sync: flushed batch to cloud"
        );
        return Ok(FlushOutcome::Synced);
    }

    if status.as_u16() == 401 {
        // Distinguish unknown_device (needs re-link) from a bad signature using a
        // typed error body rather than a brittle substring match.
        let body = resp.text().await.unwrap_or_default();
        if is_unknown_device(&body) {
            return Err(SyncError::ReLinkRequired);
        }
        return Err(SyncError::Fatal(format!("401 from ingest: {body}")));
    }

    if status.is_server_error() {
        return Err(SyncError::Transient(format!("ingest {status}")));
    }

    // 4xx other than 401: a client-side contract problem. A typed `unknown_device`
    // can also arrive here (some gateways map auth failures to 403), so check it
    // before treating as fatal. Otherwise log and back off — the same batch would
    // just fail again.
    let body = resp.text().await.unwrap_or_default();
    if is_unknown_device(&body) {
        return Err(SyncError::ReLinkRequired);
    }
    Err(SyncError::Fatal(format!("ingest {status}: {body}")))
}

/// Gather partial-rollup descriptors for long-running un-ended sessions from the
/// live registry (Phase 6c). Holds the registry lock only to snapshot — no await
/// inside. Returns empty when partial rollups are disabled (`partial_rollup_after
/// = 0`) or the lock is poisoned.
fn partial_rollups(
    state: &AppState,
    now: time::OffsetDateTime,
) -> Vec<dira_core::sync::PartialSession> {
    let older_than = state.config.partial_rollup_after();
    if older_than <= time::Duration::ZERO {
        return Vec::new();
    }
    let candidates = match state.sessions.lock() {
        Ok(reg) => reg.partial_rollup_candidates(now, older_than),
        Err(_) => return Vec::new(),
    };
    candidates
        .into_iter()
        .map(|s| dira_core::sync::PartialSession {
            session_id: s.session_id,
            harness: s.harness,
            kind: s.kind,
            repo_canonical: s.project,
            identity_email: s.identity_email,
            started_at: s.started_at,
            active_seconds: s.active_seconds,
            // The live registry does not count prompts or track a branch per
            // session, so a partial omits both (the eventual ended rollup, built
            // from the event window, carries them). Omitted-when-None on the wire.
            prompts: None,
            branch: None,
        })
        .collect()
}

/// RFC 3339 timestamp of an event (the storage format for `token_usage.at`).
fn fmt_at(e: &dira_core::model::RawEvent) -> String {
    e.at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Parse a 2xx ingest response body into a typed [`IngestAck`]. Tolerant by
/// design: an empty body or a body the current schema can't fully read both
/// degrade to a default ack (all zeros), so a successful flush is never
/// downgraded to a failure over a logging detail.
fn parse_ingest_ack(body: &str) -> IngestAck {
    if body.trim().is_empty() {
        return IngestAck::default();
    }
    serde_json::from_str(body).unwrap_or_default()
}

/// Whether a non-2xx ingest error body is the typed `unknown_device` signal that
/// the device needs a re-link. Parses the typed [`IngestError`]; an unparseable
/// body is treated as *not* unknown_device (some other client/server fault).
fn is_unknown_device(body: &str) -> bool {
    serde_json::from_str::<IngestError>(body)
        .map(|e| e.error == "unknown_device")
        .unwrap_or(false)
}

/// Best-effort schema-version handshake (Phase 3d).
///
/// Fetches the cloud's supported contract range from `GET /api/v1/meta` and logs
/// a clear warning if our [`SCHEMA_VERSION`] falls outside it. Entirely
/// non-fatal: a missing `cloud_url`, an unreachable cloud, an old cloud without
/// the endpoint, or an unparseable body all simply skip the check. Run once at
/// daemon startup; sync/heartbeat are unaffected by its outcome.
pub async fn check_schema_handshake(cloud_url: Option<&str>) {
    let Some(cloud_url) = cloud_url else {
        return; // sync disabled — nothing to handshake with
    };
    let client = match reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("handshake: failed to build http client: {e}");
            return;
        }
    };
    let url = format!("{}/api/v1/meta", cloud_url.trim_end_matches('/'));
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            // Old cloud / offline — not an error, the handshake is advisory.
            tracing::debug!("handshake: GET meta failed (skipping): {e}");
            return;
        }
    };
    if !resp.status().is_success() {
        tracing::debug!("handshake: GET meta returned {} (skipping)", resp.status());
        return;
    }
    let meta: ServerMeta = match resp.json().await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("handshake: meta body unparseable (skipping): {e}");
            return;
        }
    };
    if !schema_version_in_range(SCHEMA_VERSION, &meta) {
        tracing::warn!(
            our_version = SCHEMA_VERSION,
            cloud_min = %meta.min_schema_version,
            cloud_max = %meta.schema_version,
            "handshake: our contract version is outside the cloud's supported range — \
             some fields may be rejected or ignored; consider upgrading"
        );
    } else {
        tracing::info!(
            our_version = SCHEMA_VERSION,
            cloud_min = %meta.min_schema_version,
            cloud_max = %meta.schema_version,
            "handshake: contract version compatible with cloud"
        );
    }
}

/// Whether `ours` is within `[meta.min_schema_version, meta.schema_version]` by
/// `major.minor.patch` ordering. Empty/unparseable bounds are treated as "no
/// constraint on that side" so a partially-populated meta never produces a false
/// warning. We compare only the numeric core (no pre-release/build metadata) —
/// enough for a coarse advisory handshake without pulling in a semver crate.
fn schema_version_in_range(ours: &str, meta: &ServerMeta) -> bool {
    let Some(ours) = parse_version(ours) else {
        return true; // can't reason about it — don't warn
    };
    let min_ok = parse_version(&meta.min_schema_version).map_or(true, |min| ours >= min);
    let max_ok = parse_version(&meta.schema_version).map_or(true, |max| ours <= max);
    min_ok && max_ok
}

/// Parse a `major.minor.patch` version into a comparable tuple. Tolerates a
/// trailing `-prerelease`/`+build` suffix by reading only the numeric core, and
/// missing minor/patch (defaults 0). Returns `None` if the major isn't numeric.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().split(['-', '+']).next().unwrap_or("").trim();
    if core.is_empty() {
        return None;
    }
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_ack_parses_counts() {
        let ack = parse_ingest_ack(
            r#"{"serverTime":"2026-06-29T10:00:00Z","accepted":7,"duplicates":2,"schemaVersion":"1.0.0"}"#,
        );
        assert_eq!(ack.accepted, 7);
        assert_eq!(ack.duplicates, 2);
    }

    #[test]
    fn ingest_ack_tolerates_empty_and_garbage_bodies() {
        // Empty body (older cloud, 200 with no JSON) → default ack, not a panic.
        assert_eq!(parse_ingest_ack("").accepted, 0);
        assert_eq!(parse_ingest_ack("   ").accepted, 0);
        // Non-JSON body → default ack rather than an error.
        assert_eq!(parse_ingest_ack("ok").accepted, 0);
        // Partial body → fields present are read, absent ones default.
        let ack = parse_ingest_ack(r#"{"accepted":3}"#);
        assert_eq!(ack.accepted, 3);
        assert_eq!(ack.duplicates, 0);
    }

    #[test]
    fn unknown_device_is_detected_via_typed_body() {
        assert!(is_unknown_device(r#"{"error":"unknown_device"}"#));
        // A different error code is not a re-link trigger.
        assert!(!is_unknown_device(r#"{"error":"bad_signature"}"#));
        // A non-JSON body (e.g. a proxy's plain-text 401) is not treated as
        // unknown_device — we only re-link on the explicit typed signal.
        assert!(!is_unknown_device("unknown_device"));
        assert!(!is_unknown_device(""));
    }

    fn meta(min: &str, max: &str) -> ServerMeta {
        ServerMeta {
            schema_version: max.into(),
            min_schema_version: min.into(),
        }
    }

    #[test]
    fn schema_version_inside_range_is_compatible() {
        assert!(schema_version_in_range("1.0.0", &meta("1.0.0", "1.2.0")));
        assert!(schema_version_in_range("1.1.0", &meta("1.0.0", "1.2.0")));
        // Inclusive bounds.
        assert!(schema_version_in_range("1.2.0", &meta("1.0.0", "1.2.0")));
    }

    #[test]
    fn schema_version_outside_range_is_incompatible() {
        // Below min.
        assert!(!schema_version_in_range("1.0.0", &meta("1.1.0", "1.2.0")));
        // Above max (a newer device than the cloud knows).
        assert!(!schema_version_in_range("2.0.0", &meta("1.0.0", "1.2.0")));
    }

    #[test]
    fn partial_or_unparseable_meta_never_warns() {
        // Empty bounds = no constraint on that side.
        assert!(schema_version_in_range("1.0.0", &meta("", "")));
        assert!(schema_version_in_range("9.9.9", &meta("1.0.0", "")));
        assert!(schema_version_in_range("0.0.1", &meta("", "1.0.0")));
        // Unparseable ours = don't warn.
        assert!(schema_version_in_range(
            "not-a-version",
            &meta("1.0.0", "1.2.0")
        ));
    }

    #[test]
    fn version_parsing_tolerates_suffixes_and_short_forms() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.0.0-develop.4"), Some((1, 0, 0)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1"), Some((1, 0, 0)));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("abc"), None);
    }
}
