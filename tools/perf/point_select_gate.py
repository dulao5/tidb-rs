#!/usr/bin/env python3
"""Performance gate for prepared point selects: tidb-rs vs Go TiDB v8.5.8 (issue 263).

Runs sysbench `oltp_point_select` (server-side prepared statements, autocommit) against Go TiDB
(A) and tidb-rs (B) on the same cluster, and prints new/Go ratios for throughput, p50 / p99
latency and allocations per statement, with the noise rule below. Exit status 0 = every metric
passes, 1 = at least one fails, 2 = the run itself failed.

  tools/perf/point_select_gate.py --go 127.0.0.1:29000 --go-status 127.0.0.1:35080 \\
      --rs-bin target/release/tidb-server --pd 127.0.0.1:27379

Data: `scripts/sysbench-prepare.sh -P <go port> -t 1 -s 10000 -r 4` (1 table x 10 000 rows,
4 regions) beforehand; writes go through Go TiDB.

Method
  * The gate starts tidb-rs itself from --rs-bin (fresh process, `--new-stack=fallback` unless
    --rs-args says otherwise), or uses a running one given by --rs (then no allocation numbers).
  * Go TiDB's instance-level plan cache is switched with SET GLOBAL before the run
    (--go-instance-cache, default ON: the reference the owner chose).
  * One 5 s warm-up per side, then --rounds rounds (default 3). In each round every thread count
    (default 1 and 8) runs A and B for --time seconds (default 20) one after the other; the order
    alternates per round (AB, BA, AB, ...) so slow drift of the machine hits both sides alike.
  * Latency: tools/perf/point_select_lat.lua times every statement (~0.5% buckets); p50 / p99
    come from the histogram merged over all rounds.
  * Allocations per statement (only with --rs-bin):
      Go:      /metrics `go_memstats_mallocs_total` before and after --alloc-events statements on
               one connection, minus the idle rate measured over 5 s just before, / events.
      tidb-rs: jemalloc `nrequests` (all arenas, small + large) printed at exit
               (`_RJEM_MALLOC_CONF=stats_print:true`): one process runs --alloc-events
               statements, a second runs 1 statement and then idles for the same wall time;
               the difference / (events - 1).

Noise rule
  * Each round gives one paired ratio per metric (B/A of that round). The gate value is the ratio
    of the totals over all rounds (QPS: sum B / sum A; latency: percentile of the merged B
    histogram / merged A); allocations are measured once (they are counts, ~1% repeatable).
  * The 95% confidence interval of the per-round ratios (Student t, n-1 degrees of freedom) is
    printed next to it.
  * Verdict by the gate value against the owner's threshold (throughput >= 1.0; p50, p99 and
    allocations <= 1.0). When the interval contains the threshold the verdict is marked
    "within noise": a FAIL within noise is still a FAIL, rerun with more rounds (--rounds 5)
    to decide; a PASS within noise is a PASS.
"""

import argparse
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
LUA = os.path.join(HERE, "point_select_lat.lua")
# Two-sided 95% Student t quantiles by degrees of freedom.
T95 = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365, 8: 2.306, 9: 2.262}


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def split_ep(ep):
    host, port = ep.rsplit(":", 1)
    return host, int(port)


def mysql(ep, sql):
    host, port = split_ep(ep)
    return subprocess.run(
        ["mysql", "-h", host, "-P", str(port), "-u", "root", "-N", "-e", sql],
        check=True, capture_output=True, text=True,
    ).stdout


def wait_up(ep, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            mysql(ep, "SELECT 1")
            return
        except subprocess.CalledProcessError:
            time.sleep(0.5)
    raise RuntimeError(f"{ep} did not answer within {timeout}s")


def sysbench(ep, args, threads, seconds=None, events=None, lat_dir=None):
    host, port = split_ep(ep)
    cmd = ["sysbench", LUA if lat_dir else "oltp_point_select"]
    if lat_dir:
        cmd.append(f"--lat_dir={lat_dir}")
    cmd += [
        f"--mysql-host={host}", f"--mysql-port={port}", "--mysql-user=root",
        f"--mysql-db={args.db}", f"--tables={args.tables}", f"--table-size={args.table_size}",
        "--db-ps-mode=auto", f"--threads={threads}",
    ]
    if events is not None:
        cmd += [f"--events={events}", "--time=0"]
    else:
        cmd += [f"--time={seconds}"]
    cmd.append("run")
    out = subprocess.run(cmd, capture_output=True, text=True)
    text = out.stdout + out.stderr
    m = re.search(r"queries:\s+\d+\s+\(([\d.]+) per sec", text)
    errs = re.search(r"ignored errors:\s+(\d+)", text)
    if out.returncode != 0 or not m or "FATAL" in text:
        raise RuntimeError(f"sysbench against {ep} failed:\n{text[-2000:]}")
    if errs and int(errs.group(1)) > 0:
        raise RuntimeError(f"sysbench against {ep} reported {errs.group(1)} errors")
    return float(m.group(1))


def read_hist(lat_dir):
    h = {}
    for name in os.listdir(lat_dir):
        with open(os.path.join(lat_dir, name)) as f:
            for line in f:
                b, n = line.split()
                h[int(b)] = h.get(int(b), 0) + int(n)
    return h


def merge(hists):
    out = {}
    for h in hists:
        for b, n in h.items():
            out[b] = out.get(b, 0) + n
    return out


def pct(h, q):
    total = sum(h.values())
    acc = 0
    for b in sorted(h):
        acc += h[b]
        if acc >= total * q:
            return math.exp((b + 1) / 200) / 1000.0  # ms
    return float("nan")


def ci(values):
    n = len(values)
    mean = sum(values) / n
    if n < 2:
        return mean, mean
    sd = math.sqrt(sum((v - mean) ** 2 for v in values) / (n - 1))
    half = T95.get(n - 1, 1.96) * sd / math.sqrt(n)
    return mean - half, mean + half


class RsServer:
    """A tidb-rs process started by the gate."""

    def __init__(self, args, port, stats_print=False):
        self.args = args
        self.ep = f"127.0.0.1:{port}"
        env = dict(os.environ)
        env.pop("_RJEM_MALLOC_CONF", None)
        if stats_print:
            env["_RJEM_MALLOC_CONF"] = "stats_print:true"
        self.err = tempfile.TemporaryFile(mode="w+")
        cmd = [args.rs_bin, "--pd", args.pd, "--host", "127.0.0.1", "-P", str(port),
               "--status-port", "0"] + args.rs_args.split()
        self.proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=self.err)
        wait_up(self.ep)

    def stop(self):
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            raise RuntimeError("tidb-rs did not exit within 60s after SIGTERM")
        self.err.seek(0)
        return self.err.read()


def jemalloc_requests(stats):
    """Sum of nrequests of the merged arenas' small and large rows."""
    merged = stats.split("Merged arenas stats:", 1)
    if len(merged) != 2:
        raise RuntimeError("no jemalloc stats in tidb-rs output (stats_print)")
    total = 0
    for kind in ("small:", "large:"):
        m = re.search(rf"^\s*{kind}\s+(\d+)\s+(\d+)\s+\d+\s+(\d+)\s+\d+\s+(\d+)", merged[1], re.M)
        if not m:
            raise RuntimeError(f"cannot parse jemalloc '{kind}' row")
        total += int(m.group(4))  # allocated nmalloc (#/sec) ndalloc (#/sec) nrequests
    return total


def go_mallocs(status):
    with urllib.request.urlopen(f"http://{status}/metrics", timeout=10) as r:
        for line in r.read().decode().splitlines():
            if line.startswith("go_memstats_mallocs_total "):
                return float(line.split()[1])
    raise RuntimeError("go_memstats_mallocs_total not found")


def allocs(args):
    n = args.alloc_events
    # Go: idle rate first, then the statements.
    m0 = go_mallocs(args.go_status)
    time.sleep(5)
    idle_rate = (go_mallocs(args.go_status) - m0) / 5
    m1 = go_mallocs(args.go_status)
    t0 = time.time()
    sysbench(args.go, args, 1, events=n)
    elapsed = time.time() - t0
    go = (go_mallocs(args.go_status) - m1 - idle_rate * elapsed) / n
    # tidb-rs: n statements, then 1 statement + the same idle time.
    srv = RsServer(args, args.rs_port, stats_print=True)
    t0 = time.time()
    sysbench(srv.ep, args, 1, events=n)
    elapsed = time.time() - t0
    big = jemalloc_requests(srv.stop())
    srv = RsServer(args, args.rs_port, stats_print=True)
    sysbench(srv.ep, args, 1, events=1)
    time.sleep(elapsed)
    small = jemalloc_requests(srv.stop())
    return go, (big - small) / (n - 1)


def verdict(value, lo, hi, higher_is_better):
    ok = value >= 1.0 if higher_is_better else value <= 1.0
    noisy = lo <= 1.0 <= hi
    return ("PASS" if ok else "FAIL") + (" (within noise)" if noisy else "")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--go", required=True, help="Go TiDB host:port (A)")
    p.add_argument("--go-status", help="Go TiDB status host:port (allocations)")
    p.add_argument("--rs", help="running tidb-rs host:port (B); or give --rs-bin")
    p.add_argument("--rs-bin", help="tidb-server binary to start (enables allocations)")
    p.add_argument("--rs-port", type=int, default=29101)
    p.add_argument("--rs-args", default="--new-stack=fallback")
    p.add_argument("--pd", help="PD address for --rs-bin")
    p.add_argument("--go-instance-cache", choices=["on", "off", "keep"], default="on")
    p.add_argument("--threads", default="1,8")
    p.add_argument("--time", type=int, default=20)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--alloc-events", type=int, default=100000)
    p.add_argument("--no-allocs", action="store_true")
    p.add_argument("--allocs-only", action="store_true", help="only allocations per statement")
    p.add_argument("--db", default="sbtest")
    p.add_argument("--tables", type=int, default=1)
    p.add_argument("--table-size", type=int, default=10000)
    args = p.parse_args()
    if bool(args.rs) == bool(args.rs_bin):
        p.error("give exactly one of --rs and --rs-bin")
    if args.rs_bin and not args.pd:
        p.error("--rs-bin needs --pd")
    threads = [int(t) for t in args.threads.split(",")]

    if args.go_instance_cache != "keep":
        mysql(args.go, f"SET GLOBAL tidb_enable_instance_plan_cache = {args.go_instance_cache.upper()}")
    if args.allocs_only:
        if not (args.rs_bin and args.go_status):
            p.error("--allocs-only needs --rs-bin and --go-status")
        go, rsa = allocs(args)
        print(f"allocs/stmt: Go {go:.1f}, tidb-rs {rsa:.1f}, new/Go {rsa / go:.3f}")
        return 0
    srv = RsServer(args, args.rs_port) if args.rs_bin else None
    rs = srv.ep if srv else args.rs
    work = tempfile.mkdtemp(prefix="point-select-gate-")
    qps = {(s, t): [] for s in "AB" for t in threads}
    hists = {(s, t): [] for s in "AB" for t in threads}
    try:
        for ep in (args.go, rs):
            sysbench(ep, args, max(threads), seconds=5)
        for r in range(args.rounds):
            order = "AB" if r % 2 == 0 else "BA"
            for t in threads:
                for side in order:
                    ep = args.go if side == "A" else rs
                    lat = os.path.join(work, f"{side}-t{t}-r{r}")
                    os.makedirs(lat)
                    q = sysbench(ep, args, t, seconds=args.time, lat_dir=lat)
                    qps[side, t].append(q)
                    hists[side, t].append(read_hist(lat))
                    log(f"round {r + 1} threads {t:>2} {side}: {q:10.1f} QPS")
    finally:
        if srv:
            srv.stop()
        shutil.rmtree(work, ignore_errors=True)

    alloc = None
    if args.rs_bin and args.go_status and not args.no_allocs:
        alloc = allocs(args)

    failed = False
    print(f"\nGo instance plan cache: {args.go_instance_cache}; {args.rounds} rounds x {args.time}s; "
          f"A = Go {args.go}, B = tidb-rs {rs}")
    print(f"{'threads':>7} {'metric':<11} {'Go':>10} {'tidb-rs':>10} {'new/Go':>7} {'95% CI':>15}  verdict")
    for t in threads:
        a, b = qps["A", t], qps["B", t]
        ratio = sum(b) / sum(a)
        lo, hi = ci([y / x for x, y in zip(a, b)])
        v = verdict(ratio, lo, hi, True)
        failed |= v.startswith("FAIL")
        print(f"{t:>7} {'QPS':<11} {sum(a) / len(a):>10.1f} {sum(b) / len(b):>10.1f} {ratio:>7.3f} "
              f"[{lo:.3f}, {hi:.3f}]  {v}")
        for name, q in (("p50 ms", 0.5), ("p99 ms", 0.99)):
            ga, gb = pct(merge(hists["A", t]), q), pct(merge(hists["B", t]), q)
            per = [pct(hb, q) / pct(ha, q) for ha, hb in zip(hists["A", t], hists["B", t])]
            lo, hi = ci(per)
            v = verdict(gb / ga, lo, hi, False)
            failed |= v.startswith("FAIL")
            print(f"{t:>7} {name:<11} {ga:>10.3f} {gb:>10.3f} {gb / ga:>7.3f} [{lo:.3f}, {hi:.3f}]  {v}")
    if alloc:
        go, rsa = alloc
        v = verdict(rsa / go, rsa / go, rsa / go, False)
        failed |= v.startswith("FAIL")
        print(f"{'1':>7} {'allocs/stmt':<11} {go:>10.1f} {rsa:>10.1f} {rsa / go:>7.3f} {'':>15}  {v}")
    print("\nGATE:", "FAIL" if failed else "PASS")
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError) as e:
        log(f"error: {e}")
        sys.exit(2)
