# ADR 0008: client-rust (fork) replaces client-go — an L3 same-origin replacement

Status: proposed. Date: 2026-11-13. Decided by: project owner (pending).

## Context

Constitution article 2.3 allows an **L3 same-origin replacement**: a Rust implementation from
the same origin may replace a Go component, provided there is an ADR and comparison tests
against Go. tidb-rs reads from TiKV through `tikv-client`, our fork `dulao5/client-rust`
(branch `tidb-rs/metrics`), instead of porting client-go. This is the largest L3 replacement
in the project:

- the client-go areas involved are about 17k lines of Go (`internal/locate`, `internal/client`,
  `txnkv/txnsnapshot`, `txnkv/txnlock`, `oracle/oracles`, `config/retry`,
  `internal/resourcecontrol`);
- the corresponding client-rust code is about 5.7k lines.

Until now there was no ADR for it. The lock resolver was compared and aligned case by case
(#101: `specs/D07.1-store/lock-resolver.md`, client-rust#1–#9). The rest of the client had
not been compared.

## Decision

1. **client-rust stays the TiKV client of tidb-rs (L3).** It is not ported function by function
   from client-go. It is a same-origin implementation (TiKV project, same protocol, same
   kvproto), already in use, and a port of 17k lines of Go with its goroutine model would mostly
   reproduce the carrier, not business logic.
2. **The contract is client-go's observable behaviour on the read path.** Where they differ,
   client-rust is changed to match client-go c3b5079 (TiDB v8.5.8) unless an exception says
   otherwise. "Observable" here means:
   - which errors reach SQL and when;
   - what is retried and for how long;
   - what is sent to TiKV (request fields, `Context` hints);
   - what is resolved and how.

   Internal structure (caches, task layout, async model) may differ.
3. **The parity table is the work list.** `specs/D07.1-store/client-go-parity.md` lists every
   area with Go's behaviour (file:line), client-rust's status, the gap, the impact and a
   priority. Its three P0 items must be fixed before the D07.1 / D07.2 read paths run against a
   real cluster:
   - **P0-1**: point gets and lock resolution use a fixed ~1.5 s retry budget; ServerIsBusy is
     retried after 2 ms.
   - **P0-2**: every KV RPC has a 2 s timeout.
   - **P0-3**: the gRPC receive limit is 4 MiB.
4. **Comparison tests** (the constitution's requirement):
   - per-item unit tests with the mock KV client, counting RPCs and checking request fields
     against client-go's behaviour (the pattern of client-rust#1–#9);
   - the mixed-cluster read comparison (`tools/ab/locks`, #126), Go TiDB vs tidb-rs on the same
     cluster;
   - the 3-TiKV fault-scenario comparison (#128 item 2: region split / merge, leader transfer,
     store down), which exercises region cache, replica selection and backoff.
5. **Differences kept on purpose** are recorded in the parity table and in the fork's PR
   descriptions, each with its reason. Examples: collapsed resolves share only success
   (client-rust#6); the low-resolution timestamp is refreshed lazily rather than by a
   background loop (client-rust#8). Anything user-visible also goes into
   `docs/compatibility.md`.
6. **Fork maintenance:** changes go to stacked `tidb-rs/*` branches of `dulao5/client-rust`, each
   PR with a client-go cross-reference table and tests, CI on `tidb-rs/*` (client-rust#7). tidb-rs
   pins a commit (`Cargo.toml`) and bumps it in a separate PR. Upstreaming to `tikv/client-rust`
   is welcome but not required.

## Consequences

- The fork carries real work: the parity table's P1 items (time-budgeted backoff, store
  liveness, region cache TTL / `CurrentRegions`, request deadlines) and larger optional projects
  (replica read, BatchCommands, region buckets, the RU interceptor of N10). The ROADMAP counts
  this work separately from the per-function Go port.
- tracker: client-go functions are not counted as "not ported". They are covered by this ADR and
  tracked through the parity table's items.
- Risk: drift from client-go as TiDB upgrades. Mitigation: the parity table cites client-go by
  commit and must be revisited when tidb-rs moves to a newer TiDB version.
