#!/usr/bin/env bash
# Mixed-cluster checks of the SET GLOBAL writer (D05.2 spec §6.6, block D06.7): Go TiDB and
# tidb-systable (crates/poc/domain/examples/systable_cli.rs) write mysql.global_variables /
# mysql.tidb of one playground. Start one first, e.g.
#   scripts/playground.sh start -t maya-pc3 -o 27000
#   scripts/systable-mixed.sh 127.0.0.1:29379 31000
set -euo pipefail
pd=${1:?pd host:port}
port=${2:?Go TiDB port}
cargo build -q -p poc-domain --example systable_cli
C=target/debug/examples/systable_cli
m() { mysql -h 127.0.0.1 -P "$port" -u root -N -e "$1"; }
fail() { echo "FAIL: $*"; exit 1; }
raw() { $C "$pd" raw "$1" "$2"; }

echo "1. tidb-rs writes, Go reads within 1.5 s (etcd notification); ADMIN CHECK TABLE"
$C "$pd" set-global max_execution_time 1234 >/dev/null
sleep 1.5
[ "$(m 'select @@global.max_execution_time')" = 1234 ] || fail "Go did not see the value"
m "admin check table mysql.global_variables"

echo "2. Go writes, tidb-rs reads"
m "set global max_execution_time = 777"
[ "$($C "$pd" get global_variables max_execution_time)" = 777 ] || fail "tidb-rs did not read Go's value"

echo "3. same row written by Go and by tidb-rs: byte-identical record and index value (handle aside)"
v=$((3221225472 + RANDOM))
m "set global tidb_mem_quota_query = $v"; a=$(raw global_variables tidb_mem_quota_query | sed -E 's/^(index .*)[0-9a-f]{16}$/\1/')
m "set global tidb_mem_quota_query = 1"
$C "$pd" set-global tidb_mem_quota_query "$v" >/dev/null; b=$(raw global_variables tidb_mem_quota_query | sed -E 's/^(index .*)[0-9a-f]{16}$/\1/')
[ "$a" = "$b" ] || fail "bytes differ: Go $a / tidb-rs $b"
c="All versions within life time will not be collected by GC, at least 10m, in Go format."
m "set global tidb_gc_life_time = '25m'"; a=$(raw tidb tikv_gc_life_time | sed -E 's/^(index .*)[0-9a-f]{16}$/\1/')
$C "$pd" set-tidb tikv_gc_life_time 1m0s x >/dev/null; $C "$pd" set-tidb tikv_gc_life_time 25m0s "$c" >/dev/null
b=$(raw tidb tikv_gc_life_time | sed -E 's/^(index .*)[0-9a-f]{16}$/\1/')
[ "$a" = "$b" ] || fail "mysql.tidb bytes differ"
m "admin check table mysql.tidb"

echo "4. no etcd notification: Go sees the value on its 30 s reload"
NO_ETCD=1 $C "$pd" set-global max_execution_time 4321 >/dev/null
for _ in $(seq 1 40); do [ "$(m 'select @@global.max_execution_time')" = 4321 ] && break; sleep 1; done
[ "$(m 'select @@global.max_execution_time')" = 4321 ] || fail "not seen within 40 s"

echo "5. concurrent SET GLOBAL of one variable from Go and tidb-rs: no error, one row"
(for i in $(seq 1 40); do m "set global tidb_mem_quota_query = $((1000000 + i))"; done) &
$C "$pd" loop 40 tidb_mem_quota_query 2000 >/dev/null
wait
m "admin check table mysql.global_variables"
[ "$(m "select count(*) from mysql.global_variables where variable_name = 'tidb_mem_quota_query'")" = 1 ] || fail "duplicate rows"
echo OK
