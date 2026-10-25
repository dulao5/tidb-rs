# ADR 0004: Prototype SQL layer (planner, executors, sessions)

- Status: proposed (2026-10-26)
- Context: ADR 0001 (scope, correctness requirements), ADR 0003 (schema loading). Code:
  `crates/sql` (IR, name resolution, ranges, plans, result metadata), `crates/executor` (root
  executors), `crates/session` (`SessionFactory`/`SessionHandler`), `crates/server` (`--pd`).
- Milestone: `sysbench oltp_read_only --skip_trx=on --db-ps-mode=auto` runs against tidb-rs
  with results identical to Go TiDB.

## Decision

A deliberately small SQL layer: single-table, read-only, autocommit SELECTs on tables with an
integer row handle, planned into one fixed pipeline. Everything outside that subset is
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
  `SET [SESSION] autocommit = 1/ON`, `SET character_set_*`/`collation_connection`, `SET
  sql_mode` (unless it contains PAD_CHAR_TO_FULL_LENGTH), a few timeouts/isolation variables
  that cannot affect results; `SHOW WARNINGS`/`SHOW ERRORS` (always empty).
- Errors that match Go: 1146 unknown table, 1054 unknown column (with Go's clause names), 1046
  no database selected, 1051 unknown table in `t.*`, 1064 syntax errors, 1193 unknown system
  variable, 1210 bad LIMIT arguments.

### Rejected with 1235

Any predicate on a non-handle column; OR, IN, NOT BETWEEN, LIKE, IS NULL, `!=`; expressions or
functions in the select list (beyond the aggregates above); AVG and other aggregates, DISTINCT
inside aggregates, HAVING, WITH ROLLUP, window functions; joins, subqueries, derived tables,
CTEs, set operations; FOR UPDATE/SHARE, SELECT INTO, AS OF TIMESTAMP, partitions selection,
TABLESAMPLE, optimizer hints; GROUP BY expressions or DESC, DISTINCT together with GROUP BY,
non-aggregated columns outside GROUP BY; non-numeric strings compared with the handle; ranges
on an UNSIGNED primary key; TIMESTAMP/ENUM/SET/BIT/JSON columns, virtual generated columns,
clustered non-integer primary keys, partitioned tables, views; BEGIN/START TRANSACTION, COMMIT,
ROLLBACK, `SET autocommit=0`, user variables, SET GLOBAL, any other SET; all other statements
(DML, DDL, other SHOW, ...).

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
- Warnings: TiKV warnings are dropped and warning counts are always 0 (Go reports e.g. 2
  truncation warnings for `id='abc'`, which is rejected here anyway).
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
