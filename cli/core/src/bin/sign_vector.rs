//! Emit a deterministic cross-language signing test vector.
//!
//! Prints `{ publicKey, sig, payload }` where `sig = ed25519(JCS(payload))`.
//! What this actually proves, locally:
//! - **Deterministic regeneration**: CI (`.github/workflows/ci.yml`) reruns
//!   `just vector` and diffs against the committed file — a byte-for-byte
//!   drift gate on the Rust producer alone.
//! - **The signature verifies**: `cli/core/tests/signing_vector.rs` loads this
//!   fixture back and checks `sig` against `publicKey` with the real
//!   `dira_core::signing::verify_payload`, which nothing previously did.
//! - **Non-ASCII survives JCS**: the payload below carries a Cyrillic string, a
//!   non-BMP emoji (forces a surrogate pair in any JS canonicalizer), and
//!   U+FFFD — the byte classes most likely to make two independent RFC 8785
//!   implementations diverge, and previously exercised nowhere.
//!
//! What it does **not** prove: that the cloud's TypeScript canonicalizer
//! produces the same bytes. True cross-language parity is checked by the
//! cloud's `verify-vector.ts`, run there against the *released* asset (this
//! fixture is attached to every GitHub release) — this repo has no TypeScript
//! toolchain to verify that itself.

use dira_contract::{AttestationBatch, Harness, Interval, SessionKind, SessionRollup};
use dira_core::signing::DeviceKey;

// Fixed 32-byte seed (base64) so the vector is reproducible.
const SEED_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

// Every value below is synthetic on purpose, and must stay that way: this
// fixture is attached as an asset to every GitHub release and is vendored by
// the cloud repo, so anything in it is public and permanent with no retraction
// path. `example.com` is reserved by RFC 2606 §3 — it can never be registered,
// so the address can never route or be harvested. Never substitute a real
// address here, personal or corporate; the value only exists to make the JCS
// byte stream non-trivial and carries no information. The Cyrillic strings are
// generic dictionary words ("meeting", "review", "test"), never anything
// personal, and the note below folds in an emoji and U+FFFD purely to force
// the surrogate-pair / replacement-character escaping paths.

/// Build the exact payload `main` signs and prints. Split out (rather than
/// inlined in `main`) so `cli/core/tests/signing_vector.rs` can load this file
/// as a module (`#[path = "../src/bin/sign_vector.rs"]`) and call it directly
/// — the test and the fixture must build from one source, or the test would
/// only be checking a copy-paste of itself.
pub fn build_vector() -> serde_json::Value {
    let key = DeviceKey::from_secret_base64(SEED_B64).expect("valid seed");

    let payload = AttestationBatch {
        batch_id: "01TESTBATCH0000000000000000".to_string(),
        device_id: "01TESTDEVICE000000000000000".to_string(),
        generated_at: "2026-06-27T10:00:00Z".to_string(),
        intervals: vec![Interval {
            id: "01INTERVAL00000000000000000".to_string(),
            repo_canonical: Some("github.com/dodi-smart/dira".to_string()),
            identity_email: "dev@example.com".to_string(),
            started_at: "2026-06-27T09:00:00Z".to_string(),
            ended_at: "2026-06-27T09:45:00Z".to_string(),
            human_seconds: 2700,
            activity: Some("среща".to_string()),
            source_session: "01SESSION000000000000000000".to_string(),
        }],
        sessions: vec![SessionRollup {
            session_id: "01SESSION000000000000000000".to_string(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            repo_canonical: Some("github.com/dodi-smart/dira".to_string()),
            identity_email: "dev@example.com".to_string(),
            started_at: "2026-06-27T09:00:00Z".to_string(),
            ended_at: Some("2026-06-27T09:45:00Z".to_string()),
            agent_wall_seconds: 2700,
            prompts: None,
            // `branch`/`note`/`label` are populated on purpose (not omitted, as
            // they used to be) to exercise non-ASCII JCS escaping — see the
            // module doc.
            branch: Some("feat/тест".to_string()),
            note: Some(
                "преглед на сесията 📝 — transcript decode had a stray \u{FFFD} byte".to_string(),
            ),
            label: Some("ревю".to_string()),
        }],
        token_usage: vec![],
        artifacts: vec![],
    };

    let sig = key.sign_payload(&payload).expect("sign");
    serde_json::json!({
        "publicKey": key.public_base64(),
        "sig": sig,
        "payload": payload,
    })
}

// `#[allow(dead_code)]`: this file is also loaded as a module by
// `cli/core/tests/signing_vector.rs` (to call `build_vector` from the same
// source the binary uses), where `main` itself is never invoked.
#[allow(dead_code)]
fn main() {
    let out = build_vector();
    println!("{}", serde_json::to_string(&out).expect("serialize"));
}
