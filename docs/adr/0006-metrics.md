# ADR 0006: Prometheus metrics under Go TiDB's names, and a status port

Status: accepted (prototype). Date: 2026-10-28.

## Context

The stock TiDB Grafana dashboard (`pkg/metrics/grafana/tidb.json`, the one tiup deploys)
should show a tidb-rs node during a demo. That dashboard reads fixed metric names, label names
and label values, filters on `job="tidb"`, `instance=~"$instance"`, and lists instances from
`label_values(tidb_server_connections, instance)`.

Before this change tidb-rs exported no metrics and ran no HTTP server. `ServerInfo.status_port`
was advertised (`--advertise-status-port`, default 0), but nothing listened.

## Decision

1. **Status server** (`crates/server/src/status.rs`) on `--status-host` (default `--host`)
   and `--status-port`:
   - The default port is **10081**. It sits next to Go's 10080, the way `-P 4001` sits next
     to 4000, so a Go TiDB and tidb-rs on one host do not clash. `--status-port 0` disables
     the server.
   - Routes: `GET /metrics` (Prometheus text) and `GET /status`
     (`{"connections":N,"version":...,"git_hash":...}`, Go's field order). Every other path is
     `404 page not found`.
   - The server speaks HTTP/1.1 and HTTP/2 (hyper-util `auto`).
   - The server info advertises the bound port. `--advertise-status-port` still overrides it.
2. **Metric names, label names, label values and buckets are Go's.** They are copied from
   tidb `v8.5.8-20261021-849a85c` (`R:`), client-go `v2.0.8-0.20261020042752-6ea7c1662c95`
   (`CG:`) and pd/client `v0.0.0-20260804033407-85a975a5ca78` (`PD:`), the versions tidb's
   go.mod pins. The table is below; `crates/metrics/src/golden.rs` holds the same table as
   data.
   - Everything goes into the `prometheus` crate's (0.13, the one version in the tree)
     default registry.
   - tikv-client records into the same registry.
3. **Go's text format.** `crates/metrics/src/encode.rs` writes the exposition itself, because
   the crate's encoder spells floats differently from Go's `strconv.FormatFloat(v,'g',-1,64)`.
   - Example: `le="0.00004"` from the crate versus `le="4e-05"` from Go.
   - tiup's Prometheus (2.49) keeps `le` as a string, so different spellings would split one
     bucket into two series across Go and tidb-rs instances.
   - The encoder is tested against 253 values formatted by Go.
4. **Where the metrics live.**
   - The `tidb-metrics` crate defines the server/session/executor/distsql families and the
     process collector.
   - The protocol, session and distsql crates record into it.
   - Our tikv-client fork records the client families (branch `tidb-rs/metrics`, feature
     `client-go-metrics`, off by default; the fork's own `stats.rs` metrics are unchanged).
   - `tidb_kv::TsFuture` records PD's TSO wait split.
5. **Hot path.**
   - Every series a statement touches is a pre-resolved child: an `Arc` to a histogram or
     counter, held in a static `OnceLock`/`LazyLock` or an array indexed by enum. tikv-client
     keeps a per-store table indexed by store id.
   - So a statement does no label hashing, no lock and no allocation. Recording is a few
     relaxed atomic adds.
   - Packet bytes are summed per connection and published once per command, not once per
     row packet.

## Status port: impact on Go TiDB (v8.5.8)

`reports/serverinfo-impact-v8.5.md` §1/§4 lists every v8.5.8 consumer that calls a registered
TiDB's `status_port`. `git grep '\.StatusPort'` on R adds nothing remote beyond these.

| Consumer | Call | Before: port 0 | Now: listening, 404 | Measured (`tests/metrics_process.rs`) |
|---|---|---|---|---|
| `CLUSTER_PROCESSLIST`, `CLUSTER_SLOW_QUERY`, `CLUSTER_STATEMENTS_SUMMARY*`, `CLUSTER_TIDB_TRX` (and so `mysql.tidb_mdl_view`), `CLUSTER_DEADLOCKS`, `CLUSTER_MEMORY_USAGE*`, global KILL fan-out (never, `server_id` 0) | TiDB coprocessor gRPC (`R:pkg/store/copr/coprocessor.go:609`) | gRPC `Unavailable` (dial fails), warning | HTTP/2 404 → gRPC `Unimplemented`, warning | `cluster_processlist` 41.6 s (listening) vs 40.2 s (port 0) |
| `cluster_config` | HTTP `GET /config` (`R:pkg/executor/memtable_reader.go:219-251`) | dial error, warning | `404 Not Found`, warning | 45 ms vs 45 ms |
| `cluster_load`, `cluster_hardware`, `cluster_systeminfo`, `cluster_log` | diagnostics gRPC | `Unavailable`, warning | `Unimplemented`, warning | `cluster_load` 1.55 s vs 1.56 s |
| `cluster_info`, `tidb_servers_info` | display only (shows the port) | `0` | the real port | 40 ms vs 40 ms |
| `metrics_schema`, `inspection_*`, `metrics_summary` | Prometheus queries by `instance` | node absent | **node present** (its `tidb_*` series) | benign: more rows |

**Finding: the old "fails fast" claim (ADR 0005) is false for the coprocessor-based cluster
tables.**
- Go's TiDB-coprocessor client retries a failed RPC to a TiDB store with backoff whatever the
  gRPC code. One query against `cluster_processlist` takes about 41 s, both with port 0 and
  with the status server.
- So the listening server is no worse, but no better either.
- Making those tables fast needs a real reply on the status port, which means implementing
  enough of TiDB's coprocessor gRPC service to return an empty result or a non-retryable
  error. That is out of scope here and reported to the owner.
- `tests/metrics_process.rs::cluster_tables_no_slower_with_status_server` asserts "listening
  ≤ port 0 × 1.25 + 2 s" for each query.
- Measured on playground v8.5.8 with exactly one tidb-rs node registered (2026-10-28). With
  more nodes registered, each one adds its own fan-out failure.

## Metric table

`Go source` gives the line of the `Name:` field. The **Value** column says how tidb-rs fills
the series: **faithful** = Go's semantics; **approx** = close, with the difference given;
**zero** = the series exists (Go creates it at start too) but this node never moves it.

### tidb (R:)

| Metric | Type, labels, buckets | Go source | Value |
|---|---|---|---|
| `tidb_server_info{version,hash}` | gauge | `pkg/metrics/server.go:311` | faithful: start time (s). `version` = `v8.5.8-tidb-rs-<hash7>` (the part after `-TiDB-` of the server-info version). Go uses `TiDBReleaseVersion`. |
| `tidb_server_maxprocs` | gauge | `server.go:286` | approx: tokio worker threads (`TOKIO_WORKER_THREADS` or available parallelism), in place of `GOMAXPROCS`. |
| `tidb_server_connections{resource_group}` | gauge | `server.go:127` | faithful: from accept to close, `default`. |
| `tidb_server_tokens` | gauge | `server.go:319` | faithful: commands in progress. There is no token limiter, which is Go's behaviour below `token-limit`. |
| `tidb_server_get_token_duration_seconds` | histogram, exp(1,2,30) | `server.go:232` | faithful: one observation of 0 per command. Go observes µs truncated, so 0 when not blocked. |
| `tidb_server_query_total{type,result,resource_group}` | counter | `server.go:119` | faithful: by command (`CmdToString`), `OK`/`Error`. The 13 named commands × 2 are created at start, as in Go. |
| `tidb_server_handle_query_duration_seconds{sql_type,db,resource_group}` | histogram, exp(0.0005,2,29) | `server.go:92` | faithful: per command, from packet read to response flushed. `sql_type` = the session's last statement type (Go's stale `StmtCtx.StmtType`), `general` after a prepare or before any statement. `db=""` (Go: `record-db-label` false). |
| `tidb_server_conn_idle_duration_seconds{in_txn}` | histogram, exp(0.0005,2,29) | `server.go:302` | faithful: `in_txn="0"` (autocommit only). |
| `tidb_server_execute_error_total{type,db,resource_group}` | counter | `server.go:150` | approx: `type` = Go's `class:code` for codes whose class is unambiguous (1064 parser, 1146/1049 schema, 1054/1046/1052 planner), otherwise `unknown`. |
| `tidb_server_multi_query_num` | histogram, exp(1,2,20) | `server.go:241` | zero: multi-statements are rejected. |
| `tidb_server_disconnection_total{result}` | counter | `server.go:135` | faithful: `ok` after COM_QUIT, `error` on read failure (EOF included, as in Go). `undetermined` is never counted. |
| `tidb_server_packet_io_bytes{type}` | counter | `server.go:84` | faithful: `In` = payload bytes, `Out` = payload + 4 per logical packet, handshake included. |
| `tidb_server_plan_cache_total{type}` | counter | `server.go:182` | zero: there is no plan cache. |
| `tidb_server_plan_cache_miss_total{type}` | counter | `server.go:190` | faithful to "no cache": `prepared` += 1 per COM_STMT_EXECUTE. COM_QUERY is not counted (Go's non-prepared cache is off by default). |
| `tidb_server_slow_query_process_duration_seconds{sql_type}` | histogram, exp(0.001,2,28) | `server.go:250` | approx: `general`, total command time ≥ 300 ms (Go's default `tidb_slow_log_threshold`). There is no `tidb_server_slow_query_total` in v8.5.8; the dashboard's "Slow query" panel and `metrics_schema.tidb_slow_query_total_count` use this histogram. |
| `tidb_session_parse_duration_seconds{sql_type}` | histogram, exp(0.00004,2,28) | `pkg/metrics/session.go:60` | faithful: COM_QUERY parse+lower, `general`. Executes do not parse, as in Go. |
| `tidb_session_compile_duration_seconds{sql_type}` | histogram, exp(0.00004,2,28) | `session.go:69` | approx: time in the planner (`plan_select`, all attempts) per statement. |
| `tidb_session_execute_duration_seconds{sql_type}` | histogram, exp(0.0001,2,30) | `session.go:79` | approx: command total − parse − compile, so it includes writing the result, as Go's `GetExecuteDuration` does for streamed results. |
| `tidb_session_transaction_duration_seconds{txn_mode,type,scope}` | histogram, exp(0.001,2,28) | `session.go:130` | faithful: `optimistic`,`commit`,`general` for successful statements that took a start TSO. MaxTS point gets and constant SELECTs have no transaction, as in Go (measured: Go 66 k vs tidb-rs 69 k per 10 s run). Failed statements are not observed (Go rolls back). |
| `tidb_executor_statement_total{type,db,resource_group}` | counter | `pkg/metrics/executor.go:95` | faithful: `ast.GetStmtLabel` types (`Select`, `Use`, `Set`, `Show`, `Begin`...). Includes `Use` for COM_INIT_DB and the handshake database. |
| `tidb_executor_expensive_total{type}` | counter | `executor.go:86` | approx: root `HashAggExec` (GROUP BY or DISTINCT), `StreamAggExec` (scalar aggregate), `SortExec`, `TopNExec` (sort + limit). The sysbench mix gives the same counts as Go. The other 6 types stay 0. |
| `tidb_distsql_handle_query_duration_seconds{type,sql_type,copr_type}` | histogram, exp(0.0005,2,29) | `pkg/metrics/distsql.go:38` | faithful: `dag`,`general`, `paging` or `common` from the request. Our range reads do not page, so they are `common`. Go pages them. |
| `process_start_time_seconds`, `process_cpu_seconds_total`, `process_resident_memory_bytes`, `process_virtual_memory_bytes`, `process_open_fds`, `process_max_fds` | client_golang names and help | client_golang process collector | faithful on Linux and macOS. CPU from `getrusage`; RSS/VSZ from `/proc/self/statm` or `proc_pidinfo`; fds from `/proc/self/fd` or `/dev/fd` and `RLIMIT_NOFILE`. Start time = collector creation, a few ms after exec. No `go_*` metrics are faked. |

### client-go (CG:, namespace `tidb`, subsystem `tikvclient`, `R:pkg/metrics/metrics.go:315`)

| Metric | Type, labels, buckets | Source | Value |
|---|---|---|---|
| `request_seconds{type,store,stale_read,scope}` | histogram, exp(0.0005,2,24) | `metrics/metrics.go:183`; observed in `internal/client/client.go:1034` | faithful: per RPC around the gRPC call. `type` = tikvrpc `CmdType` string, `store` = `Context.peer.store_id`. `stale_read="false"`, `scope="false"` (no stale reads; schema loads are not marked internal). |
| `rpc_net_latency_seconds{store,scope}` | histogram, exp(0.0001,2,20) | `:204`; `client.go:1049-1063` | faithful: RPC time − `ExecDetailsV2.time_detail_v2.total_rpc_wall_time_ns` (v1 fallback). Only for responses that carry it: Get, BatchGet, Cop, txn commands. Scan and Raw responses do not. |
| `backoff_seconds{type}` | histogram, exp(0.0005,2,29) | `:173`; per sleep at `config/retry/backoff.go:197` | approx: each retry sleep. `tikvRPC` (gRPC errors), `tikvLockFast` (live locks), `regionMiss`/`regionScheduling`/`serverBusy`/`staleCommand` (region errors by kind), `""` (txn not found). The fork's attempt-based backoff values are recorded, not client-go's. |
| `region_err_total{type,store}` | counter | `:224`; `internal/locate/region_request.go:1799` | faithful labels (`regionErrorToLabel` ported). Counted when the error is handled, so the error that exhausts the retry budget is not counted. |
| `region_cache_operations_total{type,result}` | counter | `:312`; `metrics/shortcuts.go:280-292` | faithful: `get_region_when_miss`, `get_region_by_id`, `get_store` (`ok`/`err`) on PD loads, and `invalidate_region_from_cache` `ok`. |
| `load_region_cache_seconds{type}` | histogram, exp(0.0001,2,20) | `:329` | faithful: duration of successful PD loads, same types. |
| `lock_resolver_actions_total{type}` | counter | `:215`; `txnkv/txnlock/lock_resolver.go` | approx: `resolve`, `expired`, `not_expired`, `wait_expired`, `query_txn_status`(`_committed`/`_rolled_back`), `query_check_secondary_locks`, `query_resolve_locks`, `resolve_async_commit`, `batch_resolve`, at the fork's equivalent steps. |
| `replica_selector_failure_counter{type}` | counter | `:664` | zero: the fork has no replica selector. `exhausted`/`invalid`/`stale_store` exist at 0. |

### PD client (PD:)

| Metric | Type, labels, buckets | Source | Value |
|---|---|---|---|
| `pd_client_cmd_handle_cmds_duration_seconds{type}` | histogram, exp(0.0005,2,13) | `metrics/metrics.go:97` | faithful: `tso` (request→result, in the fork), `wait` and `tso_async_wait` (`tidb_kv::TsFuture`, as `clients/tso/request.go:80-93`), `get_region`, `get_region_byid`, `get_store`, `get_all_stores`, `update_gc_safe_point`, `load_keyspace` (successful calls, retries included). |
| `pd_client_cmd_handle_failed_cmds_duration_seconds{type}` | histogram, exp(0.0005,2,13) | `:107` | faithful: failed calls. Children are created at start, as in Go. |
| `pd_client_request_handle_requests_duration_seconds{type}` | histogram, exp(0.0005,2,13) | `:117`; `clients/tso/stream.go:421-434` | faithful: `tso` = one TSO batch's stream round trip. `tso-failed` exists. |

Metrics in the owner's list that do not exist in v8.5.8: `tidb_server_slow_query_total`.
Its panel uses `tidb_server_slow_query_process_duration_seconds`, see above.

## Hot-path cost

Setup:
- sysbench `oltp_read_only --skip_trx=on --db-ps-mode=auto`, 30 s per point, 1 table of 100k
  rows in 8 regions. Playground v8.5.8 on an Apple M4 Pro shared with other agents.
- Two interleaved rounds (base, metrics, port0, Go, then again). QPS is the mean of the two.
  - **base**: `main@8061bb4`, no metrics code.
  - **metrics**: this branch, scraped every 5 s.
  - **port0**: this branch with `--status-port 0`, recording but not scraped.

| threads | base | metrics | Δ | port0 | Δ | Go TiDB |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 6,265 | 6,070 | −3.1% | 6,017 | −4.0% | 5,396 |
| 8 | 20,906 | 21,366 | +2.2% | 20,337 | −2.7% | 19,810 |
| 16 | 26,638 | 25,893 | −2.8% | 25,559 | −4.1% | 26,407 |

Reading:
- Run-to-run spread on this machine is about ±3%: base t8 was 20,170 and then 21,642.
- The cost is about 3% at t1 and t16, or ~0.3 µs per statement at t1. That is at or slightly
  above the 2% target and not clearly separable from noise.
- Scraping costs nothing measurable: port0 is no faster than metrics.
- `--status-port 0` only disables the HTTP server. Recording stays on, so port0 measures the
  recording cost.

Per statement, the cost is about 12 histogram observations and 6 counter increments on shared
atomics, plus about 6 extra `Instant::now()` reads:
- protocol: idle, token, duration;
- session: compile, execute, transaction;
- distsql;
- tikv-client: request + net latency per RPC;
- PD: `tso`, `wait`, `tso_async_wait`.

If the target must hold under noise, the next step is to batch the per-statement histograms
per session (`prometheus::local::LocalHistogram`, flushed every N statements or on idle).
That removes the shared-cache-line traffic at t16.

`tools/ab` (sysbench workload with `-n 200`, `constants.sql`, `sql-layer.sql`) gives
**identical results for this branch and `main@8061bb4`**. Both show the same 6 + 1 pre-existing
mismatches:
- full-range scans (`BETWEEN -2^63 AND 2^63-1`, 100k rows) fail in tikv-client with gRPC
  "decoded message length too large";
- the `DATABASE()` column flag in COM_STMT_PREPARE.

So the ab run is not "0 mismatches", but this change adds none.

## Consequences

- A tidb-rs node appears in the TiDB dashboard, in Prometheus-backed `metrics_schema` and
  `inspection_*` queries, and with a real status port in `cluster_info`.
- `tikv-client` temporarily comes from a `file://` clone of the fork (rev `2fc142f`, branch
  `tidb-rs/metrics`, not pushed). Switch back to the GitHub URL after review.
- The coprocessor-based cluster tables still take about 40 s with a tidb-rs node registered.
  That is unchanged from before and needs its own fix.
