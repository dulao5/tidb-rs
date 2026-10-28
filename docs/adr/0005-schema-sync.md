# ADR 0005: Joining TiDB's schema sync (M4 phase B)

- Status: proposed (2026-10-27)
- Context: ADR 0003 (per-statement schema check, which stays as the fallback), M4 perf gap
  (`docs/perf/m4a-profile.md`), and the audit of what registering as a TiDB server means in a
  v8.5.8 cluster (`reports/serverinfo-impact-v8.5.md`, cited below as "the audit"). Code:
  `crates/domain` (`ServerInfo`, `Registry`, `SchemaSync`), `crates/session`
  (`Session::synced_read`), `crates/server` (flags), client-rust branch `tidb-rs/maxts`.

## Decision

The node joins the protocol Go TiDB uses to keep schema versions in sync, so the DDL owner
waits for it as it waits for any Go TiDB. In exchange, statements stop paying for schema
freshness:

- they use the newest schema loaded in the background (no version read per statement), and
- autocommit point gets by handle read at **MaxTS** (`u64::MAX`), with no TSO at all.

The node uses this **synced mode** only while it can prove it is in sync. Otherwise it
leaves the cluster's wait set and serves statements through the ADR 0003 path, which is
correct without any cooperation from the owner. `--schema-sync=false` turns all of this off:
the node then behaves exactly as in phase A and writes nothing to etcd.

## Protocol

### Registration (etcd = PD's client URLs)

- `/tidb/server/info/<ddl_id>`: Go's `ServerInfo` JSON (`crates/domain/src/serverinfo.rs`,
  golden-tested against a record written by Go v8.5.8). The fields follow the audit's
  MUST list:
  - `ddl_id` is a random UUIDv4 and equals the key suffix.
  - `ip:listening_port` is real and unique. The IP is `--advertise-address`, else `--host`
    when it is not a wildcard, else the local address of the route to PD.
  - `start_timestamp` is the process start time.
  - `version` is `8.0.11-TiDB-v8.5.8-tidb-rs-<short git sha>`. Go parses the part after
    `-TiDB-` as semver ≥ 8.4.0, so DDL keeps job version 2, and `FormatTiDBVersion` stays
    well-formed.
  - `git_hash` is the build commit, `server_id` is 0 (no `/tidb/server_id` registration, so a
    global KILL never routes here), `labels` is empty (in particular no `zone`), and
    `disable_rpc` is `true`. v8.5 ignores `disable_rpc`; newer TiDB uses it to skip the node
    for TiDB-to-TiDB RPC.
  - `status_port` is the port of the status server (`--status-port`, default 10081, which
    serves `/metrics` and `/status` and 404s everything else), or `--advertise-status-port`,
    or 0 with `--status-port 0`. Go's cluster-table fan-out (`CLUSTER_*`, `cluster_config`,
    `cluster_load`, ...) to this node fails fast with a warning either way: see
    `docs/adr/0006-metrics.md`, "Status port".
- `/tidb/ddl/all_schema_versions/<ddl_id>` = the loaded version (used by the owner when
  `tidb_enable_metadata_lock=OFF`). It is put at registration and advanced after each reload,
  but only once no in-flight synced statement uses an older version (see "MDL").
- Both keys are bound to one lease with TTL 90 s (Go's `SessionTTL`), kept alive every
  TTL/3. Not registered: `/topology/tidb/...` (TiProxy, TiCDC and Dashboard must not see the
  node), `/tidb/server_id`, `/tidb/server/minstartts`, and no owner campaign of any kind.
- On SIGINT or SIGTERM the node leaves synced mode and revokes the lease before closing
  connections, which deletes both keys.

### Schema domain (background, `SchemaSync::sync_loop`)

- Watches `/tidb/ddl/global_schema_version` (Go's `GlobalVersionCh`) and also runs a *full
  cycle* every 1 s. Go polls every lease/2 = 22.5 s, but our tick is also our health
  heartbeat.
- A full cycle:
  1. Take a fresh TSO and read the *effective* schema version there (ADR 0003: the version
     whose diff exists).
  2. If it is newer than the loaded one, do a **full reload** at that TSO. Schema diffs are
     not applied incrementally.
  3. Register if not registered (see "Joining" below).
  4. Read the MDL table.
  5. Report jobs.
- While a job is held back by an in-flight statement, the registry is re-checked every 50 ms
  (Go's `mdlCheckLookDuration`).

### MDL (same protocol as Go)

This mirrors `pkg/domain/domain.go` `mdlCheckLoop` / `refreshMDLCheckTableInfo` and
`pkg/ddl/schemaver/syncer.go` `UpdateSelfVersion` (audit §3).

1. After a reload, read `SELECT job_id, version, table_ids FROM mysql.tidb_mdl_info` at a
   fresh TSO, through our own planner and executor with the loaded schema. Keep the rows with
   `version <= loaded`. We use `minJobID = 0`: Go's `job_id >= minJobID` filter is only a
   scan optimisation.
2. **In-flight registry** (`Registry`). Every synced statement holds an entry
   `(schema_version, epoch, table_ids)` from admission until its response has been written
   (`SessionHandler::command_done`). Taking the schema and inserting the entry happen under
   one lock, so the following holds: any statement that is *not* in the registry started
   after the newer schema was installed, and uses that schema. The entry covers "all tables"
   until planning narrows it to the plan's table.
3. A job is reported once no entry of the current epoch has `version < job.version` and
   touches one of the job's tables (Go: `RemoveLockDDLJobs`).
4. The report is Go's `PutKVToEtcdMono` on
   `/tidb/ddl/all_schema_by_job_versions/<job_id>/<ddl_id>` = decimal version, with no lease:
   - read the key's mod revision (0 if absent);
   - write in a transaction guarded by `ModRevision == rev`;
   - retry 3 times, 30 ms apart.

   Reported `(job, version)` pairs are cached, and the cache is cleared above 1000 entries,
   as in Go.

The same drain rule (on all tables, since there is no table list) gates advancing
`all_schema_versions/<ddl_id>`. So with MDL off, the owner also cannot run more than one
version ahead of an in-flight statement.

### Statements in synced mode (`Session::synced_read`)

- **Admission** (`Registry::admit`) succeeds only while the node is healthy (see
  "Self-protection"). It returns the newest loaded schema and the registry entry.
- **Point gets at MaxTS.** Autocommit point gets by handle read at `u64::MAX`, at high
  priority and without a TSO. This is the scope of Go's
  `IsPointGetWithPKOrUniqueKeyByAutoCommit`: a handle `PointGet` with nothing on top, not on
  a cached table. Every tidb-rs statement is autocommit, and there are no unique-index point
  gets or batch point gets yet.
- **Other reads** take a TSO (requested after planning) and do no schema-version check.
- **Fencing.** When the read completes, the result is accepted only if the node is still in
  the registration epoch it was admitted in *and* the lease is proven alive for at least
  3 more seconds (`StmtGuard::results_valid`). Otherwise the result is discarded and the
  statement runs again on the ADR 0003 path.
- **Name resolution.** `USE` and `COM_STMT_PREPARE` resolve names against the synced schema
  when it is admissible, and against the schema at a fresh TSO otherwise.

**MaxTS lock semantics** (client-rust `tidb-rs/maxts`, `get_at_max_ts`) follow client-go's
`KVSnapshot.get` with `version == maxTimestamp` together with TiKV's `point_getter` /
`check_ts_conflict_si`:

- TiKV already ignores a lock on the read key when it is the lock's primary and the
  transaction uses neither async commit nor 1PC, because that transaction cannot have
  committed yet.
- Only the *first* lock met blocks. If a retry meets a lock of a different transaction, that
  transaction started after our read, so the read may skip it: its start ts goes into
  `Context.resolved_locks`.
- The first lock is checked with `CheckTxnStatus(caller_start_ts = u64::MAX)`:
  - committed or rolled-back transactions are resolved and the read is retried;
  - a live transaction for which TiKV answers `MinCommitTsPushed` is not committed, so it
    is bypassed (TiKV deliberately does not push `min_commit_ts` for a MaxTS caller);
  - a live async-commit transaction (TiKV answers `NoAction`) is waited for with the lock
    backoff.
- `Timestamp::version()` no longer panics on `from_version(u64::MAX)`.

### Joining, leaving, and self-protection

The node is **healthy**, and may admit statements, only while all of these hold:

- it is registered and its lease is proven alive (keepalive send time + granted TTL) beyond
  a 3 s margin;
- a full cycle (reload check + MDL refresh + reports, all without error) that *started after
  the current registration* completed within `--schema-sync-max-staleness` (default 5 s);
- it has not been behind the newest `global_schema_version` it saw for longer than that
  bound.

When the node becomes unhealthy, statements stop being admitted at once, because
admissibility is evaluated at every statement. A watchdog (every 100 ms) then checks the
deregistration condition: the lease is not proven alive, or the loop has made no progress
for the bound, or the node has lagged for the bound. If the condition holds, the watchdog:

1. starts a new **epoch** under the registry lock, and
2. revokes the lease, so both keys disappear; if the revoke fails, it deletes the keys.

The node registers again only from the sync loop and only after a successful reload, and it
admits statements again only after a full cycle that started after the new registration.

**In-flight statements during deregistration.** Once the epoch has changed, statements
admitted earlier no longer hold back MDL reports (the node is leaving the wait set anyway),
and their results are refused. If such a statement's reads complete after the epoch change,
`results_valid` fails and the statement is re-run on the fallback path. If they completed
before the change, the result is accepted. That is safe: until the revoke, which comes
after the epoch change, the owner was still waiting for this node, so the reads saw at most
the next schema version. A result already accepted may finish streaming after the
deregistration. Its rows were read earlier, so nothing more is read under the old schema.
So deregistration never waits for in-flight statements. That is the point: a stuck node
must never block DDL for longer than the bound.

## Why synced mode is correct

Online DDL (F1) moves an element through states in which any two *adjacent* schema
versions are compatible. The owner commits version `v+1` only after every registered node
has acknowledged `v`. With MDL, a node acknowledges `v` for a job's tables only once no
statement on those tables uses a version older than `v`. A synced statement with schema `v`
therefore runs while the cluster is at `v` or `v+1`, and the data it reads (written by
transactions on `v` or `v+1`) is readable with `v`. ADR 0003 explains why such data is
readable: non-public columns are not read, and origin defaults fill in. This holds whatever
the read timestamp:

- for a TSO read, data at `start_ts` was written under `≤ v+1`;
- for a MaxTS point get, the latest data at read time was written under `≤ v+1`, since the
  owner cannot pass `v+1` while our entry exists.

The owner's wait must actually include the node during the whole statement:

- **Joining.** Statements are admitted only after a cycle that *started after the keys were
  visible*. Any step the owner commits after reading the server list without us committed
  before our registration completed. A TSO taken afterwards (our reload) therefore sees it,
  and statements use at least that version. Every later step re-reads the server list
  (every 1 s) and finds us.
- **Staying.** The lease is checked again when the read completes (fencing above), with a
  margin against expiry. The keepalive deadline is computed from the send time, so it is
  conservative with respect to the etcd server's clock.
- **The first DDL step after a long gap.** If the node falls behind for longer than the
  bound, it deregisters rather than letting the owner pass it.

Results are discarded, not returned, whenever these conditions cannot be shown at the end of
the read. The fallback path is ADR 0003's own snapshot-consistent read, so correctness never
depends on the sync loop being healthy.

## Failure modes (measured on a v8.5.8 playground, 1 PD / 1 TiKV / 1 Go TiDB)

| Event | DDL impact | Statements on the node |
|---|---|---|
| graceful stop (SIGTERM/SIGINT) | node unlisted ~20 ms after the signal; next ADD COLUMN 90 ms (124 ms from the signal) | connections closed after the keys are gone |
| `kill -9` / host loss | every DDL step waits until the lease expires: ADD COLUMN took **91.4 s** | — |
| sync loop stuck (test hook) | node deregisters at the bound (5.04 s default; 2.0 s with `--schema-sync-max-staleness 2`); a DDL started at the stall finished 5.15 s later | served via ADR 0003, correct (new column visible); re-registers ~1 s after recovery |
| reload failing / lagging `global_schema_version` | same bound as above | same |
| etcd unreachable | keepalive fails, so the node is unhealthy once the lease is no longer proven alive beyond the margin; the owner drops the node at lease expiry | ADR 0003 path (needs only TiKV/PD TSO) |
| whole process frozen (no watchdog) | like `kill -9`: until lease expiry (≤ 90 s) | — |

A long synced statement on a table holds back DDL steps on that table until it ends, exactly
like a Go TiDB statement. Statements on other tables do not.

## Results (sysbench oltp_read_only, 1 table × 100k rows, 8 regions, 30 s per run)

Same playground, all runs in one session; `--skip_trx=on --db-ps-mode=auto`. QPS:

| threads | Go TiDB | tidb-rs synced | ratio | tidb-rs `--schema-sync=false` | phase A final (m4a) |
|--:|--:|--:|--:|--:|--:|
| 1 | 5,221 | 6,467 | 1.24 | 4,757 | 4,722 |
| 8 | 20,571 | 22,760 | 1.11 | 15,877 | 15,708 |
| 16 | 27,865 | 27,769 | 1.00 | 20,832 | 20,610 |

p95 (ms per event) synced vs Go: 2.44 / 2.98, 5.43 / 6.00, 8.96 / 9.51. With
`--schema-sync=false`, throughput matches phase A within noise.

Under load: 60 s of t16 sysbench on the synced node while Go TiDB ran ADD COLUMN / DROP COLUMN
on `sbtest1` in a loop gave 27,667 QPS and 0 errors. All 266 DDLs succeeded (p50 198 ms,
max 473 ms).

## User-visible caveats (from the audit)

- While the node is registered, every DDL step also waits for its reload and MDL report.
  An idle ADD/DROP COLUMN pair went from p50 118 ms to 157 ms per DDL (30 DDLs each, the
  node registered vs. not). A crashed node blocks DDL for up to 90 s.
- The node is listed in `information_schema.tidb_servers_info` and `cluster_info`.
  `inspection_result` flags the git hash as inconsistent, and `/info/all` and `/upgrade/show`
  report a version mismatch.
- `CLUSTER_*` memory tables, `cluster_config`, `cluster_load/hardware/systeminfo` and
  `cluster_log` return Go rows plus a warning for this node (it serves no status port).
  `mysql.tidb_mdl_view` cannot show tidb-rs statements that block a DDL.
- `SET GLOBAL tidb_enable_ddl=OFF` on the only Go TiDB is no longer refused, because the
  server count is 2. Don't do it.
- `CALIBRATE RESOURCE` counts the node as one more TiDB.
- Rolling upgrades: the bootstrap and upgrade DDLs on `mysql.*` wait for the node too. If it
  cannot load a newer schema, it lags, deregisters after the bound, and serves via
  ADR 0003. That path fails statements cleanly if the meta is undecodable.

## Not done

- **`/tidb/server/minstartts`.** GC does not know about our reads. That is fine as long as
  no read lasts longer than `tidb_gc_life_time` (10 min by default); tidb-rs statements are
  short autocommit reads.
- **Incremental `SchemaDiff` application.** Every version is a full reload, which is fine
  for small catalogs. Large catalogs need Go's `infoschema.Builder` approach, and the reload
  time also adds to every DDL step.
- **More MaxTS scope.** Unique-index point gets and batch point gets do not exist in tidb-rs
  yet, so they cannot read at MaxTS.
- **Requesting the TSO earlier.** Synced non-point reads request the TSO only after planning.
  The point/non-point decision needs the plan.
- **Diagnostics.** No status port and no `cluster_tidb_trx` rows.
- **Keyspaces (next-gen).** etcd keys are used without the keyspace prefix.
