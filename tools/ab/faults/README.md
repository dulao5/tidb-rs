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

**Exit status:**

- **1**: a read returned a wrong result, or tidb-rs showed an error kind that Go TiDB did not and that `expected.json` does not list. Known gaps go in `expected.json` with a reason, so the run stays green until they are fixed and goes red on anything new.
- **2**: nothing new, but the run is incomplete: a scenario was skipped or could not inject its fault. Each one is printed as `INCOMPLETE:` and listed under `incomplete` in the JSON. A misconfigured runner therefore never reports green.
- **0**: every scenario ran and nothing new was found.

Error kinds seen on **both** sides are not gated, because they are not a parity gap. They are still printed (`BOTH:`) and saved as `errors_on_both`, so a regression on both sides stays visible.

**What is checked:**

- reads of `sbtest1`, which nothing writes, are compared with a snapshot taken before the faults (corrupted or partial results);
- reads of `sbtest.counters`, which a writer updates throughout the run, must not go back in time (stale reads, and reads that miss a committed write). See "Writes" below.

**Requirements:** tiup, the mysql client, `python3-pymysql`, and passwordless `sudo`. Without passwordless sudo, `slow_tikv` (cgroup v2) and `net_cut` (iptables) are skipped and the run exits 2. The other scenarios need no root. `kill_tikv` / `kill_tikv_meta` read the TiKV's command line from `/proc` on Linux and from `ps` / `lsof` elsewhere (macOS); `slow_tikv` and `net_cut` are Linux only.

**Point gets through the store driver:** with `TIDB_RS_POINT_GET=d07.1` in the environment of `run.sh`, tidb-rs (`poc-kv`) does its point gets and batch gets through the ported store driver (`tidb_store::driver`, D07.1 part 1, VS1-18) instead of client-rust directly, so the same scenarios compare the two implementations.

## Read load

Two threads per side and shape, picking random keys:

| shape | SQL | path |
|---|---|---|
| `point-maxts` | `select c … where id = ?` | autocommit point get at MaxTS (no TSO) on both sides |
| `point-tso` | `select max(c) … where id = ?` | point get at the statement's start_ts (tidb-rs: client-rust `Transaction::get`) |
| `range` | `select id, k … where id between ? and ?+9 order by id` | coprocessor table scan |
| `agg` | `select count(*), sum(k) … where id between ? and ?+1999` | coprocessor aggregation |

## Writes

`sbtest.counters` has 8 rows, each in its own region. One Go TiDB connection keeps updating them with autocommit, using values from one increasing sequence:

- odd steps update one row: a 1PC commit;
- even steps update two rows in different regions in one statement: an async commit with a secondary lock.

Both are TiDB v8.5.8 defaults, so commit-path locks are present when TiKV is killed or leaders move. The writer records, for every row, the time each commit returned and its value.

Both sides read the rows (one thread per side and shape):

| shape | SQL |
|---|---|
| `w-point-maxts` | `select v … where id = ?` |
| `w-point-tso` | `select max(v) … where id = ?` |
| `w-scan` | `select id, v … order by id` (all 8 rows) |

**Rule:** a read that started after a commit returned must see at least that commit's value. It also must not see more than the largest value attempted. A scan must also see each two-row commit whole: if one row shows a value written together with another row, that other row must show at least the same value. This catches a snapshot that sees a committed primary but misses its secondary (the lock-resolution case R1–R7 changed). A write whose outcome is unknown (an error) does not raise the floor. A violation counts as a wrong result. The first 20 per scenario are printed as `WRONG:` and saved as `wrong_samples`. `transfer_leader` also moves the leaders of the 8 counter regions.

## Scenarios

| name | fault | recovery |
|---|---|---|
| `split_merge` | `SPLIT TABLE … REGIONS 48` through A; then PD merge settings made aggressive (`max-merge-region-size` 64, `split-merge-interval` 1s, …) | merge settings restored |
| `transfer_leader` | `evict-leader-scheduler` on the store with most leaders for one phase, then `transfer-leader` of 40 of the table's regions (PD HTTP API) | scheduler removed |
| `kill_tikv` | `kill -9` of the TiKV with most leaders | restarted with its original command line after two phases |
| `kill_tikv_meta` | like `kill_tikv`, but first moves the leader of the meta region (the region holding the schema version key, found with PD's `region/key` API) onto that TiKV with a PD `transfer-leader` operator, and waits until PD reports it there. The schema version is unreadable until a new leader is elected, so the schema sync cannot reload (#136). tidb-rs must keep serving on its cached schema within the schema lease (45 s), as Go TiDB does | as `kill_tikv` |
| `slow_tikv` | that TiKV moved into a cgroup v2 limited to 3% of one CPU (`cpu.max 3000 100000`) | moved back |
| `net_cut` | `iptables` drops all TCP to and from that TiKV's port (clients, PD and raft peers alike) | rules removed |
| `pause_tikv` | `SIGSTOP` of that TiKV for 5 s (a hung store: connections stay open, nothing answers) | `SIGCONT` |
| `wide_rows` | not a fault: reads ~6 MiB in one coprocessor response from `sbtest.wide` (100 rows × 64 KiB, created through A), plus a small read of the same table. Reproduces parity P0-3 (4 MiB gRPC receive limit) | — |

Run only the #136 regression (Linux, like `kill_tikv`: it reads `/proc/<pid>/cmdline` to restart the TiKV):

```sh
tools/ab/faults/run.sh -s kill_tikv_meta
```

`-s` picks scenarios (default: all of the above). A phase is `-p` seconds (default 10). The cluster gets half a phase to settle between scenarios.
