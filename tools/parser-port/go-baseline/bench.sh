#!/bin/sh
# Measure TiDB's Go parser on the same corpus as crates/parser/examples/bench.rs.
# Usage: tools/parser-port/go-baseline/bench.sh <tidb-checkout>
# Builds in a temporary module that `replace`s github.com/pingcap/tidb/pkg/parser with the
# checkout (nothing is written into the checkout). Needs `go` and its module cache/network.
set -eu
TIDB=$(cd "$1" && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
CASES=$(cd "$HERE/../../../crates/parser/tests/data" && pwd)/all_cases.jsonl
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cp "$HERE/main.go" "$TMP/main.go"
cd "$TMP"
go mod init goparsebench >/dev/null 2>&1
go mod edit -require=github.com/pingcap/tidb/pkg/parser@v0.0.0 \
  -replace=github.com/pingcap/tidb/pkg/parser="$TIDB/pkg/parser"
go mod tidy >/dev/null 2>&1
go build -o goparse .
./goparse bench "$CASES"
