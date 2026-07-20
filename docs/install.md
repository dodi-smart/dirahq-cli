# Installing dira

```sh
curl -fsSL https://dirahq.sh/install | sh
```

This downloads `install.sh` from the landing site (a vendored copy of the script at the
root of this repo — see its header comment) and runs it. The script is POSIX `sh`, does
nothing until its very last line (`main "$@"`), and every download is **sha256-verified
before anything is installed — there is no `--no-verify` escape hatch.**

Flags always beat their matching environment variable. Everything below is read straight
out of `install.sh`'s own `--help` and `dira update --help` — if the two ever disagree with
this file, trust the `--help` output and file a bug.

## Supported targets

| Platform | Target triple | Notes |
|---|---|---|
| macOS, Apple Silicon | `universal-apple-darwin` | Same download as Intel — a `lipo` fat binary |
| macOS, Intel | `universal-apple-darwin` | Same download as Apple Silicon |
| Linux x86_64 | `x86_64-unknown-linux-musl` | Static musl — works on Alpine and old-glibc distros alike |
| Linux arm64 | `aarch64-unknown-linux-musl` | Static musl |
| Windows | — | Not supported natively. Install inside WSL2 and run the script from there; the Linux target then applies. |

There is no `x86_64-apple-darwin` artifact and no glibc (`gnu`) Linux artifact — the
universal macOS binary and the statically-linked musl binaries make both unnecessary.

## Flags

| Flag | Same as | Default |
|---|---|---|
| `--version <VERSION>` | `DIRA_VERSION` | `latest` |
| `--channel <CHANNEL>` | `DIRA_CHANNEL` | `stable` (`stable` \| `prerelease`) |
| `--prerelease` | shorthand for `--channel prerelease` | — |
| `--bin-dir <DIR>` | `DIRA_BIN_DIR` | `${XDG_BIN_HOME:-$HOME/.local/bin}` |
| `--target <TRIPLE>` | `DIRA_TARGET` | auto-detected (see the table above) |
| `--daemon` | `DIRA_START_DAEMON=1` | off — start `dirad` after installing |
| `--service` | `DIRA_INSTALL_SERVICE=1` | off — also run `dira daemon install` (launchd/systemd-user) |
| `--no-daemon` | — | off — never start, restart, or install the daemon, even if one is already running |
| `--force` | — | off — overwrite a `just install` dev symlink; skip the `--uninstall` confirmation |
| `--uninstall` | — | off — remove `dira` + `dirad` (never config or data; see [Uninstalling](#uninstalling)) |
| `-h`, `--help` | — | show usage and exit |

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `DIRA_VERSION` | Exact version to install instead of resolving one | `latest` |
| `DIRA_CHANNEL` | `stable` \| `prerelease` | `stable` |
| `DIRA_BIN_DIR` | Where `dira` + `dirad` are installed | `${XDG_BIN_HOME:-$HOME/.local/bin}` |
| `DIRA_TARGET` | Override target-triple detection | auto-detected |
| `DIRA_REPO` | GitHub repo to install from | `dodi-smart/dirahq-cli` |
| `DIRA_API_URL` | GitHub API base URL | `https://api.github.com` |
| `DIRA_DOWNLOAD_URL` | Override the release-asset base URL (air-gapped / local installs, also accepts `file://`) | unset — derived from `DIRA_REPO` + the resolved tag |
| `GITHUB_TOKEN` / `GH_TOKEN` | Bearer token for the authenticated-asset path (needed while the repo is private); `GH_TOKEN` wins if both are set | unset — unauthenticated public path |
| `DIRA_START_DAEMON` | Same as `--daemon` (set to `1`) | `0` |
| `DIRA_INSTALL_SERVICE` | Same as `--service` (set to `1`) | `0` |
| `DIRA_ALLOW_ROOT` | Silence the "running as root" warning (set to `1`) | `0` |
| `DIRA_DEBUG` | Verbose debug output on stderr (set to `1`) | `0` |

Two more `DIRA_*` variables are read only by the `dira update` self-updater, not by
`install.sh` — see [`dira update` semantics](#dira-update-semantics) below.

## Verifying checksums by hand

Every release attaches one `dira-<version>-<target>.sha256` per archive, in the same
format `sha256sum` itself produces (`<hash>  <filename>`, two spaces). If the tarball and
its `.sha256` are in the same directory, verification is a one-liner:

```sh
curl -fsSLO "https://github.com/dodi-smart/dirahq-cli/releases/download/v<version>/dira-<version>-<target>.tar.gz"
curl -fsSLO "https://github.com/dodi-smart/dirahq-cli/releases/download/v<version>/dira-<version>-<target>.sha256"
sha256sum -c "dira-<version>-<target>.sha256"        # Linux
shasum -a 256 -c "dira-<version>-<target>.sha256"     # macOS
```

`install.sh` does the equivalent internally (`_extract_expected_digest` in the script),
matching the checksum file's filename field rather than trusting line order — a `.sha256`
file may in principle carry more than one line, one per asset built in that release job.

Each release also carries an aggregate `checksums.txt` (all legs' `.sha256` files
concatenated, sorted by filename) purely for human convenience — `install.sh` never reads
it; it always verifies against the per-asset `.sha256` for the exact target it downloaded.

## Manual / air-gapped install

To install without letting `install.sh` reach GitHub at all — a machine with no outbound
network, or one you'd rather not hand a bearer token to — download the tarball and its
checksum through whatever channel you have (another machine, a mirror, a USB stick), put
them in one directory, then point the installer at that directory with `DIRA_DOWNLOAD_URL`
using a `file://` URL:

```sh
mkdir -p /tmp/dira-offline
cp dira-<version>-<target>.tar.gz dira-<version>-<target>.sha256 /tmp/dira-offline/
DIRA_DOWNLOAD_URL="file:///tmp/dira-offline" \
  DIRA_VERSION=<version> \
  DIRA_TARGET=<target> \
  sh install.sh
```

`install.sh` still verifies the checksum in this path — a local tarball you sourced
yourself is not exempt. This is also exactly the mechanism `just install-local` uses in
this repo to test the installer without cutting a real release (see the justfile).

If you'd rather skip the installer entirely: extract the tarball yourself, `chmod 0755`
both `dira` and `dirad`, and copy them onto `$PATH` — there is nothing else the installer
does to the binaries themselves.

## Corporate proxies

`install.sh` has no proxy handling of its own — it shells out to plain `curl` or `wget`,
and both honor the standard `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` (and lowercase)
environment variables natively. Export them before running the installer as you would for
any other `curl`/`wget` invocation:

```sh
export HTTPS_PROXY=http://proxy.example.internal:8080
export NO_PROXY=localhost,127.0.0.1,.example.internal
curl -fsSL https://dirahq.sh/install | sh
```

If your proxy terminates TLS and re-signs with an internal CA, `curl`/`wget` need that CA
trusted at the OS level (or via `CURL_CA_BUNDLE`/`SSL_CERT_FILE`) — this is standard
`curl`/`wget` behavior, nothing dira-specific.

## Adding `~/.local/bin` to PATH

`install.sh` never edits a dotfile for you — under `curl | sh`, stdin *is* the script, so
there's nothing to read a confirmation from safely. If the install directory isn't already
on `PATH`, it prints one of these hints (exactly matching what the script emits):

**fish**
```fish
fish_add_path ~/.local/bin
```

**zsh** — add to `~/.zshrc`, then restart your shell:
```sh
export PATH="$HOME/.local/bin:$PATH"
```

**bash on Linux** — add to `~/.bashrc`, then restart your shell:
```sh
export PATH="$HOME/.local/bin:$PATH"
```

**bash on macOS** — add to `~/.bash_profile` instead (macOS bash doesn't source `.bashrc`
for login shells), then restart your shell:
```sh
export PATH="$HOME/.local/bin:$PATH"
```

**Any other shell** — add to `~/.profile`, then restart your shell:
```sh
export PATH="$HOME/.local/bin:$PATH"
```

(Substitute your actual `$DIRA_BIN_DIR` if you overrode it.)

## Uninstalling

```sh
curl -fsSL https://dirahq.sh/install | sh -s -- --uninstall
# or, from a checkout:
sh install.sh --uninstall
```

This asks for confirmation (unless `--force`), stops the daemon, tears down any
launchd agent (`~/Library/LaunchAgents/sh.dirahq.dirad.plist`) or systemd-user unit
(`~/.config/systemd/user/dirad.service`) it finds — even if the binaries themselves are
already gone — then removes `dira` and `dirad` from the bin directory. It refuses outright
on a `just install` dev symlink; that's not its install to remove.

**What is *not* removed:** config, local capture data, and cache. Uninstalling only ever
touches the two binaries and the service unit. To see exactly where your config lives,
run `dira config path` before uninstalling (the uninstaller prints it too, if `dira` is
still runnable at that point). By default (`ProjectDirs::from("sh", "dirahq", "dira")`):

| | macOS | Linux |
|---|---|---|
| Config (`config.toml`) + local DB (`dira.db`) | `~/Library/Application Support/sh.dirahq.dira/` | `${XDG_CONFIG_HOME:-~/.config}/dira/`, `${XDG_DATA_HOME:-~/.local/share}/dira/` |
| Cache (update-check cache only) | `~/Library/Caches/sh.dirahq.dira/` | `${XDG_CACHE_HOME:-~/.cache}/dira/` |

To remove data too: reinstall dira and run `dira nuke` (`--yes` to skip the prompt), or
delete those directories by hand. `dira nuke` deletes every locally-stored event and token
row — the full statistics history on this device — but **keeps the device identity, signing
key, cloud link, and config**; it does not touch anything already synced to the cloud, and
it does not remove the installed binaries (that's what `--uninstall` is for).

## `dira update` semantics

`dira update` is dira's own self-updater — a Rust reimplementation of `install.sh`'s
resolve/download/verify steps that also does an in-place atomic swap and restarts the
daemon. It reuses `install.sh`'s own environment variables where they overlap
(`DIRA_REPO`, `DIRA_API_URL`, `DIRA_DOWNLOAD_URL`, `DIRA_TARGET`,
`GITHUB_TOKEN`/`GH_TOKEN`), plus its own flags:

| Flag | Default | Behavior |
|---|---|---|
| `--check` | off | Resolve only — never downloads, never touches a binary. Exits `0` in every non-error case, including offline, so it's safe to run speculatively. Also refreshes the cache behind the passive "update available" notice (see below). |
| `--version <VERSION>` | resolve `latest` | Update **or downgrade** to this exact version — downgrading is allowed. |
| `--channel <CHANNEL>` | `stable` | `stable` \| `prerelease` |
| `--force` | off | Skip the dev-install guard — but only for a symlinked (`just install`) dev install. Never bypasses sha256 verification, and never overrides a build actually running out of `target/{release,debug}` (there is no "old" binary to overwrite in that case). |
| `--no-restart` | off | Swap the binaries but leave a running daemon on the old version. |
| `--bin-dir <DIR>` | alongside the running `dira` (`DIRA_BIN_DIR` env, or scanned off `PATH`) | Install directory for the new binaries. |

**Channels and pinning.** `--channel prerelease` opts into the newest `…-develop.N`
build; the default `stable` channel only ever resolves a clean `x.y.z` release. `--version`
overrides channel resolution entirely and pins to (or downgrades to) that exact tag.

**The dev-install refusal.** `dira update` refuses to run at all against a `just install`
setup: a `dira` that is a **symlink** into `target/{release,debug}` is refused unless
`--force` (the symlink is just a pointer — overwriting it costs nothing real); a `dira`
that **is itself** a `target/{release,debug}` build (running via `cargo run`, or invoking
`./target/debug/dira update` directly) is refused *unconditionally*, `--force` included —
there is no old binary to replace, only the build directory `cargo` is about to rebuild
into. Use `just install` to update a contributor checkout instead.

**Atomicity and rollback.** Both binaries are staged into `.{name}.new.<pid>` next to their
final path and `rename(2)`d into place — never opened for writing at the final path, which
would fail `ETXTBSY` against a binary a running `dirad` has mapped executable. `dirad` is
swapped first, then `dira`; a `.bak` hard link of each previous binary is kept until a
post-swap `dira --version` check confirms the new binary actually reports the expected
version, at which point the backups are deleted. Any failure before that point best-effort
rolls both binaries back to their previous content.

**Restart.** Unless `--no-restart` was given, a daemon that was already running is always
restarted after a successful swap (see `dira daemon restart` and the
[troubleshooting](#troubleshooting) table below for how that's supervised). A daemon that
wasn't running beforehand is left stopped.

**The passive update notice.** Separately from `dira update` itself, `dira status`, `dira
version`, and `dira daemon status` print a rate-limited "update available" notice to
**stderr** (never stdout, so `dira status | cat` stays byte-identical) when a newer release
is cached. The check that populates that cache never blocks the foreground command — it's a
detached, fire-and-forget `dira update --check` spawned in the background, at most once
every 24h after a successful check (6h after a failed one). It's suppressed by:

- a non-TTY stderr
- `CI` (any value)
- `NO_UPDATE_NOTIFIER` (any value — the npm-ecosystem convention)
- `DIRA_NO_UPDATE_CHECK=1` (or any value other than `0`)
- the `update.check` config knob set to `off` (`dira config set update.check off`, or
  `DIRA_UPDATE__CHECK=false` — the config layer's usual `__` → `.` env mapping)
- running a `target/{release,debug}` dev build

The first four only suppress *printing*; the config knob and a dev build also stop the
background refresh itself.

## What the installer does and does not touch

`install.sh` and `dira update`:

- **Do** verify a sha256 checksum before installing anything, unconditionally.
- **Do not** edit any dotfile — PATH hints are printed, never applied.
- **Do not** install a launchd/systemd-user service unless you pass `--service` (or ask
  `dira daemon install` directly).
- **Do not** phone home anywhere beyond GitHub (`api.github.com`, or your `DIRA_API_URL`
  override, plus the release-asset host) to resolve and download a release.
- **Do not** touch config, local capture data, or the device's cloud link, ever — only
  `dira nuke` (data) or manual deletion (config) do that, and neither removes the binaries.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `dira: command not found` right after install | The bin dir isn't on `PATH` yet | See [Adding `~/.local/bin` to PATH](#adding-localbin-to-path); start a new shell after editing your rc file |
| `checksum mismatch for dira-<version>-<target>.tar.gz` | Corrupted download, a captive/corporate proxy rewriting the response, or a genuinely tampered mirror | Re-run the install (transient network corruption is common); if it persists behind a proxy, compare against the `.sha256` [by hand](#verifying-checksums-by-hand) from an unproxied network |
| `dira update`/`install.sh` restart reports `could not restart the launchd agent automatically` | `launchctl kickstart` and the `launchctl stop` fallback both failed (agent not loaded, wrong user session) | Run the printed command yourself: `launchctl kickstart -k gui/$(id -u)/sh.dirahq.dirad` |
| `dira update`/`install.sh` restart reports `could not restart the systemd-user unit automatically` | Usually a **headless box without `loginctl enable-linger`**, so `systemctl --user` can't reach the user's systemd instance outside an active login session | `sudo loginctl enable-linger "$USER"`, then retry `dira daemon restart`; or run `systemctl --user restart dirad.service` yourself once linger is enabled |
| Install refuses with `... is a 'just install' dev build symlinked at ... refusing to overwrite it` | You're installing over a contributor checkout's `just install` symlink | Pass `--force` if you really want a real install there, or `just install` to update the dev build instead |
| `dira update` refuses even with `--force` | The **running** `dira` is itself a `target/{release,debug}` build (e.g. `cargo run -p dira -- update`) | There's no installed binary to replace — use an actual installed `dira`, or `just install` |
| Need `jq` and don't have it | You set `GITHUB_TOKEN`/`GH_TOKEN` — the authenticated asset-resolution path requires `jq`; the normal unauthenticated path never does | Unset the token for a normal public install, or install `jq` |
