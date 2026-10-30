#!/usr/bin/env bash
# Run TiDB's integration tests (tests/integrationtest/t/*.test) against tidb-rs in hybrid mode,
# end to end: build, start a tiup playground, join tidb-rs, run `ab integrationtest`, tear down.
#
#   scripts/integrationtest.sh [-t tag] [-o port-offset] [-v version] [-d tests-dir] [-O out-dir]
#                              [-k] [-- test names or prefixes...]
#
#   -d  tests/integrationtest/t of a TiDB checkout (default: $TIDB_SRC/tests/integrationtest/t,
#       TIDB_SRC defaults to ~/src/pingcap/tidb-all-source-code/tidb)
#   -O  output directory (default target/integrationtest): statements.tsv, files.tsv,
#       mismatches.jsonl, run.tsv, ab.log, tidb-rs.log
#   -k  keep the playground and tidb-rs running afterwards (default: stop and clean them)
#
# Defaults: tag maya-it, offset 25000, version v8.5.8. Ports: Go TiDB 4000+offset, tidb-rs
# 4001+offset (status 10081+offset), PD 2379+offset. Only this tag's processes are touched.
# Like TiDB's run-tests.sh, everything runs with TZ=Asia/Shanghai. Needs tiup, mysql, git, curl.
# The tidb-rs-tracker imports the output with `scripts/tracker.py import-integration <out-dir>`.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
tag=${PLAYGROUND_TAG:-maya-it}
offset=${PLAYGROUND_OFFSET:-25000}
version=${PLAYGROUND_VERSION:-v8.5.8}
tidb_src=${TIDB_SRC:-$HOME/src/pingcap/tidb-all-source-code/tidb}
tests_dir=
out=$repo/target/integrationtest
keep=0
while getopts "t:o:v:d:O:k" opt; do
    case $opt in
        t) tag=$OPTARG ;;
        o) offset=$OPTARG ;;
        v) version=$OPTARG ;;
        d) tests_dir=$OPTARG ;;
        O) out=$OPTARG ;;
        k) keep=1 ;;
        *) sed -n '2,20p' "$0" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ "${1:-}" = "--" ] && shift
tests_dir=${tests_dir:-$tidb_src/tests/integrationtest/t}
[ -d "$tests_dir" ] || { echo "no tests directory $tests_dir" >&2; exit 2; }

export TZ=Asia/Shanghai
a_port=$((4000 + offset))
b_port=$((4001 + offset))
b_status=$((10081 + offset))
mkdir -p "$out"

cleanup() {
    if [ "$keep" = 0 ]; then
        "$repo/scripts/playground-join.sh" stop -t "$tag" -P "$b_port" >/dev/null 2>&1 || true
        "$repo/scripts/playground.sh" clean -t "$tag" -o "$offset" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

echo "== build"
(cd "$repo" && cargo build --release -p tidb-server -p tidb-ab)

echo "== playground $tag (offset $offset)"
"$repo/scripts/playground.sh" start -t "$tag" -o "$offset" -v "$version" \
    -c "$repo/tools/ab/integrationtest.toml"
# As in scripts/sysbench-prepare.sh: the ingest backfill refuses to run on a nearly full disk.
mysql -h 127.0.0.1 -P "$a_port" -u root -e \
    "SET GLOBAL tidb_ddl_enable_fast_reorg = OFF; SET GLOBAL tidb_enable_dist_task = OFF"

echo "== tidb-rs on $b_port"
"$repo/scripts/playground-join.sh" start -t "$tag" -P "$b_port" -s "$b_status"

echo "== ab integrationtest ($tests_dir) -> $out"
start=$(date +%s)
set +e
"$repo/target/release/ab" -a "127.0.0.1:$a_port" -b "127.0.0.1:$b_port" --timeout 60 \
    --out-dir "$out" integrationtest --dir "$tests_dir" "$@" 2>&1 | tee "$out/ab.log"
status=${PIPESTATUS[0]}
set -e
seconds=$(( $(date +%s) - start ))

b_alive=no
curl -fsS --max-time 2 "http://127.0.0.1:$b_status/status" >/dev/null 2>&1 && b_alive=yes
cp "$HOME/.tiup/data/$tag/tidb-rs-$b_port/tidb-rs.log" "$out/tidb-rs.log" 2>/dev/null || true
tidb_version=$(mysql -h 127.0.0.1 -P "$a_port" -u root -N -e 'SELECT version()' | sed 's/.*-TiDB-//')
{
    printf 'key\tvalue\n'
    printf 'tidb_rs_commit\t%s\n' "$(git -C "$repo" rev-parse HEAD)"
    printf 'tidb_rs_dirty\t%s\n' "$(git -C "$repo" status --porcelain --untracked-files=no | grep -q . && echo yes || echo no)"
    printf 'tidb_version\t%s\n' "${tidb_version:-$version}"
    printf 'tests_commit\t%s\n' "$(git -C "$tests_dir" rev-parse HEAD 2>/dev/null || echo unknown)"
    printf 'date\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'runner\t%s\n' "$(uname -sm) $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -n)"
    printf 'seconds\t%s\n' "$seconds"
    printf 'tidb_rs_alive_at_end\t%s\n' "$b_alive"
} >"$out/run.tsv"
echo "== done in ${seconds}s (ab exit $status, tidb-rs alive at end: $b_alive); output in $out"
# ab exits 1 when there are mismatches; that is a result, not a failure of the run.
[ "$status" -le 1 ]
