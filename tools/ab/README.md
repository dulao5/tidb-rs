# tools/ab: A/B verification against Go TiDB

`ab` runs the same statements on Go TiDB (A, the reference) and tidb-rs (B) on the same TiKV
cluster and compares what comes back. The scripts in `scripts/` set up the cluster and data and
run sysbench against both.

## End to end

```sh
source <toolchains>/tidb-rs-env.sh

# 1. Cluster: tiup playground v8.5.8, tag maya-ab, port offset 23000
#    -> PD 127.0.0.1:25379, Go TiDB 127.0.0.1:27000 (auto-TLS on)
scripts/playground.sh start            # status | stop | clean; -t tag -o offset -v version

# 2. Data, loaded through Go TiDB: sbtest1, 100k rows (ids 1..100000), split into 8 regions
scripts/sysbench-prepare.sh -t 1 -s 100000 -r 8

# 3. tidb-rs next to it (auto-TLS is on by default)
cargo run --release -p tidb-server -- -P 27001 --pd 127.0.0.1:25379    # --pd: once the SQL layer lands

# 4. Result comparison (exit 1 on any mismatch)
cargo run --release -p tidb-ab -- -a 127.0.0.1:27000 -b 127.0.0.1:27001 sysbench -n 200
cargo run --release -p tidb-ab -- -a 127.0.0.1:27000 -b 127.0.0.1:27001 file tools/ab/workloads/constants.sql

# 5. Performance: oltp_read_only (--skip_trx=on --db-ps-mode=auto) on A then B
scripts/sysbench-run.sh compare -a 127.0.0.1:27000 -b 127.0.0.1:27001 -T "1 8 16" -d 30

# 6. Tear down
scripts/playground.sh clean
```

Self-check of the harness: point both sides at Go TiDB (`-b 127.0.0.1:27000`); everything must
match.

## What is compared

For each statement, via the text protocol (COM_QUERY) and, when the workload marks it
parameterizable, as a server-side prepared statement (COM_STMT_PREPARE/EXECUTE with bound
parameters):

- rows: values exactly, including their wire type (`"1"` from the text protocol, `1` / `1u` /
  `1.5f64` / `DATE(...)` from the binary one). In order when the statement has a top-level
  `ORDER BY`, otherwise as multisets. Note: `ORDER BY` on a non-unique key can legitimately order
  ties differently; such diffs say "same rows in a different order".
- column definitions: name, org_name, schema, table, org_table, type, length, decimals, flags,
  charset/collation; for prepared statements also the parameter count and the columns returned
  by COM_STMT_PREPARE.
- errors: both fail with the same code => `both_error` (counts as agreement); B fails with 1235
  (`ER_NOT_SUPPORTED_YET`) => `unsupported`; anything else (one side errors, different codes,
  connection errors, timeouts) => `mismatch`. Error messages and warning counts are not compared.

Output: a table of match / mismatch / unsupported / both_error per template and protocol, and
`<out-dir>/mismatches.jsonl` (query, params, both results with metadata and up to
`--report-rows` rows, and the diff) plus `<out-dir>/unsupported.jsonl`. `-v` prints mismatches
as they happen. Exit status: 0 = no mismatch, 1 = mismatches, 2 = setup error (e.g. cannot
connect).

Common options (before or after the workload): `-a/-b host:port`, `-u/-p`, `-D db`,
`--tls prefer|on|off` (self-signed certificates accepted), `--seed N`, `--protocol both|text|ps`,
`--out-dir` (default `target/ab`), `--timeout` seconds.

## Workloads

`sysbench [--tables N] [--table-size N] [--range-size 100] [-n 100] [--no-edge-cases]`: the five
oltp_read_only statements (`SELECT c ... WHERE id=?`, `... BETWEEN ? AND ?`, `SELECT SUM(k)`,
`... ORDER BY c`, `SELECT DISTINCT c ... ORDER BY c`) with `-n` seeded random parameters each,
plus edge cases: first/last/middle ids, 0, -1, beyond the max id, i64/u64 bounds, string
parameters, single-row and empty (`a > b`) ranges, ranges crossing the max id, negative and
whole-table ranges, and ranges straddling, ending at and starting on every region boundary
(read from A with `SHOW TABLE sbtestN REGIONS`). The table size defaults to `MAX(id)` of
sbtest1 on A.

`file PATH [--setup FILE] [--no-prepared]`: statements separated by `;` (outside quotes and
comments), or one per line if the file has no `;`. `#`/`--` comment lines between statements
are skipped; mysqltest's `--sorted_result` makes the next statement unordered, other mysqltest
directives are ignored. Only read-only statements are sent to B: SELECT/TABLE/VALUES/WITH
(without FOR UPDATE/SHARE, INTO OUTFILE/DUMPFILE, or data-changing WITH), SHOW, session SET and
USE. Everything else is counted as skipped and never sent anywhere. Writes go to A only in a
setup section: lines after `-- ab:setup` (until `-- ab:test`) or the `--setup` file; there SET
and USE also run on B so both sessions stay in sync. SELECT-like statements also run as
prepared statements without parameters unless `--no-prepared`. Examples:
`workloads/constants.sql` (no data needed), `workloads/sample.sql` (setup section).

## Scripts

- `scripts/playground.sh start|stop|status|clean [-t tag] [-o offset] [-v version]`: defaults
  maya-ab / 23000 / v8.5.8. Starts `tiup playground` in the background (no TiFlash, no
  monitor) with Go TiDB `security.auto-tls`, waits until TiDB answers, prints the addresses.
  `stop` interrupts the playground and then kills whatever is left of that tag only (processes
  whose arguments point into `~/.tiup/data/<tag>/`). `clean` also runs `tiup clean <tag>`.
- `scripts/sysbench-prepare.sh [-H host] [-P port] [-d db] [-t tables] [-s size] [-r regions]`:
  recreates the database, loads with `--auto_inc=off` (ids 1..size), optionally
  `SPLIT TABLE ... REGIONS n`, then ANALYZE. It turns off `tidb_ddl_enable_fast_reorg` and
  `tidb_enable_dist_task` globally: the ingest index backfill refuses to run on a nearly full
  disk.
- `scripts/sysbench-run.sh run|compare ...`: see the header of the script. Raw output goes to
  `target/sysbench/`. p95 comes from `scripts/oltp_read_only_lat.lua` (the stock workload plus
  a per-event latency histogram) because Homebrew's sysbench 1.0.20 on Apple Silicon prints all
  percentiles as 0.

Homebrew's sysbench links MariaDB Connector/C, which wants TLS; both Go TiDB (auto-tls, set by
playground.sh) and tidb-server (auto-tls by default) provide it. Do not pass `--mysql-ssl=on`.

Baseline numbers: [BASELINE.md](BASELINE.md).
