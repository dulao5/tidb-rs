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
# (scripts/eval_gate_summary.py). Close other heavy processes first.
#
# Load rule (plan.md "性能门禁的测量规则", #165): each round starts only when the 1-minute load
# average is below EVAL_GATE_MAX_LOAD (default 2); otherwise the script waits, polling every
# 10 s, for at most EVAL_GATE_LOAD_WAIT seconds (default 600). A round that starts above the
# limit after the wait is marked "LOADED" in <outdir>/machine.txt (and so in summary.md) and
# a warning is printed; such a measurement does not count for the gate and is re-run. The
# load at the start of every round is recorded in machine.txt.
set -euo pipefail

max_load=${EVAL_GATE_MAX_LOAD:-2}
load_wait=${EVAL_GATE_LOAD_WAIT:-600}

# The 1-minute load average.
load1() {
  if [ "$(uname)" = Darwin ]; then
    sysctl -n vm.loadavg | awk '{print $2}'
  else
    cut -d' ' -f1 /proc/loadavg
  fi
}

# Waits until load1 < max_load or load_wait seconds pass; prints the status line of the round.
wait_for_load() {
  local waited=0 l
  l=$(load1)
  while awk -v l="$l" -v m="$max_load" 'BEGIN { exit !(l >= m) }'; do
    if [ "$waited" -ge "$load_wait" ]; then
      echo "LOADED: load $l >= $max_load after waiting ${waited}s; this round does not count, re-run" >&2
      echo "load1 $l (LOADED, >= $max_load after ${waited}s wait)"
      return
    fi
    echo "load $l >= $max_load, waiting (${waited}s/${load_wait}s)" >&2
    sleep 10
    waited=$((waited + 10))
    l=$(load1)
  done
  echo "load1 $l (< $max_load, waited ${waited}s)"
}

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

rust_bin=$(cd "$root" && cargo bench -p tidb-expression --bench eval_gate --no-run --message-format=json |
  python3 -c 'import json,sys
for l in sys.stdin:
    m = json.loads(l)
    if m.get("reason") == "compiler-artifact" and m["target"]["name"] == "eval_gate" and m.get("executable"):
        print(m["executable"])')
[ -n "$rust_bin" ] || { echo "eval_gate did not build" >&2; exit 1; }

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
  status=$(wait_for_load)
  echo "round $r/$rounds: $status; load average $(uptime | sed 's/.*load average[s]*: //')" | tee -a "$out/machine.txt"
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
if grep -q LOADED "$out/machine.txt"; then
  echo "WARNING: some rounds started with load >= $max_load (see machine.txt); re-run the gate" >&2
fi
