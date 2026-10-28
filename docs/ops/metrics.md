# Monitoring tidb-rs with the stock TiDB Grafana dashboard

tidb-rs exports Prometheus metrics under Go TiDB v8.5.8's names, labels and buckets
(`docs/adr/0006-metrics.md`), so the **TiDB** dashboard that tiup deploys
(`pkg/metrics/grafana/tidb.json`) shows a tidb-rs node like a Go TiDB once Prometheus scrapes it
**under `job="tidb"`**.

## The status port

```sh
tidb-server --pd <pd> --host 0.0.0.0 -P 4001 --status-port 10081   # defaults: --status-host = --host, 10081
curl http://<host>:10081/metrics      # Prometheus text format
curl http://<host>:10081/status       # {"connections":N,"version":"...","git_hash":"..."}
```

- `--status-port 0` disables the status server. The node then advertises `status_port` 0.
- With the server on, the node advertises its real status port in `/tidb/server/info`, so
  `information_schema.cluster_info` / `tidb_servers_info` show it.
- Prometheus must reach `--status-host`. The default is `--host`, which is `127.0.0.1` unless
  you change it. For a remote Prometheus, listen on a routable address.
- Every other path returns 404. Go TiDB's cluster tables (`cluster_processlist`,
  `cluster_config`, ...) then add a warning for this node, as they did before the status server
  existed (ADR 0006).

## Add tidb-rs to a tiup cluster's Prometheus

tiup generates `prometheus.yml` from the topology, and a tidb-rs node is not in the topology.
Add it with `additional_scrape_conf` on the monitoring server.

1. Run `tiup cluster edit-config <cluster>`:

   ```yaml
   monitoring_servers:
     - host: 10.0.1.10
       additional_scrape_conf:
         scrape_configs:
           - job_name: "tidb"            # must be "tidb": the dashboard filters on job="tidb"
             honor_labels: true
             static_configs:
               - targets: ["10.0.1.21:10081"]   # tidb-rs --status-host:--status-port
   ```

   tiup merges `additional_scrape_conf` into the generated `prometheus.yml`. If its own `tidb`
   job already exists, Prometheus refuses duplicate job names. In that case add the target to
   the existing job: edit `conf/prometheus.yml` on the monitoring host and add the address to
   the `tidb` job's `static_configs.targets`. A later `tiup cluster reload -R prometheus`
   overwrites that edit, so re-apply it after reloads.
   *(Check against your tiup version: whether tiup merges a same-named job or rejects it
   depends on the version. Neither path was exercised here; see "Verified" below.)*
2. Run `tiup cluster reload <cluster> -R prometheus`.
3. Open Grafana, then *<cluster>-TiDB*, and pick the tidb-rs address in **Instance**. The
   variable lists every `instance` of `tidb_server_connections`.

## Add tidb-rs to a tiup playground

Playground's Prometheus uses file-based discovery. Its `prometheus.yml` scrapes `targets.json`
next to it, and Prometheus re-reads that file on change, so no restart is needed.

```sh
D=~/.tiup/data/<playground tag>/prometheus
# Add "<host>:<status port>" to the targets of the entry whose labels are {"job": "tidb"}:
#   {"targets": ["127.0.0.1:10080", "127.0.0.1:10081"], "labels": {"job": "tidb"}}
$EDITOR $D/targets.json
```

`scripts/playground.sh` starts the playground `--without-monitor`. For a demo, start
Prometheus and Grafana from the tiup components yourself:

- **Prometheus.** Use `~/.tiup/components/prometheus/v8.5.8/prometheus/prometheus` with a
  `prometheus.yml` that has one `file_sd_configs` job and a `targets.json` with the `pd`,
  `tikv` and `tidb` jobs (Go TiDB and tidb-rs status addresses under `tidb`).
- **Grafana.** Run `~/.tiup/components/grafana/v8.5.8/bin/grafana-server --homepath <that dir>
  --config custom.ini`. Provision one Prometheus datasource. Load the dashboard from
  `~/.tiup/components/grafana/v8.5.8/tidb.json`, which is byte-identical to
  `pkg/metrics/grafana/tidb.json` at v8.5.8, with `${DS_TEST-CLUSTER}` replaced by the
  datasource name. tiup keeps the `k8s_cluster`/`tidb_cluster` matchers. Their variables are
  empty, which matches series without those labels.

## Verified

Setup: playground v8.5.8, Prometheus 2.49.1 and Grafana 7.5.17 from tiup, sysbench
`oltp_read_only` at 16 threads on tidb-rs. Every panel query of the rows below was evaluated
through the Prometheus HTTP API with `$instance` set to the tidb-rs target. Screenshots were
taken with tidb-rs selected.

Not exercised: the `tiup cluster` path (`additional_scrape_conf`) needs a deployed cluster.

## What the dashboard shows for tidb-rs

**Panels with data**

| Row | Panels |
|---|---|
| Query Summary | Duration, Command Per Second, QPS (by statement type), CPS By Instance, Connection Idle Duration (`in_txn=0`), 999/99/95/80 Duration |
| Query Detail | Duration 80/95/99/999 By Instance |
| Server | TiDB Server Status, Uptime, CPU Usage (+ quota from `tidb_server_maxprocs`), Memory Usage (RSS line), Open FD Count, Connection Count (incl. tokens), Disconnection Count, Get Token Duration, Client Data Traffic |
| Transaction | Transaction OPS / Duration (`optimistic`/`commit`, `general`: statements that took a TSO; MaxTS point gets do not start a transaction, as in Go) |
| Executor | Compile Duration, Execution Duration, Expensive Executors OPS (HashAgg / StreamAgg / Sort / TopN), Plan Cache Miss OPS (`prepared`: every execute, there is no plan cache) |
| Distsql | Distsql Duration, Distsql QPS (`copr_type="common"`: tidb-rs does not page these reads), Coprocessor Seconds 999 |
| KV Errors | KV Backoff Duration/OPS, TiClient Region Error OPS, Lock Resolve OPS, Replica Selector Failure (these show 0 unless errors, locks or backoffs occur) |
| KV Request | KV Request OPS, Duration 99 by store / by type, Region Cache OK/Error OPS, Load Region Duration, RPC Layer Latency |
| PD Client | PD Client CMD OPS/Duration/Fail OPS, PD TSO OPS, PD TSO Wait Duration, PD TSO RPC Duration, Async TSO Duration |

**Panels that stay empty, and why**

| Panel | Why |
|---|---|
| Failed Query OPM, Slow query | No errors / no statement ≥ 300 ms in the workload. They fill when these happen. Slow query has the total-time line only: cop process/wait and MVCC ratio are not recorded. |
| Parse Duration | Only `COM_QUERY` parses. sysbench uses prepared statements (Go shows almost nothing here either). |
| Queries in Multi-Statement | Multi-statements are rejected. The series exists and stays 0. |
| Queries Using Plan Cache OPS, Plan Cache Memory/Plan Num | No plan cache. |
| Memory Usage: Go heap / GC / `tidb_server_memory_usage` lines, Runtime GC, Goroutine Count | Go runtime metrics. Not faked. |
| Events, Panic, Keep Alive, Time Jump, Skip Binlog, RCCheckTS, Handshake Error, Internal Sessions, Prepare Statement Count | Not implemented (no such events or concept). |
| Affected Rows, Internal SQL OPS, txn write/lock/commit panels, batch client, TopSQL, TTL, DDL, statistics, owner, GC, schema load, ... | Read-only node without those subsystems. |
| KV Request Forwarding, Stale Read, slow scores, Read Req Traffic, `TiDB Wait TSO Future Duration` | Not recorded (no forwarding/stale read, no per-store slow score). |
| Command Per Second "`offset 1d`" line, Transaction OPS `scope="internal"`, Connection Idle `in_txn='1'` | No history, no internal SQL, no explicit transactions. |
| TiClient Region Error OPS, second query | The dashboard's expression `{...}{EXTERNAL_LABELtype=...}` is invalid for every instance, Go included. |
