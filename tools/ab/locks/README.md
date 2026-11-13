# tools/ab/locks: lock resolution against real locks (#101 step 3)

Leaves real Percolator locks in the A/B cluster with client-go (the version TiDB v8.5.8 uses),
then reads the locked rows through Go TiDB (A) and tidb-rs (B) and compares the results, the
errors, the time taken and the lock-resolution RPCs. Results:
[`specs/D07.1-store/mixed-cluster-locks.md`](../../../specs/D07.1-store/mixed-cluster-locks.md).

```sh
# 1. Cluster and data, as in tools/ab/README.md
scripts/playground.sh start
scripts/sysbench-prepare.sh -t 1 -s 100000 -r 8

# 2. tidb-rs on 27001 (status port 10081)
cargo run --release -p poc-server -- -P 27001 --pd 127.0.0.1:25379 --auto-tls=false

# 3. Build lockgen (Go 1.25) and run the comparison
(cd tools/ab/locks && go build -o /tmp/lockgen ./lockgen)
tools/ab/locks/mixed_locks.py --lockgen /tmp/lockgen --json /tmp/locks.json
```

`lockgen` (Go, `lockgen/main.go`) drives the two-phase commit step by step through client-go's
exported test probes (`StoreProbe`, `TxnProbe`, `CommitterProbe`):

| command | leaves |
|---|---|
| `prewrite -keys … -values … -ttl N` | prewrite locks of a 2PC transaction (the first key is the primary), not committed |
| `… -commit-primary` | the same, with the primary committed (a raw `CommitRequest`), secondaries still locked |
| `… -async` | async-commit prewrite locks of every key, not committed |
| `pessimistic -keys … -hold S` | pessimistic locks of an open transaction, held S seconds |
| `scanlocks -start -end` | prints the locks as TiKV returns them (a raw `ScanLock`: shared lock wrappers stay wrappers) |
| `get -keys …` | prints committed values |

`mixed_locks.py` runs every case on fresh rows, separately for A and B and for each read shape,
so that one reader never resolves the locks the other is about to meet. A locked row's new value
is a copy of row 99999, so every returned row reads NEW (the locking transaction committed) or
OLD. B reads twice; between the two reads `scanlocks` shows whether the locks are gone (the
background cleanup, R2). RPC counts are the `tidb_tikvclient_request_seconds_count` deltas of
each side's `/metrics`, during the statement and including 300 ms after it. The foreign-key case
creates `locktest.fk_parent` / `fk_child` on A and keeps a pessimistic transaction with
`tidb_foreign_key_check_in_shared_lock = ON` open while both sides read the parent rows.

Exit status 1 when A and B disagree on any read.
