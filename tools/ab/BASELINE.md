# Baseline: Go TiDB on the A/B playground

Reference numbers for `scripts/sysbench-run.sh`. tidb-rs runs are compared against a fresh A run
on the same machine and cluster, never against this file alone: the numbers move with whatever
else the machine is doing.

## Setup (2026-10-26)

- Machine: Apple M4 Pro, 12 cores, 48 GiB. A second, idle playground (another agent's) was up on
  the same machine.
- Cluster: `scripts/playground.sh start` (tiup playground v8.5.8: 1 PD, 1 TiKV, 1 TiDB, no
  TiFlash, no monitor; Go TiDB with auto-TLS).
- Data: `scripts/sysbench-prepare.sh -t 1 -s 100000 -r 8` (sbtest1, ids 1..100000, 8 record
  regions, analyzed).
- Run: `scripts/sysbench-run.sh run -e 127.0.0.1:27000 -n go-tidb -T "1 8 16" -d 30`, i.e.
  `oltp_read_only --skip_trx=on --db-ps-mode=auto`, 30 s per thread count. One event = 10 point
  selects + 4 range queries.

## Go TiDB v8.5.8

| threads | QPS | avg ms / event | p95 ms / event | errors |
|--:|--:|--:|--:|--:|
| 1 | 5,665 | 2.47 | 2.81 | 0 |
| 8 | 21,715 | 5.16 | 6.00 | 0 |
| 16 | 28,211 | 7.94 | 9.32 | 0 |

## Harness self-check (same day)

Both sides pointed at Go TiDB (`-a 127.0.0.1:27000 -b 127.0.0.1:27000`):

- `sysbench -n 200`: 2,372 comparisons (5 templates x text/prepared, including edge cases and
  every region boundary), 0 mismatches.
- `file tools/ab/workloads/constants.sql`: 48 comparisons, 0 mismatches (2 `both_error`).

Against the old protocol stub as B, the same constants file reports 22 mismatches (for example
`SELECT 'abc'`: column length 12 vs 1024, decimals 31 vs 0), so metadata differences are caught.
