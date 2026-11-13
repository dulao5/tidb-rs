#!/usr/bin/env bash
# Upstream sync drill (#128): which ported Go functions changed between the baseline and an
# upstream revision. See docs/porting/sync-drill-8.5.x.md.
#
#   tools/sync-drill/run.sh [target] [out-dir]
#
# target: an upstream revision (default origin/release-8.5); out-dir default /tmp/sync-drill.
# Env: TIDB_UP (upstream clone, default /tmp/tidb-up, cloned blobless if missing),
#      TRACKER (tidb-rs-tracker checkout, default ../tidb-rs-tracker),
#      GO_SRC (Go TiDB v8.5.8 source, default the module cache copy), GO (go binary).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
target=${1:-origin/release-8.5}
out=${2:-/tmp/sync-drill}
TIDB_UP=${TIDB_UP:-/tmp/tidb-up}
TRACKER=${TRACKER:-$root/../tidb-rs-tracker}
GO_SRC=${GO_SRC:-$HOME/go/pkg/mod/github.com/pingcap/tidb@v1.1.0-beta.0.20260827064614-8b857efa2036}
GO=${GO:-go}

if [ ! -d "$TIDB_UP/.git" ]; then
  git clone -q --filter=blob:none --no-checkout https://github.com/pingcap/tidb "$TIDB_UP"
fi
git -C "$TIDB_UP" fetch -q --tags origin
mkdir -p "$out"
GO_SRC=$GO_SRC python3 "$here/ported.py" --func-blocks "$TRACKER/data/func_blocks.tsv" \
  "$root/crates" >"$out/ported.tsv" 2>"$out/ported.err"
head -1 "$out/ported.err"
# ported.py prints rs_files relative to its argument; make them repo-relative
sed -i "s#$root/##g" "$out/ported.tsv"
(cd "$here/funcdiff" && "$GO" run . -repo "$TIDB_UP" -base v8.5.8 -target "$target" \
  -ported "$out/ported.tsv" -out "$out")
echo "newest v8.5.x tags:"; git -C "$TIDB_UP" tag -l 'v8.5.*' | grep -Ev -- '-(nextgen|fts|20[0-9]{6})' | sort -V | tail -2
