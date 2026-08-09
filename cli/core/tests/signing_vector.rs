//! Guards on `contract/testdata/signing-vector.json` — the deterministic
//! Ed25519 cross-language signing fixture (see `cli/core/src/bin/sign_vector.rs`).
//!
//! The fixture is 819 bytes of pure ASCII today, which exercises none of the
//! byte-escaping rules JCS canonicalizers most commonly disagree on. These
//! tests pin the gap (non-ASCII coverage) and close the other one this repo
//! never checked at all: that the fixture's own signature actually verifies,
//! and that regenerating it in-process reproduces the on-disk bytes exactly
//! (mirrors the CI drift gate, but runnable locally without `just vector`).

use dira_contract::AttestationBatch;
use dira_core::signing::verify_payload;
use std::path::PathBuf;

fn vector_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contract/testdata/signing-vector.json")
}

fn vector_bytes() -> Vec<u8> {
    std::fs::read(vector_path()).expect("read signing-vector.json")
}

#[test]
fn vector_contains_non_ascii() {
    let bytes = vector_bytes();
    assert!(
        bytes.iter().any(|&b| b > 0x7F),
        "signing-vector.json is pure ASCII — it cannot prove the Rust producer and \
         the TypeScript consumer agree on JCS string escaping, which is exactly \
         where two independent canonicalizers are most likely to diverge"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("signing-vector.json is valid JSON");
    let flat = json.to_string();

    assert!(
        flat.contains('\u{0441}') || flat.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)),
        "vector must carry a Cyrillic character (2-byte UTF-8 case)"
    );
    assert!(
        flat.chars().any(|c| (c as u32) > 0xFFFF),
        "vector must carry a non-BMP character (e.g. an emoji), which forces a \
         surrogate pair in any JS-based JCS canonicalizer"
    );
    assert!(
        flat.contains('\u{FFFD}'),
        "vector must carry U+FFFD (the lossy-decode replacement character)"
    );
}

#[test]
fn vector_signature_verifies() {
    let bytes = vector_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("signing-vector.json is valid JSON");

    let public_key = json["publicKey"].as_str().expect("publicKey field");
    let sig = json["sig"].as_str().expect("sig field");
    let payload: AttestationBatch =
        serde_json::from_value(json["payload"].clone()).expect("payload deserializes");

    assert!(
        verify_payload(public_key, sig, &payload).expect("verify_payload"),
        "the signature embedded in signing-vector.json must verify against its own \
         publicKey and payload — nothing else in this repo checks that"
    );
}

#[test]
fn vector_is_not_stale() {
    // Mirrors CI's drift gate (`just vector` + diff) locally: regenerate the
    // exact payload `sign_vector` builds, sign it with the same fixed seed, and
    // assert the JSON produced matches the file on disk byte-for-byte.
    let bytes = vector_bytes();
    let on_disk: serde_json::Value =
        serde_json::from_slice(&bytes).expect("signing-vector.json is valid JSON");

    let regenerated = sign_vector_bin::build_vector();

    assert_eq!(
        regenerated, on_disk,
        "signing-vector.json is stale — regenerate it with `just vector` \
         (never hand-edit; see contract-sync skill)"
    );
}

// Load the actual `sign_vector` binary source as a module so this test
// exercises the exact same `build_vector()` the `just vector` / CI drift gate
// runs, rather than a hand-copied duplicate that could silently drift from it.
#[path = "../src/bin/sign_vector.rs"]
mod sign_vector_bin;
