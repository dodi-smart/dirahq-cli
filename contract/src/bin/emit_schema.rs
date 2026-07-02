//! Emit the JSON Schemas for the wire types to `contract/*.schema.json`.
//!
//! Run via `just contract` (or `cargo run -p dira-contract --bin emit-schema`).
//! The cloud generates its Zod + TS types from the emitted files, so this is the
//! one place the boundary is defined.

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    // Write next to the crate manifest regardless of CWD.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let attestation = schemars::schema_for!(dira_contract::Envelope);
    let attestation_json = serde_json::to_string_pretty(&attestation).expect("schema serializes");
    let attestation_out = dir.join("attestation.schema.json");
    std::fs::write(&attestation_out, attestation_json + "\n")?;
    eprintln!("wrote {}", attestation_out.display());

    let presence = schemars::schema_for!(dira_contract::PresenceEnvelope);
    let presence_json = serde_json::to_string_pretty(&presence).expect("schema serializes");
    let presence_out = dir.join("presence.schema.json");
    std::fs::write(&presence_out, presence_json + "\n")?;
    eprintln!("wrote {}", presence_out.display());

    // The signed key-rotation envelope (Phase 3e): the cloud generates its
    // verifier types from this, so it's emitted alongside the two main channels.
    let rotate = schemars::schema_for!(dira_contract::RotateKeyEnvelope);
    let rotate_json = serde_json::to_string_pretty(&rotate).expect("schema serializes");
    let rotate_out = dir.join("rotate-key.schema.json");
    std::fs::write(&rotate_out, rotate_json + "\n")?;
    eprintln!("wrote {}", rotate_out.display());

    // The signed billing-summary request: a policy-free *query* envelope. The
    // cloud validates it like presence; its money-carrying response is
    // deliberately outside the contract (billing resolves late, in the cloud).
    let billing = schemars::schema_for!(dira_contract::BillingSummaryEnvelope);
    let billing_json = serde_json::to_string_pretty(&billing).expect("schema serializes");
    let billing_out = dir.join("billing-summary.schema.json");
    std::fs::write(&billing_out, billing_json + "\n")?;
    eprintln!("wrote {}", billing_out.display());

    Ok(())
}
