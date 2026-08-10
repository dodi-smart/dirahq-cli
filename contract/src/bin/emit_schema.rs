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

    // The signed knowledge batch (M2): zavet's consent-tiered channel beside
    // attestations — its own endpoint, cursors, and consent gate. Content
    // fields (bodyMd / trailer value) exist in the schema but flow only under
    // double opt-in; see the contract's no-content-fields invariant.
    let knowledge = schemars::schema_for!(dira_contract::KnowledgeEnvelope);
    let knowledge_json = serde_json::to_string_pretty(&knowledge).expect("schema serializes");
    let knowledge_out = dir.join("knowledge.schema.json");
    std::fs::write(&knowledge_out, knowledge_json + "\n")?;
    eprintln!("wrote {}", knowledge_out.display());

    // The RESPONSE side. Every root above is a request envelope, so schemars —
    // which walks from a root — never reaches a type the cloud only ever
    // produces. `ContractResponses` exists purely to be that root: without it
    // the cloud hand-authors each ack with nothing to diff against.
    let responses = schemars::schema_for!(dira_contract::ContractResponses);
    let responses_json = serde_json::to_string_pretty(&responses).expect("schema serializes");
    let responses_out = dir.join("responses.schema.json");
    std::fs::write(&responses_out, responses_json + "\n")?;
    eprintln!("wrote {}", responses_out.display());

    Ok(())
}
