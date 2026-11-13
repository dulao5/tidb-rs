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
#
# Environment: TIDB_RS_BIN (tidb-server, default target/release/tidb-server), TIDB_AB_BIN
# (default target/release/ab), SYSBENCH_TIME (seconds per run, default 20),
# SYSBENCH_THREADS (default "1 8").
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
    "$root/scripts/playground.sh" start >"$out/$part-playground.log" 2>&1 || return 2
    "$root/scripts/sysbench-prepare.sh" -P 27000 -t 1 -s 100000 -r 8 >>"$out/$part-playground.log" 2>&1 || return 2
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
    "$ab" -a "$a" -b "$b" -D sbtest --tls off --out-dir "$out/ab-sysbench" sysbench -n 100 \
        >"$out/ab-sysbench.log" 2>&1 || rc=$(( $? > rc ? $? : rc ))
    runs+=("$out/ab-sysbench/results.jsonl")
    for w in "$root"/tools/ab/workloads/*.sql; do
        n=$(basename "$w" .sql)
        "$ab" -a "$a" -b "$b" -D sbtest --tls off --out-dir "$out/ab-$n" file "$w" \
            >"$out/ab-$n.log" 2>&1 || rc=$(( $? > rc ? $? : rc ))
        runs+=("$out/ab-$n/results.jsonl")
    done
    python3 "$root/tools/weekly/classify.py" --a "$a" --results "${runs[@]}" --json "$out/ab-classified.json" \
        || rc=2
    write_status "$(status_of $rc)" "{\"classified\": \"ab-classified.json\", \"exit\": $rc}"
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
    python3 - "$out" "$rc" <<'EOF' || rc=2
import glob, json, sys
out, rc = sys.argv[1], int(sys.argv[2])
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
status = "fail" if bad else ("incomplete" if rc else "ok")
json.dump({"part": "sysbench", "status": status, "results": res}, open(f"{out}/sysbench.json", "w"), indent=1)
sys.exit({"ok": 0, "fail": 1, "incomplete": 2}[status])
EOF
    exit "$rc"
    ;;
faults)
    rc=0
    (cd "$root" && tools/ab/faults/run.sh -b "$bin") >"$out/faults.log" 2>&1 || rc=$?
    f=$(ls -t "$root"/target/faults/*.json 2>/dev/null | head -1)
    [ -n "$f" ] && cp "$f" "$out/faults-raw.json"
    write_status "$(status_of $rc)" "{\"raw\": \"faults-raw.json\", \"exit\": $rc}"
    exit "$rc"
    ;;
locks)
    start_cluster || { write_status incomplete '{"error": "cluster setup failed"}'; exit 2; }
    rc=0
    python3 "$root/tools/ab/locks/mixed_locks.py" --lockgen "${LOCKGEN:?LOCKGEN}" --a "$a" --b "$b" \
        --a-status 127.0.0.1:33080 --b-status 127.0.0.1:10081 --pd "$pd" \
        --json "$out/locks-raw.json" >"$out/locks.log" 2>&1 || rc=$?
    write_status "$(status_of $rc)" "{\"raw\": \"locks-raw.json\", \"exit\": $rc}"
    exit "$rc"
    ;;
*)
    sed -n '2,20p' "$0" >&2
    exit 2
    ;;
esac
