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

use anyhow::{anyhow, Context, Result};
use dira_contract::{RotateKeyEnvelope, RotateKeyRequest};
use dira_core::signing::DeviceKey;
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
        device_id: device_id.to_string(),
        payload,
        sig,
    })
}

/// `dira device rotate-key`: rotate this device's signing key.
///
/// Generates a fresh keypair, signs a [`RotateKeyRequest`] with the **old** key
/// (so the cloud can verify against the currently-registered pubkey), POSTs it to
/// `/api/v1/devices/rotate-key`, and only on a 2xx swaps the stored secret/pubkey
/// to the new key. The old key is kept until success, so a failed/lost response
/// leaves the device fully functional on its existing key (re-run to retry).
///
/// NOTE: cloud-side verification (check the signature against the registered
/// pubkey, then install `newPubkey`) lives in the separate cloud repo and is out
/// of scope here — this is the producer side only.
pub async fn rotate_key(config: &Config) -> Result<()> {
    let base = cloud_url(config)?;
    let store = Store::open(&config.db_path)
        .await
        .with_context(|| format!("open store at {}", config.db_path.display()))?;

    let device_id = identity::device_id(&store)
        .await?
        .ok_or_else(|| anyhow!("device is not linked — run `dira device link` first"))?;

    // Load the CURRENT (old) key; we sign with it and keep it until the cloud acks.
    let old_key = identity::load_key(&store)
        .await?
        .ok_or_else(|| anyhow!("no device key found — run `dira device link` first"))?;

    let new_key = DeviceKey::generate();
    let rotated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let envelope = build_rotate_envelope(&device_id, &old_key, &new_key, &rotated_at)?;

    let url = format!("{}/api/v1/devices/rotate-key", base.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&envelope)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // The old key is untouched — the device keeps working; just retry later.
        return Err(anyhow!(
            "key rotation failed ({status}): {}",
            message_of(&text)
        ));
    }

    // 2xx: the cloud accepted the new pubkey. Swap the stored secret/pubkey now.
    identity::install_rotated_key(&store, &new_key)
        .await
        .context("install rotated key")?;

    println!("rotated device key for {device_id}");
    println!("new pubkey: {}", new_key.public_base64());
    Ok(())
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
    Ok(())
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
}
