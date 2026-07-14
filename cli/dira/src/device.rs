//! `dira device link` / `dira device status` — bind this device to the cloud.
//!
//! Linking is a one-time pairing: the web app mints a short-lived code, the user
//! runs `dira device link --code <code>`, and this command claims it. We:
//!
//! 1. resolve the cloud base URL (config / `DIRA_CLOUD_URL`);
//! 2. load-or-create the device keypair via [`dira_core::identity`] (the secret
//!    lives in the OS keychain; only the pubkey + a client nonce leave the
//!    machine — never a client-chosen device id);
//! 3. `POST /api/v1/devices/claim` with `{ code, ed25519Pubkey, label,
//!    clientNonce }` and persist **only** the `deviceId` the cloud returns.
//!
//! ## Cloud invariant: the server assigns the device id
//!
//! The device id is **server-assigned**. The client never chooses or pre-persists
//! its own id, because a client-chosen id is spoofable (a caller could claim a
//! code under any id it likes, or collide with another device's id). We send a
//! `clientNonce` purely as an *idempotency* hint — it lets the cloud collapse a
//! retried claim onto the same device row instead of minting a second one — but
//! the nonce is **not** an identity: the cloud is the sole authority on the
//! `deviceId`, and we persist exactly what it returns.
//!
//! Retry-safety without a pre-chosen id: the device simply stays *unlinked* until
//! a claim succeeds (it has a key + pubkey but no `device_id`, so sync no-ops). A
//! lost response just means the next `dira device link` re-claims with the same
//! `clientNonce`, and the cloud — keyed on `(code, nonce)` — returns the id it
//! already bound rather than a fresh one. So nothing is persisted until we hold an
//! authoritative id, and a retry is idempotent.
//!
//! The CLI opens the same `dira.db` the daemon uses (WAL allows the concurrent
//! access). The daemon re-reads `device_id` from `meta` every sync run, so the
//! link takes effect without restarting the daemon.

use crate::client;
use anyhow::{anyhow, Context, Result};
use dira_contract::{
    BillingSummaryEnvelope, BillingSummaryRequest, RotateKeyEnvelope, RotateKeyRequest,
    SCHEMA_VERSION,
};
use dira_core::protocol::{Request, Response};
use dira_core::signing::DeviceKey;
use dira_core::sync::META_CLOUD_WATERMARK;
use dira_core::{identity, Config, Store};
use std::io::Write;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

/// Resolve the cloud base URL or fail with an actionable message.
fn cloud_url(config: &Config) -> Result<String> {
    config.cloud_url.clone().ok_or_else(|| {
        anyhow!(
            "no cloud URL configured — run `dira config set cloud_url https://app.dirahq.sh` \
             (or set DIRA_CLOUD_URL)"
        )
    })
}

/// `dira device link`: claim a link code and bind this device.
pub async fn link(config: &Config, code: Option<String>, label: Option<String>) -> Result<()> {
    let base = cloud_url(config)?;
    let store = Store::open(&config.db_path)
        .await
        .with_context(|| format!("open store at {}", config.db_path.display()))?;

    // Already linked? Tell the user rather than minting a second device.
    if let Some(existing) = identity::device_id(&store).await? {
        println!("device already linked as {existing}");
        println!("(to re-link, clear the device_id in the store and run link again)");
        return Ok(());
    }

    let key = identity::load_or_create_unlinked(&store)
        .await
        .context("load or create device key")?;

    let code = match code {
        Some(c) => c,
        None => prompt("Enter link code: ")?,
    };
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err(anyhow!("a link code is required"));
    }

    let label = label.or_else(default_label);

    // A client nonce for idempotency ONLY — not an identity. The cloud uses it to
    // collapse a retried claim onto the same device row; it never becomes the
    // device id (the cloud assigns that). We do NOT persist anything yet: the
    // device stays unlinked until the cloud hands back an authoritative id.
    let client_nonce = Ulid::new().to_string();

    let url = format!("{}/api/v1/devices/claim", base.trim_end_matches('/'));
    let body = claim_request_body(&code, &key.public_base64(), label.as_deref(), &client_nonce);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Nothing was persisted, so a failed claim leaves us cleanly unlinked —
        // re-running with the same nonce is idempotent on the cloud side.
        return Err(anyhow!("link failed ({status}): {}", message_of(&text)));
    }

    // The cloud is the sole authority on the device id — persist exactly what it
    // returns. We never fall back to a client-chosen id; if the response carries
    // no `deviceId`, the claim is unusable and we stay unlinked.
    let returned = device_id_from_response(&text)
        .ok_or_else(|| anyhow!("link succeeded but the cloud returned no deviceId"))?;
    identity::set_device_id(&store, &returned).await?;

    println!("linked as {returned}");
    println!("sync will start on the next event or backstop (no daemon restart needed)");
    Ok(())
}

/// Build the `POST /api/v1/devices/claim` request body.
///
/// Pure + deterministic given its inputs, so the wire shape is unit-testable
/// without a network or store. Carries the link `code`, our `ed25519Pubkey`, an
/// optional `label`, and a `clientNonce` for idempotency — but **never** a
/// client-chosen `deviceId`: the cloud assigns the identity (see the module docs).
fn claim_request_body(
    code: &str,
    pubkey_b64: &str,
    label: Option<&str>,
    client_nonce: &str,
) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "ed25519Pubkey": pubkey_b64,
        "label": label,
        "clientNonce": client_nonce,
    })
}

/// Extract the cloud-assigned `deviceId` from a successful claim response body.
/// `None` when the body is missing/blank the field — the caller treats that as a
/// failed link rather than inventing an id.
fn device_id_from_response(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("deviceId")
                .and_then(|d| d.as_str())
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty())
}

/// Build a [`RotateKeyEnvelope`] for `device_id` rotating to `new_key`, signed by
/// `old_key` over `JCS(payload)`. Pure + deterministic given its inputs, so the
/// signing/roundtrip is unit-testable without any network or store.
fn build_rotate_envelope(
    device_id: &str,
    old_key: &DeviceKey,
    new_key: &DeviceKey,
    rotated_at: &str,
) -> Result<RotateKeyEnvelope> {
    let payload = RotateKeyRequest {
        device_id: device_id.to_string(),
        new_pubkey: new_key.public_base64(),
        rotated_at: rotated_at.to_string(),
    };
    // Signed by the OLD key — proves the current key-holder authorized the swap.
    let sig = old_key
        .sign_payload(&payload)
        .map_err(|e| anyhow!("sign rotation request: {e}"))?;
    Ok(RotateKeyEnvelope {
        schema_version: SCHEMA_VERSION.to_string(),
        device_id: device_id.to_string(),
        payload,
        sig,
    })
}

/// `dira device rotate-key`: rotate this device's signing key.
///
/// **Two-phase, crash-safe (WP-B1b).** The cloud's rotation is a single atomic
/// CAS (`UPDATE devices SET ed25519Pubkey = new WHERE id = ? AND ed25519Pubkey
/// = old`, see `cloud/app/api/v1/devices/rotate-key/route.ts`) — at every
/// instant either the OLD key or the NEW key is the one the cloud accepts, and
/// there's no window where both or neither work. Our job is to make the LOCAL
/// bookkeeping survive a crash at any point without ever leaving the device
/// unable to figure out which key is actually live:
///
/// 1. Generate the new keypair and [`identity::persist_pending_key`] it —
///    **before** any network call. A crash here just means: nothing was sent
///    yet, so the OLD key is still the cloud's active key; the next run loads
///    this SAME pending key (never generates a fresh one over it) and picks up
///    where we left off.
/// 2. **Probe first.** Sign a cheap, side-effect-free authenticated request
///    (a billing-summary POST — the cheapest signed device route) with the
///    PENDING key. If the cloud accepts it, the CAS has ALREADY committed
///    (either our own earlier POST landed but its response was lost, or the
///    swap otherwise already happened) — promote immediately, no need to
///    touch the rotation endpoint again.
/// 3. Otherwise **(re-)POST the rotation envelope**, signed by the OLD key,
///    with the SAME pending pubkey + `rotatedAt` every time (determinism is
///    what makes a retry idempotent against the cloud's strictly-increasing
///    replay guard):
///    - **2xx** — the CAS just committed — promote.
///    - **409 `stale_rotation`** or **401 `bad_signature`** — the OLD key no
///      longer verifies / our `rotatedAt` is no longer newer than the floor.
///      Re-probe once more (closes the tiny window between step 2 and this
///      POST) and inspect what it PROVES, via [`ProbeOutcome`]:
///      - **`Live`** — a concurrent rotation (or our own earlier, lost-response
///        POST) already committed this SAME pending key — promote.
///      - **`NotRegistered`** — a definitive, typed `bad_signature` 401 from
///        the probe itself: the pending key is PROVABLY not the cloud's
///        registered key. That combination (rotate-key rejected it AND the
///        probe definitively rejects it) only arises from a genuinely
///        different, concurrent rotation (two `rotate-key` runs racing) — not
///        from any single-process crash-and-retry sequence. Clear it and
///        report the conflict; a fresh `rotate-key` starts over with a new
///        keypair.
///      - **`Ambiguous`** — anything else the probe returned (untyped 4xx,
///        404 on an older cloud without this route, 429 from the shared
///        billing-poller budget, 400 `stale_request` under clock skew — the
///        probe enforces a tighter freshness window than rotate-key does —
///        5xx, or a transport error). None of these PROVE the pending key
///        isn't live, so we must NOT clear it: treat this exactly like a
///        transient failure below and keep the pending key for the next
///        retry, logging why the evidence was inconclusive.
///    - **401 `unknown_device`** — the device isn't linked cloud-side.
///    - anything else (network error, 5xx, ...) — transient; the pending key
///      is left exactly as-is (same keypair, same `rotatedAt`) for the next
///      retry to reuse.
///
/// **Invariant:** at every crash point, exactly one of (old key, pending key)
/// authenticates against the cloud, and re-running `rotate-key` converges to
/// promoting the pending key, clearing it (only on a DEFINITIVE, typed
/// rejection proving a concurrent rotation won), or retrying (every other
/// non-2xx outcome, which is at most ambiguous, never proof of death) — see
/// the module tests for the crash-point matrix this claims to satisfy.
/// Promote-or-clear is really promote-or-clear-or-retry: clearing requires
/// definitive evidence, not just "this attempt didn't succeed."
pub async fn rotate_key(config: &Config) -> Result<()> {
    let base = cloud_url(config)?;

    // Issue #22: `identity::load_key` gives the env seed absolute precedence,
    // but rotation installs the NEW key into the keychain/meta only — with the
    // env var still set, every later load returns the OLD key and each
    // signature is rejected, with the pending key already promoted away. The
    // rotation is only safe when the operator updates the var themselves, so
    // warn up front and print the new secret at the end (see below).
    let env_seeded = std::env::var(identity::ENV_DEVICE_SECRET).is_ok();
    if env_seeded {
        eprintln!(
            "WARNING: {env} is set and takes precedence over the stored device key.\n\
             The rotated key is installed to the keychain only — the daemon keeps\n\
             signing with the OLD key from {env} until you update the variable.\n\
             The new secret is printed after the rotation completes; update {env}\n\
             and restart dirad immediately, or Ctrl-C now to abort.",
            env = identity::ENV_DEVICE_SECRET,
        );
    }

    let store = Store::open(&config.db_path)
        .await
        .with_context(|| format!("open store at {}", config.db_path.display()))?;

    let device_id = identity::device_id(&store)
        .await?
        .ok_or_else(|| anyhow!("device is not linked — run `dira device link` first"))?;

    // Load the CURRENT (old) key; we sign with it and keep it until the cloud
    // confirms the swap.
    let old_key = identity::load_key(&store)
        .await?
        .ok_or_else(|| anyhow!("no device key found — run `dira device link` first"))?;

    // Resume an interrupted rotation if one is pending (SAME keypair + SAME
    // `rotatedAt` — never a fresh generation over an unresolved attempt), else
    // start a brand-new one: generate, persist as pending BEFORE any network
    // call, then proceed identically.
    let (pending_key, rotated_at) = match identity::load_pending_key(&store).await? {
        Some(existing) => existing,
        None => {
            let new_key = DeviceKey::generate();
            let rotated_at = OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default();
            identity::persist_pending_key(&store, &new_key, &rotated_at)
                .await
                .context("persist pending rotation key")?;
            (new_key, rotated_at)
        }
    };

    let client = reqwest::Client::new();
    resume_rotation(
        &client,
        &base,
        &store,
        &device_id,
        &old_key,
        &pending_key,
        &rotated_at,
    )
    .await?;

    println!("rotated device key for {device_id}");
    println!("new pubkey: {}", pending_key.public_base64());
    if env_seeded {
        // The only copy the operator can put into the env var — without this
        // the device is bricked (env seed wins every load, cloud already
        // swapped to the new pubkey). Deliberately printed only in the
        // env-seeded flow; keychain-managed devices never see their secret.
        println!(
            "IMPORTANT: {env} still holds the OLD key — update it to the new secret\n\
             below and restart dirad, or every request will be rejected:\n{secret}",
            env = identity::ENV_DEVICE_SECRET,
            secret = pending_key.secret_base64(),
        );
    }
    Ok(())
}

/// Drive (or resume) a two-phase key rotation to completion. See
/// [`rotate_key`]'s doc comment for the full ladder + the crash-safety
/// argument; this is its implementation.
async fn resume_rotation(
    client: &reqwest::Client,
    base: &str,
    store: &Store,
    device_id: &str,
    old_key: &DeviceKey,
    pending_key: &DeviceKey,
    rotated_at: &str,
) -> Result<()> {
    // Step 1: probe. If the pending key already authenticates, the CAS
    // already committed — nothing left to POST. A non-`Live` outcome here
    // (whether `NotRegistered` or merely `Ambiguous`) just means "not
    // confirmed live yet" — either way we fall through to the POST path,
    // which produces its own, more informative outcome.
    if matches!(
        probe_key(client, base, device_id, pending_key).await,
        ProbeOutcome::Live
    ) {
        return identity::promote_pending_key(store)
            .await
            .context("promote pending key");
    }

    // Step 2: (re-)send the rotation envelope, signed by the OLD key, over the
    // SAME pending pubkey + rotatedAt (determinism ⇒ idempotent retry).
    let envelope = build_rotate_envelope(device_id, old_key, pending_key, rotated_at)?;
    let url = format!("{}/api/v1/devices/rotate-key", base.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&envelope)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        return identity::promote_pending_key(store)
            .await
            .context("promote pending key");
    }

    if status.as_u16() == 409 || is_error_code(&text, "bad_signature") {
        // The OLD key no longer verifies (or the cloud says our rotatedAt is
        // stale). Re-probe once more to close the tiny window between step 1
        // and this POST (e.g. a concurrent `rotate-key` run sharing this SAME
        // pending key just committed it in between) — and inspect what the
        // re-probe actually PROVES before deciding anything irreversible.
        match probe_key(client, base, device_id, pending_key).await {
            ProbeOutcome::Live => {
                return identity::promote_pending_key(store)
                    .await
                    .context("promote pending key");
            }
            ProbeOutcome::NotRegistered => {
                // Provably dead: the probe's own typed `bad_signature` proves
                // this pending key is not (and per the CAS, never can become)
                // the cloud's registered key — some OTHER rotation won. Clear
                // it — a fresh `rotate-key` will start over with a new
                // keypair, which is safe because nothing here was left
                // half-applied.
                identity::clear_pending_key(store).await.ok();
                return Err(anyhow!(
                    "key rotation conflict ({status}): {} — a different rotation already \
                     changed this device's key (only possible from a concurrent `rotate-key` \
                     run); the pending attempt was discarded. Run `dira device rotate-key` \
                     again to start a fresh rotation, or `dira device link` if that also fails.",
                    message_of(&text)
                ));
            }
            ProbeOutcome::Ambiguous(reason) => {
                // The probe did NOT prove the pending key is dead — it could
                // still be the cloud's live key (e.g. the rotate-key POST's
                // CAS committed but its response was lost, and the re-probe
                // itself hit clock skew, a rate limit, or an older cloud
                // without this route). Clearing here could brick a device
                // that's holding the actually-live key. Keep it and let the
                // caller retry — identical to the plain-transient path below.
                eprintln!(
                    "warning: key rotation: re-probe after {status} was ambiguous ({reason}) — \
                     keeping the pending key rather than risk clearing a live one; \
                     re-run `dira device rotate-key` to retry"
                );
                return Err(anyhow!(
                    "key rotation failed ({status}): {} — the re-probe was inconclusive \
                     ({reason}), so the pending key was kept rather than risk discarding a \
                     live one; this attempt is safely resumable — just re-run `dira device \
                     rotate-key`",
                    message_of(&text)
                ));
            }
        }
    }

    if is_error_code(&text, "unknown_device") {
        return Err(anyhow!(
            "key rotation failed ({status}): device is not linked cloud-side — run \
             `dira device link`"
        ));
    }

    // Transient / unexpected (network error, 5xx, ...): the pending key is
    // left exactly as-is — the SAME keypair and `rotatedAt` are reused on the
    // next `rotate-key`, which is what keeps the retry idempotent.
    Err(anyhow!(
        "key rotation failed ({status}): {} — the old key is still installed locally and \
         this attempt is safely resumable; just re-run `dira device rotate-key`",
        message_of(&text)
    ))
}

/// What a [`probe_key`] call proved about whether `key` is currently the
/// cloud's registered key for a device. Deliberately TRI-STATE rather than a
/// bool: the probe endpoint (`POST /api/v1/billing/summary`) can return a
/// non-2xx against a key that IS actually live for reasons that have nothing
/// to do with the key itself — a tighter `sentAt` freshness window than
/// rotate-key's under clock skew (`400 stale_request`), an older cloud
/// without this route (`404`), the 6/min budget shared with the daemon's
/// billing poller (`429`), a plain `5xx`, or a transport error. Only a
/// definitive, typed `bad_signature` 401 proves the key is NOT registered —
/// everything else is [`Ambiguous`](ProbeOutcome::Ambiguous) and must never
/// be treated as proof of death (see `resume_rotation`).
#[derive(Debug, PartialEq, Eq)]
enum ProbeOutcome {
    /// 2xx — the key currently authenticates.
    Live,
    /// A definitive, typed `bad_signature` 401 — the key is provably not the
    /// cloud's registered key.
    NotRegistered,
    /// Every other outcome (untyped 4xx, 404, 429, `400 stale_request`, 5xx,
    /// transport error, or a local signing failure) — inconclusive; carries a
    /// short human-readable reason for logging.
    Ambiguous(String),
}

/// Cheap, side-effect-free probe: does `key` currently authenticate against
/// the cloud for `device_id`? Uses the billing-summary endpoint (the cheapest
/// signed device route — a read-only query, no state it mutates) rather than
/// re-POSTing the rotation itself, so asking "is this key already live" never
/// risks a second CAS attempt. See [`ProbeOutcome`] for why the result is
/// tri-state rather than a bool — collapsing every non-2xx to "not live"
/// would let a false negative (clock skew, a rate limit, ...) look identical
/// to a definitive rejection, and callers must NOT treat them the same way.
async fn probe_key(
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    key: &DeviceKey,
) -> ProbeOutcome {
    let request = BillingSummaryRequest {
        device_id: device_id.to_string(),
        sent_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        period: "week".to_string(),
    };
    let Ok(sig) = key.sign_payload(&request) else {
        return ProbeOutcome::Ambiguous("failed to sign the probe request locally".to_string());
    };
    let envelope = BillingSummaryEnvelope {
        schema_version: SCHEMA_VERSION.to_string(),
        device_id: device_id.to_string(),
        payload: request,
        sig,
    };
    let url = format!("{}/api/v1/billing/summary", base.trim_end_matches('/'));
    let resp = match client.post(&url).json(&envelope).send().await {
        Ok(resp) => resp,
        Err(e) => return ProbeOutcome::Ambiguous(format!("probe request failed: {e}")),
    };
    let status = resp.status();
    if status.is_success() {
        return ProbeOutcome::Live;
    }
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 && is_error_code(&text, "bad_signature") {
        return ProbeOutcome::NotRegistered;
    }
    ProbeOutcome::Ambiguous(format!("probe returned {status}: {}", message_of(&text)))
}

/// Whether a JSON error body's `error` field equals `code`. Unparseable ⇒
/// `false` (never misread a proxy's plain-text error as a typed signal).
fn is_error_code(body: &str, code: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s == code))
        .unwrap_or(false)
}

/// `dira device status`: print linkage, cloud URL, and the un-synced backlog.
pub async fn status(config: &Config) -> Result<()> {
    let store = Store::open(&config.db_path)
        .await
        .with_context(|| format!("open store at {}", config.db_path.display()))?;

    match identity::device_id(&store).await? {
        Some(id) => println!("device:    linked ({id})"),
        None => println!("device:    not linked — run `dira device link` to pair this device"),
    }
    match &config.cloud_url {
        Some(url) => println!("cloud:     {url}"),
        None => println!(
            "cloud:     (unset — `dira config set cloud_url <url>` or set DIRA_CLOUD_URL to enable sync)"
        ),
    }

    let cursor = store
        .meta_get(SYNC_CURSOR_KEY)
        .await?
        .filter(|s| !s.is_empty());
    let pending = store.count_events_after(cursor.as_deref()).await?;
    match &cursor {
        Some(c) => println!("cursor:    {c}"),
        None => println!("cursor:    (none — nothing synced yet)"),
    }
    println!("pending:   {pending} event(s) awaiting sync");

    // Cloud coverage, from the watermark the daemon cached on its last flush.
    let cloud_wm = store
        .meta_get(META_CLOUD_WATERMARK)
        .await?
        .filter(|s| !s.is_empty());
    let local_head = store.max_event_id().await?;
    println!(
        "{}",
        cloud_status_line(cloud_wm.as_deref(), local_head.as_deref(), pending)
    );
    Ok(())
}

/// `dira device resync`: ask the daemon (over the control socket, so the live flush
/// task acts) to rewind the sync cursor and re-send. The cloud dedups, so this can
/// never double-count.
pub async fn resync(config: &Config, from: Option<String>) -> Result<()> {
    match client::send(&config.socket_path, &Request::ResyncCursor { from }).await? {
        Response::ResyncQueued { pending, from } => {
            match from {
                Some(id) => println!("resync:    cursor rewound to {id}"),
                None => println!("resync:    cursor rewound to the beginning (full re-send)"),
            }
            println!(
                "pending:   {pending} event(s) will re-sync now — safe; the cloud dedups (no double counting)"
            );
            Ok(())
        }
        Response::Error { message } => Err(anyhow!("resync failed: {message}")),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

/// Honest one-line cloud-coverage summary for `device status`. Compares the cloud's
/// cached watermark (a batch ULID, timestamp-stamped with the latest event it
/// covers) against the local event head to judge drift. Pure, so it's unit-testable.
fn cloud_status_line(cloud_wm: Option<&str>, local_head: Option<&str>, pending: u64) -> String {
    let Some(wm) = cloud_wm else {
        return "cloud:     (no handshake yet — sync at least once)".into();
    };
    // The cloud persisted noticeably less than the local head while nothing is
    // queued ⇒ a silent gap the reconciler/epoch didn't cover; resync forces it.
    let behind = matches!(
        (Ulid::from_string(wm), local_head.and_then(|h| Ulid::from_string(h).ok())),
        (Ok(w), Some(h)) if w.timestamp_ms() + 1000 < h.timestamp_ms()
    ) && pending == 0;
    if behind {
        "cloud:     behind — cloud is missing data; run `dira device resync`".into()
    } else if pending == 0 {
        format!("cloud:     in sync (watermark {wm})")
    } else {
        format!("cloud:     {pending} event(s) queued for the next sync")
    }
}

/// `dira device unlink`: locally unlink this device. Clears the cloud-assigned
/// id so the daemon stops syncing on its next tick, while KEEPING the signing
/// key so a later `link` reclaims the same identity. No cloud call — there is no
/// revoke endpoint, so this is a local break. Warns and asks to confirm when
/// events are still awaiting sync, unless `--yes` is given.
pub async fn unlink(config: &Config, yes: bool) -> Result<()> {
    let store = Store::open(&config.db_path)
        .await
        .with_context(|| format!("open store at {}", config.db_path.display()))?;

    if identity::device_id(&store).await?.is_none() {
        println!("device:    not linked — nothing to unlink");
        return Ok(());
    }

    let cursor = store
        .meta_get(SYNC_CURSOR_KEY)
        .await?
        .filter(|s| !s.is_empty());
    let pending = store.count_events_after(cursor.as_deref()).await?;

    if needs_confirmation(pending, yes) {
        println!(
            "warning:   {pending} event(s) are still awaiting sync and won't be sent once unlinked."
        );
        let ans = prompt("Unlink anyway? [y/N]: ")?;
        if !matches!(ans.trim(), "y" | "Y" | "yes" | "Yes") {
            println!("aborted — device left linked.");
            return Ok(());
        }
    }

    identity::clear_device_id(&store).await?;
    println!("device:    unlinked — the daemon stops syncing on its next tick.");
    println!("key:       retained — run `dira device link` to re-link with the same identity.");
    Ok(())
}

/// Whether `unlink` must ask the user to confirm: only when an unsynced backlog
/// exists and `--yes` was not passed. Pure, so the gate is unit-testable.
fn needs_confirmation(pending: u64, yes: bool) -> bool {
    pending > 0 && !yes
}

/// Mirror of `dirad::sync::META_SYNC_CURSOR` (kept in sync; the daemon owns the
/// canonical constant, but the CLI reads the same `meta` key for status).
const SYNC_CURSOR_KEY: &str = "sync_cursor_event_id";

/// A reasonable default device label: the machine hostname.
fn default_label() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(hostname_via_uname)
        .filter(|s| !s.is_empty())
}

fn hostname_via_uname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Prompt on stdout and read a line from stdin.
fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read link code from stdin")?;
    Ok(line)
}

/// Extract a human message from a JSON error body, falling back to the raw text.
fn message_of(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::signing::verify_payload;

    #[test]
    fn claim_body_carries_no_client_chosen_device_id() {
        // The claim request must NOT smuggle a client-chosen device id under any
        // name. Identity is server-assigned; we only send a clearly-named nonce.
        let body = claim_request_body("CODE-123", "PUBKEYb64", Some("laptop"), "01NONCE");
        assert_eq!(body["code"], "CODE-123");
        assert_eq!(body["ed25519Pubkey"], "PUBKEYb64");
        assert_eq!(body["label"], "laptop");
        assert_eq!(body["clientNonce"], "01NONCE");
        // No `deviceId` field at all — the cloud owns the id.
        assert!(
            body.get("deviceId").is_none(),
            "claim body must not contain a client-chosen deviceId"
        );
    }

    #[test]
    fn claim_body_omits_label_as_null_when_absent() {
        let body = claim_request_body("CODE", "PK", None, "01NONCE");
        assert!(body["label"].is_null());
        assert!(body.get("deviceId").is_none());
    }

    #[test]
    fn device_id_comes_only_from_the_cloud_response() {
        // The server-assigned id is taken verbatim from the response.
        assert_eq!(
            device_id_from_response(r#"{"deviceId":"01CLOUDID"}"#).as_deref(),
            Some("01CLOUDID")
        );
        // A response missing/blanking the id yields None — the caller must NOT
        // invent or fall back to a client id, so the device stays unlinked.
        assert_eq!(device_id_from_response(r#"{"ok":true}"#), None);
        assert_eq!(device_id_from_response(r#"{"deviceId":""}"#), None);
        assert_eq!(device_id_from_response("not json"), None);
    }

    #[test]
    fn rotate_envelope_is_signed_by_the_old_key() {
        let old = DeviceKey::generate();
        let new = DeviceKey::generate();
        let env =
            build_rotate_envelope("01DEVICEULID", &old, &new, "2026-06-29T10:00:00Z").unwrap();

        // The envelope carries the NEW pubkey...
        assert_eq!(env.payload.new_pubkey, new.public_base64());
        assert_eq!(env.payload.device_id, "01DEVICEULID");
        assert_eq!(env.device_id, "01DEVICEULID");

        // ...but the signature verifies against the OLD pubkey (the cloud checks it
        // against the device's currently-registered key before swapping).
        assert!(
            verify_payload(&old.public_base64(), &env.sig, &env.payload).unwrap(),
            "signature must verify under the old key"
        );
        // And NOT against the new key.
        assert!(
            !verify_payload(&new.public_base64(), &env.sig, &env.payload).unwrap(),
            "signature must not verify under the new key"
        );
    }

    #[test]
    fn rotate_envelope_roundtrips_through_json() {
        let old = DeviceKey::generate();
        let new = DeviceKey::generate();
        let env =
            build_rotate_envelope("01DEVICEULID", &old, &new, "2026-06-29T10:00:00Z").unwrap();

        let json = serde_json::to_string(&env).unwrap();
        let back: RotateKeyEnvelope = serde_json::from_str(&json).unwrap();
        // The deserialized payload still verifies under the old key — JCS parity.
        assert!(verify_payload(&old.public_base64(), &back.sig, &back.payload).unwrap());
        assert_eq!(back.payload.new_pubkey, new.public_base64());
    }

    #[test]
    fn unlink_confirms_only_on_backlog_without_yes() {
        // A clean (fully-synced) device unlinks without prompting.
        assert!(!needs_confirmation(0, false));
        // A backlog requires confirmation...
        assert!(needs_confirmation(3, false));
        // ...unless `--yes` is given.
        assert!(!needs_confirmation(3, true));
        assert!(!needs_confirmation(0, true));
    }

    // --- WP-B1b: two-phase key rotation ------------------------------------

    use crate::test_support::{keychain_lock, use_mock_keychain, MockCloud, MockResp};

    const ROTATE_PATH: &str = "/api/v1/devices/rotate-key";
    const PROBE_PATH: &str = "/api/v1/billing/summary";
    const DEVICE_ID: &str = "01TESTDEVICE";
    const ROTATED_AT: &str = "2026-07-09T10:00:00Z";

    /// A fresh old/pending keypair + an in-memory store with nothing pending
    /// yet — the common setup every rotation test starts from.
    async fn rotation_fixture() -> (Store, DeviceKey, DeviceKey) {
        let store = Store::open_in_memory().await.unwrap();
        let old_key = DeviceKey::generate();
        let pending_key = DeviceKey::generate();
        (store, old_key, pending_key)
    }

    /// Assert the rotation fully converged: the pending key is now ACTIVE and
    /// no pending markers remain — the "promote" half of "promote-or-clear".
    async fn assert_promoted(store: &Store, pending_key: &DeviceKey) {
        assert!(
            identity::load_pending_key(store).await.unwrap().is_none(),
            "pending markers must be cleared after a promote"
        );
        assert_eq!(
            store
                .meta_get(identity::META_PUBKEY)
                .await
                .unwrap()
                .as_deref(),
            Some(pending_key.public_base64().as_str()),
            "the ACTIVE pubkey must now be the pending key's"
        );
    }

    /// WP-B1b crash point: the pending key was persisted but NOTHING was ever
    /// POSTed (a crash between `persist_pending_key` and the network call).
    /// At this point only the OLD key authenticates. A retry's probe
    /// correctly reports "not live" and falls through to POST, which the
    /// cloud accepts — converging to promoted.
    #[tokio::test]
    async fn crash_before_any_post_converges_via_the_post_path() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let (store, old_key, pending_key) = rotation_fixture().await;
        identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
            .await
            .unwrap();

        let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;
        // Only the OLD key authenticates right now — the probe (pending) fails.
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        cloud.push(ROTATE_PATH, MockResp::ok(r#"{"deviceId":"01TESTDEVICE"}"#));

        resume_rotation(
            &reqwest::Client::new(),
            cloud.base_url(),
            &store,
            DEVICE_ID,
            &old_key,
            &pending_key,
            ROTATED_AT,
        )
        .await
        .expect("must converge");

        assert_promoted(&store, &pending_key).await;
        assert_eq!(cloud.requests(ROTATE_PATH).len(), 1);
    }

    /// WP-B1b crash point: the rotation envelope was POSTed and the cloud's
    /// atomic CAS committed it, but the RESPONSE never reached the CLI (a lost
    /// connection / proxy timeout AFTER the commit — modeled here as an
    /// unhelpful 500 the caller can't distinguish from "nothing happened").
    /// At this point only the PENDING key authenticates, even though the
    /// caller doesn't know it yet. The first `resume_rotation` call correctly
    /// reports failure (it has no way to know better) and leaves the pending
    /// key untouched; a RETRY's probe now sees the committed swap and
    /// converges immediately, without ever re-hitting the rotate-key endpoint.
    #[tokio::test]
    async fn crash_after_commit_response_lost_then_retry_converges_via_probe() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let (store, old_key, pending_key) = rotation_fixture().await;
        identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
            .await
            .unwrap();

        let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;

        // Attempt 1: probe says not-live-yet (correct, at the time it runs);
        // the POST reaches the cloud and its CAS commits, but the CLI only
        // ever sees a bare 500 for it — response lost after commit.
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        cloud.push(ROTATE_PATH, MockResp::status(500, "upstream timeout"));

        let err = resume_rotation(
            &reqwest::Client::new(),
            cloud.base_url(),
            &store,
            DEVICE_ID,
            &old_key,
            &pending_key,
            ROTATED_AT,
        )
        .await;
        assert!(
            err.is_err(),
            "the first attempt has no way to know it actually succeeded"
        );

        // Nothing was destroyed: the SAME pending key/timestamp is still there
        // for a retry to reuse.
        let (still_pending, still_at) = identity::load_pending_key(&store).await.unwrap().unwrap();
        assert_eq!(still_pending.public_base64(), pending_key.public_base64());
        assert_eq!(still_at, ROTATED_AT);

        // Attempt 2 (a retry): the cloud's CAS already committed (from attempt
        // 1), so the probe now succeeds — promotes without ever re-POSTing
        // the rotation itself.
        cloud.push(PROBE_PATH, MockResp::ok("{}"));
        resume_rotation(
            &reqwest::Client::new(),
            cloud.base_url(),
            &store,
            DEVICE_ID,
            &old_key,
            &pending_key,
            ROTATED_AT,
        )
        .await
        .expect("the retry must converge");

        assert_promoted(&store, &pending_key).await;
        // rotate-key was hit exactly once — by attempt 1. The retry resolved
        // entirely through the probe.
        assert_eq!(cloud.requests(ROTATE_PATH).len(), 1);
    }

    /// WP-B1b crash point: `resume_rotation` got a 2xx and started promoting,
    /// but crashed BETWEEN installing the new key as ACTIVE and clearing the
    /// pending markers (`identity::promote_pending_key`'s two sub-steps). At
    /// this point the pending key is ALREADY the active one — a retry's probe
    /// sees that immediately and finishes the (idempotent) promote/clear.
    #[tokio::test]
    async fn crash_mid_promote_converges_idempotently() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let (store, old_key, pending_key) = rotation_fixture().await;
        identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
            .await
            .unwrap();
        // Simulate the interrupted first half of `promote_pending_key`: the
        // new key is already installed active, but the pending markers were
        // never cleared.
        identity::install_rotated_key(&store, &pending_key)
            .await
            .unwrap();
        assert!(identity::load_pending_key(&store).await.unwrap().is_some());

        let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;
        cloud.push(PROBE_PATH, MockResp::ok("{}")); // pending IS active already

        resume_rotation(
            &reqwest::Client::new(),
            cloud.base_url(),
            &store,
            DEVICE_ID,
            &old_key,
            &pending_key,
            ROTATED_AT,
        )
        .await
        .expect("must converge");

        assert_promoted(&store, &pending_key).await;
        // The rotation endpoint was never touched — the probe alone resolved it.
        assert!(cloud.requests(ROTATE_PATH).is_empty());
    }

    /// Not a crash point — a genuinely different, CONCURRENT `rotate-key` run
    /// wins the race (only possible from two overlapping invocations, not
    /// from any single-process interruption). Both the probe and the retried
    /// POST prove the pending key never went live; `resume_rotation` must
    /// clear it (not leave it stuck) and report a clear, actionable conflict —
    /// the "or-clear" half of "promote-or-clear". The OLD key is left
    /// untouched either way.
    #[tokio::test]
    async fn genuine_conflict_clears_pending_and_reports_it() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let (store, old_key, pending_key) = rotation_fixture().await;
        identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
            .await
            .unwrap();

        let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;
        // Probe (before POST): pending not live.
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        // POST: the cloud reports a conflict (some other rotation won).
        cloud.push(
            ROTATE_PATH,
            MockResp::status(409, r#"{"error":"stale_rotation"}"#),
        );
        // Re-probe (after the 409, before giving up): still not live.
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );

        let err = resume_rotation(
            &reqwest::Client::new(),
            cloud.base_url(),
            &store,
            DEVICE_ID,
            &old_key,
            &pending_key,
            ROTATED_AT,
        )
        .await
        .expect_err("a genuine conflict must be reported, not silently swallowed");
        assert!(
            format!("{err:#}").contains("conflict"),
            "the error must name the situation: {err:#}"
        );

        // Pending is cleared — NOT left stuck forever.
        assert!(identity::load_pending_key(&store).await.unwrap().is_none());
        // The store never had an active key installed by this path at all —
        // this test never calls `identity::load_or_create_unlinked`/
        // `install_rotated_key`, so there's nothing to assert "untouched"
        // against beyond the pending markers already checked above.
    }

    /// Regression for the HIGH finding: an AMBIGUOUS re-probe (429/404/400 —
    /// none of which prove the key is dead) must NEVER clear a pending key
    /// that is actually still live, unlike the definitive `bad_signature` 401
    /// in [`genuine_conflict_clears_pending_and_reports_it`]. Exercises the
    /// three concrete non-typed statuses the finding calls out: a 429 from
    /// the probe's shared rate budget, a 404 from an older cloud without the
    /// route, and a 400 `stale_request` from clock skew (the probe enforces a
    /// tighter freshness window than rotate-key does). In every case: attempt
    /// 1 fails without clearing, and a RETRY's first-step probe (now seeing
    /// the key actually IS live — e.g. attempt 1's own POST committed but hit
    /// this same ambiguity on its confirming re-probe) converges to promote
    /// without ever re-hitting the rotation endpoint.
    #[tokio::test]
    async fn ambiguous_reprobe_keeps_pending_key_then_retry_converges_via_probe() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        for (status, body) in [
            (429u16, "rate limited"),
            (404u16, "not found"),
            (400u16, r#"{"error":"stale_request"}"#),
        ] {
            let (store, old_key, pending_key) = rotation_fixture().await;
            identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
                .await
                .unwrap();

            let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;
            // Attempt 1, step 1: pending not yet known live.
            cloud.push(
                PROBE_PATH,
                MockResp::status(401, r#"{"error":"bad_signature"}"#),
            );
            // Attempt 1, POST: cloud reports a conflict-shaped response...
            cloud.push(
                ROTATE_PATH,
                MockResp::status(409, r#"{"error":"stale_rotation"}"#),
            );
            // ...but the re-probe meant to confirm that is ONLY ambiguous —
            // it does NOT prove the pending key is dead.
            cloud.push(PROBE_PATH, MockResp::status(status, body));

            let err = resume_rotation(
                &reqwest::Client::new(),
                cloud.base_url(),
                &store,
                DEVICE_ID,
                &old_key,
                &pending_key,
                ROTATED_AT,
            )
            .await
            .expect_err("an ambiguous re-probe must not be treated as success");
            let msg = format!("{err:#}");
            assert!(
                !msg.contains("conflict"),
                "an ambiguous re-probe must not be reported as a definitive conflict: {msg}"
            );

            // KEPT — not cleared — because ambiguous evidence never proves death.
            let (still_pending, still_at) =
                identity::load_pending_key(&store).await.unwrap().unwrap();
            assert_eq!(
                still_pending.public_base64(),
                pending_key.public_base64(),
                "status {status}: pending key must survive an ambiguous re-probe"
            );
            assert_eq!(still_at, ROTATED_AT);

            // Attempt 2 (a retry): the pending key is now confirmed live —
            // the first-step probe alone resolves it, no second rotate-key
            // POST needed.
            cloud.push(PROBE_PATH, MockResp::ok("{}"));
            resume_rotation(
                &reqwest::Client::new(),
                cloud.base_url(),
                &store,
                DEVICE_ID,
                &old_key,
                &pending_key,
                ROTATED_AT,
            )
            .await
            .unwrap_or_else(|e| panic!("status {status}: the retry must converge: {e:#}"));

            assert_promoted(&store, &pending_key).await;
            // rotate-key was hit exactly once — by attempt 1. The retry
            // resolved entirely through the probe.
            assert_eq!(cloud.requests(ROTATE_PATH).len(), 1, "status {status}");
        }
    }

    /// A transient failure (a plain 5xx, not `stale_rotation`/`bad_signature`)
    /// must NOT clear the pending key — it might still be the one the cloud
    /// eventually applies, so the caller must be able to retry with the
    /// SAME keypair/timestamp.
    #[tokio::test]
    async fn transient_failure_keeps_pending_key_for_a_later_retry() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let (store, old_key, pending_key) = rotation_fixture().await;
        identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
            .await
            .unwrap();

        let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        cloud.push(ROTATE_PATH, MockResp::status(503, "service unavailable"));

        let err = resume_rotation(
            &reqwest::Client::new(),
            cloud.base_url(),
            &store,
            DEVICE_ID,
            &old_key,
            &pending_key,
            ROTATED_AT,
        )
        .await;
        assert!(err.is_err());

        let (still_pending, still_at) = identity::load_pending_key(&store).await.unwrap().unwrap();
        assert_eq!(still_pending.public_base64(), pending_key.public_base64());
        assert_eq!(still_at, ROTATED_AT);
    }

    /// The re-POSTed rotation envelope must be byte-identical across retries
    /// (same pending pubkey, same `rotatedAt`) — the determinism the cloud's
    /// replay guard relies on to treat a retry as the SAME request rather
    /// than a new (and thus stale-by-comparison) one.
    #[tokio::test]
    async fn retried_envelope_is_byte_identical_across_attempts() {
        let _keychain_lock = keychain_lock().await;
        use_mock_keychain();
        let (store, old_key, pending_key) = rotation_fixture().await;
        identity::persist_pending_key(&store, &pending_key, ROTATED_AT)
            .await
            .unwrap();

        let cloud = MockCloud::start(&[PROBE_PATH, ROTATE_PATH]).await;
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        cloud.push(ROTATE_PATH, MockResp::status(500, "boom"));
        cloud.push(
            PROBE_PATH,
            MockResp::status(401, r#"{"error":"bad_signature"}"#),
        );
        cloud.push(ROTATE_PATH, MockResp::status(500, "boom again"));

        let client = reqwest::Client::new();
        for _ in 0..2 {
            let _ = resume_rotation(
                &client,
                cloud.base_url(),
                &store,
                DEVICE_ID,
                &old_key,
                &pending_key,
                ROTATED_AT,
            )
            .await;
        }

        let bodies = cloud.requests(ROTATE_PATH);
        assert_eq!(bodies.len(), 2);
        assert_eq!(
            bodies[0], bodies[1],
            "a retried rotation envelope must be byte-identical"
        );
    }
}
