#!/bin/sh
# install.sh -- installer for dira + dirad (https://dirahq.sh)
#
#   curl -fsSL https://dirahq.sh/install | sh
#   curl -fsSL https://dirahq.sh/install | sh -s -- --channel prerelease
#
# This file is POSIX `sh` only: no `pipefail`, no `[[ ]]`, no arrays, no
# `${var,,}`. It DOES use `local` inside functions -- that is not in the
# POSIX spec proper, but every shell that ships as `/bin/sh` in practice
# (dash, ash/busybox, bash's POSIX mode, zsh's sh emulation) implements it
# as a de facto extension, so it is safe here. ShellCheck's SC3043 ("local
# is undefined in POSIX sh") is a known-safe informational note and is
# disabled file-wide below.
#
# Truncation safety: apart from `set -eu` below -- which only sets shell
# options and touches nothing outside this process -- everything in this file
# is a function definition, and nothing is CALLED until the very last line,
# `main "$@"`. If a `curl | sh` transfer is cut short, the shell either hits
# EOF mid function body (a parse error, so nothing at all executes) or
# reaches EOF having only ever *defined* functions, never calling any of
# them. Do not add a top-level statement with side effects anywhere in this
# file, and do not call anything except the final `main "$@"`.
#
# shellcheck disable=SC3043

set -eu

# ---------------------------------------------------------------------------
# output helpers
# ---------------------------------------------------------------------------

_is_tty() {
  [ -t 1 ]
}

info() {
  printf '%s\n' "$*" >&2
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

err() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

debug_log() {
  if [ "${debug:-0}" = "1" ]; then
    printf 'debug: %s\n' "$*" >&2
  fi
}

# _confirm <prompt> -- reads y/N from /dev/tty, never from stdin (stdin *is*
# the script itself under `curl | sh`). When stdout isn't a terminal there is
# nobody to ask, so we proceed rather than hang a non-interactive install.
_confirm() {
  local prompt="$1" reply
  if ! _is_tty; then
    return 0
  fi
  printf '%s [y/N] ' "$prompt" >&2
  if [ -r /dev/tty ]; then
    read -r reply <"/dev/tty" || reply=""
  else
    reply=""
  fi
  case "$reply" in
  y | Y | yes | YES) return 0 ;;
  *) return 1 ;;
  esac
}

# ---------------------------------------------------------------------------
# usage
# ---------------------------------------------------------------------------

usage() {
  cat <<'EOF'
dira installer

USAGE:
    curl -fsSL https://dirahq.sh/install | sh
    curl -fsSL https://dirahq.sh/install | sh -s -- [FLAGS]

    Flags after `--` are passed through to install.sh. For example, to
    install the newest prerelease build instead of the latest stable one:

        curl -fsSL https://dirahq.sh/install | sh -s -- --channel prerelease

    Or download the script and run it directly:

        sh install.sh [FLAGS]

FLAGS:
    --version <VERSION>    Install this exact version instead of resolving one (default: latest)
    --channel <CHANNEL>    stable | prerelease                                 (default: stable)
    --prerelease            Shorthand for --channel prerelease
    --bin-dir <DIR>         Where to install dira + dirad
                            (default: ${XDG_BIN_HOME:-$HOME/.local/bin})
    --target <TRIPLE>       Override target-triple detection
    --daemon                Start dirad after installing
    --service                Also install dirad as a launchd/systemd-user service
    --no-daemon              Never start, restart, or install the daemon -- even if
                              one is already running
    --force                  Overwrite a `just install` dev symlink; skip the
                              --uninstall confirmation
    --uninstall              Remove dira + dirad (never touches config or data --
                              see `dira nuke`)
    -h, --help                Show this help and exit

ENVIRONMENT:
    DIRA_VERSION              Same as --version                          (default: latest)
    DIRA_CHANNEL               Same as --channel                           (default: stable)
    DIRA_BIN_DIR                Same as --bin-dir
    DIRA_TARGET                  Same as --target
    DIRA_REPO                     GitHub repo to install from               (default: dodi-smart/dirahq-cli)
    DIRA_API_URL                   GitHub API base URL                        (default: https://api.github.com)
    DIRA_DOWNLOAD_URL                Override the release-asset base URL (air-gapped / local installs)
    GITHUB_TOKEN / GH_TOKEN            Bearer token for the private-repo asset path
                                        (GH_TOKEN wins if both are set)
    DIRA_START_DAEMON                   Same as --daemon        (set to 1)
    DIRA_INSTALL_SERVICE                 Same as --service        (set to 1)
    DIRA_ALLOW_ROOT                       Silence the "running as root" warning (set to 1)
    DIRA_DEBUG                             Verbose debug output on stderr (set to 1)

    Flags always beat their matching environment variable.

Every download is sha256-verified before anything is installed -- there is
no --no-verify escape hatch. See docs/install.md for manual checksum
verification, air-gapped installs, proxies, and troubleshooting.
EOF
}

# ---------------------------------------------------------------------------
# preflight
# ---------------------------------------------------------------------------

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"
}

preflight() {
  need_cmd uname
  need_cmd tar
  need_cmd mktemp
  if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    err "need curl or wget to download dira (neither is on PATH)"
  fi
  if ! command -v sha256sum >/dev/null 2>&1 &&
    ! command -v shasum >/dev/null 2>&1 &&
    ! command -v openssl >/dev/null 2>&1; then
    err "need sha256sum, shasum, or openssl to verify checksums (none is on PATH)"
  fi
  if [ "$(id -u 2>/dev/null || printf '0')" = "0" ] && [ "$allow_root" != "1" ]; then
    warn "running as root -- dira is normally installed per-user. Continuing anyway (set DIRA_ALLOW_ROOT=1 to silence this)."
  fi
}

# ---------------------------------------------------------------------------
# OS / arch / target detection
# ---------------------------------------------------------------------------

# Darwin always resolves to universal-apple-darwin: the release ships a lipo
# fat binary covering Apple Silicon and Intel from one artifact, so there is
# deliberately no Rosetta check and no Intel-Mac error path here.
detect_target() {
  local os
  os=$(uname -s)
  case "$os" in
  Darwin)
    printf '%s\n' universal-apple-darwin
    ;;
  Linux)
    local arch
    arch=$(uname -m)
    case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) err "unsupported Linux architecture: $arch (supported: x86_64, aarch64)" ;;
    esac
    printf '%s-unknown-linux-musl\n' "$arch"
    ;;
  MINGW* | MSYS* | CYGWIN* | Windows_NT)
    err "native Windows is not supported. Install inside WSL2 (Windows Subsystem for Linux) and run this script from there: https://learn.microsoft.com/windows/wsl/install"
    ;;
  *)
    err "unsupported OS: $os (supported targets: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, universal-apple-darwin)"
    ;;
  esac
}

# ---------------------------------------------------------------------------
# POSIX semver compare (sort -V is not POSIX and mis-orders prereleases like
# 0.1.0-develop.9 vs 0.1.0-develop.10)
# ---------------------------------------------------------------------------

_is_numeric() {
  case "$1" in
  '' | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

# POSIX-portable string ordering via a byte-wise `sort`, since `[ a > b ]`
# is not defined by POSIX `test`.
_str_gt() {
  [ "$1" != "$2" ] || return 1
  [ "$(printf '%s\n%s\n' "$1" "$2" | LC_ALL=C sort | tail -n1)" = "$1" ]
}

_semver_prerelease_gt() {
  local ap="$1" bp="$2" af bf arest brest
  while :; do
    if [ -z "$ap" ] && [ -z "$bp" ]; then return 1; fi
    if [ -z "$ap" ]; then return 1; fi
    if [ -z "$bp" ]; then return 0; fi
    case "$ap" in
    *.*)
      af="${ap%%.*}"
      arest="${ap#*.}"
      ;;
    *)
      af="$ap"
      arest=""
      ;;
    esac
    case "$bp" in
    *.*)
      bf="${bp%%.*}"
      brest="${bp#*.}"
      ;;
    *)
      bf="$bp"
      brest=""
      ;;
    esac
    if _is_numeric "$af" && _is_numeric "$bf"; then
      if [ "$af" -gt "$bf" ]; then return 0; fi
      if [ "$af" -lt "$bf" ]; then return 1; fi
    else
      if _is_numeric "$af"; then return 1; fi
      if _is_numeric "$bf"; then return 0; fi
      if [ "$af" != "$bf" ]; then
        if _str_gt "$af" "$bf"; then return 0; else return 1; fi
      fi
    fi
    ap="$arest"
    bp="$brest"
  done
}

# _semver_gt A B -- true (0) if bare version A ("MAJOR.MINOR.PATCH[-PRE]",
# no leading "v") is greater than B. A release outranks a prerelease with
# the same core version.
_semver_gt() {
  local a="$1" b="$2"
  local a_core a_pre b_core b_pre
  case "$a" in
  *-*)
    a_core="${a%%-*}"
    a_pre="${a#*-}"
    ;;
  *)
    a_core="$a"
    a_pre=""
    ;;
  esac
  case "$b" in
  *-*)
    b_core="${b%%-*}"
    b_pre="${b#*-}"
    ;;
  *)
    b_core="$b"
    b_pre=""
    ;;
  esac

  local a_maj a_min a_pat b_maj b_min b_pat
  a_maj="${a_core%%.*}"
  a_min="${a_core#*.}"
  a_min="${a_min%%.*}"
  a_pat="${a_core##*.}"
  b_maj="${b_core%%.*}"
  b_min="${b_core#*.}"
  b_min="${b_min%%.*}"
  b_pat="${b_core##*.}"

  if [ "${a_maj:-0}" -gt "${b_maj:-0}" ]; then return 0; fi
  if [ "${a_maj:-0}" -lt "${b_maj:-0}" ]; then return 1; fi
  if [ "${a_min:-0}" -gt "${b_min:-0}" ]; then return 0; fi
  if [ "${a_min:-0}" -lt "${b_min:-0}" ]; then return 1; fi
  if [ "${a_pat:-0}" -gt "${b_pat:-0}" ]; then return 0; fi
  if [ "${a_pat:-0}" -lt "${b_pat:-0}" ]; then return 1; fi

  if [ -z "$a_pre" ] && [ -z "$b_pre" ]; then return 1; fi
  if [ -z "$a_pre" ]; then return 0; fi
  if [ -z "$b_pre" ]; then return 1; fi
  _semver_prerelease_gt "$a_pre" "$b_pre"
}

# Reads newline-separated "vX.Y.Z[-pre]" tags on stdin, prints the highest.
_pick_highest_tag() {
  local best="" best_ver="" line ver
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    ver="${line#v}"
    if [ -z "$best" ]; then
      best="$line"
      best_ver="$ver"
      continue
    fi
    if _semver_gt "$ver" "$best_ver"; then
      best="$line"
      best_ver="$ver"
    fi
  done
  [ -n "$best" ] || return 1
  printf '%s\n' "$best"
}

# ---------------------------------------------------------------------------
# HTTP: GitHub API (JSON) + generic file download
# ---------------------------------------------------------------------------

# _gh_get <path> -- GET ${api_url}<path>, print the JSON body to stdout.
# Adds a bearer Authorization header when the global $token is non-empty.
_gh_get() {
  local path="$1" url
  url="${api_url}${path}"
  if command -v curl >/dev/null 2>&1; then
    set -- curl -fsSL -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28"
    case "$url" in https://*) set -- "$@" --proto '=https' --tlsv1.2 ;; esac
    if [ -n "$token" ]; then set -- "$@" -H "Authorization: Bearer $token"; fi
    set -- "$@" "$url"
    "$@"
  elif command -v wget >/dev/null 2>&1; then
    set -- wget -q -O - --header "Accept: application/vnd.github+json" --header "X-GitHub-Api-Version: 2022-11-28"
    if [ -n "$token" ]; then set -- "$@" --header "Authorization: Bearer $token"; fi
    set -- "$@" "$url"
    "$@"
  else
    err "need curl or wget"
  fi
}

# _dl <url> <out-file> -- unauthenticated download (public asset URLs).
_dl() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    set -- curl -fsSL --retry 3 -o "$out"
    case "$url" in https://*) set -- "$@" --proto '=https' --tlsv1.2 ;; esac
    set -- "$@" "$url"
    "$@" || err "download failed: $url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --tries=3 -O "$out" "$url" || err "download failed: $url"
  else
    err "need curl or wget"
  fi
}

# _dl_asset <asset-id> <out-file> -- authenticated download by asset id.
# browser_download_url is not bearer-fetchable on a private repo, so the
# authenticated path must hit /repos/<repo>/releases/assets/<id> with
# Accept: application/octet-stream instead.
_dl_asset() {
  local id="$1" out="$2" url
  url="${api_url}/repos/${repo}/releases/assets/${id}"
  if command -v curl >/dev/null 2>&1; then
    set -- curl -fsSL --retry 3 -H "Accept: application/octet-stream" -H "X-GitHub-Api-Version: 2022-11-28" -H "Authorization: Bearer $token" -o "$out"
    case "$url" in https://*) set -- "$@" --proto '=https' --tlsv1.2 ;; esac
    set -- "$@" "$url"
    "$@" || err "authenticated download failed for asset $id"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --tries=3 --header="Accept: application/octet-stream" --header="X-GitHub-Api-Version: 2022-11-28" \
      --header="Authorization: Bearer $token" -O "$out" "$url" ||
      err "authenticated download failed for asset $id"
  else
    err "need curl or wget"
  fi
}

# ---------------------------------------------------------------------------
# version + asset resolution
# ---------------------------------------------------------------------------

# Sets globals: version, tag, tarball_name, sha_name, tarball_url, sha_url.
# No jq, no API calls beyond a single grep/sed scrape of "tag_name" -- this
# is the path every real end user takes.
_resolve_unauthenticated() {
  if [ "$version_pin" != "latest" ]; then
    version="${version_pin#v}"
    tag="v${version}"
  elif [ "$channel" = "prerelease" ]; then
    local body tags
    body=$(_gh_get "/repos/${repo}/releases?per_page=30") ||
      err "failed to list releases for ${repo} (network error, or the repo is private -- set GITHUB_TOKEN/GH_TOKEN)"
    tags=$(printf '%s' "$body" |
      grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' |
      sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')
    [ -n "$tags" ] || err "no releases found for ${repo}"
    tag=$(printf '%s\n' "$tags" | _pick_highest_tag) || err "could not determine the newest prerelease tag"
    version="${tag#v}"
  else
    local body
    body=$(_gh_get "/repos/${repo}/releases/latest") ||
      err "failed to resolve the latest stable release for ${repo} (no stable release yet? try --channel prerelease, or the repo is private -- set GITHUB_TOKEN/GH_TOKEN)"
    tag=$(printf '%s' "$body" |
      grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' |
      head -n1 |
      sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')
    [ -n "$tag" ] || err "could not determine the latest release tag for ${repo}"
    version="${tag#v}"
  fi

  tarball_name="dira-${version}-${target}.tar.gz"
  sha_name="dira-${version}-${target}.sha256"
  local base
  base="${download_url:-https://github.com/${repo}/releases/download/${tag}}"
  base="${base%/}"
  tarball_url="${base}/${tarball_name}"
  sha_url="${base}/${sha_name}"
}

# _asset_id <name> -- looks up an asset id by exact filename in the global
# $assets_json (a JSON array of GitHub release asset objects).
_asset_id() {
  local name="$1"
  printf '%s' "$assets_json" | jq -r --arg n "$name" '.[] | select(.name==$n) | .id' | head -n1
}

# Sets globals: version, tag, tarball_name, sha_name, tarball_id, sha_id.
# Requires jq (only reached when a GITHUB_TOKEN/GH_TOKEN is present).
_resolve_authenticated() {
  local body
  if [ "$version_pin" != "latest" ]; then
    version="${version_pin#v}"
    tag="v${version}"
    body=$(_gh_get "/repos/${repo}/releases/tags/${tag}") ||
      err "failed to resolve release ${tag} for ${repo} (authenticated) -- does that tag have a published release?"
    assets_json=$(printf '%s' "$body" | jq -c '.assets')
  elif [ "$channel" = "prerelease" ]; then
    local tags
    body=$(_gh_get "/repos/${repo}/releases?per_page=30") ||
      err "failed to list releases for ${repo} (authenticated)"
    tags=$(printf '%s' "$body" | jq -r '.[] | select(.draft==false) | .tag_name')
    [ -n "$tags" ] || err "no releases found for ${repo}"
    tag=$(printf '%s\n' "$tags" | _pick_highest_tag) || err "could not determine the newest prerelease tag"
    version="${tag#v}"
    assets_json=$(printf '%s' "$body" | jq -c --arg tag "$tag" '[.[] | select(.tag_name==$tag)][0].assets')
  else
    body=$(_gh_get "/repos/${repo}/releases/latest") ||
      err "failed to resolve the latest stable release for ${repo} (authenticated) -- does a stable release exist yet? try --channel prerelease"
    tag=$(printf '%s' "$body" | jq -r '.tag_name')
    if [ -z "$tag" ] || [ "$tag" = "null" ]; then
      err "no stable release found for ${repo}"
    fi
    version="${tag#v}"
    assets_json=$(printf '%s' "$body" | jq -c '.assets')
  fi

  tarball_name="dira-${version}-${target}.tar.gz"
  sha_name="dira-${version}-${target}.sha256"
  tarball_id=$(_asset_id "$tarball_name")
  sha_id=$(_asset_id "$sha_name")
  [ -n "$tarball_id" ] || err "release ${tag} has no asset named ${tarball_name} -- was it built for this target?"
  [ -n "$sha_id" ] || err "release ${tag} has no checksum asset named ${sha_name}"
}

# ---------------------------------------------------------------------------
# checksum verification (mandatory -- there is no --no-verify)
# ---------------------------------------------------------------------------

_sha256_hex() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    openssl dgst -sha256 "$file" | awk '{print $NF}'
  fi
}

# The .sha256 asset holds one line per asset built in that release job
# (raw sha256sum output), so we must pick the line whose filename field
# matches our tarball -- not just read the first line.
_extract_expected_digest() {
  local sha_file="$1" want_name="$2"
  awk -v want="$want_name" '
    {
      fname = $2
      sub(/^\*/, "", fname)
      if (tolower(fname) == tolower(want)) { print $1; found = 1; exit }
    }
    END { if (!found) exit 1 }
  ' "$sha_file"
}

verify_checksum() {
  local sha_file="$1" want_name="$2" file_path="$3"
  local expected actual
  expected=$(_extract_expected_digest "$sha_file" "$want_name") ||
    err "checksum file has no entry for $want_name"
  actual=$(_sha256_hex "$file_path")
  expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
  actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
  if [ "$expected" != "$actual" ]; then
    err "checksum mismatch for $want_name: expected $expected, got $actual -- download is corrupt or tampered, aborting"
  fi
  debug_log "checksum OK for $want_name"
}

# ---------------------------------------------------------------------------
# install-time helpers
# ---------------------------------------------------------------------------

# True if $1 is a symlink into a `just install` dev build (target/release or
# target/debug) -- re-running this installer over one of those must refuse
# unless --force.
_is_dev_symlink() {
  local path="$1" link_target
  [ -L "$path" ] || return 1
  link_target=$(readlink "$path") || return 1
  case "$link_target" in
  */target/release/* | */target/debug/* | target/release/* | target/debug/*) return 0 ;;
  *) return 1 ;;
  esac
}

_warn_shadow() {
  local dir="$1" found
  found=$(command -v dira 2>/dev/null || true)
  if [ -n "$found" ] && [ "$found" != "$dir/dira" ]; then
    warn "an existing 'dira' is earlier on PATH at $found -- it will shadow $dir/dira unless you reorder PATH."
  fi
}

# _atomic_install_one <name> <src-file> <dst-dir> -- cp into a same-directory
# staging file, then `mv -f` it over the destination. The rename is a same-
# filesystem inode swap, never a truncating write onto a binary a running
# dirad might have open.
_atomic_install_one() {
  local name="$1" src="$2" dst_dir="$3" staging
  staging="$dst_dir/.$name.new.$$"
  cp "$src" "$staging"
  chmod 0755 "$staging"
  mv -f "$staging" "$dst_dir/$name"
}

# The literal $PATH inside these messages is deliberate example syntax shown
# to the user, not meant to expand -- shellcheck SC2016 is a known-safe note.
# shellcheck disable=SC2016
_path_hint() {
  local dir="$1"
  case ":$PATH:" in
  *":$dir:"*) return 0 ;;
  esac
  local shell_name
  shell_name=$(basename "${SHELL:-}")
  printf '\n%s is not on your PATH.\n' "$dir" >&2
  case "$shell_name" in
  fish)
    printf 'Add it with:\n  fish_add_path %s\n' "$dir" >&2
    ;;
  zsh)
    printf 'Add this to %s/.zshrc, then restart your shell:\n  export PATH="%s:$PATH"\n' "${HOME:-\$HOME}" "$dir" >&2
    ;;
  bash)
    local rc="${HOME:-\$HOME}/.bashrc"
    case "$(uname -s)" in
    Darwin) rc="${HOME:-\$HOME}/.bash_profile" ;;
    esac
    printf 'Add this to %s, then restart your shell:\n  export PATH="%s:$PATH"\n' "$rc" "$dir" >&2
    ;;
  *)
    printf 'Add this to %s/.profile, then restart your shell:\n  export PATH="%s:$PATH"\n' "${HOME:-\$HOME}" "$dir" >&2
    ;;
  esac
}

# ---------------------------------------------------------------------------
# uninstall (binaries + service units only -- never config or data)
# ---------------------------------------------------------------------------

do_uninstall() {
  local dira_path dirad_path installed
  dira_path="$bin_dir/dira"
  dirad_path="$bin_dir/dirad"

  installed=1
  if [ ! -e "$dira_path" ] && [ ! -L "$dira_path" ] && [ ! -e "$dirad_path" ] && [ ! -L "$dirad_path" ]; then
    installed=0
  fi

  local config_hint=""
  if [ "$installed" = "1" ]; then
    if _is_dev_symlink "$dira_path"; then
      err "$dira_path is a symlink into a 'just install' dev build ($(readlink "$dira_path")) -- this installer only manages its own installs. Remove it yourself if that's what you want."
    fi

    if [ "$force" != "1" ] && ! _confirm "Remove dira and dirad from $bin_dir?"; then
      info "aborted -- nothing removed."
      return 0
    fi

    if [ -x "$dira_path" ]; then
      config_hint=$("$dira_path" config path 2>/dev/null || true)
      "$dira_path" daemon stop >/dev/null 2>&1 || true
    fi
  fi

  # Best-effort service-unit teardown, independent of whether the binaries
  # are still present -- a stray unit from a previous install must still go.
  case "$(uname -s)" in
  Darwin)
    if [ -n "${HOME:-}" ]; then
      local plist="$HOME/Library/LaunchAgents/sh.dirahq.dirad.plist"
      if [ -f "$plist" ]; then
        launchctl unload "$plist" >/dev/null 2>&1 || true
        rm -f "$plist"
        info "removed launchd agent: $plist"
      fi
    fi
    ;;
  Linux)
    if [ -n "${HOME:-}" ]; then
      local unit="$HOME/.config/systemd/user/dirad.service"
      if [ -f "$unit" ]; then
        systemctl --user disable --now dirad.service >/dev/null 2>&1 || true
        rm -f "$unit"
        systemctl --user daemon-reload >/dev/null 2>&1 || true
        info "removed systemd-user unit: $unit"
      fi
    fi
    ;;
  esac

  if [ "$installed" = "0" ]; then
    info "dira is not installed at $bin_dir -- nothing to remove."
    return 0
  fi

  rm -f "$dira_path" "$dirad_path"
  info "removed dira and dirad from $bin_dir"

  printf '\nConfig and data were NOT removed.\n' >&2
  if [ -n "$config_hint" ]; then
    printf '  config: %s\n' "$config_hint" >&2
  fi
  printf 'To remove everything, reinstall dira and run '\''dira nuke'\'', or delete the\nXDG config/data/cache directories by hand -- see docs/install.md.\n' >&2
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

main() {
  umask 022

  # 1. defaults from environment
  version_pin="${DIRA_VERSION:-latest}"
  channel="${DIRA_CHANNEL:-stable}"
  bin_dir_opt="${DIRA_BIN_DIR:-}"
  target_opt="${DIRA_TARGET:-}"
  repo="${DIRA_REPO:-dodi-smart/dirahq-cli}"
  api_url="${DIRA_API_URL:-https://api.github.com}"
  download_url="${DIRA_DOWNLOAD_URL:-}"
  start_daemon=0
  [ "${DIRA_START_DAEMON:-0}" = "1" ] && start_daemon=1
  install_service=0
  [ "${DIRA_INSTALL_SERVICE:-0}" = "1" ] && install_service=1
  allow_root=0
  [ "${DIRA_ALLOW_ROOT:-0}" = "1" ] && allow_root=1
  debug=0
  [ "${DIRA_DEBUG:-0}" = "1" ] && debug=1
  no_daemon=0
  force=0
  uninstall_flag=0

  # 2. flags (beat env)
  while [ $# -gt 0 ]; do
    case "$1" in
    --version)
      [ $# -ge 2 ] || err "--version requires a value"
      version_pin="$2"
      shift 2
      ;;
    --version=*)
      version_pin="${1#*=}"
      shift
      ;;
    --channel)
      [ $# -ge 2 ] || err "--channel requires a value"
      channel="$2"
      shift 2
      ;;
    --channel=*)
      channel="${1#*=}"
      shift
      ;;
    --prerelease)
      channel="prerelease"
      shift
      ;;
    --bin-dir)
      [ $# -ge 2 ] || err "--bin-dir requires a value"
      bin_dir_opt="$2"
      shift 2
      ;;
    --bin-dir=*)
      bin_dir_opt="${1#*=}"
      shift
      ;;
    --target)
      [ $# -ge 2 ] || err "--target requires a value"
      target_opt="$2"
      shift 2
      ;;
    --target=*)
      target_opt="${1#*=}"
      shift
      ;;
    --daemon)
      start_daemon=1
      shift
      ;;
    --service)
      install_service=1
      shift
      ;;
    --no-daemon)
      no_daemon=1
      shift
      ;;
    --force)
      force=1
      shift
      ;;
    --uninstall)
      uninstall_flag=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      err "unknown flag: $1 (see --help)"
      ;;
    *)
      err "unexpected argument: $1 (see --help)"
      ;;
    esac
  done

  case "$channel" in
  stable | prerelease) ;;
  *) err "--channel must be 'stable' or 'prerelease' (got: $channel)" ;;
  esac

  # 3. bin_dir -- same env var name the justfile already uses.
  bin_dir="${bin_dir_opt:-${XDG_BIN_HOME:-}}"
  if [ -z "$bin_dir" ]; then
    [ -n "${HOME:-}" ] || err "the HOME environment variable is not set -- pass --bin-dir explicitly, or set DIRA_BIN_DIR"
    bin_dir="$HOME/.local/bin"
  fi

  # 4. auth token: GH_TOKEN wins if both are set (matches `gh`'s own precedence).
  token=""
  if [ -n "${GH_TOKEN:-}" ]; then
    token="$GH_TOKEN"
  elif [ -n "${GITHUB_TOKEN:-}" ]; then
    token="$GITHUB_TOKEN"
  fi

  preflight

  if [ "$uninstall_flag" = "1" ]; then
    do_uninstall
    return 0
  fi

  # 5. target
  if [ -n "$target_opt" ]; then
    target="$target_opt"
  else
    target=$(detect_target)
  fi
  debug_log "target: $target"

  # 6. tmp dir + cleanup trap, installed before the first download.
  _tmp=$(mktemp -d "${TMPDIR:-/tmp}/dira-install.XXXXXX") || err "mktemp -d failed"
  trap 'rm -rf "$_tmp"' EXIT INT TERM HUP

  # 7. version + asset resolution
  version="" tag="" tarball_name="" sha_name=""
  tarball_url="" sha_url="" tarball_id="" sha_id="" assets_json=""
  if [ -n "$token" ]; then
    need_cmd jq
    _resolve_authenticated
  else
    _resolve_unauthenticated
  fi
  info "installing dira ${version} (${target})"

  # 8. download
  local_tarball="$_tmp/$tarball_name"
  local_sha="$_tmp/$sha_name"
  if [ -n "$token" ]; then
    _dl_asset "$tarball_id" "$local_tarball"
    _dl_asset "$sha_id" "$local_sha"
  else
    _dl "$tarball_url" "$local_tarball"
    _dl "$sha_url" "$local_sha"
  fi

  # 9. checksum verification is mandatory.
  verify_checksum "$local_sha" "$tarball_name" "$local_tarball"

  # 10. extract; the tarball root is flat (dira + dirad, no leading dir).
  extract_dir="$_tmp/extract"
  mkdir -p "$extract_dir"
  tar -xzf "$local_tarball" -C "$extract_dir"
  [ -f "$extract_dir/dira" ] || err "downloaded archive is missing 'dira' at its root -- packaging layout may have changed (see docs/install.md)"
  [ -f "$extract_dir/dirad" ] || err "downloaded archive is missing 'dirad' at its root -- packaging layout may have changed (see docs/install.md)"
  [ -s "$extract_dir/dira" ] || err "downloaded 'dira' binary is empty"
  [ -s "$extract_dir/dirad" ] || err "downloaded 'dirad' binary is empty"
  chmod 0755 "$extract_dir/dira" "$extract_dir/dirad"

  # 11. existing-install checks, before writing anything.
  mkdir -p "$bin_dir"
  _warn_shadow "$bin_dir"

  if [ -e "$bin_dir/dira" ] || [ -L "$bin_dir/dira" ]; then
    if _is_dev_symlink "$bin_dir/dira"; then
      if [ "$force" != "1" ]; then
        err "$(readlink "$bin_dir/dira") is a 'just install' dev build symlinked at $bin_dir/dira -- refusing to overwrite it. Re-run with --force, or remove the symlink yourself."
      fi
      warn "overwriting dev symlink at $bin_dir/dira (--force)"
    elif [ -e "$bin_dir/dirad" ] &&
      cmp -s "$extract_dir/dira" "$bin_dir/dira" &&
      cmp -s "$extract_dir/dirad" "$bin_dir/dirad"; then
      info "dira ${version} is already installed at $bin_dir -- nothing to do."
      return 0
    fi
  fi

  # 12. was a daemon already running, before we touch anything?
  daemon_was_running=0
  if [ -x "$bin_dir/dira" ] && "$bin_dir/dira" daemon status >/dev/null 2>&1; then
    daemon_was_running=1
  fi

  # 13. atomic install. dirad FIRST, then dira -- same order as the updater's
  # swap_binaries, and load-bearing for the same reason (D-0003): dying
  # between the two leaves a new dirad under an old dira, which `dira version`
  # already detects and warns about. The reverse leaves a new CLI silently
  # driving a stale daemon, which looks like success.
  _atomic_install_one dirad "$extract_dir/dirad" "$bin_dir"
  _atomic_install_one dira "$extract_dir/dira" "$bin_dir"
  info "installed dira + dirad ${version} -> $bin_dir"

  # 14. daemon handling: default do nothing. If one was already running,
  # always restart it -- otherwise the user is left with a new CLI nagging
  # about an old daemon. --no-daemon opts all the way out.
  if [ "$no_daemon" != "1" ]; then
    if [ "$daemon_was_running" = "1" ]; then
      info "restarting dirad..."
      "$bin_dir/dira" daemon restart || warn "could not restart dirad automatically -- run '$bin_dir/dira daemon restart' yourself"
    elif [ "$start_daemon" = "1" ]; then
      info "starting dirad..."
      "$bin_dir/dira" daemon start || warn "could not start dirad automatically -- run '$bin_dir/dira daemon start' yourself"
    fi
    if [ "$install_service" = "1" ]; then
      info "installing the dirad service..."
      "$bin_dir/dira" daemon install || warn "could not install the dirad service automatically -- run '$bin_dir/dira daemon install' yourself"
    fi
  fi

  # 15. PATH hint + next steps. No interactive prompt here: installing a
  # launchd/systemd agent is a persistent system change, and `curl | sh`
  # has no usable stdin to ask with anyway.
  _path_hint "$bin_dir"
  cat <<EOF

Next steps:
  $bin_dir/dira --version
  $bin_dir/dira daemon start
  $bin_dir/dira status
EOF
}

main "$@"
# end of install.sh
