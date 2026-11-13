#!/usr/bin/env bash
# Entry point of the TiKV fault-scenario comparison (#128 item 2), meant for a weekly build.
#
#   tools/ab/faults/run.sh [-b path/to/tidb-server] [-s scenarios] [-p phase-seconds] [-k] [-j results.json]
#
# Starts (or reuses) a tiup playground v8.5.8 with 1 PD + 3 TiKV + Go TiDB (tag maya-faults,
# port offset 24000: Go TiDB 127.0.0.1:28000, PD 127.0.0.1:26379), loads sbtest1 (100k rows,
# 8 regions), starts tidb-rs on 127.0.0.1:28001, runs tools/ab/faults/faults.py and writes
# target/faults/<timestamp>.json and .log. -k keeps the cluster afterwards (default: clean it).
#
# Exit status: faults.py's: 1 if any read returned a wrong result (including a read of the
# written rows that went back in time), or tidb-rs showed an error kind that Go TiDB did not and
# that tools/ab/faults/expected.json does not list; 2 if nothing new was found but a scenario
# was skipped or failed to inject its fault; 0 otherwise.
# Requires: tiup, mysql client, python3-pymysql; passwordless sudo for slow_tikv / net_cut.
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)
bin=""
scenarios="split_merge,transfer_leader,kill_tikv,kill_tikv_meta,slow_tikv,net_cut,pause_tikv,wide_rows"
phase=10
keep=0
json_out=""
while getopts "b:s:p:kj:" opt; do
    case $opt in
        b) bin=$OPTARG ;;
        s) scenarios=$OPTARG ;;
        p) phase=$OPTARG ;;
        k) keep=1 ;;
        j) json_out=$OPTARG ;;
        *) echo "usage: $0 [-b tidb-server] [-s scenarios] [-p phase] [-k] [-j results.json]" >&2; exit 2 ;;
    esac
done

export PLAYGROUND_TAG=maya-faults PLAYGROUND_OFFSET=24000 PLAYGROUND_KV=3
a=127.0.0.1:28000 b=127.0.0.1:28001 pd=127.0.0.1:26379

if ! mysql -h 127.0.0.1 -P 28000 -u root -N -e 'select 1' >/dev/null 2>&1; then
    "$root/scripts/playground.sh" start
    "$root/scripts/sysbench-prepare.sh" -P 28000 -t 1 -s 100000 -r 8
fi

if [ -z "$bin" ]; then
    (cd "$root" && cargo build --release --locked -p poc-server)
    bin="$root/target/release/tidb-server"
fi
out="$root/target/faults"
mkdir -p "$out"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
"$bin" -P 28001 --status-port 10082 --pd "$pd" --auto-tls=false >"$out/$stamp-tidb-rs.log" 2>&1 &
rs_pid=$!
cleanup() {
    kill "$rs_pid" 2>/dev/null || true
    wait "$rs_pid" 2>/dev/null || true
    # FAULTS_LOG_DIR: keep the cluster's own logs (PD, TiKV, Go TiDB) before they are cleaned.
    if [ -n "${FAULTS_LOG_DIR:-}" ]; then
        mkdir -p "$FAULTS_LOG_DIR" || true
        for f in "${TIUP_HOME:-$HOME/.tiup}/data/$PLAYGROUND_TAG"/*/*.log; do
            [ -f "$f" ] && { cp "$f" "$FAULTS_LOG_DIR/$(basename "$(dirname "$f")")-$(basename "$f")" || true; }
        done
        cp "$out/$stamp-tidb-rs.log" "$FAULTS_LOG_DIR/tidb-rs.log" 2>/dev/null || true
    fi
    if [ "$keep" = 0 ]; then "$root/scripts/playground.sh" clean || true; fi
}
trap cleanup EXIT
for _ in $(seq 120); do
    mysql -h 127.0.0.1 -P 28001 -u root -N -e 'select 1' >/dev/null 2>&1 && break
    sleep 0.5
done

set +e
python3 "$root/tools/ab/faults/faults.py" --a "$a" --b "$b" --pd "$pd" \
    --scenarios "$scenarios" --phase "$phase" \
    --expected "$root/tools/ab/faults/expected.json" --json "$out/$stamp.json" | tee "$out/$stamp.log"
status=${PIPESTATUS[0]}
set -e
if [ -n "$json_out" ] && [ -f "$out/$stamp.json" ]; then cp "$out/$stamp.json" "$json_out"; fi
echo "results: $out/$stamp.json (exit $status)"
exit "$status"
