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

/// Keychain service name for the device secret seed.
const KEYCHAIN_SERVICE: &str = "sh.dirahq.dira";
/// Keychain account (entry) name for the device secret seed.
const KEYCHAIN_ACCOUNT: &str = "device-secret";
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
    if let Some(secret) = keychain_get() {
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
    if keychain_set(&secret).is_ok() {
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

fn keychain_entry() -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
}

/// Read the secret seed from the OS keychain; `None` on any keychain error or
/// missing entry (callers fall back to the meta-stored seed).
fn keychain_get() -> Option<String> {
    keychain_entry().ok()?.get_password().ok()
}

/// Store the secret seed in the OS keychain.
fn keychain_set(secret: &str) -> Result<(), keyring::Error> {
    keychain_entry()?.set_password(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    /// Serializes every test that reads or mutates `DIRA_DEVICE_SECRET`. The env
    /// var is process-global and `cargo test` runs this module's tests in parallel
    /// in one binary, so without this lock the env-override test could leak the
    /// seed into a concurrent `load_or_create_unlinked` and skip its pubkey persist.
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

    /// Install keyring's in-memory mock credential store once per test process,
    /// so tests never touch (or block on) the real OS keychain. This is the
    /// documented test hook; production uses the platform-native store.
    fn use_mock_keychain() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
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
}
