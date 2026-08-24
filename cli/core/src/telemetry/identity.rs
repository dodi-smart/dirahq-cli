//! Telemetry install identity: a stable anonymous id and a per-install salt,
//! both persisted in the local store's `meta` table.
//!
//! Distinct from [`crate::identity`] (the device's Ed25519 keypair, used for
//! signed attestation batches): `install_id` identifies a telemetry stream, not
//! a device, and `salt` never leaves the machine — it exists only so
//! [`crate::telemetry::repo_facts::compute`] can hash a repo remote without the
//! hash round-tripping to the plain remote. Neither value is derived from the
//! device key, so disabling telemetry and re-enabling it later does not
//! resurrect a hash computed before the gap (a fresh `dira telemetry reset`,
//! if ever added, would mint both anew).

use crate::store::Store;
use crate::Error;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use getrandom::rand_core::{Rng, UnwrapErr};

/// `meta` key holding the telemetry install id (a ULID, minted once).
pub const META_TELEMETRY_INSTALL_ID: &str = "telemetry_install_id";
/// `meta` key holding the standard-base64 32-byte telemetry salt.
pub const META_TELEMETRY_SALT: &str = "telemetry_salt";

/// A device's telemetry identity: the id events are tagged with, and the salt
/// [`crate::telemetry::repo_facts::compute`] HMACs canonical repo remotes with.
pub struct TelemetryIdentity {
    pub install_id: String,
    pub salt: [u8; 32],
}

/// Load the telemetry identity, minting + persisting whatever is missing on
/// first use. Idempotent: repeated calls against the same store return
/// identical values, mirroring [`crate::identity::load_or_create_unlinked`].
pub async fn load_or_mint(store: &Store) -> Result<TelemetryIdentity, Error> {
    let install_id = match store.meta_get(META_TELEMETRY_INSTALL_ID).await? {
        Some(id) => id,
        None => {
            let id = ulid::Ulid::generate().to_string();
            store.meta_set(META_TELEMETRY_INSTALL_ID, &id).await?;
            id
        }
    };
    let salt = match store.meta_get(META_TELEMETRY_SALT).await? {
        Some(encoded) => decode_salt(&encoded)?,
        None => {
            let bytes = random_salt();
            store
                .meta_set(META_TELEMETRY_SALT, &B64.encode(bytes))
                .await?;
            bytes
        }
    };
    Ok(TelemetryIdentity { install_id, salt })
}

/// 32 random bytes from the OS CSPRNG. Mirrors [`crate::signing::DeviceKey::generate`]'s
/// use of `getrandom`'s infallible `SysRng` — the lightest source already in the
/// tree, so no new randomness dependency is needed for a salt this small.
fn random_salt() -> [u8; 32] {
    let mut rng = UnwrapErr(getrandom::SysRng);
    let mut buf = [0u8; 32];
    rng.fill_bytes(&mut buf);
    buf
}

fn decode_salt(encoded: &str) -> Result<[u8; 32], Error> {
    let bytes = B64
        .decode(encoded.trim())
        .map_err(|e| Error::Decode(format!("bad telemetry salt base64: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| Error::Decode("telemetry salt must be 32 bytes".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mints_an_id_and_salt_on_first_use() {
        let store = Store::open_in_memory().await.unwrap();
        let identity = load_or_mint(&store).await.unwrap();
        assert!(!identity.install_id.is_empty());
        assert_ne!(identity.salt, [0u8; 32]);
    }

    #[tokio::test]
    async fn is_idempotent_across_calls() {
        let store = Store::open_in_memory().await.unwrap();
        let first = load_or_mint(&store).await.unwrap();
        let second = load_or_mint(&store).await.unwrap();
        assert_eq!(first.install_id, second.install_id);
        assert_eq!(first.salt, second.salt);
    }

    #[tokio::test]
    async fn persists_across_separate_loads() {
        let store = Store::open_in_memory().await.unwrap();
        let minted = load_or_mint(&store).await.unwrap();
        let reloaded_id = store
            .meta_get(META_TELEMETRY_INSTALL_ID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(minted.install_id, reloaded_id);
        let reloaded_salt = store.meta_get(META_TELEMETRY_SALT).await.unwrap().unwrap();
        assert_eq!(B64.decode(reloaded_salt).unwrap(), minted.salt.to_vec());
    }
}
