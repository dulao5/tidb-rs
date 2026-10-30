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

## Integration tests

`integrationtest --dir <tidb>/tests/integrationtest/t [tests...]` runs TiDB's integration tests
(the mysql-tester `.test` files) in **hybrid mode**: tidb-rs only executes autocommit reads, so
every statement runs on Go TiDB (A), exactly as mysql-tester would run it, and the reads also run
on tidb-rs (B) and are compared with A's answer, live on the same cluster and data. A is the
reference, not the `.result` files (those are recorded on TiDB master by a mock-store server).
Tests are selected by name (`executor/aggregate`) or prefix (`executor/`); default all, in
mysql-tester's order (sorted by name), one file after the other.

`scripts/integrationtest.sh [-t tag] [-o offset] [-d tests-dir] [-O out-dir] [-k] [-- tests...]`
does the whole run: build, `playground.sh start -c tools/ab/integrationtest.toml` (the
integration tests' TiDB settings that apply to a TiKV cluster), turn off the ingest backfill,
join tidb-rs (`playground-join.sh`), run `ab integrationtest`, write `run.tsv` (commits,
version, date, runner, duration) and tear everything down (`-k` keeps it). Like TiDB's
`run-tests.sh` it sets `TZ=Asia/Shanghai`. A full run of the 263 files takes about an hour on a
laptop; it needs tiup, mysql, git and curl and should work unchanged on Linux CI.

Per file, as mysql-tester does: create database `<test name with / -> __>` and connect to it
(A and B), run the file, drop the database. Statements are read like mysql-tester reads them
(`#` comments, `--directive` and `-- directive` lines, SQL up to a line ending in `;`).

What happens to a statement:

| class | statements | A | B |
|---|---|---|---|
| read | SELECT/TABLE/VALUES/WITH without locking or INTO OUTFILE/DUMPFILE/@var and without a data-changing WITH; SHOW; EXPLAIN/DESC/DESCRIBE (EXPLAIN ANALYZE only of a read); EXECUTE of a prepared read | runs | runs, compared |
| session | session SET, USE, PREPARE, DEALLOCATE, SELECT ... INTO @var | runs | mirrored (not compared); a rejection is counted in `b_set_rejected` |
| write | everything else: DML, DDL, transactions, SET GLOBAL, ADMIN, locking reads, ... | runs | - |

Outcome per statement:

- `match`: B answered like A: same rows (as multisets, in order when the statement has a
  top-level ORDER BY and no `--sorted_result`), same column metadata, or the same error code
  (also when the test expects the error). With `--enable_warnings`, `SHOW WARNINGS` must match
  too. Under `--disable_result_log` only success vs error is compared. `--replace_regex` and
  `--replace_column` are applied to both sides' values first.
- `mismatch`: B answered differently. Kinds: `rows` (wrong values or row count), `order`,
  `columns` (names or count), `metadata` (type, length, flags, charset), `error` (different
  error code, or B returned a result where A failed), `shape` (result set vs OK), `warnings`.
- `unsupported`: B answered 1235 (`ER_NOT_SUPPORTED_YET`), or B could not log in as the user of
  a `connect` (tidb-rs has a single account).
- `b_error`: A succeeded and B failed otherwise (other error code, timeout, lost connection).
- `a_error`: A failed and the test does not expect an error (`--error`). Any class; not scored.
  Mostly features of TiDB master missing in the release under test.
- `skipped`: a read not sent to B: inside a transaction on A (BEGIN/START TRANSACTION until
  COMMIT/ROLLBACK/DDL, or `autocommit=0`), naming a local temporary table of A's session, or
  depending on session history B does not share: SHOW WARNINGS/ERRORS, `@@warning_count`,
  `ROW_COUNT()`, `FOUND_ROWS()`, `@@last_plan_from_cache`, ... right after a statement that ran
  on A only, and `LAST_INSERT_ID()` / `@@tidb_last_txn_info` / `@@tidb_last_query_info` always.
- `write`: a session or write statement that A ran as the test expected.

One statement is never run: `SET GLOBAL tidb_enable_ddl = OFF` (`ddl/db_integration`). The test
expects error 8246 because the test server is the only TiDB, but tidb-rs registers as a TiDB
server (ADR 0005), so Go TiDB accepts it and the cluster loses its DDL owner. It is counted as
`skipped`.

After a B error 1146/1049/1054/1051/1046 on a read that A answered, B is asked again after 1.5 s;
if the answer changes, the statement's kind gets `retried` (schema-sync lag; ADR 0005 says this
should not happen because the DDL owner waits for tidb-rs).

`connect (name, host, user, password, db)` opens a session pair as that user, `connection`
switches, `disconnect` closes (and falls back to `default`). `--echo`, `--enable_info`,
`--remove_file` and `--sleep` do not affect what is compared and are ignored. The session time
zone is the process's (`TZ`), not set by SQL.

Output in `--out-dir`:

- `statements.tsv`: one row per statement: file, line, connection, class, outcome, kind (mismatch
  kind, B error code, skip reason, `session`), detail (B's message, the diff, A's error), elapsed
  ms, SQL. Fields are escaped (`\t`, `\n`, `\\`).
- `files.tsv`: per file: statements, reads, match, mismatch, unsupported, b_error, a_error,
  skipped, write, b_set_rejected, retried, seconds.
- `mismatches.jsonl`: every mismatch and b_error with both answers (columns and up to
  `--report-rows` rows) and the session statements B had rejected in that connection.

The tidb-rs-tracker imports the directory (`scripts/tracker.py import-integration`) and
publishes per-file counts, reasons and every mismatch (R1).

## Scripts

- `scripts/playground.sh start|stop|status|clean [-t tag] [-o offset] [-v version] [-c toml]`: defaults
  maya-ab / 23000 / v8.5.8. Starts `tiup playground` in the background (no TiFlash, no
  monitor) with Go TiDB `security.auto-tls`, waits until TiDB answers, prints the addresses.
  `stop` interrupts the playground and then kills whatever is left of that tag only (processes
  whose arguments point into `~/.tiup/data/<tag>/`). `clean` also runs `tiup clean <tag>`.
  `-c` appends a TOML file to Go TiDB's config.
- `scripts/integrationtest.sh`: the integration-test run end to end (see above).
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
