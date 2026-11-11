#!/usr/bin/env bash
# Mixed-cluster check of block D06.2 (meta reads): Go TiDB v8.5.8 runs DDL on a playground,
# tidb-infoschema reads the meta data it wrote from TiKV (crates/poc/domain/examples/meta_cli.rs)
# and the results are compared with Go's SQL view. Start a playground first, e.g.
#   scripts/playground.sh start -t maya-d062 -o 28000
#   scripts/meta-mixed.sh 127.0.0.1:30379 32000
set -euo pipefail
pd=${1:?pd host:port}
port=${2:?Go TiDB port}
cargo build -q -p poc-domain --example meta_cli
C=target/debug/examples/meta_cli
m() { mysql -h 127.0.0.1 -P "$port" -u root -N -r -e "$1"; }
fail() { echo "FAIL: $*"; exit 1; }

echo "setup: DDL of many kinds"
m "set global tidb_enable_dist_task = off; set global tidb_ddl_enable_fast_reorg = off"  # ADD INDEX without the ingest temp dir
m "drop database if exists mm1; drop database if exists \`m m\`; drop database if exists \`q\"x\`; drop database if exists \`表库\`"
m "create database mm1; create database \`m m\`; create database \`q\"x\`; create database \`表库\`"
m "create placement policy if not exists pp1 followers=1"
m "create resource group if not exists rg1 ru_per_sec=1000"
m "use mm1; create table t1 (a int primary key, b varchar(10));
   create table t2 (a int, b int, key(b)) partition by hash(a) partitions 3;
   create table t3 (a int primary key);
   create table t4 (a int, foreign key (a) references t3(a));
   create table t5 (a int, ts datetime) ttl = ts + interval 1 day;
   create table t6 (a int) placement policy = pp1;
   create table \`a\"b\\\\c\` (x int);
   create sequence s1;
   create view v1 as select * from t1;
   create table t7 (a int); truncate table t7; drop table t7;
   create table t8 (a int auto_increment primary key, b int); insert into t8 (b) values (1),(2);
   alter table t1 add column c int; alter table t1 add index ib (b);"
m "use \`m m\`; create table \`t t\` (a int); create global temporary table g1 (a int) on commit delete rows"
m "use \`q\"x\`; create table \`表\` (a int); create table cached1 (a int); alter table cached1 cache"
m "use \`表库\`; create table \`列\` (\`名\` int)"

echo "1. databases: ids and names"
go=$(m "select SCHEMA_NAME from information_schema.schemata where SCHEMA_NAME not in ('INFORMATION_SCHEMA','PERFORMANCE_SCHEMA','METRICS_SCHEMA')" | sort)
rs=$($C "$pd" dbs | cut -f2- | sort)
[ "$go" = "$rs" ] || fail "databases differ: go [$go] rs [$rs]"

echo "2. tables: ids and names per database (ListTables)"
go=$(m "select lower(TABLE_SCHEMA), TIDB_TABLE_ID, TABLE_NAME from information_schema.tables where TABLE_SCHEMA not in ('INFORMATION_SCHEMA','PERFORMANCE_SCHEMA','METRICS_SCHEMA')" | sort)
rs=$($C "$pd" tables | sort)
[ "$go" = "$rs" ] || { diff <(echo "$go") <(echo "$rs") | head; fail "tables differ"; }

echo "3. GetAllNameToIDAndTheMustLoadedTableInfo: names (the \"O\" name, unescaped) and ids"
go=$(m "select lower(TABLE_SCHEMA), TABLE_NAME, TIDB_TABLE_ID from information_schema.tables where TABLE_SCHEMA not in ('INFORMATION_SCHEMA','PERFORMANCE_SCHEMA','METRICS_SCHEMA')" | sort)
rs=$($C "$pd" nametoid | sort)
[ "$go" = "$rs" ] || { diff <(echo "$go") <(echo "$rs") | head; fail "name to id differs"; }

echo "4. last history jobs: ids, schema, table, type"
go=$(m "admin show ddl jobs 10" | cut -f1-3)
rs=$($C "$pd" jobs 10 | cut -f1-3)
[ "$go" = "$rs" ] || { diff <(echo "$go") <(echo "$rs") | head; fail "history jobs differ"; }

echo "5. misc: bootstrap version, MDL, system DB, resource groups, policies"
$C "$pd" misc
bv=$(m "select VARIABLE_VALUE from mysql.tidb where VARIABLE_NAME='tidb_server_version'")
[ "$($C "$pd" misc | awk '/^bootstrap_version/{print $2}')" = "$bv" ] || fail "bootstrap version"
# GetSystemDBID is the id ListDatabases gives the database named mysql (Go has no SQL view of
# database ids): a consistency check of the two readers.
[ "$($C "$pd" misc | awk '/^system_db_id/{print $2}')" = "$($C "$pd" dbs | awk -F'\t' '$2=="mysql"{print $1}')" ] || fail "system db id"
mdl=$(m "select @@global.tidb_enable_metadata_lock")
[ "$($C "$pd" misc | awk '/^mdl/{print $2}')" = "$([ "$mdl" = 1 ] && echo true || echo false)" ] || fail "mdl"
rg=$(m "select count(*) from information_schema.resource_groups")
[ "$($C "$pd" misc | awk '/^resource_groups/{print $2}')" = "$rg" ] || fail "resource groups"
pp=$(m "select count(*) from information_schema.placement_policies")
[ "$($C "$pd" misc | awk '/^policies/{print $2}')" = "$pp" ] || fail "policies"
echo "OK"
