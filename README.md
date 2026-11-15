# tidb-rs

Experimental Rust implementation of the TiDB SQL layer, starting as a read-only node that runs
alongside Go TiDB in the same cluster. See `docs/adr/0001-scope-and-baseline.md`.

The prototype covers what sysbench `oltp_read_only` needs: autocommit, single-table SELECTs on
tables with an integer primary key. The paths underneath (TSO, schema, KV access) behave like Go
TiDB's; the optimizer, execution engine, caches and system features are mostly still to come.
Results are compared with Go TiDB on the same cluster (`tools/ab`) and must match exactly.

## Status

### Implemented

**Timestamps (TSO)**
- Batched TSO: the PD client sends concurrent requests in batches (up to 64) over one gRPC
  stream, like pd client's TSO dispatcher.
- Async TSO: requested before planning and awaited afterwards (Go `GetTSAsync` / `txnFuture`).
- MaxTS point gets: autocommit primary-key point gets read at MaxTS without a TSO; lock handling
  follows client-go (`resolved_locks`, `MinCommitTsPushed`). See ADR 0005.

**Schema**
- Schema cache: `InfoSchema` cached per schema version (8 versions); the effective version is
  chosen like Go's `GetSchemaVersionWithNonEmptyDiff`.
- Optimistic schema check: plan with the cached schema while reading the schema version and the
  data concurrently; re-plan only on a mismatch (ADR 0003).
- Schema sync: registers in etcd as a TiDB server, reloads the schema in the background and
  reports metadata locks (MDL), so the DDL owner waits for this node. When it cannot stay in
  sync it deregisters itself and falls back to the per-statement check (ADR 0005).
- Only `Public` databases, tables and columns are visible.

**KV and coprocessor** (on our client-rust fork)
- Region and store caches, retries and backoff with TiDB's budgets.
- Coprocessor DAGs (TableScan -> partial HashAgg / Limit) built like Go TiDB builds them;
  keep-order and descending scans; requests to regions run concurrently and carry the schema
  version.
- Row decoding and pushed-down evaluation use TiKV's own `tidb_query_*` crates (vendored), so
  values are identical to Go TiDB's.

**SQL**
- Parser: a port of TiDB's `parser.y`; the generated automaton is identical to goyacc's. The
  grammar is complete, the semantics cover a subset.
- Planner: fixed rules; integer primary-key conditions become ranges with ranger's semantics,
  planned as PointGet, TableDual or TableReader (ADR 0004).
- Executor: root HashAgg (final/complete; count, sum, min, max, firstrow), Sort, Limit,
  Projection; comparisons and grouping use the column's collation (new collation framework).

**Protocol and session**
- MySQL protocol: handshake, `mysql_native_password` and `caching_sha2_password`, TLS
  (including auto-TLS), text and binary result sets, prepared statements
  (prepare/execute/send long data/close/reset), ping, statistics, reset connection.
- Prepared statements cache the parsed IR and are re-planned on every execute (no plan cache).
- A few session variables (autocommit=1, character sets, sql_mode, ...); error codes match Go.

**Operations**
- Status port with `/metrics` and `/status`. Metric names, labels and buckets match Go TiDB
  v8.5.8, so the stock TiDB Grafana dashboard shows tidb-rs (ADR 0006, `docs/ops/metrics.md`).
- `scripts/playground-join.sh` adds a local build to a running tiup playground, including its
  Prometheus target.

### Not implemented (roughly)

- **Caches**: plan cache (prepared and non-prepared), coprocessor cache, statistics (not loaded
  at all). The schema is reloaded in full; no incremental `SchemaDiff` loading. No batch
  commands.
- **Optimizer**: statistics, cost model, secondary indexes (IndexScan / IndexLookUp /
  IndexMerge), joins and join order, subquery rewrites, hints and bindings. Only conditions on
  the integer primary key are used.
- **Execution**: no chunk/vectorized execution (root operators work on materialized rows); no
  coprocessor paging, so one large region scan can exceed gRPC's 4 MB limit; no memory tracking
  or spilling; no Join, Window, Apply, TopN push-down; no TiFlash/MPP. Most scalar functions,
  AVG, DISTINCT aggregates and HAVING are missing.
- **Transactions and writes**: autocommit reads only. BEGIN, DML, DDL, pessimistic locking,
  async commit / 1PC are rejected.
- **Schema objects**: partitioned tables, views, sequences, temporary and cached tables,
  clustered non-integer primary keys, and TIMESTAMP/ENUM/SET/BIT/JSON columns are rejected.
- **System features**: no privilege system (one account from the command line, `mysql.user` is
  not read); no `INFORMATION_SCHEMA`, `SHOW`, system variable framework or global variable
  sync; no slow log file, statement summary, Top SQL, processlist or KILL; no minstartts for GC
  (statements are short); no keyspaces (next-gen), resource control or token limiter. The
  status port does not implement TiDB's coprocessor endpoint, so Go TiDB's `CLUSTER_*` tables
  retry on this node for about 40 s.

## Performance

sysbench `oltp_read_only`, tidb-rs compared with Go TiDB on the same cluster, Mac mini (Apple
M4 Pro):

| concurrency | CPU usage | QPS | P999 latency |
|--:|--:|--:|--:|
| 1 | -30% | +20% | 900 µs -> 500 µs |
| 8 | -25% | +14% | 1.25 ms -> 980 µs |

QPS and p95 of sysbench `oltp_read_only` on a tiup playground (v8.5.8, 1 PD / 1 TiKV, 1 table ×
100k rows, 30 s per run, `--skip_trx=on --db-ps-mode=auto`; ADR 0005):

| threads | Go TiDB QPS | tidb-rs QPS | ratio | p95 Go / tidb-rs |
|--:|--:|--:|--:|--:|
| 1 | 5,683 | 6,694 | 1.18 | 2.75 / 2.34 ms |
| 8 | 21,841 | 23,263 | 1.07 | 5.88 / 5.22 ms |
| 16 | 28,294 | 27,758 | 0.98 | 9.32 / 8.96 ms |

## Roadmap

1. **Broaden SQL coverage** so the prototype becomes practical: more of the parser's semantics,
   operators and functions.
2. **Explore problems that are hard to solve in Go TiDB**
   - JSON performance and `JSON_TABLE`.
   - Cached tables.
   - Memory areas that put heavy pressure on Go's GC: coprocessor cache, plan cache, schema
     cache (infoschema v2), stats cache, chunks.
   - SIMD instructions, which Go makes hard to use.
   - Modern SQL syntax and features from PostgreSQL and DuckDB.
3. **Grow the read-only node**: together with a modified TiProxy, route between Go TiDB and
   tidb-rs read-only nodes in mixed deployments.
4. **Evaluate the development method and engineering practice**
   - Visualize TiDB's modules, features, unit tests and end-to-end tests.
   - Build a cherry-pick pipeline from Go TiDB to tidb-rs.
   - Explore how AI agents can port code, test and verify autonomously.

## Porting rules

After the prototype, tidb-rs is ported from Go TiDB v8.5.8 by plan, block by block, keeping the Go
structure. The rules are in [CONSTITUTION.md](CONSTITUTION.md) (Chinese); the method and phases in
[docs/porting/methodology.md](docs/porting/methodology.md); deliberate deviations from Go are listed in
[specs/exceptions/](specs/exceptions/README.md).

## Glossary of IDs

IDs used in docs, issues, PRs and code comments, and where each is defined:

| ID | Meaning | Defined in |
|---|---|---|
| **P0–P5** | Porting phases: P0 preparation (charter, roadmap, foundation designs); P1 foundations (types, encoding, expression framework, system variables); P2 read path; P3 optimizer; P4 executors and the session main flow; P5 periphery (string / time / misc functions, SHOW / EXPLAIN / SET, memory tables, privileges, bindings, observability, HTTP API) | [specs/ROADMAP.md](specs/ROADMAP.md) §1–2 |
| **Dxx.y** (e.g. D06.4, D10.6) | A porting **block**: a group of Go functions, usually one package, ported as a whole. `Dxx` is the domain (D00 base, D01 types and encoding, D05 session, D06 metadata, D07 storage access, D08 expressions, D09 plan building and logical optimization, D10 physical optimization: ranger, cost, cardinality…, D11 statistics, D12 executors, D13 plan cache and hints, D14 execution stats, D15 memory…), `.y` the block within it. Each block has `specs/Dxx.y-<name>/` | [specs/ROADMAP.md](specs/ROADMAP.md) §4 |
| **Layers L1–L4 in ROADMAP tables** | Dependency layers **within** a phase: L1 first, same layer in parallel. Not the porting levels below | [specs/ROADMAP.md](specs/ROADMAP.md) §4 |
| **L1 / L2 / L3** (code markers `// go: Func L1`) | Porting **levels**: L1 faithful port (Go unit tests pass unchanged once carriers are mapped); L2 Rust-style rewrite with the same semantics; L3 replaced by a same-origin Rust implementation (e.g. client-rust), needs an ADR | [CONSTITUTION.md](CONSTITUTION.md) art. 2 |
| **M-xxx** (e.g. M-FT, M-SHARE, M-CH) | Global **mapping rules**: how a Go idiom is always rewritten in Rust (e.g. M-FT: an expression's `FieldType` is copy-on-write). Markers read `L1 [M-FT]` | [docs/porting/mappings.md](docs/porting/mappings.md) |
| **N0–N13** | **Foundation design** specs (plan representation, session context, concurrency, expression evaluation, chunk, system variables, observability, resource control, shared metadata…). They are exceptions; once approved, the N series and A1, A4 are irreversible (charter art. 3.5) | [specs/exceptions/README.md](specs/exceptions/README.md) |
| **A1–A6** | Implementation **exceptions** (memory model, execution model, caches, JSON, SIMD, zero-copy reads); A1 and A4 are irreversible like the N series | same |
| **B1–B5 / C1–C3 / D1** | Feature exceptions: B = features not ported, C = new features, D1 = out of scope (TiFlash / MPP). D1 is not a block ID | same |
| **X, EXC.\*, DEF.\*** | Pseudo-blocks used in stubs and ROADMAP: X = not ported (DDL, write path, ANALYZE…; done by Go TiDB in the cluster); EXC.B = paths dropped by exceptions B1–B5 (cascades, cost model v1, static partition pruning); DEF.MPP = deferred (TiFlash / MPP) | pseudo-block table at the end of [specs/ROADMAP.md](specs/ROADMAP.md), [CONSTITUTION.md](CONSTITUTION.md) art. 4 |
| `blocked-by:Dxx.y` / `blocked("Dxx.y")` | Boundary stub: not ported yet, waits for that block; registered in the tracker's `data/stubs.tsv` | same |
| **Q1–Qn** | Settled methodology questions (baseline version, optimizer scope, types and encoding…) | [docs/porting/methodology.md](docs/porting/methodology.md) |
| **ADR nnnn** | Architecture decision record | [docs/adr/](docs/adr/) |
| **VS1-nn**, **C2a / C2b** | Tasks of the VS1 vertical slice and its checkpoints (C2a: point queries end to end; C2b: scan-type queries) | [specs/VS1-vertical-slice/](specs/VS1-vertical-slice/plan.md) |
| **T1–Tn** | Task numbers inside one block's spec | each block's `specs/Dxx.y-*/tasks.md` |
| **govec** | Differential tests against Go: vectors generated by Go v8.5.8, replayed by Rust | [tidb-rs-tracker](https://github.com/dulao5/tidb-rs-tracker) |
| **converge** | Block progress: functions ported / stubbed / not ported (`scripts/converge.py`) | [docs/porting/methodology.md](docs/porting/methodology.md) |
| Main labels | `block` needs the owner's decision; `upstream-pitfall` upstream Go TiDB issue; `depth` vertical work beyond the operator layer; `deferred` decided, done in a later block; `plan` plans and milestones; `arch-review` weekly architecture review and follow-ups; `showcase` engineering worth showing | [all labels](https://github.com/dulao5/tidb-rs/labels) |

## Layout

```
crates/poc/server   tidb-server binary: MySQL protocol on 127.0.0.1:4001 over the SQL sessions
                    of crates/poc/session, reading the cluster behind `--pd` (`tidb-server --help`)
crates/mysql-protocol  MySQL wire-protocol server layer (see crates/mysql-protocol/README.md)
crates/parser       SQL parser ported from TiDB's parser.y (see crates/parser/README.md)
crates/query-smoke  smoke tests for the vendored TiKV query crates
crates/poc/kv       snapshot reads over tikv-client (our fork with coprocessor support): TSO,
                    get/batch_get/scan, coprocessor requests, TiDB-like retry budgets
crates/meta         TiDB schema meta (DBInfo/TableInfo JSON) read at a snapshot; InfoSchema and
                    the per-statement schema cache (docs/adr/0003-schema-loading.md)
crates/distsql      record key ranges, tipb DAG building (scan/partial agg/limit), response and
                    row decoding; cluster tests vs Go TiDB (TIDB_RS_PD, TIDB_RS_TIDB_PORT)
crates/sql          statement IR, name resolution, handle ranges, physical plans and result
                    column metadata (docs/adr/0004-prototype-sql-layer.md)
crates/poc/executor  root executors: point get / table reader sources, final and complete hash
                    aggregation, collation-aware sort, limit, datum -> protocol value conversion
crates/poc/session  SessionFactory/SessionHandler: autocommit reads at a fresh TSO, prepared
                    statements, SET/USE/constant SELECTs
crates/poc/domain   schema sync: etcd server registration, background schema reload, MDL
                    (docs/adr/0005-schema-sync.md)
crates/metrics      Prometheus metrics with Go TiDB's names (docs/adr/0006-metrics.md)
third_party/tikv    TiKV's tidb_query_* crates, vendored unmodified (see PROVENANCE.md)
third_party/tikv-shims  stand-ins for the TiKV-internal crates they depend on
tools/parser-port   converters that regenerate the parser's grammar/keyword/test inputs from a
                    TiDB checkout (regen.sh)
tools/sync-tikv-query.sh  re-vendors third_party/tikv from a TiKV commit
tools/ab            A/B harness: same queries on Go TiDB and tidb-rs, compares rows, column metadata
                    and errors (text + prepared); with scripts/ for playground, sysbench load/runs
                    (see tools/ab/README.md)
scripts            tiup playground start/stop, joining a local build to a running playground,
                    sysbench prepare/run
tools/protocol-smoke  protocol smoke/perf clients: libmysqlclient (ps_smoke.c) and a sysbench lua
                    script (prepared `SELECT ?` and text `SELECT 1`)
```

Building needs a C++ compiler, CMake, protoc (>= 3.1) and OpenSSL (grpcio-sys and openssl-sys are
pulled in by the vendored crates via tipb). With libstdc++ 13 or newer (GCC 13+, or clang
using it), grpcio-sys's bundled abseil fails to compile because the standard headers no longer
include `<cstdint>` indirectly; build with `CXXFLAGS="-include cstdint"`.

Checks (first-party crates only for fmt/clippy; vendored code keeps TiKV's style):

```
cargo fmt --check $(tools/first-party.sh)
cargo clippy --no-deps --all-targets $(tools/first-party.sh) -- -D warnings
cargo test --workspace
```
