#!/usr/bin/env bash
# Ensure the Go TiDB reference tree pinned in tidb-reference.lock is present and print its path.
# Every Go source an agent or a script reads (porting, eval-gate, integrationtest) comes from it.
#
#   scripts/tidb-reference.sh            ensure (fetch + verify if missing), print the path
#   scripts/tidb-reference.sh --check    verify only: exit 1 if missing or stale, never fetch
#
# Environment:
#   TIDB_REFERENCE_CACHE  directory for the tarball and the default tree
#                         (default ~/.cache/tidb-rs; the tree is <cache>/tidb-<tag>)
#   TIDB_REFERENCE_DIR    use this tree instead. It may be a git checkout: then HEAD must be the
#                         pinned commit, tracked files unmodified, and no untracked file (nor an
#                         ignored .go file) present; it is never fetched over.
#
# A fetched tree is the source archive of the tag, verified by sha256 and by the commit id in the
# archive header, and carries a stamp (.tidb-reference-stamp) of the pin it was built from; a
# changed pin makes it stale. The fetched tree is read-only (chmod a-w); --check fails on a
# writable file, the ensure mode makes it read-only again. Read-only guards against accidental
# writes (a test file, a stray edit), not against someone who deliberately chmods it back. Tools that need extra Go files in a
# package use `go build/test -overlay` (scripts/eval-gate.sh), not files written into the tree.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
lock="$root/tidb-reference.lock"
check=0
case "${1:-}" in
  "") ;;
  --check) check=1 ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

field() { sed -n "s/^$1=//p" "$lock"; }
repo=$(field repo)
tag=$(field tag)
commit=$(field commit)
sha=$(field sha256)
if ! [[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ && -n "$tag" && "$commit" =~ ^[0-9a-f]{40}$ &&
  "$sha" =~ ^[0-9a-f]{64}$ ]]; then
  echo "tidb-reference: malformed $lock (needs repo, tag, 40-hex commit, 64-hex sha256)" >&2
  exit 2
fi

cache=${TIDB_REFERENCE_CACHE:-$HOME/.cache/tidb-rs}
dest=${TIDB_REFERENCE_DIR:-$cache/tidb-$tag}
stamp_file="$dest/.tidb-reference-stamp"
want="$repo $tag $commit $sha"

fail() { echo "tidb-reference: $*" >&2; exit 1; }

# A git checkout (e.g. a local full clone): verify, never modify.
if [ -e "$dest/.git" ]; then
  head=$(git -C "$dest" rev-parse HEAD 2>/dev/null) || fail "$dest: not a readable git checkout"
  [ "$head" = "$commit" ] || fail "$dest is at $head, the lock pins $tag = $commit (git -C $dest checkout $tag)"
  dirty=$(git -C "$dest" status --porcelain --untracked-files=no)
  [ -z "$dirty" ] || fail "$dest has modified tracked files:
$dirty"
  # An added file (a Go file in a package, a .test file of integrationtest) changes what is read
  # as much as an edited one, and the status above does not show it: fail on any untracked file,
  # and on ignored ones too when they are Go files (other ignored files are build output).
  extra=$({ git -C "$dest" ls-files --others --exclude-standard
    git -C "$dest" ls-files --others --ignored --exclude-standard -- '*.go'; } | head -20)
  [ -z "$extra" ] || fail "$dest has files that are not in $tag (untracked, or ignored Go files):
$extra"
  echo "$dest"
  exit 0
fi

if [ -f "$stamp_file" ] && [ "$(cat "$stamp_file")" = "$want" ]; then
  # The fetched tree is read-only, so nothing (a test file of eval-gate, an agent's edit) can
  # be added to the tree every porting agent reads.
  if [ -n "$(find "$dest" ! -type l -perm -u+w -print -quit)" ]; then
    [ "$check" = 1 ] && fail "$dest has writable files (may have been modified); run scripts/tidb-reference.sh"
    chmod -R a-w "$dest"
  fi
  echo "$dest"
  exit 0
fi
if [ "$check" = 1 ]; then
  if [ -f "$stamp_file" ]; then
    fail "$dest is stale (built from: $(cat "$stamp_file"); lock: $want); run scripts/tidb-reference.sh"
  fi
  fail "$dest is missing; run scripts/tidb-reference.sh"
fi
if [ -e "$dest" ] && [ ! -f "$stamp_file" ]; then
  fail "$dest exists but is neither a git checkout nor a fetched tree; move it away or set TIDB_REFERENCE_DIR"
fi

sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}
mkdir -p "$cache"
tarball="$cache/tidb-$tag.tar.gz"
if ! { [ -f "$tarball" ] && [ "$(sha256_of "$tarball")" = "$sha" ]; }; then
  echo "tidb-reference: fetching $repo $tag" >&2
  curl -fsSL --retry 3 -o "$tarball.part" "https://github.com/$repo/archive/refs/tags/$tag.tar.gz"
  got=$(sha256_of "$tarball.part")
  if [ "$got" != "$sha" ]; then
    rm -f "$tarball.part"
    fail "sha256 mismatch for $repo $tag: got $got, lock has $sha"
  fi
  mv "$tarball.part" "$tarball"
fi
if command -v git >/dev/null; then
  tar_commit=$(gzip -dc "$tarball" | git get-tar-commit-id || true)
  [ "$tar_commit" = "$commit" ] || fail "$tarball was made from commit '$tar_commit', the lock pins $commit"
fi

echo "tidb-reference: extracting to $dest" >&2
work="$dest.tmp.$$"
# An interrupted run must not leave a half-extracted (possibly already read-only, so not
# removable with a plain rm -rf) directory next to the tree: restore write permission and
# remove it on any exit. After the final mv the directory no longer exists.
remove_work() {
  if [ -e "$work" ]; then
    chmod -R u+w "$work" 2>/dev/null || true
    rm -rf "$work"
  fi
}
trap remove_work EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
rm -rf "$work"
mkdir -p "$work"
tar -xzf "$tarball" -C "$work" --strip-components=1
if [ ! -f "$work/go.mod" ] || [ ! -d "$work/pkg/expression" ]; then
  rm -rf "$work"
  fail "unexpected archive layout in $tarball"
fi
echo "$want" >"$work/.tidb-reference-stamp"
chmod -R a-w "$work"
if [ -e "$dest" ]; then
  chmod -R u+w "$dest"
  rm -rf "$dest"
fi
mv "$work" "$dest"
echo "$dest"
