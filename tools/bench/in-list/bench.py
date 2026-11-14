#!/usr/bin/env python3
"""Large IN-list benchmark for issue #169 (problem 3), run against Go TiDB v8.5.8.

Measures what a long `IN (c1, ..., cN)` costs on the TiKV side and compares it with
loading the same values into a local temporary table and joining.

  python3 tools/bench/in-list/bench.py setup --port 35690 [--rows 2000000] [--regions 32]
  python3 tools/bench/in-list/bench.py run   --port 35690 [--sizes 1000,10000,100000,600000] [--reps 3]
          [--tikv-pid PID --tidb-pid PID] [--out results.json]

Table `inbench.t(id BIGINT PK CLUSTERED, k BIGINT (non-unique index idx_k), v BIGINT (no index),
pad VARCHAR(32))`, with k = v = id, split into --regions regions (record and index).
The lookup values are a fixed-seed random sample of existing ids (every value hits one row).

Variants (column X in {id = clustered int PK, k = secondary index, v = no index}):
  X_in          SELECT id, pad FROM t WHERE X IN (...)            -- Go's default plan
  X_tmp_<hint>  load values into CREATE TEMPORARY TABLE tmp(id BIGINT PRIMARY KEY), then
                SELECT t.id, t.pad FROM tmp JOIN t ON t.X = tmp.id (default / INL_JOIN / HASH_JOIN)
  X_tmp_subq    SELECT id, pad FROM t WHERE X IN (SELECT id FROM tmp)
--suffix _root is used for a run with `in` in mysql.expr_pushdown_blacklist (run-root.sh): the
IN filter then stays in TiDB instead of being shipped to every coprocessor task.
Per run: client wall time, slow-log metrics of the statement (tidb_slow_log_threshold = 0),
and, if --tikv-pid / --tidb-pid are given, the CPU time the process consumed during the
statement (ps; includes background work, so treat as approximate). One extra untimed
EXPLAIN ANALYZE per (variant, size) records the plan and execution info.
Temporary-table load time is reported separately (load_s) and included in total_s.
"""
import argparse
import json
import random
import statistics
import subprocess
import sys
import time

import pymysql

DB = "inbench"


def connect(a, db=DB):
    return pymysql.connect(host=a.host, port=a.port, user=a.user, password="", database=db,
                           autocommit=True, max_allowed_packet=256 * 1024 * 1024,
                           read_timeout=3600, write_timeout=3600)


def cpu_seconds(pid):
    if not pid:
        return None
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    secs = 0.0
    for part in out.split(":"):
        secs = secs * 60 + float(part)
    return secs


def setup(a):
    c = pymysql.connect(host=a.host, port=a.port, user=a.user, autocommit=True)
    cur = c.cursor()
    cur.execute(f"DROP DATABASE IF EXISTS {DB}")
    cur.execute(f"CREATE DATABASE {DB}")
    cur.execute(f"USE {DB}")
    cur.execute("CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY CLUSTERED, k BIGINT NOT NULL, "
                "v BIGINT NOT NULL, pad VARCHAR(32) NOT NULL, KEY idx_k (k))")
    cur.execute(f"SPLIT TABLE t BETWEEN (1) AND ({a.rows + 1}) REGIONS {a.regions}")
    cur.execute(f"SPLIT TABLE t INDEX idx_k BETWEEN (1) AND ({a.rows + 1}) REGIONS {a.regions}")
    batch = 20000
    t0 = time.time()
    for start in range(1, a.rows + 1, batch):
        end = min(start + batch, a.rows + 1)
        vals = ",".join(f"({i},{i},{i},'pad-{i:026d}')" for i in range(start, end))
        cur.execute("INSERT INTO t VALUES " + vals)
    print(f"loaded {a.rows} rows in {time.time() - t0:.1f}s", file=sys.stderr)
    cur.execute("ANALYZE TABLE t")
    cur.execute("SELECT COUNT(*) FROM information_schema.tikv_region_status WHERE db_name=%s AND table_name='t'", (DB,))
    print("regions:", cur.fetchone()[0], file=sys.stderr)


SLOW_COLS = ["Query_time", "Parse_time", "Compile_time", "Optimize_time", "Cop_time", "Process_time",
             "Wait_time", "Request_count", "Total_keys", "Process_keys", "Cop_proc_avg", "Cop_proc_max",
             "KV_total", "Request_unit_read", "Mem_max", "Plan_digest", "Digest"]


def slow_row(cur, tag):
    # The slow log is flushed asynchronously; poll briefly.
    for _ in range(40):
        cur.execute(f"SELECT {','.join(SLOW_COLS)} FROM information_schema.slow_query "
                    "WHERE Time > NOW() - INTERVAL 30 MINUTE AND Query LIKE %s ORDER BY Time DESC LIMIT 1",
                    (f"/* {tag} */%",))
        r = cur.fetchone()
        if r:
            return dict(zip(SLOW_COLS, r))
        time.sleep(0.25)
    return None


def explain_analyze(cur, sql):
    cur.execute("EXPLAIN ANALYZE " + sql)
    names = [d[0] for d in cur.description]
    rows = []
    for r in cur.fetchall():
        d = dict(zip(names, r))
        rows.append({"id": d.get("id"), "actRows": d.get("actRows"), "task": d.get("task"),
                     "execution info": d.get("execution info")})
    return rows


def load_tmp(cur, ids):
    cur.execute("DROP TEMPORARY TABLE IF EXISTS tmp")
    cur.execute("CREATE TEMPORARY TABLE tmp (id BIGINT NOT NULL PRIMARY KEY)")
    batch = 20000
    for i in range(0, len(ids), batch):
        cur.execute("INSERT INTO tmp VALUES " + ",".join(f"({x})" for x in ids[i:i + batch]))


def variants(col):
    hints = {"tmp_default": "", "tmp_inl": "/*+ INL_JOIN(t) */ ", "tmp_hash": "/*+ HASH_JOIN(tmp, t) */ "}
    out = [(f"{col}_in", False, lambda lst, col=col: f"SELECT id, pad FROM t WHERE {col} IN ({lst})")]
    for name, h in hints.items():
        if col == "v" and name == "tmp_inl":
            continue  # no index on v
        out.append((f"{col}_{name}", True,
                    lambda lst, col=col, h=h: f"SELECT {h}t.id, t.pad FROM tmp JOIN t ON t.{col} = tmp.id"))
    out.append((f"{col}_tmp_subq", True, lambda lst, col=col: f"SELECT id, pad FROM t WHERE {col} IN (SELECT id FROM tmp)"))
    return out


def run(a):
    sizes = [int(s) for s in a.sizes.split(",")]
    cols = a.cols.split(",")
    rng = random.Random(169)
    results = []
    c = connect(a)
    cur = c.cursor()
    cur.execute("SET tidb_slow_log_threshold = 0")
    for n in sizes:
        ids = rng.sample(range(1, a.rows + 1), n)
        lst = ",".join(map(str, ids))
        for col in cols:
            for name, needs_tmp, mk in variants(col):
                if a.only and not any(name.startswith(p) for p in a.only.split(",")):
                    continue
                sql = mk(lst)
                name = name + a.suffix
                runs = []
                for rep in range(a.reps):
                    tag = f"bench:{name}:{n}:{rep}:{int(time.time() * 1000)}"
                    load_s = 0.0
                    if needs_tmp:
                        t0 = time.time()
                        load_tmp(cur, ids)
                        load_s = time.time() - t0
                    kv0, db0 = cpu_seconds(a.tikv_pid), cpu_seconds(a.tidb_pid)
                    t0 = time.time()
                    cur.execute(f"/* {tag} */ " + sql)
                    nrows = len(cur.fetchall())
                    wall = time.time() - t0
                    kv1, db1 = cpu_seconds(a.tikv_pid), cpu_seconds(a.tidb_pid)
                    cur.execute("SHOW WARNINGS")
                    warns = [w[2] for w in cur.fetchall()]
                    slow = slow_row(cur, tag)
                    runs.append({"rep": rep, "rows": nrows, "wall_s": wall, "load_s": load_s,
                                 "total_s": wall + load_s,
                                 "tikv_cpu_s": None if kv0 is None else kv1 - kv0,
                                 "tidb_cpu_s": None if db0 is None else db1 - db0,
                                 "warnings": warns, "slow": slow})
                    print(f"{name:16s} n={n:7d} rep={rep} rows={nrows} wall={wall:.3f}s load={load_s:.3f}s "
                          f"tikv_cpu={runs[-1]['tikv_cpu_s']} tidb_cpu={runs[-1]['tidb_cpu_s']} "
                          f"proc={slow and slow['Process_time']} "
                          f"warn={warns[:1]}", file=sys.stderr, flush=True)
                if needs_tmp:
                    load_tmp(cur, ids)
                plan = explain_analyze(cur, sql)
                def med(k):
                    xs = [r[k] for r in runs if r[k] is not None]
                    return statistics.median(xs) if xs else None
                results.append({"variant": name, "n": n, "median_wall_s": med("wall_s"),
                                "median_total_s": med("total_s"), "median_load_s": med("load_s"),
                                "median_tikv_cpu_s": med("tikv_cpu_s"), "median_tidb_cpu_s": med("tidb_cpu_s"),
                                "runs": runs, "explain_analyze": plan})
                if a.out:
                    with open(a.out, "w") as f:
                        json.dump(results, f, indent=1, default=str)
    cur.execute("DROP TEMPORARY TABLE IF EXISTS tmp")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("cmd", choices=["setup", "run"])
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=35690)
    p.add_argument("--user", default="root")
    p.add_argument("--rows", type=int, default=2_000_000)
    p.add_argument("--regions", type=int, default=32)
    p.add_argument("--sizes", default="1000,10000,100000,600000")
    p.add_argument("--cols", default="id,k,v")
    p.add_argument("--only", default="", help="comma-separated variant-name prefixes")
    p.add_argument("--reps", type=int, default=3)
    p.add_argument("--suffix", default="", help="appended to variant names (e.g. _root when 'in' is in "
                   "mysql.expr_pushdown_blacklist)")
    p.add_argument("--tikv-pid", type=int)
    p.add_argument("--tidb-pid", type=int)
    p.add_argument("--out")
    a = p.parse_args()
    setup(a) if a.cmd == "setup" else run(a)


if __name__ == "__main__":
    main()
