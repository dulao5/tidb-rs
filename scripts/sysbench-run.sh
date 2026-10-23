#!/usr/bin/env bash
# Run sysbench oltp_read_only (autocommit, prepared statements) against Go TiDB or tidb-rs.
#
#   scripts/sysbench-run.sh run     [-e host:port] [-n name] [opts]
#   scripts/sysbench-run.sh compare [-a host:port] [-b host:port] [opts]
#
#   -e host:port  endpoint for `run`                       (default 127.0.0.1:27000)
#   -n name       label used in file names for `run`       (default: the endpoint)
#   -a / -b       endpoints A (Go TiDB) / B (tidb-rs)      (default 127.0.0.1:27000 / 127.0.0.1:27001)
#   -T threads    space- or comma-separated thread counts  (default "1 8 16")
#   -d seconds    duration of each run                     (default 30)
#   -t tables     sbtest tables                            (default 1)
#   -s size       rows per table (as prepared)             (default 100000)
#   -D db         database                                 (default sbtest)
#   -o dir        results directory                        (default target/sysbench)
#   -x "args"     extra sysbench arguments
#
# Each run's raw output is saved as <dir>/<timestamp>-<name>-t<threads>.txt and summarised as one
# line: QPS, average and p95 latency (ms, per oltp_read_only event = 10 point selects + 4 range
# queries), errors (sysbench "ignored errors" + reconnects), and FATAL if sysbench aborted.
# p95 comes from scripts/oltp_read_only_lat.lua (the stock workload plus our own latency
# histogram) because Homebrew's sysbench 1.0.20 on Apple Silicon reports all percentiles as 0. `compare` runs A then B for every thread count (sequentially, so
# they do not compete for CPU) and prints a table.
#
# Mode: oltp_read_only --skip_trx=on (no BEGIN/COMMIT: tidb-rs is an autocommit read-only node)
# --db-ps-mode=auto (server-side prepared statements, like real sysbench runs).
set -euo pipefail

cmd=${1:-}
[ $# -gt 0 ] && shift
endpoint=127.0.0.1:27000
name=
a=127.0.0.1:27000
b=127.0.0.1:27001
threads="1 8 16"
duration=30
tables=1
size=100000
db=sbtest
script_dir=$(cd "$(dirname "$0")" && pwd)
outdir=$(cd "$script_dir/.." && pwd)/target/sysbench
extra=
while getopts "e:n:a:b:T:d:t:s:D:o:x:" opt; do
    case $opt in
        e) endpoint=$OPTARG ;;
        n) name=$OPTARG ;;
        a) a=$OPTARG ;;
        b) b=$OPTARG ;;
        T) threads=${OPTARG//,/ } ;;
        d) duration=$OPTARG ;;
        t) tables=$OPTARG ;;
        s) size=$OPTARG ;;
        D) db=$OPTARG ;;
        o) outdir=$OPTARG ;;
        x) extra=$OPTARG ;;
        *) sed -n '2,25p' "$0" >&2; exit 2 ;;
    esac
done
mkdir -p "$outdir"
stamp=$(date +%Y%m%d-%H%M%S)

# run_one <host:port> <name> <threads> -> prints "name threads qps avg p95 errors status file"
run_one() {
    local ep=$1 label=$2 t=$3
    local host=${ep%:*} port=${ep##*:}
    local file=$outdir/$stamp-${label//[:\/]/_}-t$t.txt
    local lat=$file.lat
    rm -rf "$lat" && mkdir -p "$lat"
    local rc=0
    # The braces' 2>/dev/null swallows bash's "Segmentation fault" job message: sysbench 1.0.20
    # crashes after a failed thread_init (e.g. the server rejects the prepared statements).
    # shellcheck disable=SC2086
    { sysbench "$script_dir/oltp_read_only_lat.lua" --lat_dir="$lat" \
        --mysql-host="$host" --mysql-port="$port" --mysql-user="${MYSQL_USER:-root}" \
        --mysql-db="$db" --tables="$tables" --table-size="$size" \
        --skip_trx=on --db-ps-mode=auto --threads="$t" --time="$duration" \
        --report-interval=10 $extra run >"$file" 2>&1; } 2>/dev/null || rc=$?
    if [ "$rc" -ge 128 ]; then
        echo "FATAL: sysbench killed by signal $((rc - 128))" >>"$file"
    fi
    # p95 from our own histogram (see oltp_read_only_lat.lua): bucket b = up to exp((b+1)/50) us.
    local p95
    p95=$( (cat "$lat"/thread-*.txt 2>/dev/null || true) | awk '
        { n[$1] += $2; total += $2 }
        END {
            if (total == 0) { print "-"; exit }
            m = 0; for (b in n) keys[++m] = b + 0
            for (i = 2; i <= m; i++) { k = keys[i]; for (j = i - 1; j >= 1 && keys[j] > k; j--) keys[j + 1] = keys[j]; keys[j + 1] = k }
            want = total * 0.95; acc = 0
            for (i = 1; i <= m; i++) { acc += n[keys[i]]; if (acc >= want) { printf "%.2f\n", exp((keys[i] + 1) / 50) / 1000; exit } }
        }')
    echo "p95 latency (ms, per event, from $lat): $p95" >>"$file"
    awk -v label="$label" -v t="$t" -v file="$file" -v p95="$p95" '
        /queries:/ && /per sec/ { gsub(/[(]/, "", $3); qps = $3 }
        /^ +avg:/ { avg = $2 }
        /ignored errors:/ { err += $3 }
        /reconnects:/ { err += $2 }
        /FATAL/ { fatal = 1 }
        END {
            status = (fatal || qps == "") ? "FATAL" : "ok"
            printf "%-24s %4s %12s %9s %9s %7s  %-5s %s\n", label, t, (qps == "" ? "-" : qps),
                (avg == "" ? "-" : avg), p95, err + 0, status, file
        }' "$file"
}

header() {
    printf "%-24s %4s %12s %9s %9s %7s  %-5s %s\n" endpoint thr QPS avg_ms p95_ms errors status raw_output
}

case $cmd in
    run)
        header
        for t in $threads; do run_one "$endpoint" "${name:-$endpoint}" "$t"; done
        ;;
    compare)
        rows=$outdir/$stamp-compare.txt
        header | tee "$rows"
        for t in $threads; do
            run_one "$a" "A:$a" "$t" | tee -a "$rows"
            run_one "$b" "B:$b" "$t" | tee -a "$rows"
        done
        echo
        echo "B/A QPS ratio:"
        awk 'NR > 1 { split($1, e, ":"); q[e[1], $2] = $3; ts[$2] = 1 }
             END { for (t in ts)
                       if (q["A", t] + 0 > 0 && q["B", t] + 0 > 0) printf "  threads %4s: %.2f\n", t, q["B", t] / q["A", t]
                       else printf "  threads %4s: n/a\n", t }' "$rows" | sort -k2 -n
        ;;
    *) sed -n '2,25p' "$0" >&2; exit 2 ;;
esac
