# ADR 0004: Prototype SQL layer (planner, executors, sessions)

- Status: proposed (2026-10-26)
- Context: ADR 0001 (scope, correctness requirements), ADR 0003 (schema loading). Code:
  `crates/sql` (IR, name resolution, ranges, plans, result metadata), `crates/executor` (root
  executors), `crates/session` (`SessionFactory`/`SessionHandler`), `crates/server` (`--pd`).
- Milestone: `sysbench oltp_read_only --skip_trx=on --db-ps-mode=auto` runs against tidb-rs
  with results identical to Go TiDB.

## Decision

A deliberately small SQL layer: single-table, read-only SELECTs (autocommit or in read-only
transactions) on tables with an integer row handle, planned into one fixed pipeline. Everything outside that subset is
rejected with `ER_NOT_SUPPORTED_YET` (1235) before any data is read. Unsupported never means
"probably right".

### Pipeline

1. **Lowering** (`sql::ir`): the parser AST (which holds `Rc`s and spans) is copied into an
   owned, `Send` IR. Constructs outside the subset fail here with 1235. `?` are numbered in
   text order. Prepared statements cache the IR and are re-planned on every execute.
2. **Statement read** (`session`): each SELECT with FROM gets a fresh PD TSO `start_ts`, the
   schema visible at it (`SchemaCache::info_schema_at`, ADR 0003) and a KV snapshot at it, with
   `tidb_retry_options`. SELECTs without FROM (`@@version_comment`, `DATABASE()`, literals,
   `?`) are evaluated in the session.
3. **Planning** (`sql::plan`): name resolution against the `InfoSchema` with the current
   database, then a fixed pipeline

   ```text
   source (TableDual | PointGet | TableReader[cop: TableRangeScan -> partial HashAgg | Limit])
     -> [root HashAgg (final over partials, or complete over a point-get row)]
     -> [Sort] -> [Limit] -> projection
   ```
4. **Execution** (`executor`): read the source (KV point get, or one coprocessor request over
   the handle range with `keep_order` when rows must come back in handle order), then run the
   root operators over materialised rows, and convert datums to protocol values by result type.

### Plan shapes of the sysbench statements

| statement | plan |
|---|---|
| `SELECT c FROM sbtest1 WHERE id=?` | PointGet (KV get of `t{id}_r{h}`; no coprocessor) |
| `... WHERE id BETWEEN ? AND ?` | TableReader(TableRangeScan keep_order) |
| `SELECT SUM(k) ... BETWEEN` | HashAgg(final, sum) <- TableReader(partial HashAgg sum(k) <- scan) |
| `... BETWEEN ... ORDER BY c` | Sort(c) <- TableReader(scan) |
| `SELECT DISTINCT c ... ORDER BY c` | Sort <- HashAgg(final, firstrow, group by c) <- TableReader(partial HashAgg firstrow(c) group by c) |

`crates/sql/tests/plan.rs` pins these shapes as EXPLAIN-like strings.

### Supported SQL (precisely)

- `SELECT [DISTINCT] fields FROM [db.]table [[AS] alias] [WHERE ...] [GROUP BY cols]
  [ORDER BY items] [LIMIT [offset,] count | LIMIT count OFFSET offset]`; index hints are
  accepted and ignored.
- Fields: `*`, `t.*`, `db.t.*`, columns (optionally qualified), with aliases; `COUNT(*)`,
  `COUNT(const)`, `COUNT/SUM/MIN/MAX(column)`. SUM only over numeric columns.
- WHERE: a conjunction (`AND`) of `handle-col OP const` / `const OP handle-col` with `=, <, <=,
  >, >=` and `handle-col BETWEEN const AND const`, where const is a literal, `?` or unary
  minus of those (numbers, numeric strings, NULL). The handle column is the signed integer
  primary key of a `pk_is_handle` table. Conditions become one inclusive `i64` handle range,
  exactly as Go's ranger: `id < 5.5` is `id <= 5`, `id = 5.5` is empty, `BETWEEN 10 AND 5` is
  empty, values beyond `i64` saturate, NULL matches nothing. A single handle is a PointGet; an
  empty range is TableDual. Tables without an integer PK handle are readable through
  `_tidb_rowid` (full scans only).
- GROUP BY plain columns or positions (no ASC/DESC); DISTINCT over plain columns becomes a
  group-by. ORDER BY columns, select aliases and positions, ASC/DESC. ORDER BY the handle
  alone is satisfied by the scan order (desc scans for DESC). LIMIT is pushed to TiKV when no
  root sort or aggregation follows.
- Readable column types: integer types, YEAR, DECIMAL, FLOAT/DOUBLE, CHAR/VARCHAR/TEXT/BLOB,
  DATE, DATETIME, TIME.
- Session statements: `USE db` (1049 for unknown databases), `SET NAMES`, `SET CHARACTER SET`,
  `SET [SESSION] autocommit`, `SET character_set_*`/`collation_connection`, `SET
  sql_mode` (unless it contains PAD_CHAR_TO_FULL_LENGTH), a few timeouts/isolation variables
  that cannot affect results; `SHOW WARNINGS`/`SHOW ERRORS`. Since #489 the
  value of each of these variables is first checked by the ported `SysVar.Validate` (D05.1),
  so a value Go rejects gets Go's error (`SET autocommit = 'abc'`: 1231 with the value;
  `SET sql_mode = 'NOSUCHMODE'`: 1105). Validations that read another variable read the
  cluster's global value from the loader's sysvar cache (#539; the domain block (D06.5), spec
  §8): with `GLOBAL tidb_skip_isolation_level_check = ON` the prototype accepts
  `SET transaction_isolation = 'SERIALIZABLE'` as Go does. Until the loader has built the
  cache, the compiled defaults are used (logged once). As
  in Go's `SetExecutor.Next`, the assignments of a `SET` run in order and the first error
  ends the statement; the ones before it stay.
- Connect-time SETs and reads of the common drivers (#505: Connector/J 5.1 and 8.x,
  Connector/Python, Go-MySQL-Driver, the `mysql` client; workloads
  `tools/ab/workloads/connect/`). The prototype keeps its session values in a ported
  `SessionVars` (the session variable block (D05.1)) over the cluster's globals: an accepted
  `SET` is written there with Go's `SetSessionFromHook` and synced to the new session, and
  `@@name` of the variables clients read (`constant::READ_FROM_SESSION`: `@@init_connect`,
  `@@sql_select_limit`, `@@system_time_zone`, ...) is read from it as Go's
  `rewriteSystemVariable` reads it, typed by `GetNativeValType`. Also accepted:
  `sql_select_limit = DEFAULT` (any other limit is 1235: the prototype applies none),
  `group_concat_max_len`, `sql_safe_updates`, `foreign_key_checks`, `unique_checks`,
  `innodb_lock_wait_timeout`. As Go's `setSysVariable`: an unknown variable is 1193, a removed
  one is ignored (both checked before the `SET GLOBAL` fence, as Go checks the name first),
  a noop variable warns when `tidb_enable_noop_variables` is OFF; validation warnings go to
  the statement's warnings. `SET x = DEFAULT` is the global value (Go's `getVarValue`); made
  before the cluster's globals are loaded, it is read again from them when they are. Decimal and float values are their
  `Datum.ToString` (`SET max_execution_time = 1.5` is Go's 1232). `SET NAMES` / `SET
  CHARACTER SET` follow Go's `setCharset`: `COLLATE nosuch` is 1273, `SET NAMES utf8mb4
  COLLATE latin1_bin` 1253. The parser's charset errors are Go's TiDB errors, which Go's
  `util.SyntaxError` passes through, in every statement: an unknown charset name (`SET NAMES
  nosuch`, `CONVERT(... USING nosuch)`, `CAST(... CHARSET nosuch)`, `CREATE/ALTER ...
  CHARSET nosuch`) is 1115, an unsupported introducer (`_ujis '3F'`) and `UNICODE` (ucs2) are
  1115, an unknown collation name where the grammar checks it is 1273; names are cut at 64
  characters as Go's messages do. `@@system_time_zone` is the
  cluster's `mysql.tidb` `system_tz`, loaded at start as Go's `BootstrapSession` and
  `setSystemTimeZoneVariable` do (a missing row fails the start, as in Go).
- Read-only transactions (#547; the transaction context block (D05.4) spec §6): `BEGIN`,
  `START TRANSACTION [READ ONLY | WITH CONSISTENT SNAPSHOT]`, `BEGIN PESSIMISTIC/OPTIMISTIC`,
  `COMMIT`, `ROLLBACK` and `SET autocommit = 0`. The statements of a transaction read one
  snapshot with Go v8.5.8's start-ts rules: `BEGIN` takes the start ts at once, `autocommit =
  0` at the first statement that reads a table; pessimistic read-committed transactions take a
  new ts per statement, every other kind reads the start ts. `SET autocommit = 1` from off
  commits the transaction; `@@autocommit` and the `SERVER_STATUS_IN_TRANS` /
  `SERVER_STATUS_AUTOCOMMIT` flags follow the session. A write (`INSERT`, `REPLACE`, `UPDATE`,
  `DELETE`, `LOAD DATA`) or `SELECT ... FOR UPDATE` in a transaction or with `autocommit = 0`
  is Go's 1792 `Cannot execute statement in a READ ONLY transaction.` (the read-only-node
  decision N11 D8, #146). The VS1 router keeps these statements on the prototype. Known
  differences (no metadata lock, no `minstartts`) are listed in that spec's §6.5.
- Errors that match Go: 1146 unknown table, 1054 unknown column (with Go's clause names), 1046
  no database selected, 1051 unknown table in `t.*`, 1064 syntax errors, 1193 unknown system
  variable (8136 for a removed one), 1238 `@@global.` / `@@session.` of a variable without
  that scope (#489), 1210 bad LIMIT arguments. A system variable Go knows but the prototype
  does not answer is 1235.

### Rejected with 1235

Any predicate on a non-handle column; OR, IN, NOT BETWEEN, LIKE, IS NULL, `!=`; expressions or
functions in the select list (beyond the aggregates above); AVG and other aggregates, DISTINCT
inside aggregates, HAVING, WITH ROLLUP, window functions; joins, subqueries, derived tables,
CTEs, set operations; FOR UPDATE/SHARE, SELECT INTO, AS OF TIMESTAMP, partitions selection,
TABLESAMPLE, optimizer hints; GROUP BY expressions or DESC, DISTINCT together with GROUP BY,
non-aggregated columns outside GROUP BY; non-numeric strings compared with the handle; ranges
on an UNSIGNED primary key; TIMESTAMP/ENUM/SET/BIT/JSON columns, virtual generated columns,
clustered non-integer primary keys, partitioned tables, views; `BEGIN ... AS OF TIMESTAMP`
(stale-read transactions), user variables, SET GLOBAL, any other SET; all other statements
(DML, DDL, other SHOW, ...; DML in a transaction is 1792, above).

Known differences from Go, decided by the maintainer on #505, answered with 1235 and counted
as `unsupported` by `tools/ab` (`prototype-not-supported.txt`):

- A `SET` value that is not a literal, a `DEFAULT` or an identifier (`SET sql_mode =
  CONCAT(@@sql_mode, ',STRICT_TRANS_TABLES')`): Go evaluates the expression the planner
  rewrote (pkg/executor/set.go:304 `getVarValue`), which needs `rewriteSystemVariable` of the
  expression rewriter, part 2 (blocked-by the expression rewriter block (D09.2)). Answer:
  `this SET value`.
- User variables (`SET @x = 1`): Go stores the value with its expression's type
  (pkg/executor/set.go:90, the user-variable branch of `SetExecutor.Next`), which needs the
  ported `getvar` / `setvar` of the miscellaneous functions block (D08.9); no hand-written
  type logic in the meantime. Answer: `user variables`.

## How results are kept equal to Go TiDB

- Same snapshot semantics: one TSO per statement, schema and data read at it (ADR 0003).
- Same values: rows are decoded by TiKV's own codecs (vendored `tidb_query_datatype`), partial
  aggregation runs in TiKV exactly as for Go TiDB, and the root final aggregation follows Go's
  `aggfuncs` (SUM over decimals rounded to the result scale; COUNT merges partial counts; an
  aggregation without GROUP BY over no rows returns one row with COUNT 0 and NULLs). Sorting,
  MIN/MAX and grouping of strings use the column's collation through the vendored collators,
  with new collations if `mysql.tidb.new_collation_enabled` says so (read at startup). CHAR
  values are returned as stored: TiDB strips trailing spaces of non-binary CHAR on write
  (`table.CastValue`) and does not pad them on read without PAD_CHAR_TO_FULL_LENGTH, which is
  rejected.
- Same metadata: result columns follow Go's `ConvertColumnInfo` (length = flen × charset
  maxlen for strings, decimal sign/point, `VARCHAR` sent as `VAR_STRING`,
  `character_set_results` override). Result types of aggregates follow `typeInfer4*` (SUM(INT)
  is DECIMAL(32,0) -> length 33). Names follow Go's name resolution, including the PointGet
  fast path's table name as written, and prepare-time metadata from the normal path.
- Checked, not assumed: `tools/ab` compares rows (with wire types) and full column definitions
  for text and prepared protocols against Go TiDB v8.5.8 on the same cluster:
  `sysbench -n 1000` (10,372 comparisons, 0 mismatches; `id='abc'` is rejected),
  `workloads/sql-layer.sql` and `workloads/constants.sql` (0 mismatches).

## Not guaranteed yet

- Ties under ORDER BY on non-unique keys may come back in a different order than Go (both are
  valid; Go does not define it either).
- Warnings: TiKV warnings are dropped (Go reports e.g. 2 truncation warnings for
  `id='abc'`, which is rejected here anyway). Besides the warnings of `SET` (validation,
  noop variables) and of reading a noop variable (#505), the only warning the prototype
  reports is the plan cache's `skip prepared plan-cache: <reason>` on PREPARE and EXECUTE of an uncacheable
  prepared statement (#512; plan cache spec `specs/D13.1-plancache/spec.md` §4.4), with the
  reasons Go decides at PREPARE; reasons Go only finds while optimizing are missing, and with
  `--new-stack=off` the table checks are left out.
- Float/double text formatting relies on the protocol layer's port of Go's formatting; not
  covered by the A/B workloads.
- Errors from TiKV/PD map to 1105 with the client's message, not Go's exact codes; lock waits
  use tikv-client's resolver with TiDB-like budgets, not client-go's exact behaviour.
- Constant SELECT metadata is matched for the forms the CLI and connectors send (and
  `workloads/constants.sql`), not for arbitrary expressions (which are rejected anyway).
  Known difference: a statement such as `SELECT DATABASE()` prepared while no database was
  selected and executed after `USE` returns no NOT_NULL flag on Go (likely flags fixed at
  prepare time; not confirmed in the Go source), while tidb-rs computes them at execute time
  (NOT_NULL). Seen with `ab file constants.sql` without
  `-D`; with `-D sbtest` there is no mismatch.
- Time zone: DAG requests use UTC; correct for the supported types (TIMESTAMP is rejected).

## Performance notes

Each statement costs a TSO, the schema-version point get of ADR 0003 and the read itself, and
everything is materialised before encoding. sysbench oltp_read_only (8 threads, 100k rows) runs
at about 0.6x Go TiDB's QPS on the same laptop playground; see the session report for numbers.
