#!/usr/bin/env bash
# Start/stop a tiup playground (1 PD, 1 TiKV, 1 Go TiDB, no TiFlash) for the tidb-rs A/B harness.
#
#   scripts/playground.sh start  [-t tag] [-o port-offset] [-v version]
#   scripts/playground.sh status [-t tag] [-o port-offset]
#   scripts/playground.sh stop   [-t tag]
#   scripts/playground.sh clean  [-t tag]      # stop + `tiup clean <tag>`
#
# Defaults: tag maya-ab, offset 23000, version v8.5.8 (env: PLAYGROUND_TAG, PLAYGROUND_OFFSET,
# PLAYGROUND_VERSION). Ports are default+offset: TiDB 4000+o, PD 2379+o, TiDB status 10080+o.
# Only processes of the given tag are ever touched: other playgrounds on this machine (other
# tags/offsets) are left alone.
#
# Go TiDB is started with security.auto-tls so TLS-only clients (Homebrew sysbench links
# MariaDB Connector/C, which insists on TLS) can connect.
set -euo pipefail

cmd=${1:-}
[ $# -gt 0 ] && shift
tag=${PLAYGROUND_TAG:-maya-ab}
offset=${PLAYGROUND_OFFSET:-23000}
version=${PLAYGROUND_VERSION:-v8.5.8}
while getopts "t:o:v:" opt; do
    case $opt in
        t) tag=$OPTARG ;;
        o) offset=$OPTARG ;;
        v) version=$OPTARG ;;
        *) echo "usage: $0 start|stop|status|clean [-t tag] [-o offset] [-v version]" >&2; exit 2 ;;
    esac
done

tiup_home=${TIUP_HOME:-$HOME/.tiup}
data_dir=$tiup_home/data/$tag
state_dir=${TMPDIR:-/tmp}/tidb-rs-playground
mkdir -p "$state_dir"
log=$state_dir/$tag.log
pidfile=$state_dir/$tag.pid
tidb_port=$((4000 + offset))
pd_port=$((2379 + offset))
status_port=$((10080 + offset))

tidb_up() {
    mysql -h 127.0.0.1 -P "$tidb_port" -u root --connect-timeout=2 -N -e 'SELECT 1' >/dev/null 2>&1
}

# PIDs of every process belonging to this tag: the playground itself (`--tag <tag>`) and the
# components it spawned (their data/log paths live under $data_dir/). The trailing `/` and the
# word boundary keep e.g. tag `ab` from matching `ab2`.
tag_pids() {
    # The tag/dir are passed through the environment so this awk's own command line never matches.
    ps -axo pid=,command= | AB_DIR="$data_dir/" AB_TAG="$tag" awk '
        BEGIN { dir = ENVIRON["AB_DIR"]; tag = ENVIRON["AB_TAG"] }
        index($0, dir) > 0 { print $1; next }
        $0 ~ /tiup-playground|tiup playground/ {
            for (i = 2; i < NF; i++) if (($i == "--tag" || $i == "-T") && $(i+1) == tag) { print $1; next }
            for (i = 2; i <= NF; i++) if ($i == "--tag=" tag) { print $1; next }
        }'
}

print_addrs() {
    echo "tag:        $tag"
    echo "PD:         127.0.0.1:$pd_port"
    echo "TiDB:       127.0.0.1:$tidb_port   (mysql -h 127.0.0.1 -P $tidb_port -u root)"
    echo "TiDB status http://127.0.0.1:$status_port/status"
    echo "log:        $log"
}

start() {
    if tidb_up; then
        echo "playground $tag already answering on $tidb_port"
        print_addrs
        return 0
    fi
    if [ -n "$(tag_pids)" ]; then
        echo "processes of tag $tag exist but TiDB does not answer; run '$0 stop -t $tag' first" >&2
        tag_pids >&2
        exit 1
    fi
    local cfg=$state_dir/$tag-tidb.toml
    cat >"$cfg" <<'EOF'
[security]
auto-tls = true
EOF
    echo "starting tiup playground $version tag=$tag offset=$offset (log: $log)"
    nohup tiup playground "$version" --tag "$tag" --db 1 --pd 1 --kv 1 --tiflash 0 \
        --port-offset "$offset" --without-monitor --db.config "$cfg" >"$log" 2>&1 &
    echo $! >"$pidfile"
    local deadline=$((SECONDS + ${PLAYGROUND_TIMEOUT:-180}))
    until tidb_up; do
        if ! kill -0 "$(cat "$pidfile")" 2>/dev/null; then
            echo "tiup playground exited; last log lines:" >&2
            tail -n 30 "$log" >&2
            exit 1
        fi
        if [ $SECONDS -ge $deadline ]; then
            echo "timed out waiting for TiDB on $tidb_port; see $log" >&2
            exit 1
        fi
        sleep 2
    done
    echo "TiDB is up: $(mysql -h 127.0.0.1 -P "$tidb_port" -u root -N -e 'SELECT version()')"
    print_addrs
}

stop() {
    local pids
    pids=$(tag_pids)
    if [ -z "$pids" ]; then
        echo "no processes for tag $tag"
        rm -f "$pidfile"
        return 0
    fi
    # The playground first (it stops its children gracefully on SIGINT/SIGTERM), then whatever is
    # left of this tag (tiup sometimes leaves pd-server/tikv-server behind).
    if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
        kill -INT "$(cat "$pidfile")" 2>/dev/null || true
    fi
    for _ in $(seq 1 30); do
        pids=$(tag_pids)
        [ -z "$pids" ] && break
        # shellcheck disable=SC2086
        kill -TERM $pids 2>/dev/null || true
        sleep 1
    done
    pids=$(tag_pids)
    if [ -n "$pids" ]; then
        echo "force killing leftover processes of tag $tag: $(echo $pids)"
        # shellcheck disable=SC2086
        kill -KILL $pids 2>/dev/null || true
        sleep 1
    fi
    rm -f "$pidfile"
    echo "stopped playground $tag"
}

status() {
    local pids
    pids=$(tag_pids)
    if tidb_up; then
        echo "running (TiDB answers on $tidb_port)"
    elif [ -n "$pids" ]; then
        echo "processes exist but TiDB does not answer on $tidb_port"
    else
        echo "not running"
    fi
    if [ -n "$pids" ]; then
        # shellcheck disable=SC2086
        ps -o pid=,command= -p $(echo $pids | tr ' ' ',') | cut -c1-160
    fi
    print_addrs
    tidb_up
}

case $cmd in
    start) start ;;
    stop) stop ;;
    status) status ;;
    clean)
        stop
        tiup clean "$tag" || true
        rm -f "$state_dir/$tag-tidb.toml"
        ;;
    *) echo "usage: $0 start|stop|status|clean [-t tag] [-o offset] [-v version]" >&2; exit 2 ;;
esac
