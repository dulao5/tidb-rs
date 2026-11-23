#!/usr/bin/env bash
# Allocation profile of tidb-server under the prepared point-select load: every allocation is
# counted by its call stack, then tools/perf/alloc_report.py groups them per statement.
#
#   tools/perf/alloc-profile.sh build                 # -> target/allocprof/tidb-server
#   tools/perf/alloc-profile.sh run -p <pd> [-P port] [-n statements] [-o out-dir]
#
# `build` copies tools/perf/allocprof.rs into the server crate for one build (into its own
# target dir, target/allocprof) and restores the crate afterwards: the profiler is never part of
# a normal build. `run` starts that binary (`--new-stack=fallback`), warms up with 2000
# statements, records exactly -n statements (default 20000) of sysbench oltp_point_select on one
# connection and writes <out>/stacks.txt and <out>/report.txt.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cmd=${1:-}
[ $# -gt 0 ] && shift

build() {
    local crate=$root/crates/poc/server
    restore() {
        git -C "$root" checkout -- crates/poc/server/src/main.rs crates/poc/server/Cargo.toml Cargo.lock
        rm -f "$root/crates/poc/server/src/allocprof.rs"
    }
    trap restore EXIT
    cp "$root/tools/perf/allocprof.rs" "$crate/src/allocprof.rs"
    python3 - "$crate" <<'EOF'
import sys
crate = sys.argv[1]
p = f"{crate}/src/main.rs"
s = open(p).read()
s = s.replace("#![forbid(unsafe_code)]", "#![deny(unsafe_code)]\n#[allow(unsafe_code)]\nmod allocprof;", 1)
s = s.replace("static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;",
              "static GLOBAL: allocprof::Prof = allocprof::Prof;", 1)
s = s.replace("fn main() {", "fn main() {\n    allocprof::start();", 1)
open(p, "w").write(s)
p = f"{crate}/Cargo.toml"
s = open(p).read()
s = s.replace("[dependencies]\n", "[dependencies]\nbacktrace = \"0.3\"\n", 1)
open(p, "w").write(s)
EOF
    (cd "$root" && CARGO_TARGET_DIR=$root/target/allocprof CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
        cargo build --release -p poc-server --bin tidb-server)
    cp "$root/target/allocprof/release/tidb-server" "$root/target/allocprof/tidb-server"
    echo "built $root/target/allocprof/tidb-server"
}

run() {
    local pd= port=29102 n=20000 out=$root/target/allocprof/out
    while getopts "p:P:n:o:" opt; do
        case $opt in
            p) pd=$OPTARG ;;
            P) port=$OPTARG ;;
            n) n=$OPTARG ;;
            o) out=$OPTARG ;;
            *) sed -n '2,13p' "$0" >&2; exit 2 ;;
        esac
    done
    [ -n "$pd" ] || { echo "run needs -p <pd address>" >&2; exit 2; }
    mkdir -p "$out"
    local flag=$out/flag
    rm -f "$flag" "$out/stacks.txt"
    ALLOC_PROF_FLAG=$flag ALLOC_PROF_OUT=$out/stacks.txt "$root/target/allocprof/tidb-server" \
        --pd "$pd" --host 127.0.0.1 -P "$port" --status-port 0 --new-stack=fallback \
        >"$out/server.log" 2>&1 &
    local pid=$!
    trap 'kill $pid 2>/dev/null || true' EXIT
    until mysql -h 127.0.0.1 -P "$port" -u root -N -e 'SELECT 1' >/dev/null 2>&1; do sleep 0.5; done
    sb() {
        sysbench oltp_point_select --mysql-host=127.0.0.1 --mysql-port="$port" --mysql-user=root \
            --mysql-db=sbtest --tables=1 --table-size=10000 --db-ps-mode=auto --threads=1 \
            --events="$1" --time=0 run >/dev/null
    }
    sb 2000
    touch "$flag"
    sb "$n"
    rm -f "$flag"
    until [ -s "$out/stacks.txt" ]; do sleep 1; done
    python3 "$root/tools/perf/alloc_report.py" "$out/stacks.txt" "$n" | tee "$out/report.txt"
}

case $cmd in
    build) build ;;
    run) run "$@" ;;
    *) sed -n '2,13p' "$0" >&2; exit 2 ;;
esac
