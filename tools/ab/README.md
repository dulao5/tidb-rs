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
cargo run --release -p poc-server -- -P 27001 --pd 127.0.0.1:25379    # --pd: once the SQL layer lands

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

Every judgement goes through the one result comparator, `src/compare.rs`, which implements
`docs/testing/result-comparison.md` (statement classes, error / warning rules, the numbered
tolerances of `docs/testing/tolerances.toml`; fixtures in `fixtures/compare/`). In short:

For each statement, via the text protocol (COM_QUERY) and, when the workload marks it
parameterizable, as a server-side prepared statement (COM_STMT_PREPARE/EXECUTE with bound
parameters):

- rows: values exactly, including their wire type (`"1"` from the text protocol, `1` / `1u` /
  `1.5f64` / `DATE(...)` from the binary one), by the statement's class: `exact` (EXPLAIN,
  SHOW, ..., or an ORDER BY that does not resolve to result columns) as lists; `unordered` (no
  top-level ORDER BY) as multisets; `ordered K` (ORDER BY resolved to result columns K) runs of
  equal keys as multisets, the runs in order; `limited` (top-level LIMIT) also accepts other
  members of A's result without the LIMIT, which is then fetched from A.
- column definitions: name, org_name, schema, table, org_table, type, length, decimals, flags,
  charset/collation; for prepared statements also the parameter count and the columns returned
  by COM_STMT_PREPARE, and that B's EXECUTE returns the columns its PREPARE returned (a
  difference A does not have, or a field A changed to another value, is a `metadata` mismatch: with `--new-stack=fallback` an
  EXECUTE that falls back to the PoC must carry the new stack's PREPARE metadata,
  vertical-slice plan §4.7).
- errors: both fail with the same code, SQLSTATE and message => `both_error` (counts as
  agreement); B fails with a not-implemented 1235 (a boundary stub's `blocked-by:` answer or one
  of the prototype's fixed answers in `prototype-not-supported.txt`) while A did not answer 1235
  => `unsupported` (tolerance T1); any other 1235 is a normal error; anything else (one side errors, another code / SQLSTATE / message, connection errors,
  timeouts) => `mismatch`.
- OK packets: affected rows and warning count. Warnings: when either side reports any, `SHOW
  WARNINGS` runs on both and must be identical.

A mismatch that an approved ruling of the divergence ledger covers (`docs/testing/rulings.toml`,
spec `docs/testing/rulings.md`) is `ruled` instead: never a match, still counted, listed in
mismatches.jsonl with its diff and the ruling id. A ruling still `pending-owner` changes nothing
(the diff names it). Each run writes `<out-dir>/rulings-hits.tsv`, the hit count of every ruling
the comparator can match (0 included). The ledger is built into `ab`; `--rulings FILE` reads
another one.

Output: a table of match / mismatch / ruled / unsupported / both_error per template and protocol, and
`<out-dir>/mismatches.jsonl` (query, params, both results with metadata and up to
`--report-rows` rows, and the diff) plus `<out-dir>/unsupported.jsonl`. `-v` prints mismatches
as they happen. Exit status: 0 = no mismatch, 1 = mismatches, 2 = setup error (e.g. cannot
connect), 3 = the coverage census or its negative control failed (see below).

Common options (before or after the workload): `-a/-b host:port`, `-u/-p`, `-D db`,
`--tls prefer|on|off` (self-signed certificates accepted), `--seed N`, `--protocol both|text|ps`,
`--out-dir` (default `target/ab`), `--timeout` seconds.

`--plan-cache`: after every prepared `EXECUTE`, read `@@last_plan_from_cache` on both sides
(right after it: the next statement resets it) and print a per-template table of how often the
two agree (the instance plan cache, `specs/exceptions/A3-plan-cache/spec.md`). Run Go with
`SET GLOBAL tidb_enable_instance_plan_cache = ON` (tidb-rs's default). Because of the extra
read, `SHOW WARNINGS` then shows that read's warnings: compare warnings in a run without it.

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

Prepared statements with parameters: each `-- ab:execute PARAMS` comment line right before a
SELECT-like statement (test section) adds one EXECUTE of it with that parameter list, one value
per `?`, comma-separated, each bound with its own binary type: `42` / `-7` signed 64-bit
(`MYSQL_TYPE_LONGLONG`), `18446744073709551615u` unsigned 64-bit (`MYSQL_TYPE_LONGLONG` with the
unsigned flag), `'abc'` a string (`MYSQL_TYPE_VAR_STRING`; `''` is a quote, a backslash takes
the next character as it is); an empty list is an EXECUTE without parameters. The statement is
compared once per line, via the text protocol with the values as literals and (unless
`--no-prepared`) as EXECUTEs of one PREPARE on each side: the first line's EXECUTE prepares the
statement afresh (an earlier statement with the same text on that connection is closed first),
the later lines execute that same statement again, so the plan cache can hit and the statement
keeps whatever its first EXECUTE left behind. Records in mismatches.jsonl / unsupported.jsonl
carry the EXECUTE's number as `execute`. A wrong count of values, an unparsable value, or an
`ab:execute` line with no statement after it is a setup error (exit 2). `-- ab:template NAME`
puts the statements after it into the summary group (and the rulings' unit) `NAME` instead of
their first keyword; a bare `-- ab:template` goes back to the keyword. Outside `ab:execute`,
statements with the same text share one prepared statement per connection (the driver's
statement cache), as in the sysbench workload. Examples:

- `workloads/prepared.sql`: statements with two or more parameters (`BETWEEN ? AND ?`, `>= ?
  AND <= ?`, `k = ? AND id = ?`, `IN (?, ?, ?)`), ranges selecting only the primary key (no
  Projection, so the new stack reaches the table reader), parameters in the select list and in
  `LIMIT`, each executed two or more times with other values and the same types (#353; #352
  read every parameter from position 0).
- `workloads/prepared_type_change.sql`: a later EXECUTE binding another signedness or type
  than the first (unsigned `u64::MAX` after a signed value and the reverse, a string after an
  int and the reverse), with the parameter selected as a column. Go v8.5.8 reuses the first
  EXECUTE's column definitions (#367, owner ruling pending), so these are expected mismatches;
  their template is the file's name, `prepared_type_change`, for a ruling to match.

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
`run-tests.sh` it sets `TZ=Asia/Shanghai`. Before #175 a full run of the 263 files (52k statements) took
3.6 h on a Mac mini (M4 Pro), about 2.5 h of it ~280 ANALYZE statements at 40 s each: Go
TiDB's ANALYZE broadcasts a stats-delta flush to every registered TiDB server through its status
port (`executor.flushStatsDeltaForAnalyze`), as it does to read `CLUSTER_*` tables, and tidb-rs's
status port did not serve that request. Since #175 it does, and the 246 files of v8.5.8 take
about 21 minutes on Linux (`tools/weekly`). It needs
tiup, mysql, git and curl and should work unchanged on Linux CI. On a nearly full disk the
ingest backfill fails (`8256 ... no enough space`) for files that turn
`tidb_ddl_enable_fast_reorg` back on; those statements count as `a_error`.

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

- `match`: the comparator judged B's answer equal to A's ("What is compared" above: rows by
  the statement's class, `--sorted_result` making it unordered, column metadata, warnings, or
  the same error with code, SQLSTATE and message, also when the test expects the error).
  Under `--disable_result_log` only the shape is compared, errors still in full (T4).
  `--replace_regex` and `--replace_column` are applied to both sides' values first (T3).
- `mismatch`: B answered differently. Kinds: `rows` (wrong values or row count), `order`,
  `columns` (names or count), `metadata` (type, length, flags, charset), `error` (different
  error code / SQLSTATE / message, or B returned a result where A failed), `shape` (result set
  vs OK), `warnings`, `affected_rows`.
- `ruled`: a `mismatch` or `b_error` that an approved ruling of `docs/testing/rulings.toml`
  covers; kind `ruled:<id>`. Not a match, not a failure, counted and listed. A pending ruling
  leaves it a mismatch, with kind `<kind> (pending ruling <id>)`.
- `unsupported`: B answered 1235 (`ER_NOT_SUPPORTED_YET`), or B could not log in as the user of
  a `connect` (tidb-rs has a single account).
- `b_error`: A succeeded and B failed otherwise (other error code, timeout, lost connection).
  Also a `USE` that B rejects (kind `use rejected`): the database exists on A, so this is a
  tidb-rs defect; the reads after it are still skipped (`session diverged`).
- `a_error`: A failed and the test does not expect an error (`--error`). Any class; not scored.
  Mostly features of TiDB master missing in the release under test.
- `skipped`: a read not sent to B: inside a transaction on A (BEGIN/START TRANSACTION until
  COMMIT/ROLLBACK/DDL, or `autocommit=0`), naming a local temporary table of A's session, or
  depending on session history B does not share: SHOW WARNINGS/ERRORS, `@@warning_count`,
  `ROW_COUNT()`, `FOUND_ROWS()`, `@@last_plan_from_cache`, ... unless the previous statement
  ran on both and matched (after an error A lists it as a warning, B does not), and `LAST_INSERT_ID()` / `@@tidb_last_txn_info` / `@@tidb_last_query_info` always.
  Also `session diverged`: B rejected a session statement that sets a variable which can change
  a read's answer (`USE`, `SET NAMES`, `character_set_*`, `collation_*`, `sql_mode`,
  `time_zone`, `tidb_mem_quota_query`, `tidb_enable_external_ts_read`, ...; the list is
  `mysqltest::affects_results`), so B's session no longer answers like A's. Reads in that
  connection are skipped until the variable is set again on both sides, or set back to `DEFAULT`
  on A (B never changed it). Rejected settings of other variables (cost model, concurrency,
  chunk sizes, ...) do not stop the comparison. `SET GLOBAL` is a write, not tracked: B
  rejecting one shows up as mismatches in later connections, not as skipped reads.
- `write`: a session or write statement that A ran as the test expected.

One statement is never run: `SET GLOBAL tidb_enable_ddl = OFF` (`ddl/db_integration`). The test
expects error 8246 because the test server is the only TiDB, but tidb-rs registers as a TiDB
server (ADR 0005), so Go TiDB accepts it and the cluster loses its DDL owner. It is counted as
`skipped`. (The first full run also held back reads with a LIMIT of 2^63 or more: tidb-rs then
pushed them to TiKV as Limit 0, which panics TiKV v8.5.8. Fixed in af157fe.)

After a B error 1146/1049/1054/1051/1046 on a read that A answered, B is asked again after 1.5 s;
if the answer changes, the statement's kind gets `retried`. Errors naming system schemas or
`_tidb_rowid` are not retried (schema-sync lag; ADR 0005 says this
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
  skipped, write, b_set_rejected, retried, seconds, ruled.
- `rulings-hits.tsv`: hits per ruling (see above).
- `mismatches.jsonl`: every mismatch, ruled read and b_error with both answers (columns and up to
  `--report-rows` rows) and the session statements B had rejected in that connection.
- `results.jsonl`: every read sent to B (match, mismatch, ruled, unsupported, b_error), one JSON
  object per line in the format of the other workloads (`template` = test name, `protocol`
  `text`, `verdict`, `query`) plus `line`, and for anything but a match the `kind` and
  `detail` of `statements.tsv`. Also what the session divergence hides: every session
  statement B rejected (verdict `b_set_rejected`, `detail` = B's error; a rejected `USE` is a
  `b_error`), and every read skipped as `session diverged` (verdict `skipped`, with `cause`:
  the `line`, `sql` and variable of the rejected statement). The weekly build's
  `integrationtest` part reads it (`tools/weekly`).

The tidb-rs-tracker imports the directory (`scripts/tracker.py import-integration`) and
publishes per-file counts, reasons and every mismatch (R1).

## Coverage census and negative control

Every run also writes a coverage census (`docs/testing/coverage-census.md`, #260): each statement
it was given is `new_stack`, `fallback` (prototype, or 1235 in strict mode, with the router's
reason / `blocked-by` block), `error` (sent to B, no route attributable) or `skipped` (never sent
to B). The route comes from tidb-rs's route trace: start tidb-rs with `--route-trace` and pass
its status address as `--b-status host:port` (without it every statement B ran is `error`).
`--census-layer` names the layer (`integrationtest`, `workload`, `workload-strict`, `tpch`;
default by subcommand); unknown names are refused. A negative control (`SELECT 'negative
control'`, B's answer falsified) runs last and must be judged a mismatch. Output:
`census.tsv`, `census-summary.tsv`, `census-reasons.tsv`. If the four paths do not add up to the
statements given (INCOMPLETE CENSUS) or the negative control passed / did not run, `ab` exits 3.
Only `new_stack` statements the comparator judged equal count as new-stack coverage.

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
