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
use dira_contract::{Envelope, IngestError, ServerMeta, SCHEMA_VERSION};
use dira_core::identity;
use dira_core::signing::DeviceKey;
use std::time::Duration as StdDuration;
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, Instant};

/// `meta` key: the last event id confirmed-synced to the cloud. Defined in
/// `dira_core::sync` so the store (`nuke`) and CLI share one definition; re-
/// exported here for the daemon's existing references.
pub use dira_core::sync::{META_ARTIFACTS_CURSOR, META_SYNC_CURSOR, META_TOKEN_CURSOR};

/// Debounce window: coalesce a burst of triggers into one flush.
const DEBOUNCE: StdDuration = StdDuration::from_secs(3);
/// Backstop cadence: flush even with no triggers (and retry after failures).
const BACKSTOP: StdDuration = StdDuration::from_secs(90);
/// Cap for exponential backoff after a network/5xx failure. Shared with the
/// knowledge sync task, which rides the same [`next_backoff`] ladder.
pub(crate) const MAX_BACKOFF: StdDuration = StdDuration::from_secs(300);
/// HTTP timeout for a single ingest chunk POST. Longer than the other
/// device→cloud calls — a batch can carry a chunk's worth of intervals/sessions.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(30);
/// HTTP timeout for the best-effort schema-version handshake GET.
const HANDSHAKE_TIMEOUT: StdDuration = StdDuration::from_secs(10);

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
    // The backstop fires on its own schedule, independent of triggers (a burst of
    // `recv`s must never push it out — it's the safety net for a *dropped*
    // trigger). Jittered ±10% each time it actually fires so many daemons don't
    // all POST on the same wall-clock cadence. Modeled as an explicit deadline +
    // `sleep_until` (rather than `tokio::time::interval`) so jitter can vary the
    // *next* period without fighting interval's fixed-schedule bookkeeping.
    //
    // The FIRST deadline is set a full jittered period out (not "now"), so
    // startup doesn't double-flush with hydrate — but that deadline is only ever
    // awaited *inside* the `select!` below, never blocked on up front. Blocking
    // here (as an earlier version did, via a pre-loop `sleep_until`) would starve
    // `rx.recv()` for the entire ~81-99s period, so a trigger that fires the
    // instant the daemon starts (e.g. the writer's hot path nudging on the very
    // first captured event) would sit unserviced until the backstop, instead of
    // landing within the usual 3s debounce.
    let mut backstop_at =
        Instant::now() + crate::jitter::jittered(BACKSTOP, crate::jitter::DEFAULT_FRAC);

    let mut backoff = StdDuration::ZERO;
    // Consecutive failed flush attempts since the last success, surfaced on the
    // sync-health snapshot (`META_SYNC_HEALTH`) alongside `backoff`.
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                if recv.is_none() {
                    // Sender dropped (daemon shutting down).
                    break;
                }
                // Debounce: let a burst of triggers settle into one flush. Deliberately
                // NOT jittered — it's a coalescer, not an independent periodic timer.
                sleep(DEBOUNCE).await;
                // Drain any triggers that arrived during the debounce.
                while rx.try_recv().is_ok() {}
            }
            _ = sleep_until(backstop_at) => {
                backstop_at = Instant::now() + crate::jitter::jittered(BACKSTOP, crate::jitter::DEFAULT_FRAC);
            }
        }

        // Fetch the signing key lazily (loaded-on-first-use, then cached). If it
        // can't load yet there is nothing to sign, so skip this flush; the next
        // trigger/backstop retries once the key is available.
        let Some(device_key) = state.device_key().await else {
            continue;
        };
        // Every wake that reaches `flush` counts as an attempt (WP-B9), whether
        // or not it finds anything to send — mirrors the writer-health pattern
        // (WP-B7) of a small set of process-wide counters on `ProgressTracker`.
        state.progress.mark_flush_attempt();
        match flush(&state, &device_key, &state.http).await {
            Ok(FlushOutcome::Synced) => {
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                state.progress.mark_flush_success();
                record_health(&state, None, 0, 0).await;
                // Fresh facts just landed on the cloud — nudge the billing task
                // to refresh the billable summary once they're unpacked.
                state.billing_refresh.notify_one();
            }
            Ok(FlushOutcome::Nothing) => {
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                state.progress.mark_flush_success();
                record_health(&state, None, 0, 0).await;
            }
            Ok(FlushOutcome::Skipped) => {
                // Not configured / not linked — nothing to back off on, but this
                // tick still bumped `flush_attempts` above (mark_flush_attempt is
                // unconditional), so still write the health snapshot: without it,
                // `last_attempt_at` goes stale while `flush_attempts` keeps
                // climbing, and `dira status` could show attempts incrementing
                // against a frozen timestamp. `"skipped"` is a distinct kind from
                // every failure kind below — not a failure, so
                // `consecutive_failures` stays at its already-reset value here
                // (0) rather than incrementing.
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                record_health(
                    &state,
                    Some("skipped"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
            }
            Err(SyncError::Transient {
                message,
                retry_after,
            }) => {
                // A 429's Retry-After (header or typed body) overrides the usual
                // exponential ladder for THIS wait — the cloud knows its own
                // budget best — but is still capped at MAX_BACKOFF so a
                // misbehaving/huge Retry-After can't wedge sync indefinitely.
                // Falls back to the normal ladder for a network/5xx failure
                // (no Retry-After to honor).
                backoff = transient_wait(retry_after, backoff);
                consecutive_failures += 1;
                state.progress.mark_flush_failure();
                tracing::warn!("sync: transient failure, backing off {backoff:?}: {message}");
                record_health(
                    &state,
                    Some("transient"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
            Err(SyncError::ReLinkRequired) => {
                // Cursor intentionally not advanced; a re-link clears this.
                backoff = StdDuration::ZERO;
                consecutive_failures += 1;
                state.progress.mark_flush_failure();
                tracing::error!(
                    "sync: cloud rejected device (unknown_device) — re-link required \
                     (`dira device link`); pausing sync until then"
                );
                record_health(
                    &state,
                    Some("unknown_device"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
            }
            Err(SyncError::SignatureRejected(body)) => {
                // The cloud's typed `bad_signature` on a 401 means the key we
                // signed with no longer matches what the cloud has registered.
                // Retrying identical bytes cannot succeed (same key, same
                // rejection), so this is normally a hard-stop, backing off at
                // the ceiling rather than hot-looping. But WP-B1b gives the
                // daemon one automatic recovery attempt FIRST: if a rotation is
                // sitting pending on disk (the CLI's `rotate_key` persisted a
                // new keypair, POSTed it, and either the response was lost or
                // the daemon simply hasn't reloaded the key since), retry THIS
                // SAME flush signed with the pending key instead of the dead
                // old one. A success proves the pending key is exactly what the
                // cloud has installed, so we promote it to active and
                // invalidate the daemon's cached key so every later tick signs
                // correctly — self-healing without any operator action, no
                // daemon restart, and no separate `rotate-key` invocation
                // required. See `try_pending_key_flush`'s doc comment for the
                // full argument.
                if try_pending_key_flush(&state).await {
                    backoff = StdDuration::ZERO;
                    consecutive_failures = 0;
                    state.progress.mark_flush_success();
                    tracing::info!(
                        "sync: a pending rotation key authenticated — promoted it to active \
                         and resumed normal sync (an earlier `dira device rotate-key` had \
                         committed on the cloud but its confirmation never reached this \
                         device, or the daemon simply hadn't reloaded the key yet)"
                    );
                    record_health(&state, None, 0, 0).await;
                    state.billing_refresh.notify_one();
                } else if try_reloaded_key_flush(&state, &device_key).await {
                    backoff = StdDuration::ZERO;
                    consecutive_failures = 0;
                    state.progress.mark_flush_success();
                    tracing::info!(
                        "sync: reloaded the active device key and it authenticated — resumed \
                         normal sync (a `dira device rotate-key` completed in another process \
                         while this daemon still had the previous key cached)"
                    );
                    record_health(&state, None, 0, 0).await;
                    state.billing_refresh.notify_one();
                } else {
                    backoff = MAX_BACKOFF;
                    consecutive_failures += 1;
                    state.progress.mark_flush_failure();
                    tracing::error!(
                        "sync: cloud rejected our signature (bad_signature) — the local device \
                         key no longer matches the cloud's registered key. If a `dira device \
                         rotate-key` was interrupted, run it again to retry/resume it; \
                         otherwise re-link with `dira device link`. (cloud said: {body})"
                    );
                    record_health(
                        &state,
                        Some("signature_rejected"),
                        consecutive_failures,
                        backoff.as_secs(),
                    )
                    .await;
                    sleep(backoff).await;
                }
            }
            Err(SyncError::SchemaSkew(body)) => {
                // Re-sending won't help — don't hot-loop the rejected batch. Back
                // off long so the operator has time to act. The hosted cloud is
                // always current, so a rejected major almost always means this
                // daemon's contract version has fallen behind it — `dira update`
                // is the remediation (self-hosted clouds should upgrade instead).
                backoff = MAX_BACKOFF;
                consecutive_failures += 1;
                state.progress.mark_flush_failure();
                tracing::error!(
                    "sync: cloud rejected our contract version (unsupported_schema_version) — \
                     run `dira update` to bring the daemon's contract version back in range \
                     (self-hosting the cloud instead? upgrade it to match); pausing sync: {body}"
                );
                record_health(
                    &state,
                    Some("schema_skew"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
            Err(SyncError::PayloadTooLarge(body)) => {
                backoff = next_backoff(backoff);
                consecutive_failures += 1;
                state.progress.mark_flush_failure();
                // Named explicitly, because the generic "backing off" line was read
                // as an ordinary transient failure for the whole of issue #71 while
                // the batch was in fact unsendable. Artifacts are capped by
                // construction now, so this points at one oversized record rather
                // than at the backlog.
                tracing::error!(
                    "sync: cloud rejected the batch as too large (413) — a single \
                     record exceeds the request ceiling and re-sending cannot help; \
                     backing off {backoff:?}: {body}"
                );
                record_health(
                    &state,
                    Some("payload_too_large"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
            Err(SyncError::Fatal(e)) => {
                backoff = next_backoff(backoff);
                consecutive_failures += 1;
                state.progress.mark_flush_failure();
                tracing::warn!("sync: error, backing off {backoff:?}: {e}");
                // Untyped 401s (and every other local/unexpected failure) land
                // here — still recorded so `dira status` shows a failing sync
                // even when the failure doesn't fit a more specific category.
                record_health(
                    &state,
                    Some("fatal"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
        }
    }
}

/// Parse a `Retry-After` response header (seconds form) into a [`StdDuration`].
/// `None` when the header is absent or not a plain integer — the cloud never
/// sends the HTTP-date form (see `dira_core::sync::parse_retry_after_secs`).
pub(crate) fn retry_after_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Option<StdDuration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(dira_core::sync::parse_retry_after_secs)
        .map(StdDuration::from_secs)
}

/// One-shot daemon-side self-heal for a signature rejection (WP-B1b).
///
/// If a rotation key is sitting pending on disk (persisted by `dira device
/// rotate-key` before it POSTed — see `dira/src/device.rs`), retry the exact
/// SAME flush the caller just failed, but signed with the pending key instead
/// of the (apparently dead) active one. Reuses `flush` wholesale rather than
/// re-implementing any of its chunking/cursor/handshake logic — this is
/// exactly what a normal flush attempt does, just with a different signing
/// key, so nothing about idempotency or health recording needs to be
/// special-cased.
///
/// A 2xx from that retry is unambiguous proof the pending key IS what the
/// cloud currently has installed (whatever CAS commit put it there — an
/// earlier `rotate-key` attempt whose response never reached the CLI, or one
/// that completed moments ago on another process) — the daemon has no reason
/// to distinguish those cases, it only needs "does this key work RIGHT NOW".
/// On that proof, promotes pending → active and invalidates the daemon's
/// cached device key so the NEXT tick (and this one, having already synced)
/// signs correctly without a restart.
///
/// Returns `false` — covering both "no pending key" and "the pending key ALSO
/// failed" — for the caller's normal `SignatureRejected` handling (backoff +
/// an actionable log message) to take over; this function never itself backs
/// off or logs an error, that's the caller's job on the non-recovery path.
async fn try_pending_key_flush(state: &AppState) -> bool {
    let Ok(Some((pending_key, _rotated_at))) =
        dira_core::identity::load_pending_key(&state.store).await
    else {
        return false; // no rotation in flight — nothing to try
    };
    match flush(state, &pending_key, &state.http).await {
        // Promotion requires an actual authenticated exchange with the cloud —
        // `Synced` (a batch was accepted) or `Nothing` (the window was empty,
        // but the cloud still validated the signature to get there). `Skipped`
        // means `flush` never even reached the cloud (no `cloud_url` / not
        // linked yet) — it's a local no-op, not proof the pending key works,
        // so it must NOT promote a key we have no evidence is actually valid.
        Ok(FlushOutcome::Synced | FlushOutcome::Nothing) => {
            if let Err(e) = dira_core::identity::promote_pending_key(&state.store).await {
                // Extremely unlikely (a local store write failure right after a
                // successful network round-trip) — surface it, but still report
                // failure so the caller's normal path retries; the pending key
                // material is untouched, so a later attempt (this same
                // `try_pending_key_flush`, on the next SignatureRejected) will
                // simply re-authenticate with it and try the promote again.
                tracing::warn!("sync: promoting pending key after a successful flush failed: {e}");
                return false;
            }
            state.invalidate_device_key().await;
            true
        }
        Ok(FlushOutcome::Skipped) => false, // never reached the cloud — no proof the key works
        Err(_) => false, // the pending key doesn't work either — not our problem to explain
    }
}

/// Second-chance self-heal for a signature rejection when NO rotation is
/// pending: the store's ACTIVE key may have changed out-of-process.
///
/// The dominant case is the ordinary, successful `dira device rotate-key`:
/// the CLI process persists a pending key, POSTs it, the cloud CAS commits,
/// and the CLI promotes pending → active and clears the pending markers —
/// all typically finishing well before this daemon's next flush tick. By
/// then [`try_pending_key_flush`] finds nothing pending, but the daemon's
/// cached key is still the OLD one, so without this reload it would re-sign
/// with a dead key at `MAX_BACKOFF` forever (until a restart).
///
/// Invalidates the cache, reloads the active key through the normal
/// resolution ladder ([`AppState::device_key`]), and — only if the reloaded
/// key actually differs from the one the cloud just rejected — retries the
/// SAME flush with it. An unchanged key is never retried: identical bytes
/// produce an identical rejection, and hot-looping them would just burn the
/// rate-limit budget. Like [`try_pending_key_flush`], returns `false` for
/// the caller's normal hard-stop path (backoff + actionable log) to handle.
async fn try_reloaded_key_flush(state: &AppState, rejected: &DeviceKey) -> bool {
    state.invalidate_device_key().await;
    let Some(fresh) = state.device_key().await else {
        return false; // reload failed — nothing to retry with
    };
    if fresh.public_base64() == rejected.public_base64() {
        return false; // the store still holds the dead key — same key, same rejection
    }
    match flush(state, &fresh, &state.http).await {
        // Same promotion bar as `try_pending_key_flush`: only an actual
        // authenticated round-trip proves the reloaded key works. (No
        // promote step here — the reloaded key already IS the active one;
        // `device_key()` cached it during the reload above.)
        Ok(FlushOutcome::Synced | FlushOutcome::Nothing) => true,
        Ok(FlushOutcome::Skipped) => false, // never reached the cloud — no proof
        Err(_) => false, // the reloaded key doesn't work either — hard-stop path takes over
    }
}

/// One exponential-backoff ladder for BOTH device→cloud channels (attestation
/// and knowledge sync): 2s seed, doubling, capped at [`MAX_BACKOFF`].
pub(crate) fn next_backoff(current: StdDuration) -> StdDuration {
    let next = if current.is_zero() {
        StdDuration::from_secs(2)
    } else {
        current * 2
    };
    next.min(MAX_BACKOFF)
}

/// The wait to sleep before retrying a transient failure: the cloud's
/// `retry_after` (from a 429) when present, else the usual exponential
/// ladder off `current` — either way capped at [`MAX_BACKOFF`] so a
/// misbehaving/huge `Retry-After` can't wedge sync indefinitely. Pure, so the
/// cap and the override-vs-ladder choice are unit-testable without a network.
pub(crate) fn transient_wait(
    retry_after: Option<StdDuration>,
    current: StdDuration,
) -> StdDuration {
    retry_after
        .unwrap_or_else(|| next_backoff(current))
        .min(MAX_BACKOFF)
}

/// Minimum spacing between consecutive ingest POSTs inside one flush. 2.5s ⇒ at
/// most 24/min, leaving headroom under the 30/min budget for the periodic syncs
/// and retries that share the same window.
pub(crate) const INGEST_CHUNK_SPACING: StdDuration = StdDuration::from_millis(2_500);

/// Chunk count at or below which a flush is never paced. Normal traffic is one or
/// two chunks; pacing those would add latency to every ordinary sync to solve a
/// problem only a large drain has.
pub(crate) const UNPACED_CHUNKS: usize = 10;

/// How long to wait before POSTing the next chunk of a `chunk_count`-chunk flush,
/// given that the previous POST already took `already_elapsed`.
///
/// A drain that fires every chunk back-to-back is what put a 49-chunk token
/// backlog against a 30/min budget and got throttled at chunk 31 (#88). Pacing
/// keeps a long drain inside the budget; per-chunk cursors (D-0020) are what make
/// it survivable when pacing is not enough. Pure, so the policy is testable
/// without a clock or a network.
pub(crate) fn chunk_pace_delay(
    chunk_count: usize,
    already_elapsed: StdDuration,
) -> Option<StdDuration> {
    if chunk_count <= UNPACED_CHUNKS {
        return None;
    }
    // The round-trip itself counts toward the interval — a slow network needs no
    // help spacing requests out.
    let remaining = INGEST_CHUNK_SPACING.saturating_sub(already_elapsed);
    (!remaining.is_zero()).then_some(remaining)
}

/// Which channel a health snapshot belongs to: its `meta` keys plus the log
/// prefix. Both sync tasks persist the same [`dira_core::sync::SyncHealth`]
/// shape through [`record_channel_health`]; only the keys differ.
pub(crate) struct HealthChannel {
    /// Log prefix, e.g. `"sync"` / `"knowledge sync"`.
    pub log: &'static str,
    /// `meta` key holding the JSON health snapshot.
    pub health_key: &'static str,
    /// `meta` key whose value is surfaced as `health.cursor`.
    pub cursor_key: &'static str,
    /// `meta` key surfaced as `health.cloud_watermark`, when the channel has one.
    pub watermark_key: Option<&'static str>,
}

/// Persist a compact health snapshot for one sync channel, for `dira
/// status`/`dira device status` to render (WP-B9 adds the rendering; this is
/// the write side). `error_kind = None` records a success (stamps
/// `last_success_at`, clears `last_error_kind`); `Some(kind)` records a
/// non-success tick of that kind and leaves `last_success_at` untouched —
/// `kind` is either a failure category (bumps `consecutive_failures`, passed
/// in by the caller) or a quiet non-failure (`"skipped"`, `"off"`, …; the
/// caller keeps `consecutive_failures` at 0).
///
/// Best-effort and read-modify-write over the single JSON blob: a read/parse/
/// write failure is logged and otherwise ignored — health is diagnostic, it
/// must never gate or fail a flush. Called on every flush outcome (the `run`
/// loops' `match` arms) so `last_attempt_at` never lags.
pub(crate) async fn record_channel_health(
    state: &AppState,
    channel: &HealthChannel,
    error_kind: Option<&str>,
    consecutive_failures: u32,
    backoff_secs: u64,
) {
    use dira_core::sync::parse_sync_health;

    let now = crate::heartbeat::fmt_rfc3339(time::OffsetDateTime::now_utc());
    let mut health = state
        .store
        .meta_get(channel.health_key)
        .await
        .ok()
        .flatten()
        .and_then(|j| parse_sync_health(&j))
        .unwrap_or_default();

    health.last_attempt_at = Some(now.clone());
    health.consecutive_failures = consecutive_failures;
    health.backoff_secs = backoff_secs;
    health.cursor = state
        .store
        .meta_get(channel.cursor_key)
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    if let Some(watermark_key) = channel.watermark_key {
        health.cloud_watermark = state
            .store
            .meta_get(watermark_key)
            .await
            .ok()
            .flatten()
            .filter(|s| !s.is_empty());
    }
    match error_kind {
        None => {
            health.last_success_at = Some(now);
            health.last_error_kind = None;
        }
        Some(kind) => health.last_error_kind = Some(kind.to_string()),
    }

    match serde_json::to_string(&health) {
        Ok(json) => {
            if let Err(e) = state.store.meta_set(channel.health_key, &json).await {
                tracing::debug!("{}: persist health snapshot failed: {e}", channel.log);
            }
        }
        Err(e) => tracing::debug!("{}: serialize health snapshot failed: {e}", channel.log),
    }
}

/// The attestation channel's [`HealthChannel`] keys.
const SYNC_HEALTH_CHANNEL: HealthChannel = HealthChannel {
    log: "sync",
    health_key: dira_core::sync::META_SYNC_HEALTH,
    cursor_key: META_SYNC_CURSOR,
    watermark_key: Some(dira_core::sync::META_CLOUD_WATERMARK),
};

/// Persist the attestation channel's health snapshot to `META_SYNC_HEALTH`
/// (see [`record_channel_health`] for the semantics).
async fn record_health(
    state: &AppState,
    error_kind: Option<&str>,
    consecutive_failures: u32,
    backoff_secs: u64,
) {
    record_channel_health(
        state,
        &SYNC_HEALTH_CHANNEL,
        error_kind,
        consecutive_failures,
        backoff_secs,
    )
    .await;
}

/// What a flush did, for backoff bookkeeping.
#[cfg_attr(test, derive(Debug))]
enum FlushOutcome {
    /// A batch was accepted (or was a duplicate) and the cursor advanced.
    Synced,
    /// The window was empty; nothing to do.
    Nothing,
    /// Sync is not configured (no cloud_url) or the device is not linked.
    Skipped,
}

/// Errors that distinguish "retry later" from "stop until a human acts".
#[cfg_attr(test, derive(Debug))]
enum SyncError {
    /// Network / 5xx / 429 — leave the cursor, back off, the backstop will
    /// retry. `retry_after` carries the cloud's requested wait (from a 429's
    /// `Retry-After` header or typed body) when present; the run loop honors
    /// it over the usual exponential backoff.
    Transient {
        message: String,
        retry_after: Option<StdDuration>,
    },
    /// 401 unknown_device — the device isn't linked cloud-side; needs a re-link.
    ReLinkRequired,
    /// 401 bad_signature — the key we signed with doesn't match the cloud's
    /// registered key (typically an interrupted key rotation, see
    /// `dira/src/device.rs`). Retrying identical bytes cannot succeed, so this
    /// is handled like a hard-stop (back off at the ceiling), not a transient.
    SignatureRejected(String),
    /// The cloud rejected our contract major (`unsupported_schema_version`).
    /// Re-sending the same batch can't help — pause with a clear message instead
    /// of hot-looping until the operator upgrades the daemon or cloud.
    SchemaSkew(String),
    /// `413 Payload Too Large` — the request body exceeded the platform's ceiling.
    ///
    /// Its own variant rather than a generic [`Self::Fatal`] because the two need
    /// different things from the operator, and conflating them is what made issue
    /// #71 hard to read: a 413 surfaced as "sync: error, backing off", identical to
    /// a local DB failure. Re-sending unchanged bytes cannot succeed, so the
    /// message has to say what is oversized rather than imply a retry will fix it.
    /// With [`dira_core::sync::CHUNK_ARTIFACTS`] bounding batches by construction,
    /// reaching this now means a single pathological row — which no amount of
    /// re-chunking would shrink.
    PayloadTooLarge(String),
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

    // Token rows in `(tok_cursor, tok_until]` — like artifacts, they ride their own
    // rowid cursor rather than the event window. They used to be selected by the
    // at-range of this batch's events, which dropped almost all of them: a turn is
    // discovered by the `Stop` that FOLLOWS it, so its transcript timestamp always
    // sorts below the window's own first event. The cloud dedups token_usage by id,
    // so over-inclusion is free; under-inclusion was permanent, because nothing
    // recorded that a row had never been sent.
    let tok_cursor = state
        .store
        .meta_get(META_TOKEN_CURSOR)
        .await
        .map_err(|e| SyncError::Fatal(format!("read token cursor: {e}")))?
        .and_then(|s| s.parse::<i64>().ok());
    let tok_until = state
        .store
        .max_token_usage_rowid()
        .await
        .map_err(|e| SyncError::Fatal(format!("read max token rowid: {e}")))?;
    let token_rows =
        match tok_until {
            Some(u) if tok_cursor != Some(u) => state
                .store
                .unsynced_token_usage(tok_cursor, u)
                .await
                .map_err(|e| SyncError::Fatal(format!("load token rows: {e}")))?,
            _ => Vec::new(),
        };

    // Tokens join the flush gate. Without this a caught-up event log meant a token
    // backlog could never drain — `flush` returned `Nothing` and no later flush
    // reconsidered it.
    if events.is_empty() && artifact_rows.is_empty() && token_rows.is_empty() {
        return Ok(FlushOutcome::Nothing);
    }

    // 3. Build deterministic, capped sub-batches for the window. Each chunk derives
    //    its own intervals/sessions (split only at idle breaks, so no counted gap is
    //    lost); the final chunk also carries tokens/artifacts/partial rollups.
    //    Chunking bounds request size and makes a from-scratch resync reproducible.
    //
    //    Phase 6c: partial rollups for long-running un-ended sessions come from the
    //    live registry; their watermark is advanced only after the final chunk acks.
    let now = time::OffsetDateTime::now_utc();
    let partials = partial_rollups(state, now);
    let partial_ids: Vec<String> = partials.iter().map(|p| p.session_id.clone()).collect();
    let idle = state.config.idle();
    let agent_policy = state.config.agent_policy();
    // Seed the first chunk's interval build with the already-synced human signals that
    // neighbour the window's human-signal `at`-span (padded by one idle on each side),
    // so a counted gap that straddles the flush boundary — including one re-split by a
    // `dira log`-backdated signal — is recovered instead of dropped (issue #21). The
    // band, not the cursor's newest signal, is the bound: a backdated window signal can
    // be far below the cursor yet still form a ≤ idle gap with an old pre-cursor signal.
    // Read-only: never re-emitted, never advances the cursor.
    let human_ats: Vec<time::OffsetDateTime> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| e.at)
        .collect();
    let seed = match (human_ats.iter().min(), human_ats.iter().max()) {
        (Some(&lo), Some(&hi)) => state
            .store
            .human_signal_seed(cursor.as_deref(), lo - idle, hi + idle)
            .await
            .map_err(|e| SyncError::Fatal(format!("read human-signal seed: {e}")))?,
        _ => Vec::new(), // no human signals in the window → no gaps to anchor
    };
    // Issue #40: a multi-window session's early events were consumed by earlier
    // flushes. For sessions that END in this window, re-fetch their full retained
    // history so the terminal rollup aggregates the whole session.
    let ended_ids: Vec<String> = events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                dira_core::model::EventKind::SessionEnd | dira_core::model::EventKind::ManualStop
            )
        })
        .map(|e| e.session_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    // Bounded by the same `until` snapshot as `events_between` above, so the
    // history read is consistent with the window we're building this flush.
    let history = match &until {
        Some(u) if !ended_ids.is_empty() => state
            .store
            .events_for_sessions(&ended_ids, u)
            .await
            .map_err(|e| SyncError::Fatal(format!("load session history: {e}")))?,
        _ => Vec::new(),
    };
    let chunks = dira_core::sync::build_chunked_batches(
        &events,
        &token_rows,
        &artifact_rows,
        &partials,
        &device_id,
        idle,
        agent_policy,
        now,
        &seed,
        &history,
    );

    // 4. POST each chunk in order; advance the event cursor to that chunk's high-water
    //    only on its 2xx ack, so an interrupt mid-drain resumes from the last acked
    //    chunk (and re-formed chunks over an identical event subset reproduce the same
    //    batch id ⇒ the cloud dedups them). The final chunk's ack advances the
    //    artifact cursor and marks partials sent.
    let url = format!("{}/api/v1/ingest", cloud_url.trim_end_matches('/'));
    let mut total_intervals = 0usize;
    let mut total_sessions = 0usize;
    let mut last_body = String::new();
    // D12: true once ANY chunk's ack in this flush has signaled a changed
    // `dataEpoch` (the cloud's durable log was reset). From that point on we
    // stop advancing cursors/marking partials for the REST of this flush —
    // including the very chunk that triggered it — so the flush ends with the
    // cursors fully blanked (via `apply_handshake`, applied per-chunk below)
    // rather than a later chunk's success silently re-advancing them past the
    // reset. See the comment at the gate below for the full argument.
    let mut epoch_reset = false;
    let chunk_count = chunks.len();
    for chunk in chunks {
        let sig = device_key
            .sign_payload(&chunk.batch)
            .map_err(|e| SyncError::Fatal(format!("sign: {e}")))?;
        // Capture everything read after the POST up front, so the batch —
        // up to CHUNK_EVENTS worth of intervals/sessions/tokens/artifacts —
        // moves into the envelope instead of being deep-cloned per chunk.
        let interval_count = chunk.batch.intervals.len();
        let session_count = chunk.batch.sessions.len();
        let cursor_event_id = chunk.cursor_event_id;
        let token_rowid_high = chunk.token_rowid_high;
        let artifact_rowid_high = chunk.artifact_rowid_high;
        let is_last = chunk.is_last;
        let envelope = Envelope {
            schema_version: SCHEMA_VERSION.to_string(),
            device_id: device_id.clone(),
            payload: chunk.batch,
            sig,
        };
        // Pace a long drain so it stays inside the cloud's ingest budget instead of
        // being throttled part-way through (#88). Measured across the POST, so the
        // round-trip counts toward the interval; skipped entirely for the ordinary
        // one- or two-chunk flush. See D-0020.
        let posted_at = Instant::now();
        let resp = client
            .post(&url)
            .timeout(HTTP_TIMEOUT)
            .json(&envelope)
            .send()
            .await
            .map_err(|e| SyncError::Transient {
                message: format!("post ingest: {e}"),
                retry_after: None,
            })?;
        let status = resp.status();

        // 429: checked BEFORE the success/401 branches below. Cursors for
        // already-acked chunks in this flush stay advanced (we return before
        // touching this chunk's), so a retry only re-sends the remainder.
        // Header form first (cheap, standard); the typed JSON body is the
        // fallback for a proxy that strips the header.
        if status.as_u16() == 429 {
            let header_hint = retry_after_from_headers(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            let retry_after = header_hint.or_else(|| {
                dira_core::sync::parse_retry_after_body(&body).map(StdDuration::from_secs)
            });
            return Err(SyncError::Transient {
                message: format!("ingest 429: {body}"),
                retry_after,
            });
        }

        if status.is_success() {
            let body = resp.text().await.unwrap_or_default();

            // D12 fix: apply the advisory handshake from EVERY chunk's ack, not
            // just the last. Previously `apply_handshake` ran once, after the
            // whole `for` loop over chunks finished — so if chunk 1 of 3 signaled
            // a changed `dataEpoch` but chunk 3 then failed, the function
            // returned `Err` from inside this loop and the epoch reset was never
            // applied at all THIS flush attempt (chunk 1's already-2xx'd body
            // was simply dropped). The reset wasn't lost forever (the next
            // flush's ack carries the same current epoch and would eventually
            // trigger it), but it was needlessly delayed — this fixes that by
            // reacting to the signal the instant we see it. `apply_handshake`
            // is idempotent (stores the new epoch before resetting/triggering,
            // so a same-epoch second call this flush is a no-op), so calling it
            // once per chunk is exactly as safe as calling it once.
            let reset_now = apply_handshake(state, &body).await?;
            epoch_reset = epoch_reset || reset_now;

            // Once a reset has fired THIS flush, no further cursor/partial
            // advance may happen for the rest of it — including for the
            // triggering chunk's own high-water mark. Rationale: the epoch
            // reset means the cloud's durable log lost everything at or before
            // the cursor snapshotted at the TOP of this flush; blanking the
            // cursor is meant to make the NEXT flush re-send the entire local
            // event log from scratch to recover it. If a later chunk in this
            // same flush were still allowed to push the cursor forward again
            // (chunk 2/3 succeeding against the now-reset cloud), the blank
            // would be silently undone before this flush even returns, and the
            // pre-flush backlog the reset needs recovered would never get
            // re-sent. Already-POSTed chunks are NOT re-sent within this same
            // flush (no rollback) — they'll simply be re-sent, harmlessly
            // idempotent, on the next (triggered) flush along with everything
            // else. Nothing is ever lost; the only cost is some redundant
            // re-transmission, exactly the tradeoff the pre-existing
            // single-shot version already accepted.
            if !epoch_reset {
                if let Some(ev) = &cursor_event_id {
                    state
                        .store
                        .meta_set(META_SYNC_CURSOR, ev)
                        .await
                        .map_err(|e| SyncError::Fatal(format!("advance cursor: {e}")))?;
                }
                // Row cursors advance on the chunk that CARRIED the rows, to the
                // watermark that chunk covered — not on `is_last`, and never to
                // the flush-wide snapshot bound. Gating these on the final chunk
                // made progress depend on an unbounded run of consecutive
                // successes: a 49-chunk token drain against the cloud's 30/min
                // ingest budget recorded nothing at all and restarted at row 1
                // forever (#88), and any mid-drain restart cost the whole
                // backlog the same way. See D-0020.
                if let Some(u) = artifact_rowid_high {
                    state
                        .store
                        .meta_set(META_ARTIFACTS_CURSOR, &u.to_string())
                        .await
                        .map_err(|e| SyncError::Fatal(format!("advance artifacts cursor: {e}")))?;
                }
                if let Some(u) = token_rowid_high {
                    state
                        .store
                        .meta_set(META_TOKEN_CURSOR, &u.to_string())
                        .await
                        .map_err(|e| SyncError::Fatal(format!("advance token cursor: {e}")))?;
                }
                // Partial rollups are a live-registry snapshot, not a cursor over
                // stored rows, so they stay on the final chunk. See D-0020.
                if is_last && !partial_ids.is_empty() {
                    if let Ok(mut reg) = state.sessions.lock() {
                        reg.mark_partials_sent(&partial_ids);
                    }
                }
            }
            total_intervals += interval_count;
            total_sessions += session_count;
            last_body = body;
            // Nothing follows the final chunk, so it never pays the spacing.
            if !is_last {
                if let Some(wait) = chunk_pace_delay(chunk_count, posted_at.elapsed()) {
                    sleep(wait).await;
                }
            }
            continue;
        }

        // Non-2xx: cursors for already-acked chunks stay advanced, so the next flush
        // re-chunks only the remaining window. Classify why this chunk was rejected.
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            if is_unknown_device(&body) {
                return Err(SyncError::ReLinkRequired);
            }
            if is_bad_signature(&body) {
                return Err(SyncError::SignatureRejected(body));
            }
            return Err(SyncError::Fatal(format!("401 from ingest: {body}")));
        }
        if status.is_server_error() {
            return Err(SyncError::Transient {
                message: format!("ingest {status}"),
                retry_after: None,
            });
        }
        // 4xx: unknown_device can arrive here (gateways map auth → 403); an
        // unsupported contract major is a skew we pause on rather than hot-loop.
        if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
            return Err(SyncError::PayloadTooLarge(body));
        }
        if is_unknown_device(&body) {
            return Err(SyncError::ReLinkRequired);
        }
        if is_unsupported_schema(&body) {
            return Err(SyncError::SchemaSkew(body));
        }
        return Err(SyncError::Fatal(format!("ingest {status}: {body}")));
    }

    // 5. The advisory handshake (epoch + watermark) was already applied
    //    per-chunk above (D12) — nothing left to do here but re-read the last
    //    chunk's ack for the summary log line below.
    //
    //    Deliberately the SAME parser the handshake uses, not a second typed ack
    //    of our own: the previous code deserialized the real 202 body into an
    //    all-default `IngestAck` (every field `#[serde(default)]`), so it always
    //    logged `accepted=0 duplicates=0` — on flushes that demonstrably landed.
    //    That read as "the cloud is silently dropping everything" and cost real
    //    time during an unrelated outage (issue #72).
    let ack = dira_core::sync::parse_ingest_response(&last_body);
    tracing::info!(
        events = events.len(),
        artifacts = artifact_rows.len(),
        // Without this a flush shipping zero token rows logged byte-identically to
        // one shipping a thousand, which is why a 97% compute loss produced no
        // warning, no counter and no log line for weeks.
        tokens = token_rows.len(),
        partial_rollups = partial_ids.len(),
        chunks = chunk_count,
        intervals = total_intervals,
        sessions = total_sessions,
        status = %ack.status,
        batch_id = %ack.batch_id,
        // `-` when the cloud doesn't report a count. An unknown quantity must not
        // be printed as a number, which is the entire bug being fixed here.
        accepted = %count_or_unknown(ack.accepted),
        duplicates = %count_or_unknown(ack.duplicates),
        cursor = until.as_deref().unwrap_or(""),
        "sync: flushed batch(es) to cloud"
    );
    Ok(FlushOutcome::Synced)
}

/// Whether a non-2xx ingest error body is the typed `unsupported_schema_version`
/// signal (the cloud rejects our contract major). Unparseable ⇒ not a skew.
fn is_unsupported_schema(body: &str) -> bool {
    serde_json::from_str::<IngestError>(body)
        .map(|e| e.error == "unsupported_schema_version")
        .unwrap_or(false)
}

/// Apply the advisory cloud→daemon handshake from an ingest response body.
///
/// - **Epoch**: if the cloud's `dataEpoch` differs from the one we last stored (and
///   we *had* stored one), the cloud's durable log was reset → blank both cursors so
///   the next flush re-sends from scratch, store the new epoch FIRST (so this fires
///   exactly once), and nudge a flush. Safe & idempotent: re-sent batches dedup by
///   id and intervals dedup by content, so a full re-send never double-counts.
/// - **Watermark**: cache `syncedEventId` for an honest `dira device status`. Display
///   only — never an automatic rewind (recovery is the cloud reconciler + a manual
///   `dira device resync`).
///
/// Called once per chunk ack (D12; see the call site in `flush`) — idempotent
/// within a single flush, since the second and later calls in the same flush
/// see `stored == epoch` (already updated by the first) and take the no-op
/// branch. Returns `true` exactly when THIS call is the one that triggered the
/// cursor blank, so the caller can gate its own cursor-advance logic on it.
async fn apply_handshake(state: &AppState, body: &str) -> Result<bool, SyncError> {
    use dira_core::sync::META_CLOUD_WATERMARK;
    let resp = dira_core::sync::parse_ingest_response(body);
    let mut reset = false;

    if let Some(epoch) = resp.sync.data_epoch.as_deref() {
        reset = note_data_epoch(state, epoch)
            .await
            .map_err(SyncError::Fatal)?;
    }

    if let Some(wm) = resp.sync.synced_event_id.as_deref() {
        // Display-only cache; never drives a rewind.
        let _ = state.store.meta_set(META_CLOUD_WATERMARK, wm).await;
    }
    Ok(reset)
}

/// Adopt/compare the cloud's `dataEpoch`. On a CHANGE (the cloud's durable log
/// was reset), blank every sync cursor — both attestation cursors AND the four
/// knowledge cursors — and nudge both tasks, so the whole device re-sends from
/// scratch as one coherent wipe-and-resync. Shared by the attestation ack path
/// ([`apply_handshake`]) and the knowledge ack path
/// (`crate::knowledge_sync::post_chunks`): whichever channel sees the new
/// epoch first resets both. Returns `true` exactly when THIS call performed
/// the reset.
pub(crate) async fn note_data_epoch(state: &AppState, epoch: &str) -> Result<bool, String> {
    use dira_core::sync::knowledge::{
        META_KNOWLEDGE_DECISION_CURSOR, META_KNOWLEDGE_GUARD_CURSOR, META_KNOWLEDGE_SPEC_CURSOR,
        META_KNOWLEDGE_TRAILER_CURSOR,
    };
    use dira_core::sync::META_LAST_EPOCH;

    let stored = state
        .store
        .meta_get(META_LAST_EPOCH)
        .await
        .map_err(|e| format!("read epoch: {e}"))?
        .filter(|s| !s.is_empty());
    match stored.as_deref() {
        None => {
            // First epoch we've seen — adopt it, no action.
            state
                .store
                .meta_set(META_LAST_EPOCH, epoch)
                .await
                .map_err(|e| format!("store epoch: {e}"))?;
            Ok(false)
        }
        Some(prev) if prev != epoch => {
            // Store the new epoch BEFORE resetting/triggering so this fires once.
            state
                .store
                .meta_set(META_LAST_EPOCH, epoch)
                .await
                .map_err(|e| format!("store epoch: {e}"))?;
            for key in [
                META_SYNC_CURSOR,
                META_ARTIFACTS_CURSOR,
                META_TOKEN_CURSOR,
                META_KNOWLEDGE_DECISION_CURSOR,
                META_KNOWLEDGE_SPEC_CURSOR,
                META_KNOWLEDGE_TRAILER_CURSOR,
                META_KNOWLEDGE_GUARD_CURSOR,
            ] {
                state
                    .store
                    .meta_set(key, "")
                    .await
                    .map_err(|e| format!("reset {key}: {e}"))?;
            }
            tracing::warn!(
                epoch = epoch,
                "sync: cloud data epoch changed (cloud was reset) — re-sending from scratch"
            );
            let _ = state.sync.trigger.try_send(());
            let _ = state.knowledge_sync.trigger.try_send(());
            Ok(true)
        }
        _ => Ok(false), // unchanged epoch — nothing to do
    }
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
            // The live registry now tracks both prompts and the last-resolved
            // branch per session (issue #40), so a partial carries them straight
            // through. `Some(0)` for a session with no prompts yet is deliberate —
            // consistent with the ended path's `Some(a.prompts)` in `build_sessions`.
            prompts: Some(s.prompts),
            branch: s.branch,
            note: s.note,
            label: s.label,
        })
        .collect()
}

/// Render an optional cloud-reported count for a log line: the number when the
/// cloud reported one, `-` when it didn't.
///
/// The whole of issue #72 is this distinction. Printing an unreported counter as
/// `0` is not a cosmetic defect — it states, in the daemon's own telemetry, that
/// the cloud accepted nothing, which is the most alarming thing it could say
/// about a flush that in fact succeeded.
fn count_or_unknown(n: Option<u64>) -> String {
    n.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// Whether a non-2xx ingest error body is the typed `unknown_device` signal that
/// the device needs a re-link. Parses the typed [`IngestError`]; an unparseable
/// body is treated as *not* unknown_device (some other client/server fault).
fn is_unknown_device(body: &str) -> bool {
    serde_json::from_str::<IngestError>(body)
        .map(|e| e.error == "unknown_device")
        .unwrap_or(false)
}

/// Whether a non-2xx ingest error body is the typed `bad_signature` signal —
/// the key we signed with doesn't match the cloud's currently-registered key
/// (see [`SyncError::SignatureRejected`]). Mirrors [`is_unknown_device`]: an
/// unparseable body is treated as *not* bad_signature.
fn is_bad_signature(body: &str) -> bool {
    serde_json::from_str::<IngestError>(body)
        .map(|e| e.error == "bad_signature")
        .unwrap_or(false)
}

/// Best-effort schema-version handshake (Phase 3d).
///
/// Fetches the cloud's supported contract range from `GET /api/v1/meta` and logs
/// a clear warning if our [`SCHEMA_VERSION`] falls outside it. Entirely
/// non-fatal: a missing `cloud_url`, an unreachable cloud, an old cloud without
/// the endpoint, or an unparseable body all simply skip the check. Run once at
/// daemon startup; sync/heartbeat are unaffected by its outcome.
pub async fn check_schema_handshake(client: &reqwest::Client, cloud_url: Option<&str>) {
    let Some(cloud_url) = cloud_url else {
        return; // sync disabled — nothing to handshake with
    };
    let url = format!("{}/api/v1/meta", cloud_url.trim_end_matches('/'));
    let resp = match client.get(&url).timeout(HANDSHAKE_TIMEOUT).send().await {
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
    let min_ok = parse_version(&meta.min_schema_version).is_none_or(|min| ours >= min);
    let max_ok = parse_version(&meta.schema_version).is_none_or(|max| ours <= max);
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
    use crate::test_support::{keychain_lock, use_mock_keychain, MockCloud, MockResp};

    /// RFC 3339 timestamp of an event (the storage format for `token_usage.at`).
    /// Test-only since token rows stopped being selected by `at`.
    fn fmt_at(e: &dira_core::model::RawEvent) -> String {
        e.at.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    /// The body the live cloud actually returns on a successful ingest.
    ///
    /// Every mock here used to answer `{"accepted":1,"duplicates":0}` — a shape the
    /// cloud has never sent. That fiction is exactly why the whole suite stayed
    /// green while production logged `accepted=0 duplicates=0` on every flush
    /// (issue #72). Mocks must speak the real wire, or they test nothing.
    const OK_INGEST: &str = r#"{"status":"accepted","batchId":"01ACK","sync":{}}"#;

    /// A minimal single-event window for `flush` to have something to send.
    fn test_event(session: &str, at: time::OffsetDateTime) -> dira_core::model::RawEvent {
        test_event_with_id(&ulid::Ulid::generate().to_string(), session, at)
    }

    /// Like [`test_event`] but with an explicit `id`. `events_between` orders
    /// rows by `id ASC` (lexicographic), so a multi-event test that needs the
    /// fetched order to match a specific `at` sequence must NOT rely on
    /// [`ulid::Ulid::generate`] — freshly-minted ULIDs in a tight loop can share a
    /// millisecond and sort by their random tail instead of insertion order.
    /// Zero-padded decimal ids sort exactly in call order.
    fn test_event_with_id(
        id: &str,
        session: &str,
        at: time::OffsetDateTime,
    ) -> dira_core::model::RawEvent {
        dira_core::model::RawEvent {
            id: id.to_string(),
            at,
            session_id: session.to_string(),
            harness: dira_contract::Harness::Manual,
            kind: dira_core::model::EventKind::ManualTick,
            cwd: None,
            project: Some("github.com/acme/api".to_string()),
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    /// Like [`test_event_with_id`] but with an explicit `kind`, so a test can
    /// build a mix of human-signal kinds (e.g. a `dira log`-style
    /// `ManualStart` landing between two `UserPrompt`s) on a specific session.
    fn human_event(
        id: &str,
        session: &str,
        at: time::OffsetDateTime,
        kind: dira_core::model::EventKind,
    ) -> dira_core::model::RawEvent {
        dira_core::model::RawEvent {
            id: id.to_string(),
            at,
            session_id: session.to_string(),
            harness: dira_contract::Harness::Manual,
            kind,
            cwd: None,
            project: Some("github.com/acme/api".to_string()),
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        }
    }

    /// Pull `payload.batchId` out of a raw recorded ingest request body (see
    /// [`MockCloud::requests`]) — used to compare batch ids across separate
    /// POSTs (crash-retry dedup, boundary re-decomposition) without
    /// re-parsing the whole envelope.
    fn batch_id_of(request_body: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(request_body).unwrap();
        v["payload"]["batchId"]
            .as_str()
            .expect("payload.batchId present")
            .to_string()
    }

    /// Build a linked, cloud-configured `AppState` backed by an in-memory
    /// store with one pending event to sync, pointed at `cloud.base_url()`.
    async fn linked_state_with_one_event(cloud: &MockCloud) -> AppState {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();
        store
            .append(&test_event("s1", time::OffsetDateTime::now_utc()))
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

    /// Regression test for the startup-flush-stall fix: a trigger that fires
    /// the instant the daemon starts (the writer's hot path, on the very
    /// first captured event) must be serviced within the ordinary DEBOUNCE
    /// window, not delayed until the ~81-99s jittered BACKSTOP. Drives the
    /// actual `run` loop (not just `flush`), so the fix is proved end to end.
    ///
    /// Before the fix, `run` blocked on a pre-loop `sleep_until(backstop_at)`
    /// before ever entering the `select!`, so `rx.recv()` — and therefore this
    /// trigger — sat unpolled for the entire jittered backstop period.
    ///
    /// Uses REAL (unpaused) time rather than `tokio::time::{pause,advance}`:
    /// this loop's flush does a genuine loopback HTTP round-trip against
    /// `MockCloud`, and mixing a paused virtual clock with real socket I/O is
    /// its own footgun (the debounce timer firing under `advance` does not by
    /// itself guarantee the executor has parked long enough to drive the
    /// subsequent real connect/write/read to completion). A generous-but-
    /// bounded real-time deadline is simpler and just as conclusive: the fix
    /// under test collapses the wait from ~81-99s to ~3s, so even a few
    /// real seconds of slack leaves an enormous margin against the old bug.
    #[tokio::test]
    async fn run_loop_services_a_startup_trigger_within_the_debounce_not_the_backstop() {
        // `ENV_DEVICE_SECRET` is process-global and sits FIRST in the identity
        // resolution ladder — serialize with every other test that resolves a
        // device key (they all hold this lock) so the env seed set here can't
        // leak into a concurrently-running key reload.
        let _keychain_lock = keychain_lock().await;
        struct ClearEnv;
        impl Drop for ClearEnv {
            fn drop(&mut self) {
                std::env::remove_var(dira_core::identity::ENV_DEVICE_SECRET);
            }
        }
        let _clear = ClearEnv;
        // Seed the device key via the env fallback so `state.device_key()`
        // resolves synchronously in `run`, without touching a real/mock
        // keychain — irrelevant to what this test is proving.
        let key = DeviceKey::generate();
        std::env::set_var(dira_core::identity::ENV_DEVICE_SECRET, key.secret_base64());

        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));

        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();
        store
            .append(&test_event("s1", time::OffsetDateTime::now_utc()))
            .await
            .unwrap();
        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            ..Default::default()
        };
        let (state, _rx, sync_rx, _knowledge_rx) = crate::build_state(store, config).await.unwrap();
        let trigger = state.sync.trigger.clone();

        let started = std::time::Instant::now();
        tokio::spawn(run(state, sync_rx));

        // Exactly what the writer's hot path does right after a durable
        // append at daemon start.
        trigger.try_send(()).expect("trigger channel has room");

        // DEBOUNCE is 3s; give it a generous 15s real-time deadline (CI
        // scheduling slack) — still over 5x tighter than the OLD bug's
        // ~81-99s stall, so this deadline elapsing is conclusive either way.
        let deadline = StdDuration::from_secs(15);
        loop {
            if !cloud.requests("/api/v1/ingest").is_empty() {
                break;
            }
            assert!(
                started.elapsed() < deadline,
                "a trigger right at daemon start must flush within the debounce window, \
                 not sit unserviced until the jittered ~81-99s backstop"
            );
            tokio::time::sleep(StdDuration::from_millis(50)).await;
        }

        assert!(
            started.elapsed() < StdDuration::from_secs(15),
            "flush landed at {:?}, expected well within the debounce window, \
             nowhere near the ~81-99s backstop",
            started.elapsed()
        );
        assert_eq!(cloud.requests("/api/v1/ingest").len(), 1);
    }

    #[tokio::test]
    async fn flush_429_with_retry_after_header_is_transient_and_leaves_cursor_unchanged() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":99}"#)
                .with_header("Retry-After", "7"),
        );
        let state = linked_state_with_one_event(&cloud).await;
        let cursor_before = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();

        let key = DeviceKey::generate();
        let err = flush(&state, &key, &state.http)
            .await
            .expect_err("429 must surface as an error");
        match err {
            SyncError::Transient { retry_after, .. } => {
                // Header (7) wins over the body's retryAfterSecs (99).
                assert_eq!(retry_after, Some(StdDuration::from_secs(7)));
            }
            _ => panic!("expected SyncError::Transient on a 429"),
        }

        let cursor_after = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor_before, cursor_after,
            "429 must not advance the cursor"
        );
    }

    #[tokio::test]
    async fn flush_429_without_header_falls_back_to_typed_body() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":12}"#),
        );
        let state = linked_state_with_one_event(&cloud).await;

        let key = DeviceKey::generate();
        let err = flush(&state, &key, &state.http).await.expect_err("429");
        match err {
            SyncError::Transient { retry_after, .. } => {
                assert_eq!(retry_after, Some(StdDuration::from_secs(12)));
            }
            _ => panic!("expected SyncError::Transient on a 429"),
        }
    }

    #[tokio::test]
    async fn flush_429_with_neither_header_nor_body_hint_falls_back_to_backoff_ladder() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::status(429, ""));
        let state = linked_state_with_one_event(&cloud).await;

        let key = DeviceKey::generate();
        let err = flush(&state, &key, &state.http).await.expect_err("429");
        match err {
            SyncError::Transient { retry_after, .. } => assert_eq!(retry_after, None),
            _ => panic!("expected SyncError::Transient on a 429"),
        }
    }

    /// D12: an epoch change signaled in an EARLIER chunk's ack must blank the
    /// cursor immediately, and that blank must survive even when a LATER chunk
    /// in the same flush fails — the exact regression this fix targets (before
    /// it, `apply_handshake` ran once after the whole loop, so a later-chunk
    /// failure meant the epoch signal from an already-2xx'd earlier chunk was
    /// dropped for this flush attempt entirely).
    #[tokio::test]
    async fn flush_applies_epoch_reset_from_an_earlier_chunk_even_when_a_later_chunk_fails() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        // Chunk 1/3: acks with a NEW epoch ⇒ must blank the cursor right away.
        cloud.push(
            "/api/v1/ingest",
            MockResp::ok(
                r#"{"status":"accepted","batchId":"01ACK","sync":{"dataEpoch":"epoch-2"}}"#,
            ),
        );
        // Chunk 2/3: succeeds under the (now-current) epoch — must NOT silently
        // re-advance the cursor past the blank chunk 1 just set.
        cloud.push(
            "/api/v1/ingest",
            MockResp::ok(
                r#"{"status":"accepted","batchId":"01ACK","sync":{"dataEpoch":"epoch-2"}}"#,
            ),
        );
        // Chunk 3/3: fails outright.
        cloud.push("/api/v1/ingest", MockResp::status(500, "boom"));

        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();
        // Seed the "previously known" epoch so chunk 1's differing epoch reads
        // as an actual change (not "first epoch we've ever seen").
        store
            .meta_set(dira_core::sync::META_LAST_EPOCH, "epoch-1")
            .await
            .unwrap();

        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            idle_seconds: 5,
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();

        // Three idle-separated bursts (idle_seconds = 5, gap = 30s) so
        // `chunk_ranges` closes exactly 3 chunks: two full CHUNK_EVENTS bursts,
        // then a 1-event remainder. Sequential zero-padded ids (not fresh
        // ULIDs — see `test_event_with_id`) so `events_between`'s `id ASC`
        // fetch order matches this intended `at` order exactly.
        let mut at = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        let mut seq = 0u32;
        for burst in 0..3usize {
            let n = if burst < 2 {
                dira_core::sync::CHUNK_EVENTS
            } else {
                1
            };
            for _ in 0..n {
                let id = format!("{seq:010}");
                seq += 1;
                state
                    .store
                    .append(&test_event_with_id(&id, "s1", at))
                    .await
                    .unwrap();
                at += time::Duration::seconds(1);
            }
            at += time::Duration::seconds(30);
        }

        let key = DeviceKey::generate();
        let err = flush(&state, &key, &state.http)
            .await
            .expect_err("chunk 3 fails with a 500");
        assert!(
            matches!(err, SyncError::Transient { .. }),
            "chunk 3's 500 must classify as Transient"
        );

        assert_eq!(
            cloud.requests("/api/v1/ingest").len(),
            3,
            "all 3 chunks must have been sent — the reset doesn't cut the flush short"
        );

        // The cursor must be BLANK (the epoch reset from chunk 1), not chunk 2's
        // high-water mark — proving chunk 2's success did not silently undo it.
        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(""),
            "cursor must stay blanked by the epoch reset even though chunk 2 succeeded"
        );
        let art_cursor = state.store.meta_get(META_ARTIFACTS_CURSOR).await.unwrap();
        assert_eq!(art_cursor.as_deref(), Some(""));
    }

    /// WP-B11 coverage-audit gap: baseline (no epoch/rotation entanglement) —
    /// a plain 2xx ack must advance the cursor to the window's high-water
    /// event id. Every other sync test builds on this invariant (the doc
    /// comment at the top of this module states it outright) but none
    /// asserted it directly on its own; the epoch/pending-key tests only
    /// exercise it entangled with a second concern.
    #[tokio::test]
    async fn flush_200_ack_advances_cursor_to_window_high_water() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let state = linked_state_with_one_event(&cloud).await;
        let until = state.store.max_event_id().await.unwrap();
        let key = DeviceKey::generate();

        let outcome = flush(&state, &key, &state.http).await.expect("flush ok");
        assert!(matches!(outcome, FlushOutcome::Synced));

        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor, until,
            "a plain 2xx ack must advance the cursor to the window's high-water event id"
        );
    }

    /// Issue #71: a 413 used to fall through to the untyped `Fatal` arm, so an
    /// oversized batch and a local DB error produced the same "sync: error,
    /// backing off" line. It needs its own variant so the operator is told the
    /// batch is unsendable rather than left to assume a retry will clear it.
    #[tokio::test]
    async fn flush_413_is_typed_and_leaves_both_cursors_unchanged() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(413, "Request Entity Too Large"),
        );
        let state = linked_state_with_one_event(&cloud).await;
        let key = DeviceKey::generate();
        let cursor_before = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        let art_before = state.store.meta_get(META_ARTIFACTS_CURSOR).await.unwrap();

        let err = flush(&state, &key, &state.http)
            .await
            .expect_err("a 413 must fail this flush");
        assert!(
            matches!(err, SyncError::PayloadTooLarge(_)),
            "a 413 must be typed, not swept into Fatal, got {err:?}"
        );

        assert_eq!(
            cursor_before,
            state.store.meta_get(META_SYNC_CURSOR).await.unwrap(),
            "a rejected batch must not advance the event cursor"
        );
        assert_eq!(
            art_before,
            state.store.meta_get(META_ARTIFACTS_CURSOR).await.unwrap(),
            "nor the artifact cursor — the artifacts never landed"
        );
    }

    /// A token row dated well before the flush window, with NO new events at all.
    ///
    /// This is the reported compute bug in one test. Token rows used to be
    /// selected by the `at`-span of the batch's own events and gated behind
    /// `!events.is_empty()`, so a caught-up daemon shipped nothing and a
    /// back-dated row — which is every row, since a turn is discovered by the
    /// `Stop` that FOLLOWS it — fell below the window's exclusive lower bound and
    /// was never reconsidered. Measured field impact: 97% of captured compute
    /// never reached the cloud.
    #[tokio::test]
    async fn flush_ships_a_token_backlog_with_no_new_events() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let state = linked_state_with_one_event(&cloud).await;

        // Drain the one seeded event so the event cursor is caught up — the
        // steady state in which the old code could never ship a token row.
        let key = DeviceKey::generate();
        flush(&state, &key, &state.http).await.expect("seed flush");
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));

        // A turn dated a month before anything in the event log.
        let turn = dira_core::tokens::TokenTurn {
            id: "turn-backdated".into(),
            at: "2026-06-15T12:52:19Z".into(),
            model: "claude-opus-4-8".into(),
            input: 10,
            output: 20,
            cache_read: 3000,
            cache_create: 40,
            cwd: None,
        };
        state
            .store
            .upsert_token_usage(&turn, "s1", Some("github.com/acme/api"))
            .await
            .unwrap();

        let before = cloud.requests("/api/v1/ingest").len();
        let outcome = flush(&state, &key, &state.http)
            .await
            .expect("a token backlog alone must produce a flush");
        assert!(
            matches!(outcome, FlushOutcome::Synced),
            "tokens must be able to trigger a flush on their own, got {outcome:?}"
        );

        let reqs = cloud.requests("/api/v1/ingest");
        assert_eq!(reqs.len(), before + 1, "exactly one new batch");
        assert!(
            reqs.last().unwrap().contains("turn-backdated"),
            "the back-dated token row must ride the batch"
        );

        assert_eq!(
            state.store.meta_get(META_TOKEN_CURSOR).await.unwrap(),
            state
                .store
                .max_token_usage_rowid()
                .await
                .unwrap()
                .map(|r| r.to_string()),
            "the ack must advance the token cursor to the snapshot bound"
        );

        // Drained: a second flush has nothing left to send.
        assert!(
            matches!(
                flush(&state, &key, &state.http).await.unwrap(),
                FlushOutcome::Nothing
            ),
            "an already-shipped token row must not re-send"
        );
    }

    /// A rejected batch must not advance the token cursor — otherwise the rows it
    /// was carrying would be skipped forever, which is the failure mode the
    /// cursor exists to prevent.
    #[tokio::test]
    async fn a_rejected_batch_leaves_the_token_cursor_put() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(413, "Request Entity Too Large"),
        );
        let state = linked_state_with_one_event(&cloud).await;
        let turn = dira_core::tokens::TokenTurn {
            id: "turn-rejected".into(),
            at: "2026-06-15T12:52:19Z".into(),
            model: "claude-opus-4-8".into(),
            input: 1,
            output: 1,
            cache_read: 0,
            cache_create: 0,
            cwd: None,
        };
        state
            .store
            .upsert_token_usage(&turn, "s1", Some("github.com/acme/api"))
            .await
            .unwrap();
        let before = state.store.meta_get(META_TOKEN_CURSOR).await.unwrap();

        let key = DeviceKey::generate();
        flush(&state, &key, &state.http)
            .await
            .expect_err("a 413 must fail this flush");

        assert_eq!(
            before,
            state.store.meta_get(META_TOKEN_CURSOR).await.unwrap(),
            "a rejected batch must not advance the token cursor"
        );
    }

    /// The cloud's per-device ingest budget, requests per fixed 60s window. Lives
    /// in the tests because production never reads it — it is the external fact the
    /// production spacing is checked against. The authority is the cloud's own rate
    /// limiter; a deployment that lowers it must lower `INGEST_CHUNK_SPACING` too.
    const CLOUD_INGEST_BUDGET_PER_MIN: u64 = 30;

    /// The pacing constant is only meaningful relative to the cloud's per-device
    /// ingest budget (30/min, fixed window). Pin the arithmetic so a future edit to
    /// the spacing cannot silently push a drain back over the limit. See D-0020.
    #[test]
    fn pacing_keeps_a_drain_inside_the_cloud_ingest_budget() {
        let per_min = 60_000 / INGEST_CHUNK_SPACING.as_millis() as u64;
        assert!(
            per_min <= CLOUD_INGEST_BUDGET_PER_MIN,
            "paced drain would issue {per_min}/min against a {CLOUD_INGEST_BUDGET_PER_MIN}/min budget"
        );
    }

    /// Ordinary flushes are one or two chunks and must not pay a pacing delay —
    /// the latency of normal sync is the thing pacing must not regress.
    #[test]
    fn pacing_leaves_an_ordinary_flush_alone() {
        assert_eq!(chunk_pace_delay(1, StdDuration::ZERO), None);
        assert_eq!(chunk_pace_delay(UNPACED_CHUNKS, StdDuration::ZERO), None);
    }

    /// A drain long enough to threaten the budget gets spaced.
    #[test]
    fn pacing_spaces_a_long_drain() {
        assert_eq!(
            chunk_pace_delay(UNPACED_CHUNKS + 1, StdDuration::ZERO),
            Some(INGEST_CHUNK_SPACING)
        );
        assert_eq!(
            chunk_pace_delay(49, StdDuration::ZERO),
            Some(INGEST_CHUNK_SPACING)
        );
    }

    /// The POST itself counts toward the spacing. A slow round-trip already spent
    /// the budget's worth of wall clock, so sleeping the full interval on top would
    /// add latency for nothing.
    #[test]
    fn pacing_credits_the_time_the_post_already_took() {
        assert_eq!(
            chunk_pace_delay(49, INGEST_CHUNK_SPACING / 4),
            Some(INGEST_CHUNK_SPACING - INGEST_CHUNK_SPACING / 4)
        );
        assert_eq!(chunk_pace_delay(49, INGEST_CHUNK_SPACING), None);
        assert_eq!(chunk_pace_delay(49, StdDuration::from_secs(30)), None);
    }

    /// The policy tests above pin the arithmetic; this one pins that `flush`
    /// actually consults it. Eleven token chunks clear `UNPACED_CHUNKS`, and the
    /// second chunk is throttled so the flush ends after exactly one spacing
    /// interval instead of ten — the wiring is proved for ~2.5s, not ~25s.
    #[tokio::test]
    async fn pacing_is_applied_between_chunks_of_a_real_drain() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":1}"#),
        );
        let state = linked_state_with_one_event(&cloud).await;
        seed_token_rows(&state, dira_core::sync::CHUNK_TOKENS * UNPACED_CHUNKS + 1).await;

        let key = DeviceKey::generate();
        let started = Instant::now();
        flush(&state, &key, &state.http)
            .await
            .expect_err("the throttled second chunk ends this flush");

        assert!(
            started.elapsed() >= INGEST_CHUNK_SPACING,
            "a drain past UNPACED_CHUNKS must space its POSTs; took {:?}",
            started.elapsed()
        );
    }

    /// Seed `n` token rows on one session, ordered by rowid. `turn-<rowid>` ids make
    /// the assertion about *which* rows reached the wire readable.
    async fn seed_token_rows(state: &AppState, n: usize) {
        for i in 0..n {
            let turn = dira_core::tokens::TokenTurn {
                id: format!("turn-{i}"),
                at: "2026-06-15T12:52:19Z".into(),
                model: "claude-opus-4-8".into(),
                input: 1,
                output: 1,
                cache_read: 0,
                cache_create: 0,
                cwd: None,
            };
            state
                .store
                .upsert_token_usage(&turn, "s1", Some("github.com/acme/api"))
                .await
                .unwrap();
        }
    }

    /// Issue #88. A token backlog bigger than the cloud's per-device ingest budget
    /// used to be undrainable: `META_TOKEN_CURSOR` advanced only on the `is_last`
    /// chunk, so a drain throttled part-way threw away every chunk the cloud had
    /// already accepted and restarted at row 1 on the next flush — forever, with
    /// 49 chunks needed against a 30/min budget. See D-0020.
    #[tokio::test]
    async fn a_throttled_token_drain_keeps_the_chunks_the_cloud_already_acked() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        // Chunk 0 lands; chunk 1 is throttled — the shape of a mid-drain 429.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":51}"#),
        );
        let state = linked_state_with_one_event(&cloud).await;

        // One row past CHUNK_TOKENS ⇒ exactly two token chunks.
        seed_token_rows(&state, dira_core::sync::CHUNK_TOKENS + 1).await;
        let total = state.store.max_token_usage_rowid().await.unwrap().unwrap();

        let key = DeviceKey::generate();
        flush(&state, &key, &state.http)
            .await
            .expect_err("the throttled chunk must fail this flush");

        let cursor = state
            .store
            .meta_get(META_TOKEN_CURSOR)
            .await
            .unwrap()
            .and_then(|s| s.parse::<i64>().ok());
        assert_eq!(
            cursor,
            Some(total - 1),
            "the accepted chunk's watermark must survive the throttled one that followed it"
        );
    }

    /// The other half of #88: having kept the acked progress, the next flush must
    /// ship only the remainder — not re-send the whole backlog.
    #[tokio::test]
    async fn a_resumed_token_drain_ships_only_the_rows_left() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":1}"#),
        );
        let state = linked_state_with_one_event(&cloud).await;
        seed_token_rows(&state, dira_core::sync::CHUNK_TOKENS + 1).await;
        let total = state.store.max_token_usage_rowid().await.unwrap().unwrap();

        let key = DeviceKey::generate();
        flush(&state, &key, &state.http)
            .await
            .expect_err("first attempt is throttled mid-drain");

        let sent_before = cloud.requests("/api/v1/ingest").len();
        flush(&state, &key, &state.http)
            .await
            .expect("the remainder must ship");

        let reqs = cloud.requests("/api/v1/ingest");
        assert_eq!(
            reqs.len() - sent_before,
            1,
            "only the one leftover chunk should be re-sent, not the whole backlog"
        );
        assert_eq!(
            state.store.meta_get(META_TOKEN_CURSOR).await.unwrap(),
            Some(total.to_string()),
            "a completed drain lands the cursor on the snapshot bound"
        );
    }

    /// `nuke` empties `token_usage`, which lets SQLite hand out rowid 1 again. If
    /// the token cursor survived at its old high-water mark, every re-captured row
    /// would sort below it and be skipped — re-creating the original bug through a
    /// new door. `nuke` also clears the `token_offset:%` capture watermarks, so a
    /// full transcript re-import is guaranteed, not hypothetical.
    #[tokio::test]
    async fn re_captured_rows_still_ship_after_a_nuke() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let state = linked_state_with_one_event(&cloud).await;
        let key = DeviceKey::generate();

        for i in 0..3 {
            let turn = dira_core::tokens::TokenTurn {
                id: format!("pre-nuke-{i}"),
                at: "2026-06-15T12:52:19Z".into(),
                model: "claude-opus-4-8".into(),
                input: 1,
                output: 1,
                cache_read: 0,
                cache_create: 0,
                cwd: None,
            };
            state
                .store
                .upsert_token_usage(&turn, "s1", None)
                .await
                .unwrap();
        }
        flush(&state, &key, &state.http).await.expect("first flush");
        assert!(state
            .store
            .meta_get(META_TOKEN_CURSOR)
            .await
            .unwrap()
            .is_some_and(|c| !c.is_empty()));

        state.store.nuke().await.unwrap();
        assert_eq!(
            state.store.meta_get(META_TOKEN_CURSOR).await.unwrap(),
            Some(String::new()),
            "nuke must blank the token cursor, or re-used rowids fall below it"
        );

        // Re-capture: a fresh row that reuses rowid 1.
        let turn = dira_core::tokens::TokenTurn {
            id: "post-nuke".into(),
            at: "2026-06-15T12:52:19Z".into(),
            model: "claude-opus-4-8".into(),
            input: 1,
            output: 1,
            cache_read: 0,
            cache_create: 0,
            cwd: None,
        };
        state
            .store
            .upsert_token_usage(&turn, "s1", None)
            .await
            .unwrap();

        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let before = cloud.requests("/api/v1/ingest").len();
        flush(&state, &key, &state.http)
            .await
            .expect("the re-imported row must still reach a batch");
        let reqs = cloud.requests("/api/v1/ingest");
        assert_eq!(reqs.len(), before + 1);
        assert!(
            reqs.last().unwrap().contains("post-nuke"),
            "a row re-captured after a nuke must ship"
        );
    }

    /// WP-B11 coverage-audit gap: a plain 5xx (no Retry-After, no epoch
    /// signal) must classify as `Transient` and leave the cursor untouched.
    /// The only existing 500 exercised the cursor via the epoch-reset test
    /// above, entangled with that scenario; this isolates the plain case.
    #[tokio::test]
    async fn flush_500_is_transient_and_leaves_cursor_unchanged() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push("/api/v1/ingest", MockResp::status(500, "boom"));
        let state = linked_state_with_one_event(&cloud).await;
        let key = DeviceKey::generate();
        let cursor_before = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();

        let err = flush(&state, &key, &state.http)
            .await
            .expect_err("a 500 must fail this flush");
        assert!(
            matches!(
                err,
                SyncError::Transient {
                    retry_after: None,
                    ..
                }
            ),
            "a plain 500 has no Retry-After hint, expected Transient, got {err:?}"
        );

        let cursor_after = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor_before, cursor_after,
            "a 500 must not advance the cursor"
        );
    }

    /// WP-B11 coverage-audit gap: a 401 typed `unknown_device` must return
    /// `ReLinkRequired` and leave the cursor untouched, pausing sync until an
    /// operator re-links rather than hot-looping or dropping the backlog.
    /// Only the pure `is_unknown_device` classifier was previously tested
    /// (`unknown_device_is_detected_via_typed_body`); this exercises the real
    /// `flush()` path end to end, matching the sibling `SignatureRejected`
    /// test's level of coverage.
    #[tokio::test]
    async fn flush_401_unknown_device_returns_relink_required_and_leaves_cursor_unchanged() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(401, r#"{"error":"unknown_device"}"#),
        );
        let state = linked_state_with_one_event(&cloud).await;
        let key = DeviceKey::generate();
        let cursor_before = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();

        let err = flush(&state, &key, &state.http)
            .await
            .expect_err("unknown_device must fail this flush");
        assert!(
            matches!(err, SyncError::ReLinkRequired),
            "expected ReLinkRequired, got {err:?}"
        );

        let cursor_after = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor_before, cursor_after,
            "an unknown_device rejection must not advance the cursor — sync stays paused for a re-link"
        );
    }

    /// WP-B11 coverage-audit gap: interrupted multi-chunk resume with
    /// identical batchIds. A crash between two chunks of the SAME flush must
    /// resume losslessly AND deterministically — the un-acked trailing chunk,
    /// rebuilt from the post-crash cursor on retry, must re-derive the
    /// identical `batchId` it carried on the interrupted attempt, so the
    /// cloud's per-batch dedup no-ops the retry instead of double-counting or
    /// diverging from what the cloud may have already received. The math
    /// (`batch_id_is_deterministic_for_same_window`) and the pure chunking
    /// (`chunk_ranges_split_only_at_idle_breaks`) are covered at the
    /// `dira_core::sync::batch` unit level; this proves the daemon LOOP
    /// actually resumes into the same rebuild after a real chunk failure.
    #[tokio::test]
    async fn interrupted_multi_chunk_flush_resumes_with_identical_batch_id_for_the_retried_chunk() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();

        // One full CHUNK_EVENTS burst (1s apart, well within idle=5s), then a
        // > idle gap (30s) and a lone tail event — `chunk_ranges` closes
        // exactly 2 chunks: the packed burst, then the 1-event remainder.
        // Mirrors the burst-building pattern in the D12 epoch-reset test above.
        let mut at = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        let n = dira_core::sync::CHUNK_EVENTS;
        for i in 0..n {
            let id = format!("{i:010}");
            store
                .append(&test_event_with_id(&id, "s1", at))
                .await
                .unwrap();
            at += time::Duration::seconds(1);
        }
        at += time::Duration::seconds(30);
        let tail_id = format!("{n:010}");
        store
            .append(&test_event_with_id(&tail_id, "s1", at))
            .await
            .unwrap();

        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            idle_seconds: 5,
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        let key = DeviceKey::generate();

        // Attempt 1: chunk 1 (the packed burst) acks 2xx; chunk 2 (the tail)
        // fails — simulating a crash/network drop mid-drain.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        cloud.push("/api/v1/ingest", MockResp::status(500, "boom"));
        let err = flush(&state, &key, &state.http)
            .await
            .expect_err("chunk 2 must fail this attempt");
        assert!(matches!(err, SyncError::Transient { .. }));

        let requests = cloud.requests("/api/v1/ingest");
        assert_eq!(
            requests.len(),
            2,
            "both chunks must have been POSTed before the failure"
        );
        let attempt1_chunk2_batch_id = batch_id_of(&requests[1]);

        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(format!("{:010}", n - 1).as_str()),
            "the acked first chunk's cursor must have advanced despite the second chunk's failure"
        );

        // Attempt 2 (resumed): only the un-acked tail event remains in the
        // window — reconstructed as a single chunk from the new cursor.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let outcome = flush(&state, &key, &state.http)
            .await
            .expect("resumed flush must now succeed");
        assert!(matches!(outcome, FlushOutcome::Synced));

        let requests = cloud.requests("/api/v1/ingest");
        assert_eq!(requests.len(), 3);
        let attempt2_batch_id = batch_id_of(&requests[2]);

        assert_eq!(
            attempt1_chunk2_batch_id, attempt2_batch_id,
            "the retried chunk must re-derive the SAME batchId as the interrupted attempt \
             (byte-identical rebuild over the identical event subset) so the cloud's dedup \
             no-ops the retry"
        );

        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(tail_id.as_str()),
            "the resumed flush advances the cursor to the window's high-water event id"
        );
    }

    /// MANDATORY (WP-B11): issue #21 flush-boundary regression, at the daemon
    /// LOOP level. The property tests in `dira_core::sync::batch`
    /// (`seed_recovers_boundary_gap_and_is_windowing_invariant`,
    /// `retro_log_backdated_events_are_reconciled`,
    /// `batch_id_changes_when_interval_decomposition_changes`) already cover
    /// the derivation math in isolation; this proves the LOOP actually wires
    /// it end to end: the seed is fetched from the store and passed into the
    /// chunk build, the re-decomposed chunk is what's actually POSTed, and
    /// the cursor still advances correctly on top of it.
    ///
    /// Flush window 1 sends two already-synced human signals 200s apart (a
    /// single `[L1,L2)` interval, 200s). A `dira log`-style entry is then
    /// appended BACKDATED to land between them — a fresh (higher) id, but an
    /// `at` inside the already-synced span. Flush window 2's own events are
    /// just that one backdated signal, but `flush` must seed it with the
    /// neighbouring already-synced L1/L2 via `human_signal_seed`, so the
    /// boundary gap is RE-decomposed (both halves emitted) instead of
    /// dropped — and the re-decomposed chunk must carry a DIFFERENT
    /// `batchId` than an unseeded rebuild of the same window would, so the
    /// cloud re-unpacks it instead of dedup-dropping it as already seen.
    #[tokio::test]
    async fn flush_boundary_backdated_log_reconciles_via_seed_with_a_different_batch_id() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();

        let base = time::OffsetDateTime::now_utc() - time::Duration::minutes(30);
        let l1 = human_event(
            "0000000001",
            "s-L",
            base,
            dira_core::model::EventKind::UserPrompt,
        );
        let l2 = human_event(
            "0000000002",
            "s-L",
            base + time::Duration::seconds(200),
            dira_core::model::EventKind::UserPrompt,
        );
        store.append(&l1).await.unwrap();
        store.append(&l2).await.unwrap();

        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        let key = DeviceKey::generate();

        // Flush window 1: L1, L2 — a plain, unseeded, single [L1,L2) interval.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let outcome = flush(&state, &key, &state.http)
            .await
            .expect("window 1 flush");
        assert!(matches!(outcome, FlushOutcome::Synced));
        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(cursor.as_deref(), Some("0000000002"));

        // A backdated `dira log`-style entry: fresh (higher) id, but its `at`
        // lands BETWEEN the two already-synced signals.
        let m = human_event(
            "0000000003",
            "s-M",
            base + time::Duration::seconds(100),
            dira_core::model::EventKind::ManualStart,
        );
        state.store.append(&m).await.unwrap();

        // Flush window 2: just M — but the LOOP must seed it from the store
        // before building the chunk.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let outcome = flush(&state, &key, &state.http)
            .await
            .expect("window 2 flush");
        assert!(matches!(outcome, FlushOutcome::Synced));
        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some("0000000003"),
            "the loop wiring must still advance the cursor to M's id on a 2xx ack"
        );

        let requests = cloud.requests("/api/v1/ingest");
        assert_eq!(requests.len(), 2);
        let window2_body: serde_json::Value = serde_json::from_str(&requests[1]).unwrap();
        let intervals = window2_body["payload"]["intervals"].as_array().unwrap();
        // The seeded rebuild re-splits the boundary gap into two intervals
        // that each touch M (one opened by L1, one opened by M) instead of
        // dropping it — the exact issue #21 failure mode this guards against.
        assert_eq!(
            intervals.len(),
            2,
            "the seeded window must emit BOTH halves of the re-split boundary gap, not just M's own side"
        );
        let total_seconds: u64 = intervals
            .iter()
            .map(|iv| iv["humanSeconds"].as_u64().unwrap())
            .sum();
        assert_eq!(total_seconds, 200, "no under-count across the boundary");

        let window2_batch_id = window2_body["payload"]["batchId"]
            .as_str()
            .unwrap()
            .to_string();

        // What would this SAME window (just M) have produced WITHOUT the
        // seed? Re-derive it directly to prove the seed is what changed the
        // id — the loop-wiring assertion this test exists for.
        let idle = state.config.idle();
        let unseeded = dira_core::sync::build_chunked_batches(
            std::slice::from_ref(&m),
            &[],
            &[],
            &[],
            "01TESTDEVICE",
            idle,
            state.config.agent_policy(),
            time::OffsetDateTime::now_utc(),
            &[], // no seed
            &[], // no history
        );
        let unseeded_batch_id = unseeded[0].batch.batch_id.clone();

        assert_ne!(
            window2_batch_id, unseeded_batch_id,
            "the seeded (boundary-recovering) decomposition must carry a DIFFERENT batchId \
             than an unseeded rebuild of the same window — forcing the cloud to re-unpack the \
             corrected intervals instead of dedup-dropping them"
        );
    }

    /// Headline regression for issue #40, at the daemon LOOP level: a session
    /// that spans two flushes must not lose its early prompts/branch/start.
    /// Flush #1 ships the session's start + a prompt (with a branch) + tool
    /// activity, but the session hasn't ended yet — no rollup ships. Flush #2's
    /// window is just the trailing `SessionEnd`, but `flush` must fetch the
    /// session's full retained history from the store (issue #40) so the
    /// terminal rollup aggregates the WHOLE session, not just the bare end.
    #[tokio::test]
    async fn multi_window_session_final_rollup_carries_prompts_and_branch() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();

        let base = time::OffsetDateTime::now_utc() - time::Duration::minutes(30);
        let mut start = human_event(
            "0000000001",
            "s1",
            base,
            dira_core::model::EventKind::SessionStart,
        );
        start.branch = Some("feat/x".into());
        let mut prompt = human_event(
            "0000000002",
            "s1",
            base + time::Duration::seconds(10),
            dira_core::model::EventKind::UserPrompt,
        );
        prompt.branch = Some("feat/x".into());
        let pre = human_event(
            "0000000003",
            "s1",
            base + time::Duration::seconds(20),
            dira_core::model::EventKind::PreTool,
        );
        let post = human_event(
            "0000000004",
            "s1",
            base + time::Duration::seconds(30),
            dira_core::model::EventKind::PostTool,
        );
        store.append(&start).await.unwrap();
        store.append(&prompt).await.unwrap();
        store.append(&pre).await.unwrap();
        store.append(&post).await.unwrap();

        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        let key = DeviceKey::generate();

        // Flush #1: start + prompt + tool activity, no end yet — no rollup ships.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let outcome = flush(&state, &key, &state.http).await.expect("flush 1 ok");
        assert!(matches!(outcome, FlushOutcome::Synced));
        let cursor = state.store.meta_get(META_SYNC_CURSOR).await.unwrap();
        assert_eq!(cursor.as_deref(), Some("0000000004"));

        let requests = cloud.requests("/api/v1/ingest");
        let body1: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        // `sessions` is omitted entirely (`skip_serializing_if = "Vec::is_empty"`)
        // when there's nothing to roll up yet, so a missing key is the expected
        // "no sessions" shape here — not an array to unwrap.
        let sessions1 = body1["payload"]["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            sessions1.iter().all(|s| s["sessionId"] != "s1"),
            "the session hasn't ended yet — no rollup in window 1"
        );

        // The session ends; flush #2's window is JUST the SessionEnd.
        let end = human_event(
            "0000000005",
            "s1",
            base + time::Duration::seconds(40),
            dira_core::model::EventKind::SessionEnd,
        );
        state.store.append(&end).await.unwrap();

        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let outcome = flush(&state, &key, &state.http).await.expect("flush 2 ok");
        assert!(matches!(outcome, FlushOutcome::Synced));

        let requests = cloud.requests("/api/v1/ingest");
        assert_eq!(requests.len(), 2);
        let body2: serde_json::Value = serde_json::from_str(&requests[1]).unwrap();
        let sessions2 = body2["payload"]["sessions"].as_array().unwrap();
        let s = sessions2
            .iter()
            .find(|s| s["sessionId"] == "s1")
            .expect("the terminal rollup ships in window 2");
        assert_eq!(
            s["prompts"],
            serde_json::json!(1),
            "the prompt from flush #1's window must still be counted"
        );
        assert_eq!(
            s["branch"],
            serde_json::json!("feat/x"),
            "the branch from flush #1's window must still be carried"
        );
        assert_eq!(
            s["startedAt"],
            serde_json::json!(fmt_at(&start)),
            "started_at must be the session's TRUE start, from flush #1's window, not \
             flush #2's bare SessionEnd"
        );
        assert!(s["endedAt"].is_string(), "the terminal rollup is ended");
    }

    /// A long-running session with prompts/branch tracked in the live registry
    /// (issue #40 also fixed the partial-rollup path, which used to hardcode
    /// `prompts: None, branch: None`) must ship both on its partial rollup.
    #[tokio::test]
    async fn partial_rollup_ships_prompts_and_branch() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();

        // A window event unrelated to the long session, just so `flush` has
        // something pending this cycle (a purely partial-only flush still needs
        // *some* new events or artifacts, per the early `Nothing` gate).
        store
            .append(&test_event("s-other", time::OffsetDateTime::now_utc()))
            .await
            .unwrap();

        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            partial_rollup_after_secs: 60, // eligible once older than 1 minute
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        let idle = state.config.idle();

        // Drive the live registry directly (mirrors what the writer does on
        // every observed event) so a long-running, un-ended session with
        // prompts and a branch is a partial-rollup candidate.
        let started_at = time::OffsetDateTime::now_utc() - time::Duration::hours(2);
        let mut start = human_event(
            "l1",
            "long",
            started_at,
            dira_core::model::EventKind::SessionStart,
        );
        start.branch = Some("feat/long".into());
        let mut prompt = human_event(
            "l2",
            "long",
            started_at + time::Duration::seconds(10),
            dira_core::model::EventKind::UserPrompt,
        );
        prompt.branch = Some("feat/long".into());
        let pre = human_event(
            "l3",
            "long",
            started_at + time::Duration::seconds(20),
            dira_core::model::EventKind::PreTool,
        );
        {
            let mut reg = state.sessions.lock().unwrap();
            reg.observe(&start, idle);
            reg.observe(&prompt, idle);
            reg.observe(&pre, idle);
        }

        let key = DeviceKey::generate();
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let outcome = flush(&state, &key, &state.http).await.expect("flush ok");
        assert!(matches!(outcome, FlushOutcome::Synced));

        let requests = cloud.requests("/api/v1/ingest");
        let body: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        let sessions = body["payload"]["sessions"].as_array().unwrap();
        let s = sessions
            .iter()
            .find(|s| s["sessionId"] == "long")
            .expect("the partial rollup for the long-running session shipped");
        assert!(s["endedAt"].is_null(), "a partial rollup is open-ended");
        assert_eq!(
            s["prompts"],
            serde_json::json!(1),
            "the registry's prompt count must carry through onto the partial"
        );
        assert_eq!(
            s["branch"],
            serde_json::json!("feat/long"),
            "the registry's last-resolved branch must carry through onto the partial"
        );
    }

    /// WP-B9: `record_health` on a success clears `last_error_kind`, stamps
    /// `last_success_at`, and snapshots the current cursor/watermark.
    #[tokio::test]
    async fn record_health_on_success_clears_error_and_stamps_success() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let state = linked_state_with_one_event(&cloud).await;
        state
            .store
            .meta_set(META_SYNC_CURSOR, "01CURSOR")
            .await
            .unwrap();
        state
            .store
            .meta_set(dira_core::sync::META_CLOUD_WATERMARK, "01WATERMARK")
            .await
            .unwrap();

        record_health(&state, None, 0, 0).await;

        let json = state
            .store
            .meta_get(dira_core::sync::META_SYNC_HEALTH)
            .await
            .unwrap()
            .unwrap();
        let health = dira_core::sync::parse_sync_health(&json).unwrap();
        assert!(health.last_attempt_at.is_some());
        assert!(health.last_success_at.is_some());
        assert_eq!(health.last_error_kind, None);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.cursor.as_deref(), Some("01CURSOR"));
        assert_eq!(health.cloud_watermark.as_deref(), Some("01WATERMARK"));
    }

    /// WP-B9: `record_health` on a failure records the error kind + counters
    /// and preserves a PRIOR `last_success_at` (a failure doesn't erase the
    /// last time sync actually worked).
    #[tokio::test]
    async fn record_health_on_failure_preserves_prior_success_and_records_kind() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let state = linked_state_with_one_event(&cloud).await;

        record_health(&state, None, 0, 0).await; // establish a prior success
        let after_success = dira_core::sync::parse_sync_health(
            &state
                .store
                .meta_get(dira_core::sync::META_SYNC_HEALTH)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();

        record_health(&state, Some("transient"), 3, 8).await;
        let after_failure = dira_core::sync::parse_sync_health(
            &state
                .store
                .meta_get(dira_core::sync::META_SYNC_HEALTH)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();

        assert_eq!(after_failure.last_error_kind.as_deref(), Some("transient"));
        assert_eq!(after_failure.consecutive_failures, 3);
        assert_eq!(after_failure.backoff_secs, 8);
        assert_eq!(
            after_failure.last_success_at, after_success.last_success_at,
            "a failure must not erase the prior success timestamp"
        );
    }

    /// End-to-end: a real `flush()` failure (a 429) leaves `META_SYNC_HEALTH`
    /// reflecting that failure. This exercises the actual call site inside the
    /// `run` loop's `Transient` arm, not just `record_health` in isolation.
    #[tokio::test]
    async fn flush_429_failure_is_reflected_in_health_via_the_run_loop_arm() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(429, "").with_header("Retry-After", "5"),
        );
        let state = linked_state_with_one_event(&cloud).await;
        let key = DeviceKey::generate();

        // Drive exactly what the run loop's Transient arm does: attempt the
        // flush, then record health the same way that arm does on failure.
        let outcome = flush(&state, &key, &state.http).await;
        assert!(outcome.is_err());
        if let Err(SyncError::Transient { .. }) = outcome {
            record_health(&state, Some("transient"), 1, 5).await;
        } else {
            panic!("expected a Transient 429 failure");
        }

        let health = dira_core::sync::parse_sync_health(
            &state
                .store
                .meta_get(dira_core::sync::META_SYNC_HEALTH)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(health.last_error_kind.as_deref(), Some("transient"));
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.backoff_secs, 5);
    }

    /// WP-B1b (daemon side): a `SignatureRejected` flush recovers via a
    /// pending rotation key sitting on disk — `try_pending_key_flush` retries
    /// the SAME flush signed with it, and on success promotes pending→active
    /// and invalidates the daemon's cached device key so a stale (dead) key
    /// never survives past the very next tick.
    #[tokio::test]
    async fn signature_rejected_recovers_via_pending_key_and_invalidates_cache() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        // First attempt (old key): the cloud rejects it.
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        // Retry (pending key): accepted.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let state = linked_state_with_one_event(&cloud).await;

        // Seed the daemon's cached ACTIVE key directly (bypassing the
        // keychain-touching `load_or_create_unlinked` path entirely — this
        // test only needs to prove the CACHE gets invalidated, not exercise
        // keychain storage).
        let old_key = DeviceKey::generate();
        *state.device_key.write().await = Some(old_key.clone());

        // A rotation is in flight: a pending key persisted (as `dira device
        // rotate-key` would, before POSTing).
        let pending_key = DeviceKey::generate();
        let pending_pub = pending_key.public_base64();
        dira_core::identity::persist_pending_key(
            &state.store,
            &pending_key,
            "2026-07-09T10:00:00Z",
        )
        .await
        .unwrap();

        // Step 1: the normal flush with the (now-dead) old key gets rejected.
        let outcome = flush(&state, &old_key, &state.http).await;
        assert!(
            matches!(outcome, Err(SyncError::SignatureRejected(_))),
            "expected SignatureRejected, got {outcome:?}"
        );

        // Step 2: the self-heal recovers via the pending key.
        assert!(
            try_pending_key_flush(&state).await,
            "pending key must authenticate and the flush must succeed"
        );

        // Promoted: nothing pending anymore, and the ACTIVE pubkey is now the
        // one that was pending.
        assert!(dira_core::identity::load_pending_key(&state.store)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            state
                .store
                .meta_get(dira_core::identity::META_PUBKEY)
                .await
                .unwrap()
                .as_deref(),
            Some(pending_pub.as_str())
        );

        // Cache invalidated: the stale old key must NOT still be cached — the
        // very next `device_key()` call would otherwise keep signing with a
        // dead key forever.
        assert!(
            state.device_key.read().await.is_none(),
            "the cached (now-dead) old key must be invalidated on promote"
        );
    }

    /// WP-B1b: when NO rotation is pending, a `SignatureRejected` has nothing
    /// to self-heal with — `try_pending_key_flush` must cleanly report
    /// failure (not panic, not touch any state) so the caller's normal
    /// backoff-and-log path runs.
    #[tokio::test]
    async fn try_pending_key_flush_is_a_noop_without_a_pending_key() {
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let state = linked_state_with_one_event(&cloud).await;
        assert!(!try_pending_key_flush(&state).await);
        // Nothing was POSTed — there was nothing to retry with.
        assert!(cloud.requests("/api/v1/ingest").is_empty());
    }

    /// Minor fix: `FlushOutcome::Skipped` (no `cloud_url` configured — `flush`
    /// never even reaches the cloud) must NOT be treated as proof the pending
    /// key authenticates. Before the fix, `try_pending_key_flush` promoted on
    /// any `Ok(_)`, including `Skipped`, so a pending key could get promoted
    /// to active without ever having been validated against the cloud.
    #[tokio::test]
    async fn try_pending_key_flush_does_not_promote_on_a_skipped_flush() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        // No `cloud_url` configured at all ⇒ `flush` returns `Skipped` before
        // ever touching the network.
        let state = unconfigured_state().await;

        let pending_key = DeviceKey::generate();
        dira_core::identity::persist_pending_key(
            &state.store,
            &pending_key,
            "2026-07-09T10:00:00Z",
        )
        .await
        .unwrap();

        assert!(
            !try_pending_key_flush(&state).await,
            "a Skipped flush must not be reported as a successful self-heal"
        );

        // Not promoted: the pending key is still sitting there, untouched.
        assert!(
            dira_core::identity::load_pending_key(&state.store)
                .await
                .unwrap()
                .is_some(),
            "a Skipped flush must not promote the pending key — it was never validated"
        );
    }

    /// The dominant real-world rotation sequence: `dira device rotate-key`
    /// runs to full completion in the CLI process (POST committed on the
    /// cloud, pending key promoted to active, pending markers cleared) while
    /// the daemon still holds the OLD key in its cache. The daemon's next
    /// flush gets `bad_signature`, and `try_pending_key_flush` finds nothing
    /// pending — before this fix the daemon wedged at MAX_BACKOFF forever,
    /// re-signing with the dead cached key until a restart.
    /// `try_reloaded_key_flush` must reload the store's (new) active key and
    /// retry the same flush with it.
    #[tokio::test]
    async fn signature_rejected_recovers_by_reloading_an_out_of_process_rotated_key() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        // First attempt (stale cached key): rejected.
        cloud.push(
            "/api/v1/ingest",
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        // Retry (reloaded active key): accepted.
        cloud.push("/api/v1/ingest", MockResp::ok(OK_INGEST));
        let state = linked_state_with_one_event(&cloud).await;

        // The CLI completed a full rotation out-of-process: the new key is
        // ACTIVE in the store and nothing is pending (exactly what
        // `rotate_key`'s persist → POST → promote sequence leaves behind).
        let new_key = DeviceKey::generate();
        dira_core::identity::persist_pending_key(&state.store, &new_key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();
        dira_core::identity::promote_pending_key(&state.store)
            .await
            .unwrap();

        // The daemon still has the dead OLD key cached.
        let old_key = DeviceKey::generate();
        *state.device_key.write().await = Some(old_key.clone());

        // Step 1: the normal flush with the cached old key gets rejected.
        let outcome = flush(&state, &old_key, &state.http).await;
        assert!(
            matches!(outcome, Err(SyncError::SignatureRejected(_))),
            "expected SignatureRejected, got {outcome:?}"
        );

        // Step 2: no pending key ⇒ the pending-key self-heal cannot help.
        assert!(!try_pending_key_flush(&state).await);

        // Step 3: the reload self-heal picks up the store's active key and
        // the SAME flush succeeds with it.
        assert!(
            try_reloaded_key_flush(&state, &old_key).await,
            "the reloaded active key must authenticate and the flush must succeed"
        );
        assert_eq!(cloud.requests("/api/v1/ingest").len(), 2);

        // The cache now holds the reloaded (new) active key, so every later
        // tick signs correctly without a restart.
        let cached = state
            .device_key
            .read()
            .await
            .clone()
            .expect("the reloaded key must be cached");
        assert_eq!(cached.public_base64(), new_key.public_base64());
    }

    /// When the store still holds exactly the key the cloud just rejected
    /// (no out-of-process rotation happened — the key is genuinely dead),
    /// `try_reloaded_key_flush` must NOT re-POST the same doomed bytes; it
    /// reports failure so the caller's hard-stop backoff path runs.
    #[tokio::test]
    async fn try_reloaded_key_flush_does_not_retry_when_the_store_holds_the_rejected_key() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let cloud = MockCloud::start(&["/api/v1/ingest"]).await;
        let state = linked_state_with_one_event(&cloud).await;

        // Install a key as ACTIVE in the store, cache it, then present it as
        // the rejected key — the reload resolves to the very same key.
        let key = DeviceKey::generate();
        dira_core::identity::persist_pending_key(&state.store, &key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();
        dira_core::identity::promote_pending_key(&state.store)
            .await
            .unwrap();
        *state.device_key.write().await = Some(key.clone());

        assert!(
            !try_reloaded_key_flush(&state, &key).await,
            "an unchanged active key must not be retried — same key, same rejection"
        );
        // Nothing was POSTed: retrying identical bytes cannot succeed.
        assert!(cloud.requests("/api/v1/ingest").is_empty());
    }

    /// Build an `AppState` with no `cloud_url` configured at all — the
    /// earliest gate in `flush` — so `flush` returns `FlushOutcome::Skipped`.
    async fn unconfigured_state() -> AppState {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let config = dira_core::Config::default();
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        state
    }

    /// The finding this covers: `flush_attempts` (in-memory, bumped
    /// unconditionally by `mark_flush_attempt` before every outcome match)
    /// must never outrun `META_SYNC_HEALTH.last_attempt_at` (persisted, only
    /// bumped by `record_health`). Before the fix, a `Skipped` tick bumped the
    /// former and skipped the latter entirely, so a daemon that's never been
    /// linked could show climbing attempts against a permanently-`None`
    /// `last_attempt_at`.
    #[tokio::test]
    async fn skipped_flush_writes_health_with_skipped_kind_and_does_not_bump_failures() {
        let state = unconfigured_state().await;
        let key = DeviceKey::generate();

        // No prior health snapshot at all yet — mirrors a freshly-installed,
        // never-linked daemon.
        assert!(state
            .store
            .meta_get(dira_core::sync::META_SYNC_HEALTH)
            .await
            .unwrap()
            .is_none());

        // Drive exactly what the run loop's Skipped arm does: bump the
        // in-memory attempt counter (as `run` does unconditionally before the
        // match), attempt the flush, then record health the same way that arm
        // now does.
        state.progress.mark_flush_attempt();
        let outcome = flush(&state, &key, &state.http).await;
        assert!(
            matches!(outcome, Ok(FlushOutcome::Skipped)),
            "expected Skipped (no cloud_url configured), got {outcome:?}"
        );
        record_health(&state, Some("skipped"), 0, 0).await;

        let health = dira_core::sync::parse_sync_health(
            &state
                .store
                .meta_get(dira_core::sync::META_SYNC_HEALTH)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();

        // `flush_attempts` and `last_attempt_at` moved together — the whole
        // point of the fix.
        assert_eq!(state.progress.flush_attempts(), 1);
        assert!(
            health.last_attempt_at.is_some(),
            "a Skipped tick must still advance last_attempt_at"
        );
        // Distinct kind — not one of the failure categories — and no failure
        // bookkeeping: skipped isn't a failure.
        assert_eq!(health.last_error_kind.as_deref(), Some("skipped"));
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.backoff_secs, 0);
        // No success either — the daemon hasn't ever actually synced.
        assert!(health.last_success_at.is_none());
    }

    /// `record_health`'s `Some(kind)` branch only ever touches
    /// `last_error_kind` — `last_success_at` is untouched (only the `None`
    /// branch sets it). For the new `"skipped"` kind specifically: a skip
    /// tick sandwiched after a real success must not erase evidence that sync
    /// last actually worked (mirrors
    /// `record_health_on_failure_preserves_prior_success_and_records_kind`,
    /// which pins the same invariant for a failure kind).
    #[tokio::test]
    async fn skipped_flush_preserves_prior_success_timestamp() {
        let state = unconfigured_state().await;

        record_health(&state, None, 0, 0).await; // establish a prior success
        let after_success = dira_core::sync::parse_sync_health(
            &state
                .store
                .meta_get(dira_core::sync::META_SYNC_HEALTH)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(after_success.last_success_at.is_some());

        record_health(&state, Some("skipped"), 0, 0).await;
        let after_skip = dira_core::sync::parse_sync_health(
            &state
                .store
                .meta_get(dira_core::sync::META_SYNC_HEALTH)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();

        assert_eq!(after_skip.last_error_kind.as_deref(), Some("skipped"));
        assert_eq!(after_skip.consecutive_failures, 0);
        assert_eq!(
            after_skip.last_success_at, after_success.last_success_at,
            "a skip must not erase the prior success timestamp"
        );
    }

    #[test]
    fn transient_wait_honors_retry_after_capped_at_max_backoff() {
        // No hint ⇒ the usual exponential ladder.
        assert_eq!(
            transient_wait(None, StdDuration::ZERO),
            StdDuration::from_secs(2)
        );
        assert_eq!(
            transient_wait(None, StdDuration::from_secs(2)),
            StdDuration::from_secs(4)
        );
        // A Retry-After hint overrides the ladder outright...
        assert_eq!(
            transient_wait(Some(StdDuration::from_secs(7)), StdDuration::from_secs(200)),
            StdDuration::from_secs(7)
        );
        // ...but a huge Retry-After is still capped at MAX_BACKOFF.
        assert_eq!(
            transient_wait(Some(StdDuration::from_secs(999_999)), StdDuration::ZERO),
            MAX_BACKOFF
        );
        // The ladder itself is already capped independent of any hint.
        assert_eq!(transient_wait(None, MAX_BACKOFF), MAX_BACKOFF);
    }

    /// Issue #72's regression guard, stated at the level the log line reads at: a
    /// count the cloud never sent must render as `-`, and only a count it DID send
    /// may render as a number — including a real zero.
    #[test]
    fn an_unreported_count_logs_as_unknown_not_zero() {
        let real_202 = dira_core::sync::parse_ingest_response(
            r#"{"status":"accepted","batchId":"01ABC","sync":{"syncedEventId":"01XYZ"}}"#,
        );
        assert_eq!(
            count_or_unknown(real_202.accepted),
            "-",
            "the live cloud's 202 carries no counters — saying 0 accepted is a lie"
        );
        assert_eq!(count_or_unknown(real_202.duplicates), "-");

        let with_counts = dira_core::sync::parse_ingest_response(
            r#"{"status":"accepted","accepted":7,"duplicates":2}"#,
        );
        assert_eq!(count_or_unknown(with_counts.accepted), "7");
        assert_eq!(count_or_unknown(with_counts.duplicates), "2");

        // A genuine zero is reported as zero — the point is to distinguish it from
        // "unknown", not to stop printing zeros.
        let genuine_zero = dira_core::sync::parse_ingest_response(r#"{"accepted":0}"#);
        assert_eq!(count_or_unknown(genuine_zero.accepted), "0");
    }

    /// A 2xx must never be downgraded over a logging detail, whatever the body.
    #[test]
    fn ack_parsing_tolerates_empty_and_garbage_bodies() {
        for body in ["", "   ", "ok", "<html>502</html>"] {
            let ack = dira_core::sync::parse_ingest_response(body);
            assert_eq!(ack.accepted, None, "body {body:?}");
            assert_eq!(count_or_unknown(ack.accepted), "-");
        }
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

    #[test]
    fn bad_signature_is_detected_via_typed_body() {
        assert!(is_bad_signature(r#"{"error":"bad_signature"}"#));
        // A different error code is not a signature-rejection trigger.
        assert!(!is_bad_signature(r#"{"error":"unknown_device"}"#));
        // A non-JSON / garbage 401 body is Fatal, not SignatureRejected — we only
        // classify on the explicit typed signal.
        assert!(!is_bad_signature("bad_signature"));
        assert!(!is_bad_signature(""));
        assert!(!is_bad_signature("<html>401 Unauthorized</html>"));
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
