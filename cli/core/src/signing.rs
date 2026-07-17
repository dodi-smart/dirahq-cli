//! Device identity & attestation signing.
//!
//! Each device holds an Ed25519 keypair. Batches are signed over the **canonical
//! JSON (RFC 8785 / JCS)** encoding of the payload, so the signature verifies
//! byte-for-byte across languages — the Rust producer signs and the TypeScript
//! cloud verifies the same bytes. The private key lives in the OS keychain; only
//! the public key and a `deviceId` ever leave the machine.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::Serialize;

use crate::Error;

/// A device signing keypair.
///
/// `Clone` (WP-B1b): `AppState::device_key` caches this behind a `RwLock` so
/// it can be reloaded after a promoted key rotation without holding the lock
/// across every signing call — callers get their own owned copy per read.
/// `ed25519_dalek::SigningKey` is itself `Clone` (it's just the 32-byte
/// seed), so this is cheap.
#[derive(Clone)]
pub struct DeviceKey {
    signing: SigningKey,
}

impl DeviceKey {
    /// Generate a fresh keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        // `SigningKey::generate` wants an infallible `CryptoRng`; `UnwrapErr`
        // panics if the OS entropy source fails, which is the right call for
        // key generation — there is no safe fallback.
        let mut rng = getrandom::rand_core::UnwrapErr(getrandom::SysRng);
        Self {
            signing: SigningKey::generate(&mut rng),
        }
    }

    /// Reconstruct from the 32-byte secret seed.
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(bytes),
        }
    }

    /// The 32-byte secret seed (store this in the keychain, never sync it).
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// Standard-base64 secret seed, for keychain storage.
    pub fn secret_base64(&self) -> String {
        B64.encode(self.secret_bytes())
    }

    /// Restore from a base64 secret seed.
    pub fn from_secret_base64(s: &str) -> Result<Self, Error> {
        let bytes = B64
            .decode(s.trim())
            .map_err(|e| Error::Crypto(format!("bad secret base64: {e}")))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Crypto("secret key must be 32 bytes".into()))?;
        Ok(Self::from_secret_bytes(&arr))
    }

    /// Standard-base64 public key — what gets registered with the cloud.
    pub fn public_base64(&self) -> String {
        B64.encode(self.signing.verifying_key().to_bytes())
    }

    /// Sign a payload over its canonical-JSON encoding; returns a base64 signature.
    pub fn sign_payload<T: Serialize>(&self, payload: &T) -> Result<String, Error> {
        let canonical =
            serde_jcs::to_vec(payload).map_err(|e| Error::Crypto(format!("canonicalize: {e}")))?;
        let sig = self.signing.sign(&canonical);
        Ok(B64.encode(sig.to_bytes()))
    }
}

/// Verify a base64 signature over a payload against a base64 public key. Uses
/// strict verification (rejects malleable / small-order keys).
pub fn verify_payload<T: Serialize>(
    public_base64: &str,
    sig_base64: &str,
    payload: &T,
) -> Result<bool, Error> {
    let pk_bytes: [u8; 32] = B64
        .decode(public_base64.trim())
        .map_err(|e| Error::Crypto(format!("bad public base64: {e}")))?
        .try_into()
        .map_err(|_| Error::Crypto("public key must be 32 bytes".into()))?;
    let verifying = VerifyingKey::from_bytes(&pk_bytes)
        .map_err(|e| Error::Crypto(format!("bad public key: {e}")))?;

    let sig_bytes: [u8; 64] = B64
        .decode(sig_base64.trim())
        .map_err(|e| Error::Crypto(format!("bad signature base64: {e}")))?
        .try_into()
        .map_err(|_| Error::Crypto("signature must be 64 bytes".into()))?;
    let sig = Signature::from_bytes(&sig_bytes);

    let canonical =
        serde_jcs::to_vec(payload).map_err(|e| Error::Crypto(format!("canonicalize: {e}")))?;
    Ok(verifying.verify_strict(&canonical, &sig).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sign_then_verify_roundtrips() {
        let key = DeviceKey::generate();
        let payload = json!({ "batchId": "01ABC", "intervals": [], "n": 3 });
        let sig = key.sign_payload(&payload).unwrap();
        assert!(verify_payload(&key.public_base64(), &sig, &payload).unwrap());
    }

    #[test]
    fn tampered_payload_fails() {
        let key = DeviceKey::generate();
        let payload = json!({ "batchId": "01ABC", "n": 3 });
        let sig = key.sign_payload(&payload).unwrap();
        let tampered = json!({ "batchId": "01ABC", "n": 4 });
        assert!(!verify_payload(&key.public_base64(), &sig, &tampered).unwrap());
    }

    #[test]
    fn canonicalization_is_key_order_independent() {
        // JCS sorts keys, so differently-ordered-but-equal objects verify.
        let key = DeviceKey::generate();
        let a = json!({ "b": 1, "a": 2 });
        let sig = key.sign_payload(&a).unwrap();
        let b = json!({ "a": 2, "b": 1 });
        assert!(verify_payload(&key.public_base64(), &sig, &b).unwrap());
    }

    #[test]
    fn secret_base64_roundtrips() {
        let key = DeviceKey::generate();
        let restored = DeviceKey::from_secret_base64(&key.secret_base64()).unwrap();
        assert_eq!(key.public_base64(), restored.public_base64());
    }

    #[test]
    fn signature_does_not_verify_under_a_different_pubkey() {
        // A signature made by key A must NOT verify under an unrelated key B.
        //
        // This codifies the cloud-side trust rule: `verify_payload` is only
        // meaningful when called with the device's *stored* (registered) pubkey.
        // The cloud must never trust a pubkey carried in the payload itself —
        // which is exactly why the wire `Envelope`/`PresenceEnvelope` deliberately
        // OMIT the pubkey (it lives server-side, keyed by `deviceId`). If the
        // pubkey rode along in the payload, an attacker could sign with their own
        // key A and present A's pubkey, and this check would pass — defeating
        // attestation. Binding verification to the registered key is what makes a
        // forged signature fail.
        let key_a = DeviceKey::generate();
        let key_b = DeviceKey::generate();
        assert_ne!(key_a.public_base64(), key_b.public_base64());

        let payload = json!({ "batchId": "01ABC", "deviceId": "01DEV", "n": 7 });
        let sig = key_a.sign_payload(&payload).unwrap();

        // Verifies under the signer's own (stored) pubkey...
        assert!(
            verify_payload(&key_a.public_base64(), &sig, &payload).unwrap(),
            "must verify under the key that produced the signature"
        );
        // ...but NOT under a different device's pubkey.
        assert!(
            !verify_payload(&key_b.public_base64(), &sig, &payload).unwrap(),
            "a signature from key A must never verify under key B"
        );
    }
}
