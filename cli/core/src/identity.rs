//! Device identity over the local store's `meta` table + the OS keychain.
//!
//! A device holds one Ed25519 keypair ([`crate::signing::DeviceKey`]). The
//! secret seed lives in the OS keychain (never synced, never logged); the public
//! key and — once the device is linked to the cloud — its `device_id` live in
//! `meta`.
//!
//! Linking is two-phase: a device first exists *unlinked* (it has a key and a
//! pubkey but no cloud `device_id`), then `dira device link` claims a code and
//! [`set_device_id`] binds it. The daemon re-reads [`device_id`] each sync run,
//! so linking takes effect without a daemon restart.
//!
//! ### Keychain fallback
//! When the OS keychain is unavailable (headless CI, locked login keychain), the
//! secret is persisted to `meta` under [`META_SECRET_FALLBACK`] instead. This is
//! a documented, lower-assurance fallback: the seed then sits **at rest in
//! `dira.db` as plaintext base64** rather than in the keychain. It keeps sync
//! working on machines without a usable keychain; on a normal desktop the
//! keychain path is always taken. On unix the db file is chmod'd to `0600`
//! (owner-only) by [`crate::store::Store::open`] so the at-rest seed is at least
//! not world-readable, but anyone who can read the file as your user can read the
//! seed — treat a keychain-fallback machine accordingly.
//!
//! ### Env-provided seed (`DIRA_DEVICE_SECRET`)
//! For headless / ephemeral environments (CI, containers) where neither the OS
//! keychain nor an at-rest db secret is desirable, the secret seed may be supplied
//! out-of-band via the `DIRA_DEVICE_SECRET` env var (standard base64 of the
//! 32-byte Ed25519 seed). When set and well-formed it takes precedence over both
//! the keychain and the meta fallback, and is **never written to disk** — the
//! operator owns its lifecycle (e.g. injected from a secrets manager). A
//! malformed value is ignored (we fall through to the keychain/meta) so a typo
//! can't brick a device. This is read-only: `load_or_create_unlinked` does not
//! mint or persist anything when the env seed is present.

use crate::signing::DeviceKey;
use crate::store::Store;
use crate::Error;

/// Keychain service name for the device secret seed. Shared by `dira` and
/// `dirad` — both read/write the same service+account pairs, which is what
/// lets the daemon's `try_pending_key_flush` self-heal path resolve a pending
/// key the CLI persisted (or vice versa).
const KEYCHAIN_SERVICE: &str = "sh.dirahq.dira";
/// Keychain account (entry) name for the ACTIVE device secret seed.
const KEYCHAIN_ACCOUNT: &str = "device-secret";
/// Keychain account (entry) name for the PENDING rotation key's secret seed —
/// a distinct account from [`KEYCHAIN_ACCOUNT`] so an in-flight rotation's
/// pending key and the currently-active key never collide in the OS
/// keychain (both can be present at once mid-rotation).
const KEYCHAIN_ACCOUNT_PENDING: &str = "device-pending-secret";
/// Env var supplying the device secret seed out-of-band (standard base64 of the
/// 32-byte seed). Takes precedence over the keychain/meta and is never persisted.
pub const ENV_DEVICE_SECRET: &str = "DIRA_DEVICE_SECRET";

/// `meta` key holding the device's base64 public key.
pub const META_PUBKEY: &str = "device_pubkey_b64";
/// `meta` key holding the cloud-assigned device id (ULID). Absent ⇒ unlinked.
pub const META_DEVICE_ID: &str = "device_id";
/// `meta` key holding the secret seed, used only when the keychain is unavailable.
pub const META_SECRET_FALLBACK: &str = "device_secret_b64";

/// Load the device key, generating + persisting a fresh one on first use.
///
/// This never sets a `device_id` — a freshly generated device is *unlinked*
/// until [`set_device_id`] runs. Idempotent: repeated calls return the same key.
pub async fn load_or_create_unlinked(store: &Store) -> Result<DeviceKey, Error> {
    if let Some(key) = load_key(store).await? {
        return Ok(key);
    }
    let key = DeviceKey::generate();
    persist_secret(store, &key).await?;
    store.meta_set(META_PUBKEY, &key.public_base64()).await?;
    Ok(key)
}

/// Read a well-formed seed from [`ENV_DEVICE_SECRET`], if set. A blank or
/// malformed value yields `None` (we fall through to keychain/meta) so a typo
/// can't brick a device.
fn env_key() -> Option<DeviceKey> {
    let secret = std::env::var(ENV_DEVICE_SECRET).ok()?;
    if secret.trim().is_empty() {
        return None;
    }
    match DeviceKey::from_secret_base64(&secret) {
        Ok(key) => Some(key),
        Err(e) => {
            tracing::warn!("{ENV_DEVICE_SECRET} is set but not a valid base64 seed; ignoring: {e}");
            None
        }
    }
}

/// Load the existing device key, or `None` if this device has never generated one.
///
/// Resolution order: the [`ENV_DEVICE_SECRET`] env seed (never persisted), then
/// the OS keychain, then the meta-stored fallback seed.
pub async fn load_key(store: &Store) -> Result<Option<DeviceKey>, Error> {
    if let Some(key) = env_key() {
        return Ok(Some(key));
    }
    if let Some(secret) = keychain_get(KEYCHAIN_ACCOUNT) {
        return Ok(Some(DeviceKey::from_secret_base64(&secret)?));
    }
    if let Some(secret) = store.meta_get(META_SECRET_FALLBACK).await? {
        return Ok(Some(DeviceKey::from_secret_base64(&secret)?));
    }
    Ok(None)
}

/// Persist the device secret: keychain first, `meta` fallback if that fails.
async fn persist_secret(store: &Store, key: &DeviceKey) -> Result<(), Error> {
    let secret = key.secret_base64();
    if keychain_set(KEYCHAIN_ACCOUNT, &secret).is_ok() {
        // Clear any stale fallback so the keychain is the single source.
        let _ = store.meta_set(META_SECRET_FALLBACK, "").await;
        return Ok(());
    }
    // Loud + clear: the private key now sits at rest in the on-disk db as plaintext
    // base64, NOT in the OS keychain. The db is chmod'd 0600 on unix, but this is a
    // materially lower-assurance posture — surface it plainly. Operators can avoid
    // it entirely by supplying the seed via DIRA_DEVICE_SECRET (never persisted).
    tracing::warn!(
        "OS keychain unavailable: storing the device SECRET KEY at rest in {db} \
         (plaintext base64, db file is chmod 0600 on unix). This is a lower-assurance \
         fallback — anyone who can read the db as your user can read the key. Set \
         {env} to supply the seed out-of-band instead.",
        db = META_SECRET_FALLBACK,
        env = ENV_DEVICE_SECRET,
    );
    store.meta_set(META_SECRET_FALLBACK, &secret).await
}

/// Install a freshly-rotated device key: persist its secret (keychain first,
/// `meta` fallback) and update the stored pubkey. Call this **only after** the
/// cloud has accepted the rotation (2xx) so the old key stays usable until the
/// swap is confirmed. Idempotent: re-installing the same key is a no-op.
pub async fn install_rotated_key(store: &Store, key: &DeviceKey) -> Result<(), Error> {
    persist_secret(store, key).await?;
    store.meta_set(META_PUBKEY, &key.public_base64()).await
}

/// `meta` key holding the base64 public key of a rotation IN FLIGHT — set
/// **before** the rotation is POSTed to the cloud, promoted to [`META_PUBKEY`]
/// (or discarded) once the swap resolves (WP-B1b two-phase rotation). Absent
/// (empty) ⇒ no rotation in flight.
pub const META_PENDING_PUBKEY: &str = "device_pending_pubkey_b64";
/// `meta` key holding the pending key's secret seed — used only as the
/// fallback when the OS keychain is unavailable.
///
/// Mirrors [`META_SECRET_FALLBACK`] exactly: the pending key follows the
/// SAME keychain-first, meta-fallback ladder as the active key (just under
/// the distinct [`KEYCHAIN_ACCOUNT_PENDING`] account, so the two never
/// collide), rather than skipping the keychain entirely. A pending seed sat
/// here in plaintext even when a working keychain was available; treating it
/// as lower-assurance than the active key was never justified by anything
/// about the rotation protocol, and it left the seed at rest exactly when a
/// crash mid-rotation is most likely to be inspected.
const META_PENDING_SECRET: &str = "device_pending_secret_b64";
/// `meta` key holding the pending rotation's RFC 3339 `rotatedAt`, persisted
/// alongside the pending keypair so a retried `rotate-key` rebuilds the
/// *identical* envelope (same key, same timestamp) — the determinism that
/// makes a re-POST idempotent against the cloud's replay guard (strictly
/// increasing `rotatedAt`; see `dira/src/device.rs`).
pub const META_PENDING_ROTATED_AT: &str = "device_pending_rotated_at";

/// Persist a freshly-generated PENDING rotation key + its `rotated_at`,
/// **before** the rotation is POSTed to the cloud (WP-B1b). A crash any time
/// after this call leaves enough on disk for a retry to resume deterministically
/// (see [`load_pending_key`]). Call this only when starting a FRESH rotation
/// attempt — a retry of an interrupted one loads the existing pending key
/// instead, so the same keypair/timestamp is reused across retries.
pub async fn persist_pending_key(
    store: &Store,
    key: &DeviceKey,
    rotated_at: &str,
) -> Result<(), Error> {
    persist_pending_secret(store, key).await?;
    store
        .meta_set(META_PENDING_PUBKEY, &key.public_base64())
        .await?;
    store.meta_set(META_PENDING_ROTATED_AT, rotated_at).await
}

/// Persist the PENDING key's secret: keychain first (under the distinct
/// [`KEYCHAIN_ACCOUNT_PENDING`] account), `meta` fallback if that fails —
/// mirrors [`persist_secret`]'s ladder exactly, for the same reasons.
async fn persist_pending_secret(store: &Store, key: &DeviceKey) -> Result<(), Error> {
    let secret = key.secret_base64();
    if keychain_set(KEYCHAIN_ACCOUNT_PENDING, &secret).is_ok() {
        // Clear any stale fallback so the keychain is the single source.
        let _ = store.meta_set(META_PENDING_SECRET, "").await;
        return Ok(());
    }
    tracing::warn!(
        "OS keychain unavailable: storing the PENDING device key at rest in {db} \
         (plaintext base64, db file is chmod 0600 on unix). Same lower-assurance \
         fallback the active key uses when the keychain is unavailable — see \
         {fallback}'s doc comment.",
        db = META_PENDING_SECRET,
        fallback = "META_SECRET_FALLBACK",
    );
    store.meta_set(META_PENDING_SECRET, &secret).await
}

/// Load the in-flight pending rotation key + its `rotated_at`, if a rotation
/// is currently in flight. `None` in the common case (no rotation pending).
///
/// [`META_PENDING_ROTATED_AT`] (always meta-stored — never gated behind the
/// keychain) is the source of truth for "is a rotation pending at all", since
/// the secret itself may now live only in the keychain. Secret resolution then
/// mirrors [`load_key`]'s ladder: keychain first, `meta` fallback.
pub async fn load_pending_key(store: &Store) -> Result<Option<(DeviceKey, String)>, Error> {
    let Some(rotated_at) = store
        .meta_get(META_PENDING_ROTATED_AT)
        .await?
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let secret = match keychain_get(KEYCHAIN_ACCOUNT_PENDING) {
        Some(secret) => Some(secret),
        None => store
            .meta_get(META_PENDING_SECRET)
            .await?
            .filter(|s| !s.is_empty()),
    };
    let Some(secret) = secret else {
        return Ok(None);
    };
    Ok(Some((DeviceKey::from_secret_base64(&secret)?, rotated_at)))
}

/// Promote the pending rotation key to ACTIVE — install it exactly like
/// [`install_rotated_key`] — and clear the pending markers. Call this only
/// once the cloud is CONFIRMED to have the pending key installed (a 2xx on the
/// rotation POST, or a successful probe/flush signed with it): see
/// `dira/src/device.rs::resume_rotation` (CLI-driven retry) and
/// `dirad/src/sync.rs::try_pending_key_flush` (daemon self-heal on
/// `SignatureRejected`) for the two call sites. A no-op (not an error) when
/// nothing is pending, so it's safe to call defensively.
pub async fn promote_pending_key(store: &Store) -> Result<(), Error> {
    let Some((key, _rotated_at)) = load_pending_key(store).await? else {
        return Ok(());
    };
    install_rotated_key(store, &key).await?;
    clear_pending_key(store).await
}

/// Discard the pending rotation key WITHOUT promoting it.
///
/// Only call this once a retry has PROVEN the pending key will never become
/// the cloud's active key (a different, concurrent rotation attempt won a
/// race — see `resume_rotation`'s doc comment for exactly when this fires).
/// Safe: the ACTIVE key ([`META_PUBKEY`]/the keychain) is completely
/// untouched, so the device keeps working on whatever key IS currently live;
/// a fresh `rotate-key` starts a brand-new attempt (new keypair) next time.
///
/// Clears BOTH locations the pending secret might be sitting in — the
/// keychain entry (under [`KEYCHAIN_ACCOUNT_PENDING`]) and the `meta`
/// fallback — since [`persist_pending_secret`] may have used either one, and
/// leaving a stale keychain entry behind would let a later `load_pending_key`
/// resurrect a discarded key. Deleting a keychain entry that was never
/// written (the common case — most runs never fall back to meta) is a
/// harmless no-op, same as clearing an already-empty meta key.
pub async fn clear_pending_key(store: &Store) -> Result<(), Error> {
    let _ = keychain_delete(KEYCHAIN_ACCOUNT_PENDING);
    store.meta_set(META_PENDING_SECRET, "").await?;
    store.meta_set(META_PENDING_PUBKEY, "").await?;
    store.meta_set(META_PENDING_ROTATED_AT, "").await
}

/// The cloud-assigned device id, or `None` if the device is not yet linked.
pub async fn device_id(store: &Store) -> Result<Option<String>, Error> {
    Ok(store
        .meta_get(META_DEVICE_ID)
        .await?
        .filter(|s| !s.is_empty()))
}

/// Bind this device to the cloud-assigned id (called after a successful claim).
pub async fn set_device_id(store: &Store, id: &str) -> Result<(), Error> {
    store.meta_set(META_DEVICE_ID, id).await
}

/// Whether the device has been linked (has a non-empty `device_id`).
pub async fn is_linked(store: &Store) -> Result<bool, Error> {
    Ok(device_id(store).await?.is_some())
}

/// Locally unlink the device: clear the cloud-assigned id so the daemon stops
/// syncing, while deliberately KEEPING the signing key + pubkey so a later
/// `link` reclaims the same device identity. The daemon re-reads linkage on its
/// next sync tick, so no restart is needed.
pub async fn clear_device_id(store: &Store) -> Result<(), Error> {
    store.meta_set(META_DEVICE_ID, "").await
}

/// Build a keychain entry under [`KEYCHAIN_SERVICE`] + the given `account` —
/// [`KEYCHAIN_ACCOUNT`] for the active key, [`KEYCHAIN_ACCOUNT_PENDING`] for
/// an in-flight rotation's pending key. Both `dira` and `dirad` resolve the
/// same service+account pairs, so either process can read what the other
/// wrote.
fn keychain_entry(account: &str) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account)
}

/// Read a secret seed from the OS keychain under `account`; `None` on any
/// keychain error or missing entry (callers fall back to the meta-stored
/// seed).
fn keychain_get(account: &str) -> Option<String> {
    keychain_entry(account).ok()?.get_password().ok()
}

/// Store a secret seed in the OS keychain under `account`.
fn keychain_set(account: &str, secret: &str) -> Result<(), keyring::Error> {
    keychain_entry(account)?.set_password(secret)
}

/// Delete a keychain entry under `account`, if one exists. Errors (including
/// "no such entry") are the caller's to ignore — clearing a secret that was
/// never written to the keychain (e.g. it only ever lived in the `meta`
/// fallback) is expected, not a failure.
fn keychain_delete(account: &str) -> Result<(), keyring::Error> {
    keychain_entry(account)?.delete_credential()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    /// Serializes every test that reads or mutates `DIRA_DEVICE_SECRET` *or* the shared
    /// mock keychain. Both are process-global and `cargo test` runs this module's tests
    /// in parallel in one binary, so without this lock the env-override test could leak
    /// the seed into a concurrent `load_or_create_unlinked`, or one test's keychain reset
    /// could clobber another's mid-op — either of which skips the expected pubkey persist.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Acquire [`ENV_LOCK`]. A tokio mutex is used (not `std`) so the guard can be
    /// held across the tests' `.await` points without tripping `await_holding_lock`,
    /// and it never poisons on a failed assert in another test.
    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().await
    }

    /// Removes `DIRA_DEVICE_SECRET` on drop, so a panicking test never leaks it
    /// to the next lock holder.
    struct ClearEnvSecret;
    impl Drop for ClearEnvSecret {
        fn drop(&mut self) {
            std::env::remove_var(ENV_DEVICE_SECRET);
        }
    }

    /// Install a *fresh, empty* keyring-core mock store as the process-global default,
    /// so tests never touch (or block on) the real OS keychain — call it at the start of
    /// every keychain-touching test.
    ///
    /// Unlike keyring 3's mock (a fresh credential per `Entry`), keyring-core's mock is a
    /// single persistent store, so a secret written by one test would otherwise be read
    /// back by the next and skip its mint-and-persist path. Resetting to an empty store
    /// per test restores that isolation; callers serialize via [`env_lock`] so the reset
    /// can't race a concurrent keychain op.
    ///
    /// keyring 4's `v1` `Entry` wrapper lazily installs the platform-native store on its
    /// first `Entry::new`, which would clobber our mock. We force that one-time init now
    /// with a throwaway entry (result ignored — it errors when no platform store exists,
    /// e.g. headless CI, and never touches the keychain) so every later `Entry::new`
    /// reuses the already-fired init and resolves against whichever mock we set here.
    fn use_mock_keychain() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = keyring::Entry::new("dira-test-init", "dira-test-init");
        });
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
    }

    #[tokio::test]
    async fn unlinked_device_has_key_but_no_id() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        let key = load_or_create_unlinked(&store).await.unwrap();
        assert!(!key.public_base64().is_empty());
        assert_eq!(device_id(&store).await.unwrap(), None);
        assert!(!is_linked(&store).await.unwrap());
        // Pubkey is persisted to meta.
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(key.public_base64().as_str())
        );
    }

    #[tokio::test]
    async fn set_device_id_links_the_device() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        load_or_create_unlinked(&store).await.unwrap();
        set_device_id(&store, "01DEVICEULID").await.unwrap();
        assert!(is_linked(&store).await.unwrap());
        assert_eq!(
            device_id(&store).await.unwrap().as_deref(),
            Some("01DEVICEULID")
        );
    }

    #[tokio::test]
    async fn install_rotated_key_swaps_pubkey() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();

        // Installing a fresh key updates the per-store meta pubkey to the new one.
        // We assert on `META_PUBKEY` (per-store, deterministic) rather than reading
        // the secret back through the process-global mock keychain, which is shared
        // across tests in this binary and would race.
        let new = DeviceKey::generate();
        let new_pub = new.public_base64();
        install_rotated_key(&store, &new).await.unwrap();
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(new_pub.as_str())
        );

        // A second rotation swaps it again — idempotent install, last write wins.
        let newer = DeviceKey::generate();
        let newer_pub = newer.public_base64();
        assert_ne!(new_pub, newer_pub);
        install_rotated_key(&store, &newer).await.unwrap();
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(newer_pub.as_str())
        );
    }

    #[tokio::test]
    async fn env_seed_overrides_and_is_never_persisted() {
        let _lock = env_lock().await;
        let _clear = ClearEnvSecret; // drops before _lock → var gone before release
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();

        // A specific seed supplied via the env var must be the loaded key, taking
        // precedence over keychain/meta, and nothing is written to disk.
        let injected = DeviceKey::generate();
        let injected_pub = injected.public_base64();
        std::env::set_var(ENV_DEVICE_SECRET, injected.secret_base64());

        let key = load_or_create_unlinked(&store).await.unwrap();
        assert_eq!(key.public_base64(), injected_pub, "env seed must win");
        // No at-rest meta secret was written — the env owns the seed lifecycle.
        assert_eq!(
            store
                .meta_get(META_SECRET_FALLBACK)
                .await
                .unwrap()
                .as_deref(),
            None
        );

        // A malformed env value is ignored (falls through to generate/persist).
        std::env::set_var(ENV_DEVICE_SECRET, "not-base64!!!");
        let store2 = Store::open_in_memory().await.unwrap();
        let key2 = load_or_create_unlinked(&store2).await.unwrap();
        assert_ne!(key2.public_base64(), injected_pub);
        // `_clear` removes DIRA_DEVICE_SECRET on drop (even on panic).
    }

    #[tokio::test]
    async fn empty_device_id_reads_as_unlinked() {
        let store = Store::open_in_memory().await.unwrap();
        store.meta_set(META_DEVICE_ID, "").await.unwrap();
        assert_eq!(device_id(&store).await.unwrap(), None);
        assert!(!is_linked(&store).await.unwrap());
    }

    #[tokio::test]
    async fn clear_device_id_unlinks_but_keeps_the_key() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();

        // Establish an identity (key + pubkey) and a cloud-assigned id.
        let key = load_or_create_unlinked(&store).await.unwrap();
        let pubkey = key.public_base64();
        set_device_id(&store, "01CLOUDDEVICEID").await.unwrap();
        assert!(is_linked(&store).await.unwrap());

        // Unlink: the id is cleared, but the signing key at rest (pubkey + secret)
        // is deliberately untouched, so a later link reclaims the same identity.
        clear_device_id(&store).await.unwrap();
        assert!(!is_linked(&store).await.unwrap());
        assert_eq!(device_id(&store).await.unwrap(), None);
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(pubkey.as_str()),
            "clear_device_id must not disturb the device pubkey"
        );

        // Re-link binds an id again against the same retained key material.
        set_device_id(&store, "01CLOUDDEVICEID").await.unwrap();
        assert!(is_linked(&store).await.unwrap());
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(pubkey.as_str())
        );
    }

    #[tokio::test]
    async fn clear_device_id_is_a_noop_when_already_unlinked() {
        let store = Store::open_in_memory().await.unwrap();
        assert!(!is_linked(&store).await.unwrap());
        clear_device_id(&store).await.unwrap();
        assert!(!is_linked(&store).await.unwrap());
    }

    #[tokio::test]
    async fn no_pending_key_is_none() {
        let store = Store::open_in_memory().await.unwrap();
        assert!(load_pending_key(&store).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn persist_and_load_pending_key_roundtrips() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        let key = DeviceKey::generate();
        let pub_b64 = key.public_base64();
        persist_pending_key(&store, &key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();

        let (loaded, rotated_at) = load_pending_key(&store).await.unwrap().unwrap();
        assert_eq!(loaded.public_base64(), pub_b64);
        assert_eq!(rotated_at, "2026-07-09T10:00:00Z");
    }

    #[tokio::test]
    async fn promote_pending_key_installs_it_active_and_clears_pending() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();

        let pending = DeviceKey::generate();
        let pending_pub = pending.public_base64();
        persist_pending_key(&store, &pending, "2026-07-09T10:00:00Z")
            .await
            .unwrap();

        promote_pending_key(&store).await.unwrap();

        // Now the ACTIVE pubkey (survives a keychain-mocked install).
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(pending_pub.as_str())
        );
        // Pending markers are cleared.
        assert!(load_pending_key(&store).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn promote_pending_key_with_nothing_pending_is_a_harmless_noop() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        // No pending key was ever persisted.
        promote_pending_key(&store).await.unwrap();
        assert!(load_pending_key(&store).await.unwrap().is_none());
        assert_eq!(store.meta_get(META_PUBKEY).await.unwrap(), None);
    }

    #[tokio::test]
    async fn clear_pending_key_discards_it_without_touching_the_active_key() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();

        // Establish an active key first.
        let active = load_or_create_unlinked(&store).await.unwrap();
        let active_pub = active.public_base64();

        let pending = DeviceKey::generate();
        persist_pending_key(&store, &pending, "2026-07-09T10:00:00Z")
            .await
            .unwrap();
        assert!(load_pending_key(&store).await.unwrap().is_some());

        clear_pending_key(&store).await.unwrap();

        assert!(load_pending_key(&store).await.unwrap().is_none());
        // The active key is completely untouched.
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(active_pub.as_str())
        );
    }

    #[tokio::test]
    async fn a_retry_reuses_the_identical_pending_key_and_timestamp() {
        // The determinism a retried rotation relies on: loading a pending key
        // twice (simulating two `rotate_key` invocations against the same
        // interrupted attempt) must yield the SAME keypair and `rotated_at` —
        // never a freshly-generated one — so a re-POSTed envelope is
        // byte-identical and the cloud's replay guard treats it as the same
        // request, not a new one.
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        let key = DeviceKey::generate();
        persist_pending_key(&store, &key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();

        let (first, first_at) = load_pending_key(&store).await.unwrap().unwrap();
        let (second, second_at) = load_pending_key(&store).await.unwrap().unwrap();
        assert_eq!(first.public_base64(), second.public_base64());
        assert_eq!(first.secret_base64(), second.secret_base64());
        assert_eq!(first_at, second_at);
    }

    // --- Finding 2: the pending key mirrors the active key's keychain-first
    // ladder (rather than always landing in `meta` as plaintext) -----------

    #[tokio::test]
    async fn pending_key_secret_lands_in_the_keychain_not_meta() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        let key = DeviceKey::generate();
        persist_pending_key(&store, &key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();

        // The secret landed in the keychain, under its OWN distinct account
        // (never the active key's) — exactly mirroring `persist_secret`.
        assert_eq!(
            keychain_get(KEYCHAIN_ACCOUNT_PENDING).as_deref(),
            Some(key.secret_base64().as_str())
        );
        // ...and the meta fallback was left empty — the pending seed is NOT
        // sitting at rest in the db as plaintext when the keychain works.
        assert_eq!(
            store
                .meta_get(META_PENDING_SECRET)
                .await
                .unwrap()
                .filter(|s| !s.is_empty()),
            None,
            "the pending secret must not be persisted to meta when the keychain succeeded"
        );

        // load_pending_key resolves it via the keychain.
        let (loaded, rotated_at) = load_pending_key(&store).await.unwrap().unwrap();
        assert_eq!(loaded.secret_base64(), key.secret_base64());
        assert_eq!(rotated_at, "2026-07-09T10:00:00Z");
    }

    #[tokio::test]
    async fn pending_key_falls_back_to_meta_when_the_keychain_has_no_entry() {
        // Mirrors the ACTIVE key's meta-fallback resolution (`load_key`
        // falls through to `META_SECRET_FALLBACK` when the keychain has
        // nothing under its account): the same end state a keychain-
        // unavailable `persist_pending_secret` would have left — the secret
        // sitting ONLY in `META_PENDING_SECRET` — must still resolve.
        let _lock = env_lock().await;
        use_mock_keychain(); // fresh, empty store — no entry under either account
        let store = Store::open_in_memory().await.unwrap();
        let key = DeviceKey::generate();

        store
            .meta_set(META_PENDING_SECRET, &key.secret_base64())
            .await
            .unwrap();
        store
            .meta_set(META_PENDING_PUBKEY, &key.public_base64())
            .await
            .unwrap();
        store
            .meta_set(META_PENDING_ROTATED_AT, "2026-07-09T10:00:00Z")
            .await
            .unwrap();

        let (loaded, rotated_at) = load_pending_key(&store).await.unwrap().unwrap();
        assert_eq!(loaded.secret_base64(), key.secret_base64());
        assert_eq!(rotated_at, "2026-07-09T10:00:00Z");
    }

    #[tokio::test]
    async fn clear_pending_key_clears_both_the_keychain_and_the_meta_fallback() {
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        let key = DeviceKey::generate();
        persist_pending_key(&store, &key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();
        // Confirm it actually landed in the keychain before clearing.
        assert!(keychain_get(KEYCHAIN_ACCOUNT_PENDING).is_some());

        // Also simulate a stale leftover in the meta fallback slot (e.g. an
        // earlier attempt fell back to meta before a working keychain came
        // back) — clearing must not leave THIS behind either.
        store
            .meta_set(META_PENDING_SECRET, "stale-leftover-b64")
            .await
            .unwrap();

        clear_pending_key(&store).await.unwrap();

        assert!(
            keychain_get(KEYCHAIN_ACCOUNT_PENDING).is_none(),
            "the keychain entry must be deleted, not just shadowed by an empty meta value"
        );
        assert_eq!(
            store
                .meta_get(META_PENDING_SECRET)
                .await
                .unwrap()
                .filter(|s| !s.is_empty()),
            None
        );
        assert!(load_pending_key(&store).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn try_pending_key_flush_shares_the_same_keychain_ladder() {
        // The daemon (`dirad::sync::try_pending_key_flush`) and the CLI
        // (`dira::device::resume_rotation`) both resolve a pending key
        // through `load_pending_key`/`promote_pending_key`, which now read
        // the SAME keychain service+account this test writes to directly —
        // this is the shared code path both processes rely on, exercised
        // here without pulling in either binary crate.
        let _lock = env_lock().await;
        use_mock_keychain();
        let store = Store::open_in_memory().await.unwrap();
        let key = DeviceKey::generate();
        persist_pending_key(&store, &key, "2026-07-09T10:00:00Z")
            .await
            .unwrap();

        // A fresh `keyring::Entry` for the SAME service+account (as a second
        // process attaching to the same OS keychain would use) reads it back.
        let reattached = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_PENDING)
            .unwrap()
            .get_password()
            .unwrap();
        assert_eq!(reattached, key.secret_base64());

        promote_pending_key(&store).await.unwrap();
        assert_eq!(
            store.meta_get(META_PUBKEY).await.unwrap().as_deref(),
            Some(key.public_base64().as_str())
        );
    }
}
