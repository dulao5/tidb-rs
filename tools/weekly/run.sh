#!/usr/bin/env bash
# One part of the weekly / nightly build (#128): runs it against a fresh tiup playground and
# writes <out>/<part>.json (+ logs) for tools/weekly/report.py.
#
#   tools/weekly/run.sh <part> <out-dir>
#
# parts:
#   ab        tidb-ab result comparison (sysbench statements + tools/ab/workloads/*.sql), with
#             per-operator agreement (classify.py); 1 TiKV
#   sysbench  oltp_point_select and oltp_read_only on Go TiDB then tidb-rs, B/A QPS ratio; 1 TiKV
#   faults    tools/ab/faults/run.sh (3 TiKV, its own cluster; needs passwordless sudo)
#   locks     tools/ab/locks/mixed_locks.py (needs LOCKGEN=path to the lockgen binary); 1 TiKV
#   tpch      TPC-H queries 1-22 (tools/weekly/tpch/queries.sql) on `tiup bench tpch prepare`
#             data (SF 1: tiup bench takes an integer scale factor): tidb-ab result comparison
#             and relative time (tpch_time.py); 1 TiKV
#   integrationtest
#             TiDB v8.5.8's tests/integrationtest (sparse, shallow clone) through
#             `ab integrationtest` (hybrid mode: everything on Go TiDB, reads also on tidb-rs and
#             compared), with tools/ab/integrationtest.toml; 1 TiKV
#
# Environment: TIDB_RS_BIN (tidb-server, default target/release/tidb-server), TIDB_AB_BIN
# (default target/release/ab), SYSBENCH_TIME (seconds per run, default 20),
# SYSBENCH_THREADS (default "1 8"), IT_TIDB_DIR (an existing tests/integrationtest/t; default:
# clone it), IT_TESTS (tests or prefixes for `ab integrationtest`; default: see the part),
# IT_BUDGET (seconds, default 2400).
#
# Exit status: the part's own (0 ok, 1 wrong results / new inconsistencies, 2 incomplete);
# the JSON carries the same as "status".
set -uo pipefail

part=${1:?part}
out=${2:?out-dir}
root=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
bin=${TIDB_RS_BIN:-$root/target/release/tidb-server}
ab=${TIDB_AB_BIN:-$root/target/release/ab}

# 1-TiKV cluster of scripts/playground.sh defaults: Go TiDB 27000 (status 33080), PD 25379.
a=127.0.0.1:27000 b=127.0.0.1:27001 pd=127.0.0.1:25379
export PLAYGROUND_TAG=weekly-$part PLAYGROUND_OFFSET=23000

rs_pid=
cleanup() {
    [ -n "$rs_pid" ] && kill "$rs_pid" 2>/dev/null && wait "$rs_pid" 2>/dev/null
    "$root/scripts/playground.sh" clean >/dev/null 2>&1 || true
}

# Cluster, data (sbtest1, 100k rows, 8 regions) and tidb-rs on 27001.
start_cluster() {
    trap cleanup EXIT
    "$root/scripts/playground.sh" start ${PLAYGROUND_EXTRA_CONFIG:+-c "$PLAYGROUND_EXTRA_CONFIG"} \
        >"$out/$part-playground.log" 2>&1 || return 2
    if [ "${LOAD_SBTEST:-1}" = 1 ]; then
        "$root/scripts/sysbench-prepare.sh" -P 27000 -t 1 -s 100000 -r 8 >>"$out/$part-playground.log" 2>&1 || return 2
    fi
    "$bin" -P 27001 --status-port 10081 --pd "$pd" --auto-tls=false >"$out/$part-tidb-rs.log" 2>&1 &
    rs_pid=$!
    for _ in $(seq 120); do
        mysql -h 127.0.0.1 -P 27001 -u root -N -e 'select 1' >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    echo "tidb-rs did not come up" >&2
    return 2
}

# write_status <status> <json-object-of-details>
write_status() {
    python3 - "$out/$part.json" "$1" "$2" <<'EOF'
import json, sys
path, status, details = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
json.dump({"part": path.rsplit("/", 1)[-1][:-5], "status": status, **details}, open(path, "w"), indent=1)
EOF
}

status_of() { case $1 in 0) echo ok ;; 1) echo fail ;; *) echo incomplete ;; esac; }

case $part in
ab)
    start_cluster || { write_status incomplete '{"error": "cluster setup failed"}'; exit 2; }
    rc=0 runs=()
    # rc: 1 (a mismatch) wins over 2 (an error), so a wrong result is never reported as
    # incomplete. report.py also reads the mismatches from the raw results.
    worst() { if [ "$1" = 1 ] || [ "$rc" = 1 ]; then rc=1; elif [ "$1" != 0 ]; then rc=$1; fi; }
    "$ab" -a "$a" -b "$b" -D sbtest --tls off --out-dir "$out/ab-sysbench" sysbench -n 100 \
        >"$out/ab-sysbench.log" 2>&1; worst $?
    runs+=("$out/ab-sysbench/results.jsonl")
    for w in "$root"/tools/ab/workloads/*.sql; do
        n=$(basename "$w" .sql)
        "$ab" -a "$a" -b "$b" -D sbtest --tls off --out-dir "$out/ab-$n" file "$w" \
            >"$out/ab-$n.log" 2>&1; worst $?
        runs+=("$out/ab-$n/results.jsonl")
    done
    # Every workload must have compared something: an empty run is incomplete, not ok.
    empty=()
    for r in "${runs[@]}"; do [ -s "$r" ] || empty+=("$(basename "$(dirname "$r")")"); done
    if [ ${#empty[@]} -gt 0 ]; then
        echo "no statements compared: ${empty[*]}" >&2
        worst 2
    fi
    python3 "$root/tools/weekly/classify.py" --a "$a" --results "${runs[@]}" --json "$out/ab-classified.json" \
        || worst 2
    write_status "$(status_of $rc)" "{\"classified\": \"ab-classified.json\", \"empty_workloads\": \"${empty[*]}\", \"exit\": $rc}"
    exit "$rc"
    ;;
sysbench)
    start_cluster || { write_status incomplete '{"error": "cluster setup failed"}'; exit 2; }
    rc=0
    for w in oltp_point_select oltp_read_only; do
        "$root/scripts/sysbench-run.sh" compare -a "$a" -b "$b" -W "$w" -o "$out/sysbench-$w" \
            -T "${SYSBENCH_THREADS:-1 8}" -d "${SYSBENCH_TIME:-20}" >"$out/sysbench-$w.log" 2>&1 || rc=2
    done
    # Rows: "A:host:port threads qps avg p95 errors status file" (scripts/sysbench-run.sh).
    python3 - "$out" "$rc" "${SYSBENCH_THREADS:-1 8}" <<'EOF' || rc=$?
import glob, json, sys
out, rc, threads = sys.argv[1], int(sys.argv[2]), sys.argv[3].replace(",", " ").split()
res = {}
for w in ("oltp_point_select", "oltp_read_only"):
    rows = {}
    for f in glob.glob(f"{out}/sysbench-{w}/*-compare.txt"):
        for line in open(f).read().splitlines()[1:]:
            c = line.split()
            if len(c) >= 7:
                rows.setdefault(c[1], {})[c[0][0]] = {"qps": c[2], "avg_ms": c[3], "p95_ms": c[4],
                                                       "errors": c[5], "status": c[6]}
    res[w] = rows
bad = any(s.get("status") != "ok" or s.get("errors") not in ("0", None)
          for rows in res.values() for t in rows.values() for s in t.values())
# Every workload x thread count needs a result on both sides; anything less is incomplete.
missing = [f"{w} t{t} {side}" for w in res for t in threads for side in "AB" if side not in res[w].get(t, {})]
status = "fail" if bad else ("incomplete" if rc or missing else "ok")
json.dump({"part": "sysbench", "status": status, "missing": missing, "results": res},
          open(f"{out}/sysbench.json", "w"), indent=1)
sys.exit({"ok": 0, "fail": 1, "incomplete": 2}[status])
EOF
    exit "$rc"
    ;;
faults)
    rc=0
    # tidb-rs's and the cluster's logs (PD, TiKV, Go TiDB), to diagnose what the report shows.
    (cd "$root" && FAULTS_LOG_DIR="$out/logs" tools/ab/faults/run.sh -b "$bin" -j "$out/faults-raw.json") \
        >"$out/faults.log" 2>&1 || rc=$?
    cp /tmp/tidb-rs-playground/maya-faults.log "$out/logs/playground.log" 2>/dev/null
    if [ ! -f "$out/faults-raw.json" ]; then
        # No results at all: the cluster or tidb-rs did not come up. Incomplete, not a failure.
        rc=2
    fi
    write_status "$(status_of $rc)" "{\"raw\": \"faults-raw.json\", \"exit\": $rc}"
    exit "$rc"
    ;;
tpch)
    LOAD_SBTEST=0 PLAYGROUND_EXTRA_CONFIG=$root/tools/weekly/tpch/tidb.toml start_cluster || { write_status incomplete '{"error": "cluster setup failed"}'; exit 2; }
    tiup bench tpch --sf "${TPCH_SF:-1}" -H 127.0.0.1 -P 27000 -D tpch -T 4 prepare --analyze --dropdata \
        >"$out/tpch-prepare.log" 2>&1 || { write_status incomplete '{"error": "tpch prepare failed"}'; exit 2; }
    rc=0
    "$ab" -a "$a" -b "$b" -D tpch --tls off --timeout 600 --out-dir "$out/ab-tpch" \
        file "$root/tools/weekly/tpch/queries.sql" >"$out/ab-tpch.log" 2>&1 || rc=$?
    python3 "$root/tools/weekly/classify.py" --a "$a" --db tpch --results "$out/ab-tpch/results.jsonl" \
        --json "$out/tpch-classified.json" || rc=2
    python3 "$root/tools/weekly/tpch_time.py" --queries "$root/tools/weekly/tpch/queries.sql" \
        --a "$a" --b "$b" --json "$out/tpch-time.json" >"$out/tpch-time.log" 2>&1 || rc=2
    # A query Go TiDB cannot run at all means the data or the setup is broken: incomplete.
    if python3 -c "import json,sys; sys.exit(any(r['a_ms'] is None for r in json.load(open(sys.argv[1])).values()))" \
        "$out/tpch-time.json" 2>/dev/null; then :; else
        echo "a TPC-H query failed on Go TiDB (or no timing)" >&2
        [ "$rc" = 1 ] || rc=2
    fi
    write_status "$(status_of $rc)" "{\"classified\": \"tpch-classified.json\", \"time\": \"tpch-time.json\", \"exit\": $rc}"
    exit "$rc"
    ;;
integrationtest)
    tag=v8.5.8
    dir=${IT_TIDB_DIR:-}
    if [ -z "$dir" ]; then
        # Only tests/integrationtest of the release under test: a shallow, sparse, blob-less clone.
        src=${TMPDIR:-/tmp}/weekly-integrationtest-tidb
        rm -rf "$src"
        { git clone -q --depth 1 --branch "$tag" --filter=blob:none --sparse https://github.com/pingcap/tidb "$src" \
            && git -C "$src" sparse-checkout set tests/integrationtest; } >"$out/integrationtest-clone.log" 2>&1 \
            || { write_status incomplete '{"error": "cannot fetch pingcap/tidb tests/integrationtest"}'; exit 2; }
        dir=$src/tests/integrationtest/t
    fi
    # Default: every test without a statement that makes Go TiDB call every TiDB server's status
    # port (ANALYZE, cluster tables, tidb_decode_sql_digests): tidb-rs's does not answer, so each
    # waits about 40 s, and the 67 such files would take about 3 hours. The other 179 take about
    # 15 minutes (README.md).
    if [ -n "${IT_TESTS:-}" ]; then
        read -r -a tests <<<"$IT_TESTS"
    else
        slow='^\s*analyze\s|analyze partition|tidb_decode_sql_digests|tidb_servers_info|cluster_(slow_query|statements|processlist|config|info|load|hardware|systeminfo|log|tidb_trx|deadlocks)'
        mapfile -t tests < <(cd "$dir" && grep -LiE "$slow" -r --include='*.test' . | sed 's|^\./||; s|\.test$||' | sort)
        [ ${#tests[@]} -gt 0 ] || { write_status incomplete '{"error": "no tests found"}'; exit 2; }
    fi
    # As TiDB's run-tests.sh (and scripts/integrationtest.sh): the expected results assume it.
    export TZ=Asia/Shanghai
    LOAD_SBTEST=0 PLAYGROUND_EXTRA_CONFIG=$root/tools/ab/integrationtest.toml start_cluster \
        || { write_status incomplete '{"error": "cluster setup failed"}'; exit 2; }
    # As scripts/integrationtest.sh: the ingest backfill refuses to run on a nearly full disk.
    mysql -h 127.0.0.1 -P 27000 -u root -e \
        "SET GLOBAL tidb_ddl_enable_fast_reorg = OFF; SET GLOBAL tidb_enable_dist_task = OFF" \
        >>"$out/$part-playground.log" 2>&1
    rc=0
    start=$(date +%s)
    # A time budget (IT_BUDGET seconds, default 40 min), so a slow runner reports what it ran as
    # incomplete instead of hitting the job's timeout; ab writes its results after every file.
    timeout "${IT_BUDGET:-2400}" "$ab" -a "$a" -b "$b" --tls off --timeout 60 --out-dir "$out/ab-integrationtest" \
        integrationtest --dir "$dir" "${tests[@]}" >"$out/ab-integrationtest.log" 2>&1 || rc=$?
    seconds=$(( $(date +%s) - start ))
    # ab: 0 no mismatch, 1 mismatches, 2 could not run (or the cluster died midway); 124 timeout.
    [ "$rc" -gt 2 ] && rc=2
    if [ ! -s "$out/ab-integrationtest/results.jsonl" ]; then
        echo "no statements compared" >&2
        [ "$rc" = 1 ] || rc=2
    fi
    commit=$(git -C "$dir" rev-parse HEAD 2>/dev/null || echo unknown)
    write_status "$(status_of $rc)" "{\"results\": \"ab-integrationtest/results.jsonl\", \"tidb_tag\": \"$tag\",
        \"tidb_commit\": \"$commit\", \"tests\": ${#tests[@]}, \"seconds\": $seconds, \"exit\": $rc}"
    exit "$rc"
    ;;
locks)
    start_cluster || { write_status incomplete '{"error": "cluster setup failed"}'; exit 2; }
    rc=0
    python3 "$root/tools/ab/locks/mixed_locks.py" --lockgen "${LOCKGEN:?LOCKGEN}" --a "$a" --b "$b" \
        --a-status 127.0.0.1:33080 --b-status 127.0.0.1:10081 --pd "$pd" \
        --json "$out/locks-raw.json" >"$out/locks.log" 2>&1 || rc=$?
    # mixed_locks.py writes its JSON only after every case: none means its setup failed
    # (incomplete, not wrong results); fewer cases than expected is incomplete too.
    n=$(python3 -c "import json,sys; print(len(json.load(open(sys.argv[1]))))" "$out/locks-raw.json" 2>/dev/null || echo 0)
    if [ "$n" = 0 ]; then
        rc=2
    elif [ "$n" -lt "${LOCKS_EXPECTED:-18}" ] && [ "$rc" = 0 ]; then
        rc=2
    fi
    write_status "$(status_of $rc)" "{\"raw\": \"locks-raw.json\", \"cases\": $n, \"exit\": $rc}"
    exit "$rc"
    ;;
*)
    sed -n '2,20p' "$0" >&2
    exit 2
    ;;
esac
