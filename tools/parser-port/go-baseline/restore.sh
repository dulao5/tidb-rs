#!/bin/sh
# Record TiDB's Restore() text for SQL statements, one per line of <sql-file> ('#' lines and
# blank lines are skipped), as JSON lines {"src", "restore"} (or {"src", "error"}) on stdout.
# The first output line is a provenance record {"_generated": ...} naming the TiDB commit.
# Usage: tools/parser-port/go-baseline/restore.sh <tidb-checkout> <sql-file>
# Builds main.go in a temporary module like bench.sh (nothing is written into the checkout).
set -eu
TIDB=$(cd "$1" && pwd)
COMMIT=$(git -C "$TIDB" rev-parse --short=10 HEAD 2>/dev/null || echo unknown)
if ! git -C "$TIDB" diff --quiet HEAD -- pkg/parser 2>/dev/null; then
  COMMIT="$COMMIT+dirty"
fi
SQL=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cp "$HERE/main.go" "$TMP/main.go"
cd "$TMP"
go mod init goparserestore >/dev/null 2>&1
go mod edit -require=github.com/pingcap/tidb/pkg/parser@v0.0.0 \
  -replace=github.com/pingcap/tidb/pkg/parser="$TIDB/pkg/parser"
go mod tidy >/dev/null 2>&1
go build -o goparse .
echo "{\"_generated\": \"by tools/parser-port/go-baseline/restore.sh from $(basename "$SQL") with pkg/parser of TiDB $COMMIT\"}"
./goparse restore "$SQL"
