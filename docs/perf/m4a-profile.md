# M4 phase A: oltp_read_only profile of tidb-rs vs Go TiDB

Date: 2026-10-27. Branch `perf/m4a` (from main @0be552b).

## Setup

- Apple M4 Pro, 12 cores. tiup playground v8.5.8 (`scripts/playground.sh start -t maya-perf
  -o 24000`): 1 PD, 1 TiKV, 1 Go TiDB. Another idle playground and other agents shared the
  machine. When idle, the machine was already about 25% busy.
- Data: `scripts/sysbench-prepare.sh -P 28000 -t 1 -s 100000 -r 8` (100k rows, 8 regions).
- Load: `scripts/sysbench-run.sh` (`oltp_read_only --skip_trx=on --db-ps-mode=auto`; one
  event is 10 point selects and 4 range queries). TLS is on for both servers.
- Tools:
  - `tidb-server --stmt-timing SECS` (added on this branch, off by default) logs the average
    time per statement shape and phase.
  - CPU time per process comes from `ps -o time` deltas.
  - Thread stacks come from macOS `sample` (1 ms sampling). samply was installed into the
    workspace toolchain but not needed.
  - TiKV's own latency comes from `tikv_grpc_msg_duration_seconds` on its status port.

## Baseline (main @0be552b)

| threads | Go QPS | tidb-rs QPS | ratio |
|--:|--:|--:|--:|
| 1 | 5,978 | 3,841 | 0.64 |
| 8 | 21,702 | 13,332 | 0.61 |
| 16 | 28,107 | 14,398 | 0.51 |

Phase split, in average µs per statement on the server (`--stmt-timing`). Write = encoding
plus TLS write and flush.

| t | statement | parse/bind | TSO | schema get | plan | KV read | root ops | write | total |
|--:|--|--:|--:|--:|--:|--:|--:|--:|--:|
| 1 | point get | 0.1 | 39 | 84 | 1.2 | 82 | 0.2 | 4 | 211 |
| 1 | range | 0.1 | 39 | 83 | 1.2 | 159 | 6 | 11 | 299 |
| 1 | SUM range | 0.1 | 40 | 84 | 1.7 | 150 | 1 | 4 | 282 |
| 1 | ORDER BY range | 0.1 | 40 | 84 | 1.3 | 155 | 13 | 11 | 305 |
| 1 | DISTINCT range | 0.1 | 40 | 85 | 2.0 | 183 | 36 | 11 | 357 |
| 16 | point get | 0.1 | 101 | 433 | 2.0 | 433 | 0.4 | 7 | 979 |
| 16 | range | 0.1 | 102 | 435 | 2.0 | 579 | 8 | 15 | 1,140 |
| 16 | DISTINCT range | 0.1 | 100 | 435 | 3.5 | 607 | 47 | 14 | 1,208 |

What the baseline shows:

- **Three serial round trips per statement**: TSO, then the schema-version get, then the
  data read. Planning (1-4 µs), root operators (up to about 50 µs for DISTINCT) and
  writing (4-17 µs) are small.
- **At 16 threads, the single gRPC connection to TiKV was the bottleneck.** A TiKV point get
  waited 433 µs on our side, while TiKV's own handling stayed at about 150 µs. Both the
  schema get and the data get were equally slow.
  - In tidb-rs, 27% of the busy thread samples were `__psynch_mutexwait`, almost all on h2's
    connection-level `Streams` mutex (`OpaqueStreamRef::poll_response`, `RecvStream::poll_data`,
    `SendRequest`, the connection task). All streams to the store share one HTTP/2
    connection and its lock.
  - In TiKV, one thread ran at 78% CPU (the gRPC completion-queue thread serving our only
    connection). The other busy threads were at about 31%. With Go TiDB (4 connections plus
    batch commands), TiKV's busy threads were evenly loaded at about 28%.
- **No other lock contention mattered.** The `SchemaCache` mutex, `DagContext` cloning and
  the shared state did not show up in the samples (each under 0.1%). The tokio runtime uses
  the default 12 workers. `TOKIO_WORKER_THREADS=2/4/8` was within noise at t1 and 2-5%
  better at t16, which only reflects the overloaded machine, so the default stays.
- **CPU, not concurrency, limits t16 once the connection limit is gone.** At 16 threads the
  machine is about 99% busy for both servers (about 50% user and 50% system). Throughput is
  then set by the CPU spent per statement across all processes:

| per statement, µs of CPU (t16, final build) | tidb-rs | Go TiDB |
|--|--:|--:|
| SQL node | ~186 | ~163 |
| TiKV | ~212 | ~136 |
| PD | ~37 | ~21 |

  TiKV's extra CPU comes from RPC count. Per 14-statement event, tidb-rs sends 14 TSO
  requests, about 10 + 6 kv_get (data plus shared schema checks) and about 4 coprocessor
  requests. Go sends about 4 TSO requests, about 10 kv_get (MaxTS point gets, no schema read)
  and about 4 coprocessor requests, carried in batch-command streams.
- **The SQL node's CPU is mostly the gRPC client and syscalls, not SQL.** Shares of busy
  samples at t16:
  - h2/hyper/tonic: about 60% inclusive.
  - `writev`, `recvfrom` and `sendto`: about 30% self.
  - rustls on the MySQL side: about 7%.
  - Planning, root operators, row decoding and protocol encoding: about 8% together.

  Plan caching, streaming writes and allocation tuning therefore have little left to win.

## Changes and what each bought

Each change was measured on the same playground, with Go TiDB run in the same session. Runs
took 20-30 s each, and run-to-run noise is about ±2%.

| commit | change | t1 | t8 | t16 |
|--|--|--:|--:|--:|
| (base) | main | 3,841 | 13,332 | 14,398 |
| 1a21a7b | `--stmt-timing` (off by default) | no change | | |
| b4ab7ae, ce05eee | early TSO and optimistic schema check concurrent with the read | 4,659 | (13,718) | 15,447 |
| 019541c | 4 gRPC connections per store (client-rust `tidb-rs/perf`) | 4,575 | 14,614 | 18,352 |
| 6e9e9a2 | concurrent schema checks share version reads | 4,529 | 15,218 | 19,504 |
| final, 30 s compare | | **4,722** | **15,708** | **20,610** |
| Go TiDB, same run | | 5,857 | 20,676 | 27,942 |
| ratio | | **0.81** | **0.76** | **0.74** |

Notes:

- **Early TSO.** The TSO request goes out after parse (COM_QUERY) or at the start of
  COM_STMT_EXECUTE, and is awaited after planning. That overlaps only the 1-3 µs of planning
  and binding. The gain of that commit comes from the concurrent schema check, which saves
  one serial round trip (about 80 µs at t1).
- **Concurrent check and read cost more than one get.** At t1 the pair takes 115 µs where a
  single get took 82 µs. Both RPCs are encoded and decoded on the same task, and TiKV
  serves both.
- **Connections.** 2 and 4 connections perform the same; 8 is slightly worse. 4 is
  client-go's default.
- **Shared version reads.** These cut version reads from 1.0 to about 0.4 per statement at
  t16. At t1 there is nothing to share.
- **Tried and dropped, all within noise at t1 and t16:**
  - Kicking off the TSO future in place instead of `tokio::spawn`.
  - Larger HTTP/2 stream and connection windows (1 GiB).
  - Driving single-region coprocessor requests inline instead of on a spawned worker.

Final phase split for comparison (µs per statement):

| t | statement | TSO | KV read (with schema check) | root ops | write | total |
|--:|--|--:|--:|--:|--:|--:|
| 1 | point get | 42 | 116 | 0.4 | 4 | 164 |
| 1 | range | 42 | 186 | 5 | 11 | 246 |
| 16 | point get | 160 | 446 | 1 | 9 | 620 |
| 16 | range | 161 | 559 | 9 | 17 | 750 |

## What remains, and phase B

The remaining gap is RPC count and per-RPC cost, not SQL work:

1. **TSO for point gets.** This costs 42 µs at t1 and 160 µs at t16, plus PD CPU.
2. **The schema check.** It reads once per statement at t1 and about 0.4 times at t16, which
   costs TiKV CPU at t16 and about 35 µs of latency at t1.
3. **Per-RPC client cost.** Both tonic and h2 do a syscall per RPC, and requests from
   different sessions are never coalesced.

**Phase B estimate (measured, throwaway build, not committed).** Point gets read at MaxTS
(`u64::MAX`) without a TSO and without a schema check; ranges are unchanged. This needs a
one-line fix in client-rust's `Timestamp::version()`, which panics on `u64::MAX`. On the same
playground it ran 6,230 / 20,765 / 25,787 QPS at t1 / t8 / t16:

| | t1 | t8 | t16 |
|--|--:|--:|--:|
| Ratio to Go TiDB | 1.06 | 1.00 | 0.92 |
| Gain over the phase A result | +32% | +32% | +25% |

The real phase B also maintains the schema in the background, which costs a little more.

**After phase B, t16 is about 8% short of Go.** The SQL node then sends the same RPC count as
Go, but each RPC is its own HTTP/2 stream and syscall. client-go's batch commands coalesce
the requests of concurrent sessions into one stream message, which saves syscalls on both
sides. On macOS loopback, syscalls are about half of all CPU. That is the remaining lever. It
is a larger client-rust change (a `BatchCommands` stream per connection with
request/response multiplexing), so it was not started.

Caveat: the playground runs everything on one machine with loopback networking. Syscall and
CPU costs dominate here. On real hardware with network round trips of 100 µs or more, serial
round trips matter more: the phase A changes and phase B weigh more, and batching weighs
somewhat less.
