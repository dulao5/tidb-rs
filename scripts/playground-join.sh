#!/usr/bin/env bash
# Add a locally built tidb-rs to an already running `tiup playground`, including its monitoring.
#
#   scripts/playground-join.sh start    [-t tag] [-P port] [-s status-port] [-b binary] [-- tidb-server args...]
#   scripts/playground-join.sh stop     [-t tag] [-P port]     # without -P: every node this script started there
#   scripts/playground-join.sh status   [-t tag] [-P port]     # without -P: every node this script started there
#   scripts/playground-join.sh register [-t tag] [-P port]     # re-add the Prometheus target(s) only
#
# The playground is found by tag (-t, i.e. the directory name under ~/.tiup/data). Without -t the
# only running playground is used; with several running, -t is required. PD endpoints are read
# from the playground's pd-server processes, so playgrounds started with --port-offset work too.
#
# start runs the binary (default: target/release/tidb-server of this checkout; build it first
# with `cargo build --release`) in the background with --pd/--port/--status-port, waits until its
# status port answers, then adds 127.0.0.1:<status-port> to the `job: tidb` group of the
# playground's Prometheus file discovery (<data>/prometheus/targets.json). Prometheus re-reads
# that file by itself; in Grafana's TiDB dashboard pick the instance 127.0.0.1:<status-port>.
# Arguments after `--` are passed to tidb-server unchanged.
#
# stop sends SIGTERM (tidb-rs deregisters from schema sync in ~20 ms, so DDL is not held up),
# waits for the process to exit and removes the Prometheus target.
#
# Defaults: port 4001, status port 10081 (env: TIDB_RS_PORT, TIDB_RS_STATUS_PORT, TIDB_RS_BIN).
# Log and pid file live in <data>/tidb-rs-<port>/, so `tiup clean` removes them with the
# playground. The playground rewrites targets.json when it scales or restarts; run `register`
# again afterwards. Several tidb-rs nodes can join the same playground with distinct -P/-s.
# Stopping the playground does not stop tidb-rs (it is not a playground component): run `stop`
# first.
set -euo pipefail

cmd=${1:-}
[ $# -gt 0 ] && shift
repo=$(cd "$(dirname "$0")/.." && pwd)
tag=${PLAYGROUND_TAG:-}
port=${TIDB_RS_PORT:-}
status_port=${TIDB_RS_STATUS_PORT:-10081}
bin=${TIDB_RS_BIN:-$repo/target/release/tidb-server}
usage="usage: $0 start|stop|status|register [-t tag] [-P port] [-s status-port] [-b binary] [-- tidb-server args...]"
while getopts "t:P:s:b:" opt; do
    case $opt in
        t) tag=$OPTARG ;;
        P) port=$OPTARG ;;
        s) status_port=$OPTARG ;;
        b) bin=$OPTARG ;;
        *) echo "$usage" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ "${1:-}" = "--" ] && shift
extra_args=("$@")

tiup_home=${TIUP_HOME:-$HOME/.tiup}

# Tags of running playgrounds: data dirs whose tiup_process_meta names a live playground pid.
running_tags() {
    local meta pid
    for meta in "$tiup_home"/data/*/tiup_process_meta; do
        [ -f "$meta" ] || continue
        pid=$(python3 -I -c '
import json, sys
m = json.load(open(sys.argv[1]))
print(m.get("pid", "") if m.get("component") == "playground" else "")' "$meta" 2>/dev/null) || continue
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            basename "$(dirname "$meta")"
        fi
    done
}

resolve_tag() {
    if [ -n "$tag" ]; then
        [ -d "$tiup_home/data/$tag" ] || { echo "no playground data dir $tiup_home/data/$tag" >&2; exit 1; }
        return
    fi
    local tags
    tags=$(running_tags)
    case $(printf '%s' "$tags" | grep -c .) in
        0) echo "no running tiup playground found under $tiup_home/data" >&2; exit 1 ;;
        1) tag=$tags ;;
        *) echo "several playgrounds are running; pick one with -t:" >&2
           printf '  %s\n' $tags >&2; exit 1 ;;
    esac
}

# PD client endpoints (host:port, comma-separated) of the playground's pd-server processes.
pd_endpoints() {
    ps -axo command= | PG_DIR="$data_dir/" awk '
        BEGIN { dir = ENVIRON["PG_DIR"] }
        /pd-server/ && index($0, dir) > 0 {
            for (i = 1; i <= NF; i++) if ($i ~ /^--client-urls=/) {
                sub(/^--client-urls=/, "", $i); n = split($i, u, ",")
                for (j = 1; j <= n; j++) { sub(/^https?:\/\//, "", u[j]); print u[j] }
            }
        }' | paste -sd, -
}

listening() { lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1; }
status_up() { curl -fsS --max-time 2 "http://127.0.0.1:$1/status" 2>/dev/null; }

# Select the node on MySQL port $1: its state dir, log, pid file and (once started) status port.
use_node() {
    port=$1
    node_dir=$data_dir/tidb-rs-$port
    log=$node_dir/tidb-rs.log
    pidfile=$node_dir/tidb-rs.pid
    if [ -f "$node_dir/status_port" ]; then status_port=$(cat "$node_dir/status_port"); fi
}

# MySQL ports of the nodes this script started in the playground (or just -P).
node_ports() {
    if [ -n "$port" ]; then echo "$port"; return; fi
    local d
    for d in "$data_dir"/tidb-rs-*/; do
        if [ -f "$d/tidb-rs.pid" ]; then basename "$d" | sed 's/^tidb-rs-//'; fi
    done
}

node_pid() {
    local pid
    [ -f "$pidfile" ] || return 1
    pid=$(cat "$pidfile")
    kill -0 "$pid" 2>/dev/null && ps -o command= -p "$pid" | grep -q tidb-server && echo "$pid"
}

# add|remove 127.0.0.1:<status-port> in the `job: tidb` group of the Prometheus file discovery.
edit_targets() {
    local targets=$data_dir/prometheus/targets.json
    if [ ! -f "$targets" ]; then
        echo "note: $targets not found (playground started without monitoring?); Prometheus not touched"
        return 0
    fi
    python3 -I - "$targets" "$1" "127.0.0.1:$status_port" <<'EOF'
import json, os, sys
path, action, target = sys.argv[1:]
groups = json.load(open(path))
tidb = next((g for g in groups if g.get("labels", {}).get("job") == "tidb"), None)
if tidb is None:
    tidb = {"targets": [], "labels": {"job": "tidb"}}
    groups.append(tidb)
before = list(tidb["targets"])
if action == "add" and target not in tidb["targets"]:
    tidb["targets"].append(target)
if action == "remove":
    tidb["targets"] = [t for t in tidb["targets"] if t != target]
if tidb["targets"] != before:
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(groups, f, indent="\t")
    os.replace(tmp, path)  # atomic: Prometheus never reads a half-written file
print(f"prometheus: job=tidb targets = {', '.join(tidb['targets'])}")
EOF
}

print_addrs() {
    echo "playground: $tag ($data_dir)"
    echo "PD:         $pd"
    echo "tidb-rs:    127.0.0.1:$port   (mysql -h 127.0.0.1 -P $port -u root)"
    echo "status:     http://127.0.0.1:$status_port/status  /metrics"
    echo "log:        $log"
    if [ -f "$data_dir/prometheus/targets.json" ]; then
        echo "grafana:    TiDB dashboard, instance 127.0.0.1:$status_port"
    fi
}

start() {
    local pid
    use_node "${port:-4001}"
    if pid=$(node_pid); then
        echo "tidb-rs already running (pid $pid)"
        edit_targets add
        print_addrs
        return 0
    fi
    status_port=$opt_status_port
    [ -x "$bin" ] || { echo "binary $bin not found; run 'cargo build --release' or pass -b" >&2; exit 1; }
    [ -n "$pd" ] || { echo "no pd-server process found for playground $tag" >&2; exit 1; }
    for p in "$port" "$status_port"; do
        if listening "$p"; then echo "port $p is already in use; pick another with -P/-s" >&2; exit 1; fi
    done
    mkdir -p "$node_dir"
    echo "$status_port" >"$node_dir/status_port"
    echo "starting $bin (log $log)"
    nohup "$bin" --pd "$pd" --host 127.0.0.1 -P "$port" --status-port "$status_port" \
        ${extra_args[@]+"${extra_args[@]}"} >>"$log" 2>&1 </dev/null &
    pid=$!
    echo "$pid" >"$pidfile"
    for _ in $(seq 1 60); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "tidb-rs exited during startup; last log lines:" >&2
            tail -n 20 "$log" >&2
            rm -f "$pidfile"
            exit 1
        fi
        if status_up "$status_port" >/dev/null && listening "$port"; then
            edit_targets add
            print_addrs
            return 0
        fi
        sleep 0.5
    done
    echo "tidb-rs (pid $pid) did not answer on $port/$status_port within 30 s; see $log" >&2
    exit 1
}

stop_node() {
    local pid
    if pid=$(node_pid); then
        kill -TERM "$pid"
        for _ in $(seq 1 50); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.2
        done
        if kill -0 "$pid" 2>/dev/null; then
            echo "pid $pid still alive after 10 s, sending SIGKILL (DDL may wait ~90 s for its lease)" >&2
            kill -KILL "$pid" 2>/dev/null || true
        fi
        echo "stopped tidb-rs on $port (pid $pid)"
    else
        echo "tidb-rs on $port was not running"
    fi
    rm -f "$pidfile"
    edit_targets remove
}

status_node() {
    local pid st
    if pid=$(node_pid); then
        echo "tidb-rs on $port: running (pid $pid)"
        if st=$(status_up "$status_port"); then echo "status:     $st"; fi
    else
        echo "tidb-rs on $port: not running"
    fi
    print_addrs
}

case $cmd in
    start|stop|status|register) ;;
    *) echo "$usage" >&2; exit 2 ;;
esac
opt_status_port=$status_port
resolve_tag
data_dir=$tiup_home/data/$tag
pd=$(pd_endpoints)

if [ "$cmd" = start ]; then
    start
    exit 0
fi
ports=$(node_ports)
if [ -z "$ports" ]; then
    echo "no tidb-rs started by this script in playground $tag"
    exit 0
fi
for p in $ports; do
    use_node "$p"
    case $cmd in
        stop) stop_node ;;
        status) status_node ;;
        register) edit_targets add ;;
    esac
done
