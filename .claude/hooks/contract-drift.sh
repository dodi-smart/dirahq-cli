#!/usr/bin/env bash
# PostToolUse hook — guard the contract codegen seam.
#
# The wire schema is authored in Rust under /contract (serde + schemars). Editing
# those types invalidates BOTH derived artifacts: contract/attestation.schema.json
# and the cloud's generated TS/Zod (cloud/src/lib/contract). Two separate CI jobs
# fail on drift. When such a file changes, inject a reminder to run `just contract`.
set -euo pipefail

input=$(cat)
file=$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')

case "$file" in
  *contract/*.rs) ;;
  *) exit 0 ;;
esac

jq -nc '{
  hookSpecificOutput: {
    hookEventName: "PostToolUse",
    additionalContext: "You edited contract Rust types under /contract. Regenerate the derived artifacts with `just contract` (emits exactly two files: contract/attestation.schema.json and contract/testdata/signing-vector.json — the cloud repo vendors these separately). CI has separate drift gates for both — skipping this will turn CI red."
  }
}'

exit 0
