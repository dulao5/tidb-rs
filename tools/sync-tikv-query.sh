#!/usr/bin/env bash
# Re-vendor TiKV's TiDB-query crates into third_party/tikv from a TiKV commit.
#
# Usage: tools/sync-tikv-query.sh <path-to-tikv-checkout> <commit-ish>
#
# Each crate directory under third_party/tikv/ is replaced by the exact tree
# of components/<crate> at <commit> (via `git archive`, so the checkout's
# working tree and index are not touched). The tikv_util files that
# third_party/tikv-shims/tikv_util copies verbatim are refreshed as well.
# PROVENANCE.md's commit line is updated; review `git diff` afterwards.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <path-to-tikv-checkout> <commit-ish>" >&2
    exit 2
fi

TIKV=$1
REV=$2
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEST=$ROOT/third_party/tikv
SHIM=$ROOT/third_party/tikv-shims/tikv_util/src

# Crates vendored unmodified. Keep in sync with PROVENANCE.md and the
# workspace members in Cargo.toml.
CRATES=(
    codec
    collections
    error_code
    panic_hook
    profiler
    tidb_query_aggr
    tidb_query_codegen
    tidb_query_common
    tidb_query_datatype
    tidb_query_expr
    tipb_helper
)

# tikv_util files copied verbatim into the shim (path relative to
# components/tikv_util/src).
TIKV_UTIL_FILES=(
    buffer_vec.rs
    codec/bytes.rs
    codec/number.rs
    log.rs
    macros.rs
)

COMMIT=$(git -C "$TIKV" rev-parse --verify "$REV^{commit}")
echo "Syncing from $TIKV at $COMMIT"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

PATHS=()
for c in "${CRATES[@]}"; do
    PATHS+=("components/$c")
done
for f in "${TIKV_UTIL_FILES[@]}" codec/mod.rs; do
    PATHS+=("components/tikv_util/src/$f")
done
git -C "$TIKV" archive --format=tar "$COMMIT" "${PATHS[@]}" | tar -x -C "$TMP"

for c in "${CRATES[@]}"; do
    rm -rf "${DEST:?}/$c"
    cp -R "$TMP/components/$c" "$DEST/$c"
done

for f in "${TIKV_UTIL_FILES[@]}"; do
    cp "$TMP/components/tikv_util/src/$f" "$SHIM/$f"
done
# The shim's codec/mod.rs is upstream's minus the `stream_event` module.
grep -v '^pub mod stream_event;$' "$TMP/components/tikv_util/src/codec/mod.rs" >"$SHIM/codec/mod.rs"

sed -i.bak -E "s/^(- Commit: \`)[0-9a-f]{40}(\`)/\1$COMMIT\2/" "$DEST/PROVENANCE.md"
rm -f "$DEST/PROVENANCE.md.bak"

echo "Done. Review with: git -C $ROOT status && git -C $ROOT diff --stat"
