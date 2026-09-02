#!/bin/sh
# Create the development install: point the PATH entry at the build tree.
#
# Driven by `just link` (and so by `just install`); not a user-facing
# installer. install.sh is the one contributors and users run — this is its
# opposite number, the thing that deliberately puts a build-tree binary on
# PATH.
#
# The symlink is load-bearing, not a convenience. D-0004 has install.sh and
# `dira update` refuse to overwrite a development install, and the only thing
# that distinguishes one is that the PATH entry is a symlink into `target/`:
# `discover_install` reads it with `symlink_metadata`, because `current_exe()`
# resolves symlinks on both Linux and macOS and would see an ordinary managed
# install. Copy the binaries here instead of linking them and that detection
# goes blind — `dira update` would overwrite a contributor's dev build and
# `cargo build` would silently stop affecting the binary on PATH, which is the
# exact failure D-0004 exists to prevent. Keep it a symlink.
#
# Idempotent: safe to re-run, and `ln -sf` re-points an existing link rather
# than nesting one inside it.
set -eu

repo_dir="${1:?usage: dev-install.sh <repo-dir> <bin-dir>}"
bin_dir="${2:?usage: dev-install.sh <repo-dir> <bin-dir>}"

for name in dira dirad; do
  built="$repo_dir/target/release/$name"
  if [ ! -x "$built" ]; then
    echo "dev-install: $built is missing — run \`just release\` first." >&2
    exit 1
  fi
done

mkdir -p "$bin_dir"
for name in dira dirad; do
  ln -sf "$repo_dir/target/release/$name" "$bin_dir/$name"
done

echo "Linked dira + dirad -> $bin_dir"
