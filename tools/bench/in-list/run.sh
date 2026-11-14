#!/usr/bin/env bash
# Reproduce the #169 large-IN-list benchmark on a dedicated tiup playground (Go TiDB v8.5.8):
# start (coprocessor cache off), load 2M rows, run the main matrix, a finer size sweep, the
# "IN filter kept in TiDB" variant, and the plan-cache probe; summarize; clean the playground.
#
#   tools/bench/in-list/run.sh            # TAG=in169 OFFSET=31690 OUT=tools/bench/in-list/out
#   KEEP=1 tools/bench/in-list/run.sh     # leave the playground running
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
TAG=${TAG:-in169}
OFFSET=${OFFSET:-31690}
OUT=${OUT:-$here/out}
REPS=${REPS:-3}
port=$((4000 + OFFSET))
status=$((10080 + OFFSET))
mkdir -p "$OUT"

"$repo/scripts/playground.sh" start -t "$TAG" -o "$OFFSET" -c "$here/tidb.toml"
pid_of() { ps -axo pid=,command= | grep "$1" | grep "/data/$TAG/" | grep -v grep | awk '{print $1}' | head -1; }
tikv_pid=$(pid_of tikv-server)
tidb_pid=$(pid_of tidb-server)
sql() { mysql -h 127.0.0.1 -P "$port" -u root -e "$1"; }
bench() { python3 "$here/bench.py" "$@" --port "$port"; }

bench setup
bench run --reps "$REPS" --tikv-pid "$tikv_pid" --tidb-pid "$tidb_pid" --out "$OUT/main.json"
bench run --reps "$REPS" --tikv-pid "$tikv_pid" --tidb-pid "$tidb_pid" --sizes 3000,20000,40000 \
    --only id_in,id_tmp_hash,k_in,k_tmp_default,k_tmp_hash,v_in,v_tmp_hash --out "$OUT/sweep.json"

# Keep the IN filter in TiDB (not pushed to TiKV): approximates "hash semi join against the list".
sql "INSERT INTO mysql.expr_pushdown_blacklist VALUES ('in', 'tikv', 'bench #169'); ADMIN RELOAD expr_pushdown_blacklist"
bench run --reps "$REPS" --tikv-pid "$tikv_pid" --tidb-pid "$tidb_pid" --cols k,v --only k_in,v_in \
    --sizes 10000,100000,600000 --suffix _root --out "$OUT/root.json"
sql "DELETE FROM mysql.expr_pushdown_blacklist WHERE name = 'in'; ADMIN RELOAD expr_pushdown_blacklist"

python3 "$here/plancache_probe.py" --port "$port" --status-port "$status" >"$OUT/plancache.md"
python3 "$here/summarize.py" "$OUT"/main.json "$OUT"/sweep.json "$OUT"/root.json >"$OUT/summary.md"
echo "results: $OUT/summary.md $OUT/plancache.md"

[ "${KEEP:-0}" = 1 ] || "$repo/scripts/playground.sh" clean -t "$TAG"
