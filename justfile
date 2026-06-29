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
link:
    mkdir -p "{{bin_dir}}"
    ln -sf "{{justfile_directory()}}/target/release/dira" "{{bin_dir}}/dira"
    ln -sf "{{justfile_directory()}}/target/release/dirad" "{{bin_dir}}/dirad"
    @echo "Linked dira + dirad -> {{bin_dir}}"

# Restart the resident daemon from the freshly built binary.
daemon-restart:
    -"{{bin_dir}}/dira" daemon stop
    "{{bin_dir}}/dira" daemon start
    "{{bin_dir}}/dira" daemon status

# ---------- CI aggregate ----------

ci: check test contract
    @echo "CI checks passed"
