#!/bin/sh
# Regenerate the parser inputs that are derived from a TiDB checkout:
#
#   crates/parser/grammar/tidb_typed.y       typed grmtools grammar (convert_typed.py + handport.py)
#   crates/parser/grammar/typed_report.json  type-inference / action-kind statistics
#   crates/parser/src/gen_types.rs           stub types for Go AST types not yet hand-written
#   crates/parser/src/keywords_gen.rs        keyword tables from misc.go
#   crates/parser/src/charset_gen.rs         charset / collation tables from charset/charset.go
#   crates/parser/tests/data/all_cases.jsonl test corpus from parser_test.go
#
# Usage: tools/parser-port/regen.sh <tidb-checkout> [--check-tables]
#
#   <tidb-checkout>   root of a pingcap/tidb checkout (read-only; nothing is written there)
#   --check-tables    also build goyacc from the checkout (into a temp dir), and check that
#                     goyacc's LALR automaton and ours are isomorphic (compare_tables.py).
#                     Needs `go`; rebuilds crates/parser with TIDB_PARSER_DUMP_TABLE set.
#
# Generated files carry the TiDB commit in their header. Commit them together with any change
# to handport.py / convert_typed.py / crates/parser/src/ast.rs.
set -eu

if [ $# -lt 1 ]; then
  sed -n '2,18p' "$0"
  exit 2
fi
TIDB=$(cd "$1" && pwd)
CHECK_TABLES=${2:-}
P=$TIDB/pkg/parser
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
CRATE=$REPO/crates/parser
PY="python3 -I -B"

[ -f "$P/parser.y" ] || { echo "no $P/parser.y" >&2; exit 1; }
COMMIT=$(git -C "$TIDB" rev-parse --short=10 HEAD 2>/dev/null || echo unknown)
if ! git -C "$TIDB" diff --quiet HEAD -- pkg/parser 2>/dev/null; then
  COMMIT="$COMMIT+dirty"
fi
export PARSER_PORT_SOURCE="TiDB $COMMIT"
echo "source: $PARSER_PORT_SOURCE ($P)"

$PY "$HERE/convert_typed.py" "$P/parser.y" "$CRATE/grammar/tidb_typed.y" \
  "$CRATE/src/gen_types.rs" "$CRATE/grammar/typed_report.json" >/dev/null
$PY "$HERE/gen_keywords.py" "$P/misc.go" "$CRATE/src/keywords_gen.rs"
$PY "$HERE/gen_charset.py" "$P/charset/charset.go" "$CRATE/src/charset_gen.rs"
$PY "$HERE/extract_tests.py" "$P/parser_test.go" "$CRATE/tests/data/all_cases.jsonl"
echo "regenerated grammar, gen_types.rs, keywords_gen.rs, charset_gen.rs and the test corpus"

if [ "$CHECK_TABLES" = "--check-tables" ]; then
  TMP=$(mktemp -d)
  (cd "$P" && go build -o "$TMP/goyacc" ./goyacc)
  "$TMP/goyacc" -o "$TMP/parser.go" -v "$TMP/y.output" "$P/parser.y" >/dev/null
  # build.rs reruns when this variable changes and writes the lrtable automaton there.
  (cd "$REPO" && TIDB_PARSER_DUMP_TABLE="$TMP/lrtable_dump.txt" cargo build -p tidb-parser)
  $PY "$HERE/compare_tables.py" "$TMP/y.output" "$TMP/lrtable_dump.txt"
  rm -rf "$TMP"
fi
