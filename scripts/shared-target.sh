#!/usr/bin/env bash
# Make every git worktree of this repository share one cargo target directory by default.
#
#   scripts/shared-target.sh            install (idempotent), then show the effective target dir
#   scripts/shared-target.sh --check    exit 1 unless this worktree builds into the shared dir
#
# Worktrees are created as siblings of the main checkout (../tidb-rs-<name>). Cargo reads
# .cargo/config.toml from every parent of the working directory, so one file in the common
# parent directory,
#
#   <parent>/.cargo/config.toml:   [build] target-dir = "<main checkout>/target"
#
# applies to the main checkout and to all worktrees, without touching the repository's own
# .cargo/config.toml (CI keeps its default target/) or the user's global cargo config.
# CARGO_TARGET_DIR still overrides it. Cargo serialises builds on the directory's lock, so
# parallel builds in different worktrees wait for each other instead of each compiling the
# whole dependency graph (about 10 GB per target directory).
set -euo pipefail

here=$(git rev-parse --show-toplevel)
main=$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
parent=$(dirname "$main")
shared="$main/target"
config="$parent/.cargo/config.toml"

effective() {
  (cd "$here" && cargo metadata --format-version 1 --no-deps 2>/dev/null) |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])'
}

if [ "${1:-}" = "--check" ]; then
  got=$(effective)
  if [ "$got" != "$shared" ]; then
    echo "shared-target: $here builds into $got, not $shared; run scripts/shared-target.sh" >&2
    exit 1
  fi
  echo "$got"
  exit 0
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

line="target-dir = \"$shared\""
if [ -f "$config" ]; then
  if grep -qF "$line" "$config"; then
    echo "shared-target: $config already sets $line" >&2
  else
    echo "shared-target: $config exists with other settings; add under [build]: $line" >&2
    exit 1
  fi
else
  mkdir -p "$(dirname "$config")"
  cat >"$config" <<EOF
# Written by tidb-rs scripts/shared-target.sh: all worktrees under this directory share one
# cargo target directory (CARGO_TARGET_DIR overrides it).
[build]
$line
EOF
  echo "shared-target: wrote $config" >&2
fi
# Other cargo projects under the same parent build into this directory too.
for other in "$parent"/*/Cargo.toml; do
  d=$(dirname "$other")
  [ -e "$d/.git" ] && [ "$(git -C "$d" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" = "$main/.git" ] && continue
  [ -f "$other" ] && echo "shared-target: note $d is another cargo project under $parent; it also builds into $shared" >&2
done
[ -n "${CARGO_TARGET_DIR:-}" ] && echo "shared-target: note CARGO_TARGET_DIR=$CARGO_TARGET_DIR overrides it in this shell" >&2
echo "effective target dir: $(effective)"
