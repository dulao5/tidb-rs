#!/usr/bin/env bash
# N4 §5 expression performance gate (specs/exceptions/N4-expr-eval/spec.md §5), measured as
# specs/D08.1-expr/plan.md "性能门禁的测量规则" prescribes: Go and Rust alternately on the
# same machine, each side in several independent processes, the minimum over processes.
#
#   TIDB_SRC=<TiDB v8.5.8 checkout> scripts/eval-gate.sh [-n rounds] [-o outdir]
#
# Go side: the stock BenchmarkVectorizedBuiltin*Func of pkg/expression plus
# crates/expression/benches/go/gate_vec_test.go (copied into $TIDB_SRC/pkg/expression; the
# checkout is otherwise not modified). Rust side: crates/expression/benches/eval_gate.rs.
# Raw outputs go to <outdir>/{go,rust}-<round>.txt, the table to <outdir>/summary.md
# (scripts/eval_gate_summary.py). Close other heavy processes first; the load average of
# each round is recorded in <outdir>/machine.txt.
set -euo pipefail

rounds=5
out=target/eval-gate
while getopts "n:o:" opt; do
  case $opt in
    n) rounds=$OPTARG ;;
    o) out=$OPTARG ;;
    *) echo "usage: TIDB_SRC=<dir> $0 [-n rounds] [-o outdir]" >&2; exit 2 ;;
  esac
done
: "${TIDB_SRC:?set TIDB_SRC to a TiDB v8.5.8 checkout}"
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

cp "$root/crates/expression/benches/go/gate_vec_test.go" "$TIDB_SRC/pkg/expression/zz_gate_vec_test.go"
trap 'rm -f "$TIDB_SRC/pkg/expression/zz_gate_vec_test.go"' EXIT
(cd "$TIDB_SRC" && go test -c -o "$out/expr.test" ./pkg/expression)

rust_bin=$(cd "$root" && cargo bench -p tidb-expression --bench eval_gate --no-run --message-format=json 2>/dev/null |
  python3 -c 'import json,sys
for l in sys.stdin:
    m = json.loads(l)
    if m.get("reason") == "compiler-artifact" and m["target"]["name"] == "eval_gate" and m.get("executable"):
        print(m["executable"])')

# Go benchmark (regexp, positional filter); the filter selects the signature among the
# function family's cases (bench_test.go testFunc).
go_specs=(
  'BenchmarkVectorizedBuiltinArithmeticFunc$ builtinArithmeticPlusIntSig'
  'BenchmarkVectorizedBuiltinCompareFunc$ builtinLTIntSig'
  'BenchmarkVectorizedBuiltinCastFunc$ builtinCastIntAsRealSig'
  'BenchmarkVectorizedBuiltinStringFunc$ builtinConcatSig'
  'BenchmarkVectorizedBuiltinLikeFunc$ builtinLikeSig'
  'BenchmarkGateVecDim3$'
  'BenchmarkGateVecDim768$'
)

{
  echo "date: $(date -u +%FT%TZ)"
  echo "uname: $(uname -a)"
  if [ "$(uname)" = Darwin ]; then
    echo "cpu: $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores"
  else
    echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs), $(nproc) cores"
  fi
  echo "go: $(go version "$out/expr.test" | cut -d' ' -f2-)"
  echo "rustc: $(rustc --version)"
} > "$out/machine.txt"

for r in $(seq 1 "$rounds"); do
  echo "round $r/$rounds: load $(uptime | sed 's/.*load average[s]*: //')" | tee -a "$out/machine.txt"
  : > "$out/go-$r.txt"
  for spec in "${go_specs[@]}"; do
    read -r bench filter <<<"$spec"
    # shellcheck disable=SC2086 # the filter is one word or empty
    (cd "$TIDB_SRC/pkg/expression" &&
      "$out/expr.test" -test.run '^$' -test.bench "$bench" -test.benchmem -test.count 3 $filter) >> "$out/go-$r.txt"
  done
  "$rust_bin" --bench --warm-up-time 1 --measurement-time 3 --noplot > "$out/rust-$r.txt"
done

python3 "$root/scripts/eval_gate_summary.py" "$out" | tee "$out/summary.md"
