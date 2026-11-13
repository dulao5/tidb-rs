#!/usr/bin/env python3
"""TiKV fault scenarios under read load: Go TiDB (A) vs tidb-rs (B) (#128 item 2).

A 3-TiKV cluster serves a continuous read load sent to both A and B (point get at MaxTS, point
get at the statement's TSO, range scan, coprocessor aggregation). Each scenario injects one fault
while the load runs, then recovers. For every side, scenario and query shape it records whether
results stay correct (compared with a snapshot taken before any fault), the errors by kind, the
longest stall and the p99 latency. See README.md.

    tools/ab/faults/faults.py --a 127.0.0.1:28000 --b 127.0.0.1:28001 --pd 127.0.0.1:26379 \
        [--scenarios split_merge,transfer_leader,kill_tikv,slow_tikv,net_cut] \
        [--json out.json] [--expected expected.json]

Exit status:
- 1: a new inconsistency, i.e. a wrong result, or an error on B that A did not show and that the
  expected file does not list (known gaps, e.g. the P0 items of
  specs/D07.1-store/client-go-parity.md);
- 2: no new inconsistency, but a scenario was skipped (no passwordless sudo for slow_tikv /
  net_cut) or failed to inject its fault, so the run is incomplete;
- 0: every scenario ran and nothing new was found.
Faults use sudo (cgroup v2, iptables) and kill -9 one TiKV.

Results are checked against static data only: nothing writes during the run, so stale reads and
reads that miss a committed write are not detected (see specs/D07.1-store/fault-scenarios.md).
"""

import argparse
import bisect
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


# Rows written during the run (see Writer). One row per region, so leaders spread over the stores.
COUNTERS = "sbtest.counters"
COUNTER_ROWS = 8

# Reads of the written rows, checked against what had been committed when the read started.
WRITE_SHAPES = {
    "w-point-maxts": lambda r: f"select v from {COUNTERS} where id = {r}",
    "w-point-tso": lambda r: f"select max(v) from {COUNTERS} where id = {r}",
    "w-scan": lambda r: f"select id, v from {COUNTERS} order by id",
}


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


# ------------------------------------------------------------------ writer


def setup_counters(a):
    conn = connect(a)
    with conn.cursor() as cur:
        cur.execute(f"drop table if exists {COUNTERS}")
        cur.execute(f"create table {COUNTERS} (id int primary key, v bigint not null)")
        cur.execute(f"insert into {COUNTERS} values " + ",".join(f"({r}, 0)" for r in range(1, COUNTER_ROWS + 1)))
        cur.execute(f"split table {COUNTERS} by " + ",".join(f"({r})" for r in range(2, COUNTER_ROWS + 1)))
    conn.close()


class Writer:
    """One Go TiDB connection that keeps committing increasing values to the counter rows.

    Values come from one global sequence, so each row's value only grows in commit order (a
    later transaction that meets an earlier one's lock resolves it first). Odd steps update one
    row (1PC); even steps update two rows in different regions in one statement (async commit
    with a secondary), so commit-path locks are in play during the faults. Both are the v8.5.8
    defaults (tidb_enable_1pc, tidb_enable_async_commit).

    For every row it records (time the commit returned, value). A read that started after a
    commit returned must see at least that value; the check is in `check`. A scan must also see
    a two-row commit atomically: if one row shows a value written together with another row,
    that other row must show at least the same value (`check_pairs`), which catches a snapshot
    that sees a committed primary but misses its secondary.
    """

    def __init__(self, endpoint, stats):
        self.endpoint = endpoint
        self.stats = stats
        self.lock = threading.Lock()
        self.commits = {r: ([0.0], [0]) for r in range(1, COUNTER_ROWS + 1)}  # times, values
        self.seq = 0
        self.max_attempted = 0
        self.pair_of = {}  # value -> the two rows written with it in one transaction

    def floor(self, row, started):
        """The value of the last commit to `row` that returned before `started`."""
        with self.lock:
            times, values = self.commits[row]
            return values[bisect.bisect_left(times, started) - 1]

    def check(self, row, value, started):
        """Whether `value`, read from `row` by a read that started at `started`, is allowed."""
        if value is None:
            return False
        value = int(value)
        with self.lock:
            ceiling = self.max_attempted
        return self.floor(row, started) <= value <= ceiling

    def check_pairs(self, rows):
        """For a scan result [(id, v), ...]: every two-row commit is seen whole or not at all."""
        seen = {int(i): int(v) for i, v in rows if v is not None}
        with self.lock:
            for row, value in seen.items():
                pair = self.pair_of.get(value)
                if pair is None:
                    continue
                partner = pair[1] if row == pair[0] else pair[0]
                if seen.get(partner, -1) < value:
                    return False
        return True

    def run(self, stop):
        conn = None
        last_ok = time.monotonic()
        step = 0
        while not stop.is_set():
            step += 1
            r = step % COUNTER_ROWS + 1
            rows = [r] if step % 2 else [r, (r + COUNTER_ROWS // 2 - 1) % COUNTER_ROWS + 1]
            with self.lock:
                self.seq += 1
                v = self.seq
                self.max_attempted = v
                if len(rows) == 2:
                    self.pair_of[v] = tuple(rows)
            ids = ",".join(map(str, rows))
            t0 = time.monotonic()
            try:
                if conn is None:
                    conn = connect(self.endpoint)
                with conn.cursor() as cur:
                    cur.execute(f"update {COUNTERS} set v = {v} where id in ({ids})")
                now = time.monotonic()
                with self.lock:
                    for row in rows:
                        times, values = self.commits[row]
                        times.append(now)
                        values.append(v)
                self.stats.record("W", "write", True, False, None, now - t0, now - last_ok)
                last_ok = now
            except Exception as e:  # noqa: BLE001 - outcome unknown; the floor stays where it was
                now = time.monotonic()
                self.stats.record("W", "write", False, False, classify(e), now - t0, now - last_ok)
                try:
                    conn.close()
                except Exception:  # noqa: BLE001
                    pass
                conn = None
                time.sleep(0.05)


def write_worker(side, endpoint, shape, writer, stats, stop):
    conn = None
    last_ok = time.monotonic()
    rnd = random.Random(hash((side, shape)))
    while not stop.is_set():
        r = rnd.randint(1, COUNTER_ROWS)
        sql = WRITE_SHAPES[shape](r)
        t0 = time.monotonic()
        try:
            if conn is None:
                conn = connect(endpoint)
            with conn.cursor() as cur:
                cur.execute(sql)
                rows = cur.fetchall()
            now = time.monotonic()
            if shape == "w-scan":
                wrong = (len(rows) != COUNTER_ROWS or not all(writer.check(int(i), v, t0) for i, v in rows)
                         or not writer.check_pairs(rows))
            else:
                wrong = len(rows) != 1 or not writer.check(r, rows[0][0], t0)
            if wrong:
                stats.note_wrong(side, shape, f"{sql} -> {rows} (floor at start: "
                                 f"{[writer.floor(i, t0) for i in range(1, COUNTER_ROWS + 1)]})")
            stats.record(side, shape, not wrong, wrong, None, now - t0, now - last_ok)
            last_ok = now
        except Exception as e:  # noqa: BLE001
            now = time.monotonic()
            stats.record(side, shape, False, False, classify(e), now - t0, now - last_ok)
            try:
                conn.close()
            except Exception:  # noqa: BLE001
                pass
            conn = None
            time.sleep(0.05)


# ------------------------------------------------------------------ load


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.reset()

    def reset(self):
        with self.lock:
            self.data = {}
            self.wrong_samples = []

    def note_wrong(self, side, shape, what):
        """Keep the first few wrong results of a scenario for the report."""
        with self.lock:
            if len(self.wrong_samples) < 20:
                self.wrong_samples.append(f"{side}/{shape}: {what}")

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
            if wrong:
                stats.note_wrong(side, shape, f"{sql} -> {rows[:3]} expected {expected[key][:3]}")
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


def table_regions(a, table=TABLE):
    conn = connect(a)
    with conn.cursor() as cur:
        cur.execute(f"show table {table} regions")
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


def wait_readable(endpoint, sql, timeout=60):
    """Wait until `endpoint` can run `sql` (a new table becomes visible after a schema reload)."""
    deadline = time.time() + timeout
    while True:
        try:
            conn = connect(endpoint)
            with conn.cursor() as cur:
                cur.execute(sql)
                cur.fetchall()
            conn.close()
            return
        except Exception:  # noqa: BLE001
            if time.time() > deadline:
                raise
            time.sleep(0.5)


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
    # Then single transfers of the written rows' regions and the table's, one per 200 ms.
    st = list(stores(args.pd))
    for n, r in enumerate(table_regions(args.a, COUNTERS) + table_regions(args.a)[:40]):
        target = st[n % len(st)]
        try:
            pd_api(args.pd, "operators", {"name": "transfer-leader", "region_id": int(r["REGION_ID"]),
                                          "to_store_id": target})
        except Exception:  # noqa: BLE001 - already the leader, etc.
            pass
        time.sleep(0.2)
    log(f"transfer-leader of {COUNTER_ROWS} counter regions and 40 table regions")
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

# Scenarios that need passwordless sudo; without it they are skipped and the run exits 2.
NEEDS_SUDO = {"slow_tikv", "net_cut"}


def have_sudo():
    return subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0

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


def shared_errors(summary):
    """Error kinds seen on both sides: not gated (no parity gap), but reported so that a
    regression on both sides is still visible."""
    a, b = summary.get("A", {}), summary.get("B", {})
    out = []
    for shape, d in b.items():
        a_errors = a.get(shape, {}).get("errors", {})
        for err, n in d["errors"].items():
            if err in a_errors:
                out.append(f"{shape}: {err} (A {a_errors[err]}, B {n})")
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
    setup_counters(args.a)
    wait_readable(args.b, f"select count(*) from {COUNTERS}")

    global ARGS_STATS
    stats = Stats()
    ARGS_STATS = stats
    stop = threading.Event()
    writer = Writer(args.a, stats)
    threads = [threading.Thread(target=writer.run, args=(stop,), daemon=True)]
    for side, ep in (("A", args.a), ("B", args.b)):
        for shape in SHAPES:
            for _ in range(args.threads):
                threads.append(threading.Thread(target=worker, args=(side, ep, shape, keys[shape], exp, stats, stop),
                                                daemon=True))
        for shape in WRITE_SHAPES:
            threads.append(threading.Thread(target=write_worker, args=(side, ep, shape, writer, stats, stop),
                                            daemon=True))
    for t in threads:
        t.start()

    results = []
    new = []
    incomplete = []
    sudo_ok = have_sudo()
    try:
        time.sleep(args.phase)
        stats.reset()
        time.sleep(args.phase)
        steady = stats.summary()
        results.append({"scenario": "steady", "events": [], "summary": steady, "wrong_samples": stats.wrong_samples})
        new += inconsistencies("steady", steady, expected)
        print(f"steady: {json.dumps(steady)}", flush=True)
        for w in stats.wrong_samples:
            print(f"  WRONG: {w}", flush=True)
        for name in args.scenarios.split(","):
            events = []
            t0 = time.monotonic()

            def log(msg, events=events, t0=t0):
                events.append((round(time.monotonic() - t0, 1), msg))
                print(f"  [{name} +{events[-1][0]}s] {msg}", flush=True)

            if name in NEEDS_SUDO and not sudo_ok:
                print(f"{name}: SKIPPED (needs passwordless sudo)", flush=True)
                incomplete.append(f"{name}: skipped, needs passwordless sudo")
                results.append({"scenario": name, "skipped": "needs passwordless sudo"})
                continue
            stats.reset()
            try:
                SCENARIOS[name](args, log)
            except Exception as e:  # noqa: BLE001 - report and go on
                log(f"scenario error: {e!r}")
                incomplete.append(f"{name}: scenario error {e!r}")
            summary = stats.summary()
            both = shared_errors(summary)
            results.append({"scenario": name, "events": events, "summary": summary, "errors_on_both": both,
                            "wrong_samples": stats.wrong_samples})
            found = inconsistencies(name, summary, expected)
            new += found
            print(f"{name}: {json.dumps(summary)}", flush=True)
            for f in found:
                print(f"  NEW: {f}", flush=True)
            for f in both:
                print(f"  BOTH: {f}", flush=True)
            for w in stats.wrong_samples:
                print(f"  WRONG: {w}", flush=True)
            time.sleep(args.phase / 2)  # let the cluster settle
    finally:
        stop.set()
        for t in threads:
            t.join(timeout=130)
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"results": results, "new_inconsistencies": new, "incomplete": incomplete}, f, indent=1)
    print(f"{len(new)} new inconsistencies")
    for i in incomplete:
        print(f"INCOMPLETE: {i}")
    sys.exit(1 if new else 2 if incomplete else 0)


if __name__ == "__main__":
    main()
