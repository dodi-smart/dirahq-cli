#!/bin/sh
# Cloud-capture smoke test: does a REAL agent session in a simulated cloud
# runtime actually reach dira?
#
# Everything else in the suite tests a half: the unit tests pin what
# `dira cloud init` writes, `cloud_init_e2e` pins the artifacts on disk, and
# `doctor --probe` drives dira's own synthetic hook. None of them prove the
# thing the cloud story rests on — that a harness we do not control, wired
# only through committed config, fires hooks that land as counted events.
# This runs Claude Code headless against a throwaway repo wired by
# `dira cloud init`, then asserts on dira's own JSON.
#
# Usage:  sh .github/scripts/cloud-capture-smoke.sh
#
# Environment:
#   CLAUDE_CODE_OAUTH_TOKEN  auth for the agent run. Absent (fork PRs) ⇒ SKIP,
#                            not failure. ANTHROPIC_API_KEY also works.
#   DIRA_SMOKE_BIN_DIR       directory holding built dira + dirad
#                            (default: <repo>/target/debug)
#   DIRA_SMOKE_MODEL         model for the run (default: a cheap one)
#   DIRA_SMOKE_BUDGET_USD    hard spend cap for the run (default: 0.50)
#
# Exit: 0 pass or skip, non-zero on a real failure.

set -eu

log() { printf 'cloud-capture: %s\n' "$*" >&2; }
die() { log "FAIL — $*"; exit 1; }

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
bin_dir="${DIRA_SMOKE_BIN_DIR:-$repo_root/target/debug}"
model="${DIRA_SMOKE_MODEL:-claude-haiku-4-5-20251001}"
budget="${DIRA_SMOKE_BUDGET_USD:-0.50}"

# The magic word never appears in the prompt, so a reply carrying it proves
# the agent actually read the fixture through a tool call — which is what
# produces the PreToolUse/PostToolUse events dira counts as agent activity.
magic="PLATANOTELEFONO"

# ---- preconditions -------------------------------------------------------

# DIRA_SMOKE_ASSUME_AUTH covers environments that authenticate the CLI by some
# means other than these two variables (a cloud session hands its token to the
# CLI over a file descriptor, for instance) — without it this test can never
# run in one, which is exactly where it is most worth running.
if [ -z "${DIRA_SMOKE_ASSUME_AUTH:-}" ] &&
  [ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ] && [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  log "SKIP: no CLAUDE_CODE_OAUTH_TOKEN / ANTHROPIC_API_KEY in the environment."
  log "SKIP: (expected on pull requests from forks — secrets are not exposed there.)"
  log "SKIP: set DIRA_SMOKE_ASSUME_AUTH=1 if the CLI is authenticated another way."
  exit 0
fi
command -v claude >/dev/null 2>&1 || die "the 'claude' CLI is not on PATH"
command -v jq >/dev/null 2>&1 || die "jq is not on PATH"
for b in dira dirad; do
  [ -x "$bin_dir/$b" ] || die "no executable $bin_dir/$b — build first (cargo build -p dira -p dirad)"
done

work=$(mktemp -d)
cleanup() {
  cd /
  "$bin_dir/dira" daemon stop >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

# ---- isolation -----------------------------------------------------------
# Every path dira writes is redirected into the temp dir, so the smoke test
# cannot touch a developer's real store, socket or cache when run locally
# (D-0021's rule, in shell form). HOME is deliberately NOT redirected: the
# agent's own credentials live there.
export DIRA_DB_PATH="$work/dira.db"
export DIRA_SOCKET_PATH="$work/dira.sock"
export DIRA_HTTP_PORT="${DIRA_SMOKE_HTTP_PORT:-18722}"
export XDG_CACHE_HOME="$work/cache"
export PATH="$bin_dir:$PATH"
# Never linked, so no batch can leave the machine no matter what cloud_url
# resolves to. This test is about capture, not sync.
export DIRA_CLOUD_URL=""

# ---- a throwaway repo, wired the way a cloud repo would be ---------------

project="$work/repo"
mkdir -p "$project"
cd "$project"
git init -q -b main
git config user.email "smoke@dirahq.sh"
git config user.name "Dira Smoke"
git config commit.gpgsign false
# A resolvable remote so the writer's git enrichment has a canonical ref to
# find — asserting on it below is what proves enrichment ran, not just ingest.
git remote add origin https://github.com/dira-smoke/cloud-capture.git
printf '# Fixture\n\nThe magic word is %s.\n' "$magic" >FIXTURE.md
git add -A
git commit -q -m "chore(repo): cloud capture fixture"

# --no-pin: the fixture repo has no real GitHub release to fetch a digest
# from, and doesn't need one — the binaries under test are already on PATH
# (see the short-circuit note below), so bootstrap.sh's own download path
# never runs either way. Skipping the fetch keeps this test independent of
# GitHub's release-asset API being reachable/rate-limited in CI.
log "wiring the fixture repo with 'dira cloud init'"
dira cloud init --harness claude --no-pin >"$work/cloud-init.log" 2>&1 \
  || { cat "$work/cloud-init.log" >&2; die "dira cloud init failed"; }
[ -f .dira/bootstrap.sh ] || die "cloud init wrote no .dira/bootstrap.sh"
[ -f .claude/settings.json ] || die "cloud init wrote no .claude/settings.json"

# ---- simulate the cloud runtime -----------------------------------------
# This is the whole point of the exercise: CLAUDE_CODE_REMOTE is the marker
# Claude Code sets inside its own cloud VMs and never locally, so setting it
# here drives the exact bootstrap branch a real cloud session takes —
# including starting the daemon, which nothing else in CI does.
export CLAUDE_CODE_REMOTE=true
export CLAUDE_CODE_REMOTE_SESSION_ID="cse_smoke_${$}"
export DIRA_BOOTSTRAP_DEBUG=1
# The binaries are already on PATH, so the bootstrap's download path
# short-circuits and the freshly built code under test is what runs.

log "running the agent (model=$model, budget=\$$budget)"
set +e
claude -p "Read FIXTURE.md in the current directory and reply with only the magic word it contains." \
  --model "$model" \
  --max-budget-usd "$budget" \
  --permission-mode dontAsk \
  --allowedTools "Read" \
  --output-format json \
  </dev/null >"$work/claude.json" 2>"$work/claude.err"
agent_status=$?
set -e
if [ "$agent_status" -ne 0 ]; then
  log "the agent run exited $agent_status; stderr follows:"
  sed -n '1,40p' "$work/claude.err" >&2
  die "agent run failed — cannot judge capture from a session that never ran"
fi

# A reply containing the magic word proves the agent really used its Read
# tool. If it did not, agent-activity assertions below would be judging a
# session that genuinely had no tool calls.
if ! grep -q "$magic" "$work/claude.json"; then
  log "warning: the reply did not contain the magic word — the agent may not have"
  log "warning: used a tool. Capture assertions continue, but tool events may be absent."
fi

# ---- did dira kick in? ---------------------------------------------------
# The daemon coalesces and enriches off the hot path, so give the writer a
# moment to drain before reading. Bounded, the loop exits as soon as the
# session shows up, and every assertion below reads the SAME capture the loop
# accepted — no second `dira status` that could see different state.
log "waiting for the writer to drain"
status_json="$work/status.json"
i=0
while :; do
  # A failed capture truncates the file so stale JSON can't pass assertions.
  dira status --json >"$status_json" 2>"$work/status.err" || : >"$status_json"
  if jq -e '.today.session_count > 0' "$status_json" >/dev/null 2>&1; then
    break
  fi
  i=$((i + 1))
  [ "$i" -lt 20 ] || break
  sleep 0.5
done

# The daemon being up at all IS the assertion that the SessionStart hook
# landed and was forwarded end to end: `.dira/bootstrap.sh` is the ONLY
# generated command wired to `SessionStart` (every other harness event goes
# straight to `.dira/hook.sh`, which never starts anything), and `dira hook`
# never auto-starts the daemon on its own. There is no field on `dira
# status --json`/`dira sessions --json` that names a specific event kind
# (`SessionView` reports timers and activity flags, never a per-event-kind
# breakdown — see cli/core/src/protocol.rs), so this process-level check is
# the strongest available signal that the bootstrap's own SessionStart
# forward — not just some later hook event — is what got things running.
dira daemon status >/dev/null 2>&1 || die "the daemon is not running — the bootstrap's SessionStart hook never ran (or never started it)"
[ -s "$status_json" ] || { cat "$work/status.err" >&2; die "dira status --json failed"; }

sessions=$(jq -r '.today.session_count // 0' "$status_json")
[ "$sessions" -ge 1 ] || {
  log "status was: $(cat "$status_json")"
  die "dira captured no sessions at all — the hooks never reached the daemon"
}

# The daemon-up + session-count checks above prove SOME event landed, but not
# specifically that the bootstrap's own SessionStart forward is what did it —
# query the store directly for the row `dira_core::model::EventKind::SessionStart`
# writes (`enum_str`'s snake_case wire form, "session_start", in the `events`
# table's `kind` column). Best-effort: a missing `sqlite3` makes no claim
# rather than failing a smoke test over a tooling gap.
if command -v sqlite3 >/dev/null 2>&1; then
  session_start_rows=$(sqlite3 "$DIRA_DB_PATH" \
    "SELECT COUNT(*) FROM events WHERE kind = 'session_start';" 2>"$work/sqlite3.err") ||
    die "could not query $DIRA_DB_PATH for session_start events: $(cat "$work/sqlite3.err")"
  [ "${session_start_rows:-0}" -ge 1 ] ||
    die "no session_start event in the store — the bootstrap's own SessionStart forward never reached dirad"
else
  log "SKIP: sqlite3 is not on PATH — cannot verify a session_start row directly, relying on dira status --json alone"
fi

# Enrichment: the writer resolved the fixture's remote to a canonical ref.
# This is a strictly stronger claim than "an event arrived".
if ! jq -e '[.today.projects[].project] | any(. == "github.com/dira-smoke/cloud-capture")' \
  "$status_json" >/dev/null 2>&1; then
  log "status was: $(cat "$status_json")"
  die "no session was attributed to the fixture repo — git enrichment did not run"
fi

agent_seconds=$(jq -r '.today.total_agent_seconds // 0' "$status_json")

log "PASS — sessions=$sessions agent_seconds=$agent_seconds project=github.com/dira-smoke/cloud-capture"
log "PASS — a real Claude Code session in a simulated cloud runtime was captured by dira."
