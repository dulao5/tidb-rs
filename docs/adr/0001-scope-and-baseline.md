# ADR 0001: Scope, baseline and core technical choices

- Status: accepted (2026-10-22, decided by zhigang-du)
- Context: feasibility report `reports/rust-tidb-feasibility.md`; spikes `spike-parser`, `spike-query-core`.

## Decisions

1. **Baseline**: TiDB master `4cee7f2084` (2026-10-16). Upstream changes after this commit
   are tracked as a diff set and fed back into the task DB.
2. **Positioning**: the first deliverable is a *read-only, autocommit* SQL node that joins an
   existing TiDB cluster. DDL, writes, ANALYZE and GC stay with Go TiDB. Results can therefore be
   compared against Go TiDB on the same data from day one.
3. **Parser**: mechanical port of `pkg/parser/parser.y` to a Rust LR generator (grmtools).
   The spike showed the generated automaton is isomorphic to goyacc's (5,683 states, 0 diffs).
   CI must keep the table comparison against goyacc as an oracle.
4. **Expression/types/codec**: reuse TiKV's `tidb_query_*` crates instead of re-porting TiDB's
   Go code, so root-side and pushed-down evaluation share one implementation. Short term:
   a local TiKV branch with stable-compatible changes; whether to upstream is undecided.

## Defaults (pending explicit decision)

- Toolchain: stable Rust; gRPC: grpcio (tipb already binds grpcio + rust-protobuf 2.8).
- Task DB: SQLite (`projects/testdb`), schema portable to TiDB.
- Repo hosting: local until a remote is chosen.

## Correctness requirements for the read-only node (P1 acceptance)

- Participate in schema-version sync (`all_schema_versions` in etcd) or validate the schema
  version per statement; never serve reads with a schema older than the DDL owner allows.
- Resolve locks encountered on the read path (incl. async-commit / 1PC transactions).
- Obtain `start_ts` from PD TSO per statement.
