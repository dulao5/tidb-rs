# tools/ab/faults: TiKV fault scenarios under read load (#128 item 2)

Go TiDB (A) and tidb-rs (B) read the same 3-TiKV cluster while one fault at a time is injected.
For each side, scenario and read shape the harness records:

- whether every result matches a snapshot taken before any fault;
- the errors, by kind;
- the longest stall (the longest gap between two successful reads of one thread);
- the p99 latency.

Results and findings: [`specs/D07.1-store/fault-scenarios.md`](../../../specs/D07.1-store/fault-scenarios.md).

```sh
tools/ab/faults/run.sh                 # cluster + data + tidb-rs (cargo build) + all scenarios
tools/ab/faults/run.sh -b path/to/tidb-server -s kill_tikv,net_cut -p 10 -k
```

`run.sh` is the single entry point, meant for the weekly build:

1. It starts (or reuses) a tiup playground v8.5.8 with 1 PD, 3 TiKV and Go TiDB. The tag is `maya-faults` with port offset 24000: Go TiDB is on 28000 and PD on 26379. `scripts/playground.sh` reads `PLAYGROUND_KV`.
2. It loads `sbtest.sbtest1` (100k rows, 8 regions).
3. It starts tidb-rs on 28001 and runs `faults.py`.
4. It writes `target/faults/<timestamp>.json` / `.log`.
5. It cleans the cluster unless `-k` is given.

**Exit status:** 1 if any read returned a wrong result, or if tidb-rs showed an error kind that Go TiDB did not and that `expected.json` does not list. Known gaps go in `expected.json` with a reason, so the run stays green until they are fixed and goes red on anything new.

**Requirements:** tiup, the mysql client, `python3-pymysql`, and passwordless `sudo` for cgroup v2 and iptables.

## Read load

Two threads per side and shape, picking random keys:

| shape | SQL | path |
|---|---|---|
| `point-maxts` | `select c … where id = ?` | autocommit point get at MaxTS (no TSO) on both sides |
| `point-tso` | `select max(c) … where id = ?` | point get at the statement's start_ts (tidb-rs: client-rust `Transaction::get`) |
| `range` | `select id, k … where id between ? and ?+9 order by id` | coprocessor table scan |
| `agg` | `select count(*), sum(k) … where id between ? and ?+1999` | coprocessor aggregation |

## Scenarios

| name | fault | recovery |
|---|---|---|
| `split_merge` | `SPLIT TABLE … REGIONS 48` through A; then PD merge settings made aggressive (`max-merge-region-size` 64, `split-merge-interval` 1s, …) | merge settings restored |
| `transfer_leader` | `evict-leader-scheduler` on the store with most leaders for one phase, then `transfer-leader` of 40 of the table's regions (PD HTTP API) | scheduler removed |
| `kill_tikv` | `kill -9` of the TiKV with most leaders | restarted with its original command line after two phases |
| `slow_tikv` | that TiKV moved into a cgroup v2 limited to 3% of one CPU (`cpu.max 3000 100000`) | moved back |
| `net_cut` | `iptables` drops all TCP to and from that TiKV's port (clients, PD and raft peers alike) | rules removed |
| `pause_tikv` | `SIGSTOP` of that TiKV for 5 s (a hung store: connections stay open, nothing answers) | `SIGCONT` |
| `wide_rows` | not a fault: reads ~6 MiB in one coprocessor response from `sbtest.wide` (100 rows × 64 KiB, created through A), plus a small read of the same table. Reproduces parity P0-3 (4 MiB gRPC receive limit) | — |

`-s` picks scenarios (default: all of the above). A phase is `-p` seconds (default 10). The cluster gets half a phase to settle between scenarios.
