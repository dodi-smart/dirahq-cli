# Getting started

Two commands.

```sh
curl -fsSL https://dirahq.sh/install | sh
dira onboard
```

The installer puts `dira` + `dirad` on your PATH and offers to register the daemon as a
login service. `dira onboard` does everything else.

Windows uses `irm https://dirahq.sh/install.ps1 | iex`; see [install.md](install.md).

---

## What `dira onboard` does

Six steps, in dependency order. Each one is idempotent and skippable, and the whole
command is safe to re-run — a second run reports what is already done and picks up the
rest. There is no state file: every step re-derives its own status from the machine,
which is the only version that cannot go stale.

| Step | What it does |
|---|---|
| **detect** | Finds which harnesses, daemon, and repo this machine already has |
| **harnesses** | Wires every detected harness in one pass, at user scope |
| **daemon** | Registers `dirad` with launchd / systemd --user / a scheduled task |
| **device** | Links this device to the cloud — skippable |
| **zavet** | Installs the knowledge plugin, scaffolds this repo's `.zavet/`, sets the knowledge sync tier |
| **verify** | Reports what is still open |

Nothing aborts the run. A step that cannot proceed says why and the wizard continues;
the closing summary collects what is left. A broken harness config should not cost you
the daemon service.

### Harness detection

Each harness is looked for two ways: its CLI on `PATH`, and its config directory under
your home. **Either signal is enough** — a harness can be installed but never run (no
config dir yet), or run via an installer that never put a CLI on `PATH`. Cursor is the
obvious case: the GUI app writes `~/.cursor` and ships no `cursor-agent` unless you ask
for it.

Detected harnesses are offered pre-selected; deselect any you don't want. Already-wired
harnesses are not offered again.

Onboarding writes **user scope** (`~/.claude/settings.json` and friends), unlike a bare
`dira init`, which defaults to the current project. You are setting up a machine, not one
checkout — project scope would silently wire only whichever directory you happened to be
standing in.

### The daemon

`dira onboard` installs `dirad` as a login service so it survives reboots. If a
bare-started daemon is already running, it is stopped first — the control socket is the
single-instance guard, so the service copy cannot bind while the ad-hoc one holds it.
This ordering is the whole reason the step exists; running `dira daemon install` by hand
after `dira daemon start` fails for exactly this reason.

If the service manager refuses (a container with no systemd session, a locked-down
launchd), the wizard falls back to a plain start and says so — capture works now, but
will not survive a reboot.

Decline with `--no-service` or by answering no.

### Device linking

The one step that needs something from outside the terminal. The wizard prints your
dashboard's Connections URL and waits for a one-time code; **press Enter to skip**.

Local capture is fully functional unlinked — only sync and billables need this. Run
`dira device link` whenever you have a code.

### zavet and the knowledge tier

Three things happen here.

**The plugin.** `dira zavet install` shells out to the `claude` CLI. Without `claude` on
PATH the step hands back the manual recipe (`/plugin marketplace add
dodi-smart/dirahq-zavet` from inside Claude Code) rather than half-installing.

**The scaffold.** If cwd is a git repository without `.zavet/`, the wizard runs the
plugin's own `zavet init` and `zavet adapters` against that repo root. It does **not**
run `zavet hooks install` and never writes `core.hooksPath` — that setting is exclusive
and shared with Husky, lefthook, and pre-commit. The git hooks are written to
`.zavet/githooks/`; point git at them yourself if you want them:

```sh
git config core.hooksPath .zavet/githooks
```

The scaffold is structurally complete but **semantically empty**: `RULES.md` ships with a
placeholder. Restart Claude Code and run `/zavet:init` to write your repo's actual
standing rules. Scaffolding needs a POSIX shell, so on Windows this step is skipped in
favour of `/zavet:init`.

**The knowledge tier.** Knowledge sync is a channel of its own, with its own consent —
separate from time tracking, and never implied by linking a device or by billing consent.
The wizard asks about it explicitly:

| Tier | What leaves the machine |
|---|---|
| `off` | Nothing. The default before onboarding. |
| `metadata` | Decision and spec ids, titles, status, guard globs, record hashes |
| `full` | All of the above, **plus record bodies**, commit trailer values, and guard check commands — the text of your decisions and specs |

`full` is the default answer, because it is what makes the cloud's knowledge surfaces
useful. Choose otherwise with `--knowledge metadata` or `--knowledge off`, which also
skips the prompt. The tier is restated in the summary on every path, including `--yes`.

Two caveats the wizard will tell you about:

- Nothing syncs at all until the device is linked — an unlinked machine records the
  setting but the daemon's flush never runs.
- The tier is **half the gate**. Your cloud workspace has its own, and bodies are stored
  only when both ends say `full` (dashboard → ZAVET · KNOWLEDGE SYNC). A
  `content_not_allowed` response in the daemon log means the workspace side is still on
  metadata — that is the double gate working, not a bug.

The tier is ordinary config, changeable anytime:

```sh
dira config set sync.knowledge metadata
dira daemon restart
```

---

## Flags

```
dira onboard [--yes] [--print] [--no-service] [--no-zavet]
             [--harness <id>]... [--knowledge <off|metadata|full>]
```

- `--yes` — accept every default without prompting. Wires all detected harnesses,
  installs the service, installs zavet, sets knowledge to `full`, and skips device
  linking (there is no way to invent a code). This is the CI/scripted form.
- `--print` — show the plan and change nothing. Provably side-effect free: it does not
  even create the local database or invoke `claude`.
- `--no-service` / `--no-zavet` — opt out of one step.
- `--harness <id>` — wire exactly these, bypassing detection. Repeatable. Accepts any
  spelling `dira init` accepts (`claude`, `codex`, `gemini`, `cursor`, `opencode`,
  `grok`). An unknown harness fails before any step runs.
- `--knowledge <tier>` — set the tier and skip its prompt.

Run without a terminal and without `--yes` (a pipe, a CI job), `dira onboard` prints the
plan and exits 0 rather than hanging on a prompt or silently making system changes.

---

## Verifying

```sh
dira doctor           # every check, with a remedy per failure
dira doctor --probe   # + an end-to-end capture proof
```

Exit codes are a contract: `0` all clear, `1` at least one warning, `2` at least one
failure. `dira doctor` diagnoses and never repairs — there is no `--fix`.

```sh
dira status           # today: engaged, agent, compute, unbilled
dira zavet status     # is the knowledge layer active for this repo?
```

---

## Doing it by hand

`dira onboard` is a guided path through commands that all still exist:

```sh
dira init [harness] [--global]   # wire one harness
dira daemon install              # register the login service
dira device link                 # pair with the cloud
dira zavet install               # install the knowledge plugin
dira config set sync.knowledge full
```

See [install.md](install.md) for installer flags and [zavet.md](zavet.md) for the
knowledge module in detail.
