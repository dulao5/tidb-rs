#!/usr/bin/env python3
"""TiKV fault scenarios under read load: Go TiDB (A) vs tidb-rs (B) (#128 item 2).

A 3-TiKV cluster serves a continuous read load sent to both A and B (point get at MaxTS, point
get at the statement's TSO, range scan, coprocessor aggregation). Each scenario injects one fault
while the load runs, then recovers. For every side, scenario and query shape it records whether
results stay correct (compared with a snapshot taken before any fault), the errors by kind, the
longest stall and the p99 latency. See README.md.

    tools/ab/faults/faults.py --a 127.0.0.1:28000 --b 127.0.0.1:28001 --pd 127.0.0.1:26379 \
        [--scenarios split_merge,transfer_leader,kill_tikv,slow_tikv,net_cut] \
        [--json out.json] [--report out.md] [--expected expected.json]

Exit status: 0 when every result is correct and every error is either also seen on A or listed
in the expected file (known gaps, e.g. the P0 items of specs/D07.1-store/client-go-parity.md);
1 otherwise (a new inconsistency). Faults use sudo (cgroup v2, iptables) and kill -9 one TiKV.
"""

import argparse
import json
import os
import random
import re
import signal
import subprocess
import sys
import threading
import time
import urllib.request

import pymysql

TABLE = "sbtest.sbtest1"
ROWS = 100000

SHAPES = {
    # Autocommit point get: both sides read at MaxTS without a TSO.
    "point-maxts": lambda i: (f"select c from {TABLE} where id = {i}", ("p", i)),
    # The same under max(): read at the statement's start_ts (tidb-rs: Transaction::get).
    "point-tso": lambda i: (f"select max(c) from {TABLE} where id = {i}", ("p", i)),
    # Range scan through the coprocessor.
    "range": lambda i: (f"select id, k from {TABLE} where id between {i} and {i + 9} order by id", ("r", i)),
    # Coprocessor aggregation over ~2,000 rows.
    "agg": lambda i: (f"select count(*), sum(k) from {TABLE} where id between {i} and {i + 1999}", ("a", i)),
}


def connect(endpoint):
    host, port = endpoint.split(":")
    return pymysql.connect(host=host, port=int(port), user="root", autocommit=True,
                           connect_timeout=5, read_timeout=120)


def norm(rows):
    return [tuple("" if v is None else str(v) for v in r) for r in rows]


# ------------------------------------------------------------------ baseline


def baseline(a, keys):
    """The expected result of every query, read from A before any fault."""
    conn = connect(a)
    out = {}
    with conn.cursor() as cur:
        for shape, ids in keys.items():
            for i in ids:
                sql, key = SHAPES[shape](i)
                if key in out:
                    continue
                cur.execute(sql)
                out[key] = norm(cur.fetchall())
    conn.close()
    return out


# ------------------------------------------------------------------ load


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.reset()

    def reset(self):
        with self.lock:
            self.data = {}

    def record(self, side, shape, ok, wrong, err, latency, gap):
        with self.lock:
            d = self.data.setdefault((side, shape), {"ok": 0, "wrong": 0, "errors": {}, "lat": [], "max_stall": 0.0})
            if ok:
                d["ok"] += 1
            if wrong:
                d["wrong"] += 1
            if err:
                d["errors"][err] = d["errors"].get(err, 0) + 1
            d["lat"].append(latency)
            d["max_stall"] = max(d["max_stall"], gap)

    def summary(self):
        with self.lock:
            out = {}
            for (side, shape), d in self.data.items():
                lat = sorted(d["lat"])
                p99 = lat[int(len(lat) * 0.99) - 1] if lat else 0.0
                out.setdefault(side, {})[shape] = {
                    "ok": d["ok"], "wrong": d["wrong"], "errors": d["errors"],
                    "p99_ms": round(p99 * 1000, 1), "max_stall_ms": round(d["max_stall"] * 1000),
                }
            return out


def classify(e):
    code = e.args[0] if getattr(e, "args", None) else None
    msg = str(e.args[1]) if len(getattr(e, "args", ())) > 1 else str(e)
    msg = re.sub(r"\d+", "N", msg)[:90]
    return f"{code}: {msg}"


def worker(side, endpoint, shape, ids, expected, stats, stop):
    conn = None
    last_ok = time.monotonic()
    rnd = random.Random(hash((side, shape)))
    while not stop.is_set():
        i = rnd.choice(ids)
        sql, key = SHAPES[shape](i)
        t0 = time.monotonic()
        try:
            if conn is None:
                conn = connect(endpoint)
            with conn.cursor() as cur:
                cur.execute(sql)
                rows = norm(cur.fetchall())
            now = time.monotonic()
            wrong = rows != expected[key]
            gap = now - last_ok
            last_ok = now
            stats.record(side, shape, not wrong, wrong, None, now - t0, gap)
        except Exception as e:  # noqa: BLE001 - every error is a data point
            now = time.monotonic()
            stats.record(side, shape, False, False, classify(e), now - t0, now - last_ok)
            try:
                conn.close()
            except Exception:  # noqa: BLE001
                pass
            conn = None
            time.sleep(0.05)


# ------------------------------------------------------------------ cluster helpers


def pd_api(pd, path, body=None, method=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"http://{pd}/pd/api/v1/{path}", data=data, method=method or ("POST" if data else "GET"))
    req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=10) as r:
        raw = r.read()
    try:
        return json.loads(raw) if raw else None
    except json.JSONDecodeError:
        return raw.decode()


def stores(pd):
    return {s["store"]["id"]: s for s in pd_api(pd, "stores")["stores"]}


def table_regions(a):
    conn = connect(a)
    with conn.cursor() as cur:
        cur.execute(f"show table {TABLE} regions")
        cols = [c[0] for c in cur.description]
        rows = [dict(zip(cols, r)) for r in cur.fetchall()]
    conn.close()
    return rows


def busiest_store(pd):
    st = stores(pd)
    return max(st, key=lambda i: st[i]["status"].get("leader_count", 0))


def tikv_pid(addr):
    out = subprocess.run(["ps", "-eo", "pid,args"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if "tikv-server" in line and f"--addr={addr}" in line:
            return int(line.split()[0])
    return None


def wait_store_up(pd, store_id, timeout=120):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if stores(pd)[store_id]["store"].get("state_name") == "Up":
                return True
        except Exception:  # noqa: BLE001
            pass
        time.sleep(1)
    return False


def sh(*cmd):
    return subprocess.run(list(cmd), capture_output=True, text=True)


# ------------------------------------------------------------------ scenarios
# Each returns a list of (time offset in s, description) of what it did.


def sc_split_merge(args, log):
    conn = connect(args.a)
    with conn.cursor() as cur:
        cur.execute(f"split table {TABLE} between (1) and ({ROWS + 1}) regions 48")
    conn.close()
    log("split into 48 regions")
    time.sleep(args.phase)
    old = pd_api(args.pd, "config/schedule")
    pd_api(args.pd, "config", {"max-merge-region-size": 64, "max-merge-region-keys": 640000,
                                "split-merge-interval": "1s", "merge-schedule-limit": 32,
                                "patrol-region-interval": "10ms"})
    log("merge enabled aggressively")
    time.sleep(args.phase * 2)
    pd_api(args.pd, "config", {k: old[k] for k in ("max-merge-region-size", "max-merge-region-keys",
                                                     "split-merge-interval", "merge-schedule-limit",
                                                     "patrol-region-interval") if k in old})
    log("merge settings restored")


def sc_transfer_leader(args, log):
    store = busiest_store(args.pd)
    pd_api(args.pd, "schedulers", {"name": "evict-leader-scheduler", "store_id": store})
    log(f"evict-leader-scheduler on store {store}")
    time.sleep(args.phase)
    pd_api(args.pd, f"schedulers/evict-leader-scheduler-{store}", method="DELETE")
    log("evict-leader-scheduler removed")
    # Then single transfers of the table's regions, one per 200 ms.
    st = list(stores(args.pd))
    for n, r in enumerate(table_regions(args.a)[:40]):
        target = st[n % len(st)]
        try:
            pd_api(args.pd, "operators", {"name": "transfer-leader", "region_id": int(r["REGION_ID"]),
                                          "to_store_id": target})
        except Exception:  # noqa: BLE001 - already the leader, etc.
            pass
        time.sleep(0.2)
    log("transfer-leader of 40 regions")
    time.sleep(args.phase / 2)


def sc_kill_tikv(args, log):
    store = busiest_store(args.pd)
    addr = stores(args.pd)[store]["store"]["address"]
    pid = tikv_pid(addr)
    with open(f"/proc/{pid}/cmdline", "rb") as f:
        cmd = [c.decode() for c in f.read().split(b"\0") if c]
    cwd = os.readlink(f"/proc/{pid}/cwd")
    os.kill(pid, signal.SIGKILL)
    log(f"kill -9 tikv store {store} ({addr}, pid {pid})")
    time.sleep(args.phase * 2)
    subprocess.Popen(cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     start_new_session=True)
    up = wait_store_up(args.pd, store)
    log(f"tikv store {store} restarted (up={up})")
    time.sleep(args.phase)


def sc_slow_tikv(args, log):
    store = busiest_store(args.pd)
    addr = stores(args.pd)[store]["store"]["address"]
    pid = tikv_pid(addr)
    cg = "/sys/fs/cgroup/tidbrs-fault"
    orig = open(f"/proc/{pid}/cgroup").read().strip().split("::")[-1]
    sh("sudo", "-n", "sh", "-c", "echo +cpu > /sys/fs/cgroup/cgroup.subtree_control")
    sh("sudo", "-n", "mkdir", "-p", cg)
    sh("sudo", "-n", "sh", "-c", f"echo '{args.slow_quota} 100000' > {cg}/cpu.max")
    sh("sudo", "-n", "sh", "-c", f"echo {pid} > {cg}/cgroup.procs")
    log(f"tikv store {store} ({addr}) limited to {args.slow_quota / 1000:g}% of one CPU")
    time.sleep(args.phase * 2)
    sh("sudo", "-n", "sh", "-c", f"echo {pid} > /sys/fs/cgroup{orig}/cgroup.procs")
    sh("sudo", "-n", "rmdir", cg)
    log("cpu limit removed")
    time.sleep(args.phase)


def sc_net_cut(args, log):
    store = busiest_store(args.pd)
    addr = stores(args.pd)[store]["store"]["address"]
    port = addr.rsplit(":", 1)[1]
    rules = [["INPUT", "-p", "tcp", "--dport", port, "-j", "DROP"],
             ["INPUT", "-p", "tcp", "--sport", port, "-j", "DROP"]]
    for r in rules:
        sh("sudo", "-n", "iptables", "-I", *r)
    log(f"network to tikv store {store} ({addr}) cut")
    time.sleep(args.phase)
    for r in rules:
        sh("sudo", "-n", "iptables", "-D", *r)
    log("network restored")
    time.sleep(args.phase)


def sc_pause_tikv(args, log):
    """SIGSTOP the TiKV with most leaders for 5 s (a stuck process: long GC-like pause, frozen
    disk): requests to it neither fail nor answer. Reproduces P0-2 (2 s RPC timeout)."""
    store = busiest_store(args.pd)
    addr = stores(args.pd)[store]["store"]["address"]
    pid = tikv_pid(addr)
    os.kill(pid, signal.SIGSTOP)
    log(f"SIGSTOP tikv store {store} ({addr}) for 5 s")
    time.sleep(5)
    os.kill(pid, signal.SIGCONT)
    log("SIGCONT")
    time.sleep(args.phase)


def sc_wide_rows(args, log):
    """Not a fault: a table whose coprocessor responses exceed 4 MiB (100 rows of 64 KiB in one
    region). Reproduces P0-3 (client-rust's 4 MiB gRPC receive limit). Read on both sides by
    this scenario itself; the background load keeps running."""
    conn = connect(args.a)
    with conn.cursor() as cur:
        cur.execute("drop table if exists sbtest.wide")
        cur.execute("create table sbtest.wide (id int primary key, v longtext)")
        for i in range(1, 101):
            cur.execute("insert into sbtest.wide values (%s, repeat(char(97 + %s %% 26), 65536))", (i, i))
    conn.close()
    log("sbtest.wide: 100 rows x 64 KiB")
    time.sleep(3)  # let B see the new table
    # A small response from the same table (shows the table itself is readable on both sides).
    sql = "select id from sbtest.wide where id between 1 and 100"
    big = "select v from sbtest.wide where id between 1 and 100"
    expected = None
    for side, ep in (("A", args.a), ("B", args.b)):
        for shape, q in (("wide-small", sql), ("wide-6MiB", big)):
            for _ in range(5):
                t0 = time.monotonic()
                try:
                    c = connect(ep)
                    with c.cursor() as cur:
                        cur.execute(q)
                        rows = norm(cur.fetchall())
                    c.close()
                    if shape == "wide-6MiB":
                        digest = (len(rows), sum(len(r[0]) for r in rows))
                        if expected is None and side == "A":
                            expected = digest
                        wrong = digest != expected
                    else:
                        wrong = len(rows) != 100
                    ARGS_STATS.record(side, shape, not wrong, wrong, None, time.monotonic() - t0, 0.0)
                except Exception as e:  # noqa: BLE001
                    ARGS_STATS.record(side, shape, False, False, classify(e), time.monotonic() - t0, 0.0)
    log("wide reads done")


ARGS_STATS = None

SCENARIOS = {
    "split_merge": sc_split_merge,
    "transfer_leader": sc_transfer_leader,
    "kill_tikv": sc_kill_tikv,
    "slow_tikv": sc_slow_tikv,
    "net_cut": sc_net_cut,
    "pause_tikv": sc_pause_tikv,
    "wide_rows": sc_wide_rows,
}

# ------------------------------------------------------------------ main


def inconsistencies(name, summary, expected):
    """Wrong results anywhere, and error kinds on B that A did not show and that are not known."""
    out = []
    known = expected.get(name, [])
    for side, shapes in summary.items():
        for shape, d in shapes.items():
            if d["wrong"]:
                out.append(f"{name}/{side}/{shape}: {d['wrong']} wrong results")
    a = summary.get("A", {})
    for shape, d in summary.get("B", {}).items():
        a_errors = set(a.get(shape, {}).get("errors", {}))
        for err, n in d["errors"].items():
            if err in a_errors:
                continue
            if any(re.search(k, f"{shape} {err}") for k in known):
                continue
            out.append(f"{name}/B/{shape}: {n} × {err} (not on A, not expected)")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--a", default="127.0.0.1:28000")
    ap.add_argument("--b", default="127.0.0.1:28001")
    ap.add_argument("--pd", default="127.0.0.1:26379")
    ap.add_argument("--scenarios", default=",".join(SCENARIOS))
    ap.add_argument("--threads", type=int, default=2, help="threads per side and shape")
    ap.add_argument("--phase", type=float, default=10, help="seconds per scenario phase")
    ap.add_argument("--slow-quota", type=int, default=3000, help="slow_tikv: cpu.max quota per 100 ms period (us)")
    ap.add_argument("--json")
    ap.add_argument("--expected", help="known inconsistencies: {scenario: [regex on 'shape error'], ...}")
    args = ap.parse_args()
    expected = json.load(open(args.expected)) if args.expected else {}

    rnd = random.Random(1)
    keys = {s: [rnd.randint(1, ROWS - 2000) for _ in range(300)] for s in SHAPES}
    print("taking the baseline from A ...", flush=True)
    exp = baseline(args.a, keys)

    global ARGS_STATS
    stats = Stats()
    ARGS_STATS = stats
    stop = threading.Event()
    threads = []
    for side, ep in (("A", args.a), ("B", args.b)):
        for shape in SHAPES:
            for _ in range(args.threads):
                t = threading.Thread(target=worker, args=(side, ep, shape, keys[shape], exp, stats, stop), daemon=True)
                t.start()
                threads.append(t)

    results = []
    new = []
    try:
        time.sleep(args.phase)
        stats.reset()
        time.sleep(args.phase)
        steady = stats.summary()
        results.append({"scenario": "steady", "events": [], "summary": steady})
        new += inconsistencies("steady", steady, expected)
        print(f"steady: {json.dumps(steady)}", flush=True)
        for name in args.scenarios.split(","):
            events = []
            t0 = time.monotonic()

            def log(msg, events=events, t0=t0):
                events.append((round(time.monotonic() - t0, 1), msg))
                print(f"  [{name} +{events[-1][0]}s] {msg}", flush=True)

            stats.reset()
            try:
                SCENARIOS[name](args, log)
            except Exception as e:  # noqa: BLE001 - report and go on
                log(f"scenario error: {e!r}")
            summary = stats.summary()
            results.append({"scenario": name, "events": events, "summary": summary})
            found = inconsistencies(name, summary, expected)
            new += found
            print(f"{name}: {json.dumps(summary)}", flush=True)
            for f in found:
                print(f"  NEW: {f}", flush=True)
            time.sleep(args.phase / 2)  # let the cluster settle
    finally:
        stop.set()
        for t in threads:
            t.join(timeout=130)
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"results": results, "new_inconsistencies": new}, f, indent=1)
    print(f"{len(new)} new inconsistencies")
    sys.exit(1 if new else 0)


if __name__ == "__main__":
    main()
