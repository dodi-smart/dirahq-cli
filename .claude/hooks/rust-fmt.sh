#!/usr/bin/env bash
# PostToolUse hook — format edited Rust files with rustfmt.
#
# Matches the `cargo fmt --all -- --check` CI gate so formatting never causes a
# red-CI round-trip. Reads the hook JSON on stdin, no-ops for non-Rust files, and
# is purely advisory: a rustfmt failure never blocks the edit.
set -euo pipefail

input=$(cat)
file=$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')

[[ "$file" == *.rs ]] || exit 0
[[ -f "$file" ]] || exit 0

# Pin the edition to the workspace's so single-file formatting matches `cargo fmt`.
# Use the mise-managed toolchain when available (Rust isn't on the bare PATH here).
if command -v mise >/dev/null 2>&1; then
  mise exec -- rustfmt --edition 2021 "$file" 2>/dev/null || true
else
  rustfmt --edition 2021 "$file" 2>/dev/null || true
fi

exit 0
