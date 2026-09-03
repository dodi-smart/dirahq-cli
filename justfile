# Dira CLI task runner (open source). Run `just` to list tasks.
set shell := ["bash", "-uc"]

default:
    @just --list

# ---------- Rust (capture layer) ----------

# Build the whole Cargo workspace.
build:
    cargo build --workspace

# Build optimized release binaries (dira + dirad).
release:
    cargo build --release -p dira -p dirad

# Run all Rust tests (unit + property + integration).
test:
    cargo test --workspace

# Format + lint; fails on warnings (matches CI).
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# Criterion benchmarks (ingress hot path).
bench:
    cargo bench --workspace

# ---------- Contract seam (source of truth) ----------

# Emit contract/attestation.schema.json from the Rust source of truth.
# The cloud repo vendors this artifact and derives its TS/Zod from it.
contract-schema:
    cargo run -q -p dira-contract --bin emit-schema

# Emit the deterministic cross-language signing fixture the cloud verifies against.
vector:
    cargo run -q -p dira-core --bin sign_vector > contract/testdata/signing-vector.json

# Regenerate both contract artifacts (schema + signing fixture).
contract: contract-schema vector

# Refresh the bundled model-price table from models.dev.
#
# NOT a contract artifact and deliberately NOT part of `just ci`: it needs the
# network, and the table is an estimate for local display only. The cloud keeps
# its own copy, is authoritative, and re-prices historical rows — the two are
# allowed to drift between refreshes. Runs weekly in CI (a monthly cadence once
# left claude-fable-5-1 estimated at the sonnet fallback price for weeks after its
# 2026-09-01 launch); run it by hand too after a model launch or a price change.
#
# Build before the fetch: this used to pipe curl straight into `cargo run`,
# which meant curl's connection sat idle for the ~2 minutes cargo spent
# compiling before it read a byte — the pipe buffer filled, curl blocked, and
# models.dev dropped the connection (the 2026-09-02 CI outage, run 33498614567).
# Building the binary first guarantees nothing is ever waiting in a pipe.
#
# curl's --speed-limit/--speed-time turns a stalled transfer into a retried
# error instead of a multi-minute hang, and --retry covers ordinary transient
# failures. Still writes via a temp file and only `mv`s once `pricing_sync`
# exits 0, so a failed fetch or a rejected payload can never truncate the
# vendored table. The trailing path argument hands the binary the existing
# table so a refresh only ever appends, never drops a retired model id.
pricing-sync:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q -p dira-core --bin pricing_sync
    raw="$(mktemp)"; tmp="$(mktemp)"
    trap 'rm -f "$raw" "$tmp"' EXIT
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 \
         --connect-timeout 20 --max-time 300 --speed-limit 1024 --speed-time 60 \
         -o "$raw" https://models.dev/api.json
    cargo run -q -p dira-core --bin pricing_sync -- cli/core/pricing/models.json \
         < "$raw" > "$tmp"
    mv "$tmp" cli/core/pricing/models.json
    echo "wrote cli/core/pricing/models.json"

# ---------- Local dev loop (dogfooding) ----------

# Run the daemon in the foreground (Ctrl-C to stop).
dev-daemon:
    cargo run -p dirad

# Run the CLI; pass args after `--`, e.g. `just dev-cli -- status`.
dev-cli *ARGS:
    cargo run -q -p dira -- {{ARGS}}

# Wire Claude Code hooks for this repo and start tracking ourselves.
dogfood: release
    ./target/release/dira init
    @echo "Now run: ./target/release/dira daemon start"

# ---------- Global install (dogfood the latest build everywhere) ----------

# Where the binaries are symlinked. Must be on your PATH.
bin_dir := env_var_or_default("DIRA_BIN_DIR", env_var("HOME") + "/.local/bin")

# Build release binaries, symlink them onto PATH, and restart the daemon.
install: release link daemon-restart
    @echo "Installed. `dira` + `dirad` are live from {{bin_dir}} (latest build)."

# Symlink dira + dirad into {{bin_dir}} (idempotent; safe to re-run).
#
# The symlinking itself lives in dev-install.sh rather than inline here. It is
# install-path logic — D-0004's dev-install detection reads the PATH entry with
# `symlink_metadata` and goes blind the moment these become copies — so it
# belongs in a shellcheck'd script beside install.sh, under that decision's
# guard, instead of in a recipe the guard could only reach by claiming the
# whole justfile and every unrelated recipe in it.
link:
    sh dev-install.sh "{{justfile_directory()}}" "{{bin_dir}}"

# Restart the resident daemon from the freshly built binary.
#
# `daemon restart` — NOT `stop` + `start`. Only `restart` runs the supervision
# detection that knows how to replace a daemon it cannot see on the configured
# socket: a pre-D-0008 build still answering on the legacy `$TMPDIR/dira.sock`
# (Supervision::LegacySocket), or one owned by launchd/systemd. `daemon stop`
# only looks for a pidfile beside the CURRENT socket path, so across a
# socket-path change it finds nothing, prints "no pidfile", exits 0 — and the
# old daemon keeps running. `start` then binds the new socket while the old
# process still holds :8722, leaving the new daemon DEGRADED (and, under
# launchd, respawning every 10s against a socket it can never win).
#
# `restart` exits 0 having done nothing when no daemon is running, so `start`
# still follows to cover the fresh-install case; it is allowed to fail (`-`)
# for the commoner case where `restart` already brought one up.
daemon-restart:
    "{{bin_dir}}/dira" daemon restart
    -"{{bin_dir}}/dira" daemon start
    "{{bin_dir}}/dira" daemon status

# ---------- Packaging (reproduce the CI release archive shape locally) ----------

# Where `just install-local` installs into. Deliberately never {{bin_dir}}/~/.local/bin:
# that's the real dogfood install `just install` manages, and install.sh refuses to
# overwrite it anyway. This is a disposable rehearsal of the installer only. Override
# with DIRA_INSTALL_LOCAL_DIR if dist/local-bin doesn't suit; never point it at a real
# PATH-visible bin dir.
local_bin_dir := env_var_or_default("DIRA_INSTALL_LOCAL_DIR", justfile_directory() + "/dist/local-bin")

# Build release binaries, tar dira+dirad flat (no leading dir, matching
# taiki-e/upload-rust-binary-action's `leading-dir: false` default) into
# dist/dira-<version>-<host-target>.tar.gz, and emit a matching .sha256 in the same
# multi-line `sha256sum`-style format (`<hash>  <filename>`) the CI upload action
# produces -- so an archive built here and one built in CI are byte-compatible in shape.
package: release
    mkdir -p dist && \
    stage="$(mktemp -d "${TMPDIR:-/tmp}/dira-package.XXXXXX")" && \
    trap 'rm -rf "$stage"' EXIT && \
    version="$(./target/release/dira --version | head -n1 | awk '{print $2}')" && \
    target="$(rustc -vV | sed -n 's/^host: //p')" && \
    archive="dira-${version}-${target}" && \
    cp target/release/dira target/release/dirad "$stage/" && \
    tar -czf "dist/${archive}.tar.gz" -C "$stage" dira dirad && \
    ( cd dist && \
      if command -v sha256sum >/dev/null 2>&1; then \
        sha256sum "${archive}.tar.gz" >"${archive}.sha256"; \
      else \
        shasum -a 256 "${archive}.tar.gz" >"${archive}.sha256"; \
      fi ) && \
    echo "packaged dist/${archive}.tar.gz + dist/${archive}.sha256"

# Run install.sh end-to-end against a `just package`d archive via a file:// URL, so the
# installer is testable on a laptop with no GitHub release cut. NEVER defaults to
# ~/.local/bin -- see {{local_bin_dir}} above. Extra install.sh flags may be appended,
# e.g. `just install-local -- --daemon`.
install-local *FLAGS: package
    mkdir -p "{{local_bin_dir}}" && \
    version="$(./target/release/dira --version | head -n1 | awk '{print $2}')" && \
    target="$(rustc -vV | sed -n 's/^host: //p')" && \
    DIRA_DOWNLOAD_URL="file://{{justfile_directory()}}/dist" DIRA_VERSION="$version" DIRA_TARGET="$target" DIRA_BIN_DIR="{{local_bin_dir}}" DIRA_NO_UPDATE_CHECK=1 sh install.sh --no-daemon {{FLAGS}} && \
    "{{local_bin_dir}}/dira" --version && \
    echo "installed into {{local_bin_dir}} (scratch dir, not on PATH, never ~/.local/bin)"

# ---------- Dependency licenses ----------

# Mirror CI's license gate (policy + carve-outs live in deny.toml). Deliberately
# NOT part of `just ci`: cargo-deny isn't in mise.toml, so requiring it would
# make the standard pre-PR check depend on a tool most contributors don't have.
# CI runs it on every PR regardless.
licenses:
    @command -v cargo-deny >/dev/null 2>&1 || { \
        echo "cargo-deny not found. Install it with:"; \
        echo "  cargo install cargo-deny --locked"; \
        echo "  # or grab a prebuilt binary from https://github.com/EmbarkStudios/cargo-deny/releases"; \
        exit 1; }
    cargo deny check licenses

# ---------- CI aggregate ----------

ci: check test contract
    @echo "CI checks passed"
