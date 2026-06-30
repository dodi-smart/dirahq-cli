//! Emit a deterministic cross-language signing test vector.
//!
//! Prints `{ publicKey, sig, payload }` where `sig = ed25519(JCS(payload))`. The
//! cloud's `verifyEnvelopeSignature` must accept it — proving the Rust producer
//! and the TypeScript consumer agree on canonicalization byte-for-byte. Used by
//! `cloud/scripts/verify-vector.ts` (piped) and by CI.

use dira_contract::{AttestationBatch, Harness, Interval, SessionKind, SessionRollup};
use dira_core::signing::DeviceKey;

// Fixed 32-byte seed (base64) so the vector is reproducible.
const SEED_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn main() {
    let key = DeviceKey::from_secret_base64(SEED_B64).expect("valid seed");

    let payload = AttestationBatch {
        batch_id: "01TESTBATCH0000000000000000".to_string(),
        device_id: "01TESTDEVICE000000000000000".to_string(),
        generated_at: "2026-06-27T10:00:00Z".to_string(),
        intervals: vec![Interval {
            id: "01INTERVAL00000000000000000".to_string(),
            repo_canonical: Some("github.com/dodi-smart/dira".to_string()),
            identity_email: "asenlekoff@gmail.com".to_string(),
            started_at: "2026-06-27T09:00:00Z".to_string(),
            ended_at: "2026-06-27T09:45:00Z".to_string(),
            human_seconds: 2700,
            activity: None,
            source_session: "01SESSION000000000000000000".to_string(),
        }],
        sessions: vec![SessionRollup {
            session_id: "01SESSION000000000000000000".to_string(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            repo_canonical: Some("github.com/dodi-smart/dira".to_string()),
            identity_email: "asenlekoff@gmail.com".to_string(),
            started_at: "2026-06-27T09:00:00Z".to_string(),
            ended_at: Some("2026-06-27T09:45:00Z".to_string()),
            agent_wall_seconds: 2700,
            prompts: None,
            // Omitted-when-None on the wire, so the signing vector stays byte-identical.
            branch: None,
            note: None,
            label: None,
        }],
        token_usage: vec![],
        artifacts: vec![],
    };

    let sig = key.sign_payload(&payload).expect("sign");
    let out = serde_json::json!({
        "publicKey": key.public_base64(),
        "sig": sig,
        "payload": payload,
    });
    println!("{}", serde_json::to_string(&out).expect("serialize"));
}
