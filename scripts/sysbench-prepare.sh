#!/usr/bin/env bash
# Create and load sysbench tables through Go TiDB (writes always go through Go TiDB; tidb-rs is
# read-only).
#
#   scripts/sysbench-prepare.sh [-H host] [-P port] [-d db] [-t tables] [-s table-size]
#                               [-r regions] [-j threads]
#
# Defaults: 127.0.0.1:27000 (playground offset 23000), db sbtest, 1 table, 100000 rows, no split,
# 8 loader threads. The database is dropped and recreated.
#
# -r N > 1 runs `SPLIT TABLE sbtestX BETWEEN (1) AND (size+1) REGIONS N` after loading, so reads
# span several regions (the A/B workload's ranges then cross region boundaries). ANALYZE TABLE
# is run at the end (TiDB auto-analyzes eventually anyway; doing it now keeps plans stable).
set -euo pipefail

host=127.0.0.1
port=27000
db=sbtest
tables=1
size=100000
regions=0
threads=8
user=${MYSQL_USER:-root}
while getopts "H:P:d:t:s:r:j:" opt; do
    case $opt in
        H) host=$OPTARG ;;
        P) port=$OPTARG ;;
        d) db=$OPTARG ;;
        t) tables=$OPTARG ;;
        s) size=$OPTARG ;;
        r) regions=$OPTARG ;;
        j) threads=$OPTARG ;;
        *) sed -n '2,13p' "$0" >&2; exit 2 ;;
    esac
done

sql() { mysql -h "$host" -P "$port" -u "$user" -N "$@"; }

echo "== recreating database $db on $host:$port"
sql -e "DROP DATABASE IF EXISTS \`$db\`; CREATE DATABASE \`$db\`"
# The ingest (fast reorg) index backfill refuses to run when the disk holding its temp dir is
# nearly full, which kills sysbench's CREATE INDEX k_1. The txn backfill is fine at this size.
# (Global setting of the target cluster; meant for throwaway playgrounds.)
sql -e "SET GLOBAL tidb_enable_dist_task = OFF; SET GLOBAL tidb_ddl_enable_fast_reorg = OFF"

echo "== sysbench prepare: $tables table(s) x $size rows"
# Homebrew sysbench links MariaDB Connector/C, which negotiates TLS on its own (Go TiDB's auto-tls
# provides it). Do not pass --mysql-ssl=on: that makes sysbench load client cert files.
sysbench oltp_read_only \
    --mysql-host="$host" --mysql-port="$port" --mysql-user="$user" --mysql-db="$db" \
    --tables="$tables" --table-size="$size" --threads="$threads" --auto_inc=off prepare
# sysbench exits 0 even when a loader thread fails; check every table is complete (ids 1..size,
# thanks to --auto_inc=off) and has its secondary index.
for i in $(seq 1 "$tables"); do
    got=$(sql -e "SELECT CONCAT(COUNT(*), ':', MAX(id)), (SELECT COUNT(*) FROM information_schema.statistics
        WHERE table_schema='$db' AND table_name='sbtest$i' AND index_name='k_1') FROM \`$db\`.sbtest$i" 2>/dev/null || echo missing)
    if [ "$got" != "$size:$size	1" ]; then
        echo "sbtest$i incomplete (rows:max_id k_1_index = $got), expected $size:$size 1" >&2
        exit 1
    fi
done

for i in $(seq 1 "$tables"); do
    t=sbtest$i
    if [ "$regions" -gt 1 ]; then
        echo "== splitting $t into $regions regions"
        sql -e "SPLIT TABLE \`$db\`.$t BETWEEN (1) AND ($((size + 1))) REGIONS $regions"
    fi
    echo "== analyzing $t"
    sql -e "ANALYZE TABLE \`$db\`.$t"
    sql -e "SELECT '$t', COUNT(*), MIN(id), MAX(id) FROM \`$db\`.$t" | awk '{printf "   %s rows=%s ids=[%s, %s]\n", $1, $2, $3, $4}'
    sql -e "SELECT COUNT(*) FROM information_schema.tikv_region_status WHERE db_name='$db' AND table_name='$t' AND is_index=0" |
        awk '{printf "   record regions: %s\n", $1}'
done
