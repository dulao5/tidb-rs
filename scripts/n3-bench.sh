#!/usr/bin/env bash
# N3 runtime benchmarks (docs/bench/n3-runtime.md): sysbench point_select / read_only, point
# gets under a concurrent full-table scan, and overload, against tidb-rs builds and Go TiDB.
#
#   scripts/n3-bench.sh <label> <tidb-server-binary|go> [extra tidb-server args...]
#
# Environment (defaults in brackets):
#   PD         PD endpoint of the playground          [127.0.0.1:26379]
#   GO_EP      Go TiDB of the same playground          [127.0.0.1:28000]
#   PORT       tidb-rs MySQL port                      [28001]
#   STATUS     tidb-rs status port                     [34081]
#   DUR        seconds per sysbench run                [15]
#   THREADS    thread counts for point_select/read_only [8 32 64]
#   SCAN_T     point-get threads during the scan test  [16]
#   OVERLOAD   thread counts for the overload sweep     [16 32 64 128 256 320]
#   OUT        output directory                        [target/n3bench]
#   TESTS      which tests                             [ps ro scan overload]
#
# Appends one line per run to $OUT/results.tsv:
#   label test threads qps p95_ms p99_ms errors extra
# where latency is per sysbench event (point_select: one query; read_only: 10 point selects +
# 4 range queries), from the histogram of scripts/oltp_read_only_lat.lua. After the tidb-rs
# runs, the status port's /metrics is saved as $OUT/<label>-metrics.txt.
set -euo pipefail

label=$1
bin=$2
shift 2
pd=${PD:-127.0.0.1:26379}
go_ep=${GO_EP:-127.0.0.1:28000}
port=${PORT:-28001}
status=${STATUS:-34081}
dur=${DUR:-15}
threads=${THREADS:-"8 32 64"}
scan_t=${SCAN_T:-16}
overload=${OVERLOAD:-"16 32 64 128 256 320"}
tests=${TESTS:-"ps ro scan overload"}
script_dir=$(cd "$(dirname "$0")" && pwd)
out=${OUT:-$(cd "$script_dir/.." && pwd)/target/n3bench}
mkdir -p "$out"
results=$out/results.tsv
[ -f "$results" ] || printf "label\ttest\tthreads\tqps\tp95_ms\tp99_ms\terrors\textra\n" >"$results"

server_pid=
if [ "$bin" = go ]; then
    ep=$go_ep
else
    ep=127.0.0.1:$port
    "$bin" --pd "$pd" --port "$port" --status-port "$status" --log-level warn "$@" \
        >"$out/$label-server.log" 2>&1 &
    server_pid=$!
    trap 'kill $server_pid 2>/dev/null || true; wait $server_pid 2>/dev/null || true' EXIT
    for _ in $(seq 1 120); do
        mysql -h 127.0.0.1 -P "$port" -u root --connect-timeout=2 -N -e 'SELECT 1' >/dev/null 2>&1 && break
        sleep 1
    done
fi
host=${ep%:*}
p=${ep##*:}

# pct <lat_dir> <fraction> -> ms
pct() {
    (cat "$1"/thread-*.txt 2>/dev/null || true) | awk -v f="$2" '
        { n[$1] += $2; total += $2 }
        END {
            if (total == 0) { print "-"; exit }
            m = 0; for (b in n) keys[++m] = b + 0
            for (i = 2; i <= m; i++) { k = keys[i]; for (j = i - 1; j >= 1 && keys[j] > k; j--) keys[j + 1] = keys[j]; keys[j + 1] = k }
            want = total * f; acc = 0
            for (i = 1; i <= m; i++) { acc += n[keys[i]]; if (acc >= want) { printf "%.2f\n", exp((keys[i] + 1) / 50) / 1000; exit } }
        }'
}

# run <test> <threads> <sysbench args> [extra-note]
run() {
    local test=$1 t=$2 args=$3 note=${4:-}
    local file=$out/$label-$test-t$t-$(date +%H%M%S).txt lat
    lat=$file.lat
    rm -rf "$lat" && mkdir -p "$lat"
    # shellcheck disable=SC2086
    { sysbench "$script_dir/oltp_read_only_lat.lua" --lat_dir="$lat" \
        --mysql-host="$host" --mysql-port="$p" --mysql-user=root --mysql-db=sbtest \
        --tables=1 --table-size=1000000 --skip_trx=on --db-ps-mode=auto \
        --threads="$t" --time="$dur" $args run >"$file" 2>&1; } 2>/dev/null || true
    local qps err
    qps=$(awk '/queries:/ && /per sec/ { gsub(/[(]/, "", $3); print $3 }' "$file")
    err=$(awk '/ignored errors:/ { e += $3 } /reconnects:/ { e += $2 } END { print e + 0 }' "$file")
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$label" "$test" "$t" "${qps:--}" \
        "$(pct "$lat" 0.95)" "$(pct "$lat" 0.99)" "$err" "$note" | tee -a "$results"
}

ps_args="--point_selects=1 --simple_ranges=0 --sum_ranges=0 --order_ranges=0 --distinct_ranges=0"

for test in $tests; do
    case $test in
        ps) for t in $threads; do run ps "$t" "$ps_args"; done ;;
        ro) for t in $threads; do run ro "$t" ""; done ;;
        scan)
            # Point gets while one session scans and aggregates the whole 1M-row table in a loop.
            stop_file=$out/.scan-stop
            rm -f "$stop_file"
            scans=$out/.scan-count
            : >"$scans"
            (while [ ! -f "$stop_file" ]; do
                mysql -h "$host" -P "$p" -u root -N -e \
                    'SELECT COUNT(*), SUM(k) FROM sbtest.sbtest1' >/dev/null 2>&1 && echo x >>"$scans"
            done) &
            scan_pid=$!
            sleep 2
            run scan "$scan_t" "$ps_args"
            touch "$stop_file"
            wait $scan_pid 2>/dev/null || true
            echo "  ($(wc -l <"$scans" | tr -d ' ') full scans completed during the run)"
            ;;
        overload) for t in $overload; do run overload "$t" "$ps_args"; done ;;
    esac
done

if [ -n "$server_pid" ]; then
    curl -s "http://127.0.0.1:$status/metrics" >"$out/$label-metrics.txt" || true
fi
