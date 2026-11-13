#!/usr/bin/env python3
"""Mixed-cluster lock-resolution check (dulao5/tidb-rs#101 step 3).

Leaves real locks in a TiKV cluster with `lockgen` (client-go of TiDB v8.5.8), then reads the
locked rows through Go TiDB (A) and tidb-rs (B) and compares what each returns, how long it
takes, and how many lock-resolution RPCs it sends. See README.md next to this file.

    tools/ab/locks/mixed_locks.py [--a 127.0.0.1:27000] [--b 127.0.0.1:27001]
        [--a-status 127.0.0.1:33080] [--b-status 127.0.0.1:10081] [--pd 127.0.0.1:25379]
        [--lockgen PATH] [--report FILE]

Prerequisites: the A/B cluster of tools/ab (scripts/playground.sh start, then
scripts/sysbench-prepare.sh -t 1 -s 100000) and tidb-rs listening on --b.

Every case uses fresh rows, separately for A and B and for each read shape, so that one
reader cannot resolve the locks the other is about to meet. A locked row's "new" value is a
copy of the marker row 99999, so a read shows NEW (the locking transaction is committed) or
OLD (rolled back / not visible) for every row.
"""

import argparse
import json
import re
import subprocess
import sys
import time
import urllib.request

TABLE = "sbtest.sbtest1"
MARKER = 99999

# Each case: lockgen arguments (besides keys/values), the expected outcome for its rows, and
# what it exercises.
CASES = [
    {
        "name": "prewrite-expired",
        "what": "2PC prewrite only, TTL expired: readers roll the transaction back",
        "lock": ["prewrite", "-ttl", "1"],
        "expect": "OLD",
    },
    {
        "name": "commit-primary-only",
        "what": "primary committed, secondary still locked: readers commit the secondary",
        "lock": ["prewrite", "-ttl", "60000", "-commit-primary"],
        "expect": "NEW",
    },
    {
        "name": "prewrite-live",
        "what": "2PC prewrite only, TTL 6 s: readers wait (min_commit_ts pushed or backoff)",
        "lock": ["prewrite", "-ttl", "6000"],
        "expect": "OLD",
    },
    {
        "name": "pessimistic-held",
        "what": "pessimistic locks held by an open transaction: never block reads",
        "lock": ["pessimistic", "-hold", "20"],
        "expect": "OLD",
        "background": True,
    },
    {
        "name": "async-commit-expired",
        "what": "async-commit prewrite of every key, TTL expired: readers recover the commit from the secondaries",
        "lock": ["prewrite", "-ttl", "1", "-async"],
        "expect": "NEW",
    },
]

SHAPES = {
    # The secondary row alone: a point get.
    "point": lambda p, s: f"select id, c from {TABLE} where id = {s}",
    # (No batch point get: tidb-rs's SQL layer does not plan `IN` / `OR` on the handle yet;
    # BatchGet's lock handling is covered by client-rust's unit tests.)
    # A range over both rows: a coprocessor table scan.
    "range": lambda p, s: f"select id, c from {TABLE} where id between {p} and {s} order by id",
}


def row_key(table_id, handle):
    def enc(v):
        return (v ^ (1 << 63)).to_bytes(8, "big").hex()

    return "74" + enc(table_id) + "5f72" + enc(handle)


def mysql(endpoint, sql, timeout=120):
    host, port = endpoint.split(":")
    t0 = time.monotonic()
    p = subprocess.run(
        ["mysql", "-h", host, "-P", port, "-uroot", "-N", "-B", "--ssl-mode=DISABLED", "-e", sql],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    elapsed = time.monotonic() - t0
    if p.returncode != 0:
        err = p.stderr.strip().splitlines()[-1] if p.stderr.strip() else "error"
        m = re.search(r"ERROR (\d+)", err)
        return None, (m.group(1) if m else err), elapsed
    rows = [line.split("\t") for line in p.stdout.splitlines() if line]
    return rows, None, elapsed


def metrics(status):
    """Lock-resolution counters: lock_resolver_actions_total by type, and RPC counts."""
    out = {}
    with urllib.request.urlopen(f"http://{status}/metrics", timeout=10) as r:
        for line in r.read().decode().splitlines():
            m = re.match(r'tidb_tikvclient_lock_resolver_actions_total\{type="([a-z_]+)"\} (\S+)', line)
            if m:
                out["lr." + m.group(1)] = float(m.group(2))
                continue
            m = re.match(r'tidb_tikvclient_request_seconds_count\{(.*)\} (\S+)', line)
            if m and re.search(r'type="(CheckTxnStatus|ResolveLock|CheckSecondaryLocks|PessimisticRollback)"', m.group(1)):
                t = re.search(r'type="([A-Za-z]+)"', m.group(1)).group(1)
                out["rpc." + t] = out.get("rpc." + t, 0.0) + float(m.group(2))
    return out


def delta(before, after):
    keys = ["rpc.CheckTxnStatus", "rpc.ResolveLock", "rpc.CheckSecondaryLocks", "rpc.PessimisticRollback"]
    return {k.split(".", 1)[1]: int(after.get(k, 0) - before.get(k, 0)) for k in keys if after.get(k, 0) - before.get(k, 0)}


class Run:
    def __init__(self, args):
        self.args = args
        rows, err, _ = mysql(
            args.a,
            "select tidb_table_id from information_schema.tables where table_schema='sbtest' and table_name='sbtest1'",
        )
        if err:
            sys.exit(f"cannot read the table id from A: {err}")
        self.table_id = int(rows[0][0])
        marker = self.lockgen(["get", "-keys", row_key(self.table_id, MARKER)]).split()
        self.marker_value = marker[1]
        rows, _, _ = mysql(args.a, f"select c from {TABLE} where id = {MARKER}")
        self.marker_c = rows[0][0]
        # Fresh rows on every run: earlier runs leave committed NEW values behind.
        self.next_row = args.base_row or 20001 + (int(time.time()) % 6000) * 10
        if self.next_row + 400 > MARKER:
            sys.exit("--base-row too high")

    def lockgen(self, argv, background=False):
        cmd = [self.args.lockgen, "-pd", self.args.pd] + argv
        if background:
            return subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if p.returncode != 0:
            sys.exit(f"lockgen {' '.join(argv)} failed: {p.stderr}")
        return p.stdout

    def fresh_rows(self):
        p, s = self.next_row, self.next_row + 1
        self.next_row += 10
        return p, s

    def locks_on(self, p, s):
        out = self.lockgen(["scanlocks", "-start", row_key(self.table_id, p), "-end", row_key(self.table_id, s + 1)])
        return len([l for l in out.splitlines() if l.strip()])

    def outcome(self, rows):
        if rows is None:
            return None
        return [("NEW" if r[1] == self.marker_c else "OLD") for r in rows]

    def case(self, case, side, shape):
        endpoint = self.args.a if side == "A" else self.args.b
        status = self.args.a_status if side == "A" else self.args.b_status
        p, s = self.fresh_rows()
        keys = ",".join(row_key(self.table_id, h) for h in (p, s))
        lock_argv = list(case["lock"]) + ["-keys", keys]
        if lock_argv[0] == "prewrite":
            lock_argv += ["-values", ",".join([self.marker_value] * 2)]
        holder = None
        if case.get("background"):
            holder = self.lockgen(lock_argv, background=True)
            for _ in range(100):
                if self.locks_on(p, s) == 2:
                    break
                time.sleep(0.1)
        else:
            self.lockgen(lock_argv)
            if "-ttl" in lock_argv and lock_argv[lock_argv.index("-ttl") + 1] == "1":
                time.sleep(0.05)  # let the 1 ms TTL lapse on the TSO clock
        locks_before = self.locks_on(p, s)
        sql = SHAPES[shape](p, s)
        reads = []
        for i in range(2 if side == "B" else 1):
            m0 = metrics(status)
            rows, err, elapsed = mysql(endpoint, sql)
            m1 = metrics(status)
            time.sleep(0.3)  # give a background cleanup (R2) time to finish
            m2 = metrics(status)
            reads.append(
                {
                    "outcome": self.outcome(rows),
                    "error": err,
                    "ms": round(elapsed * 1000),
                    # RPCs sent while the statement ran, and including the background
                    # cleanup that finished within 300 ms after it.
                    "rpcs": delta(m0, m1),
                    "rpcs_total": delta(m0, m2),
                    "locks_after": self.locks_on(p, s),
                }
            )
        if holder:
            holder.kill()
        return {"rows": [p, s], "locks_before": locks_before, "reads": reads}


def shared_lock_case(run, shape):
    """`tidb_foreign_key_check_in_shared_lock = ON`: an open pessimistic transaction inserts a
    child row, which locks its parent row in shared mode. Both A and B then read the parent
    rows (spec D07.1 §3.6: TiKV ignores shared locks on reads)."""
    a = run.args.a
    host, port = a.split(":")
    parent = run.next_row  # reuse the counter for distinct parent ids
    run.next_row += 10
    holder = subprocess.Popen(
        ["mysql", "-h", host, "-P", port, "-uroot", "--ssl-mode=DISABLED", "-e",
         "set @@tidb_foreign_key_check_in_shared_lock = ON; begin pessimistic; "
         f"insert into locktest.fk_child values ({parent}, {parent}); "
         f"insert into locktest.fk_child values ({parent + 1}, {parent + 1}); "
         "do sleep(15); rollback;"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    rows, err, _ = mysql(a, "select tidb_table_id from information_schema.tables "
                            "where table_schema='locktest' and table_name='fk_parent'")
    pid = int(rows[0][0])
    start, end = row_key(pid, parent), row_key(pid, parent + 2)
    lock_types = ""
    for _ in range(100):
        lock_types = run.lockgen(["scanlocks", "-start", start, "-end", end])
        if len(lock_types.splitlines()) >= 2:
            break
        time.sleep(0.1)
    sql = {
        "point": f"select id, v from locktest.fk_parent where id = {parent + 1}",
        "range": f"select id, v from locktest.fk_parent where id between {parent} and {parent + 1} order by id",
    }[shape]
    out = {}
    for side, endpoint in (("A", run.args.a), ("B", run.args.b)):
        rows, err, elapsed = mysql(endpoint, sql)
        out[side] = {"rows": rows, "error": err, "ms": round(elapsed * 1000)}
    holder.kill()
    out["locks"] = sorted({l.split()[-1] for l in lock_types.splitlines() if l.strip()})
    return out


def setup_shared_lock_tables(args):
    _, err, _ = mysql(args.a, """
        create database if not exists locktest;
        drop table if exists locktest.fk_child;
        drop table if exists locktest.fk_parent;
        create table locktest.fk_parent (id int primary key, v int);
        create table locktest.fk_child (id int primary key, pid int,
            foreign key (pid) references locktest.fk_parent (id));
        insert into locktest.fk_parent select id, id * 10 from sbtest.sbtest1 where id between 20001 and 90000;
    """)
    if err:
        sys.exit(f"cannot create the shared-lock tables on A: {err}")
    time.sleep(3)  # let B see the new schema


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--a", default="127.0.0.1:27000")
    ap.add_argument("--b", default="127.0.0.1:27001")
    ap.add_argument("--a-status", default="127.0.0.1:33080")
    ap.add_argument("--b-status", default="127.0.0.1:10081")
    ap.add_argument("--pd", default="127.0.0.1:25379")
    ap.add_argument("--lockgen", default="lockgen")
    ap.add_argument("--json", help="write the raw results here")
    ap.add_argument("--base-row", type=int, default=0, help="first row id to lock (default: from the clock)")
    args = ap.parse_args()

    run = Run(args)
    results = []
    mismatches = 0
    for case in CASES:
        for shape in SHAPES:
            a = run.case(case, "A", shape)
            b = run.case(case, "B", shape)
            ra, rb = a["reads"][0], b["reads"][0]
            same = (ra["outcome"], ra["error"]) == (rb["outcome"], rb["error"])
            expect_ok = rb["outcome"] is not None and all(o == case["expect"] for o in rb["outcome"])
            if not same:
                mismatches += 1
            results.append({"case": case["name"], "shape": shape, "A": a, "B": b, "same": same, "expected": expect_ok})
            print(
                f"{case['name']:22} {shape:6} A={ra['outcome'] or ra['error']} {ra['ms']}ms {ra['rpcs']}/{ra['rpcs_total']} | "
                f"B={rb['outcome'] or rb['error']} {rb['ms']}ms {rb['rpcs']}/{rb['rpcs_total']}, 2nd read {b['reads'][1]['ms']}ms "
                f"{b['reads'][1]['rpcs_total']}, locks {b['locks_before']}->{rb['locks_after']}->{b['reads'][1]['locks_after']}"
                f" {'OK' if same else 'MISMATCH'}",
                flush=True,
            )
    setup_shared_lock_tables(args)
    for shape in SHAPES:
        r = shared_lock_case(run, shape)
        same = (r["A"]["rows"], r["A"]["error"]) == (r["B"]["rows"], r["B"]["error"])
        if not same:
            mismatches += 1
        results.append({"case": "fk-shared-lock", "shape": shape, **r, "same": same})
        print(
            f"{'fk-shared-lock':22} {shape:6} locks {r['locks']} A={r['A']['rows'] or r['A']['error']} "
            f"{r['A']['ms']}ms | B={r['B']['rows'] or r['B']['error']} {r['B']['ms']}ms "
            f"{'OK' if same else 'MISMATCH'}",
            flush=True,
        )
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=1)
    print(f"{len(results)} comparisons, {mismatches} mismatches")
    sys.exit(1 if mismatches else 0)


if __name__ == "__main__":
    main()
