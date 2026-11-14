#!/usr/bin/env python3
"""Plan-cache / digest behaviour of IN lists of different lengths on Go TiDB (problems 1 and 2 of #169).

  python3 tools/bench/in-list/plancache_probe.py --port 35690 --status-port 41770

Uses the table of bench.py (inbench.t). Turns on the instance plan cache and the non-prepared plan
cache (GLOBAL; restored at the end), then for each list length n:
  prepared:     PREPARE 'SELECT pad FROM t WHERE <col> IN (?,..,?)'; EXECUTE twice with different values
  non-prepared: the literal statement twice with different values
and records @@last_plan_from_cache of the second execution, the skip-plan-cache warning of the
first, the EXPLAIN plan root, and statements_summary digests. After each (mode, column) group it waits
16 s (the metric tidb_server_plan_cache_instance_plan_num_total is refreshed every 15 s,
domain.go planCacheMetricsAndVars) and reports how many instance plan-cache entries the group added.
"""
import argparse
import random
import re
import time
import urllib.request

import pymysql

LENGTHS = [1, 2, 3, 4, 5, 10, 50, 100, 200, 201, 1000]


def plan_num(status_port):
    txt = urllib.request.urlopen(f"http://127.0.0.1:{status_port}/metrics").read().decode()
    m = re.search(r'tidb_server_plan_cache_instance_plan_num_total\{type=" instance-plan-cache"\} (\S+)', txt)
    return float(m.group(1)) if m else None


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=35690)
    p.add_argument("--status-port", type=int, default=41770)
    a = p.parse_args()
    root = pymysql.connect(host=a.host, port=a.port, user="root", database="inbench", autocommit=True)
    rc = root.cursor()
    rc.execute("SET GLOBAL tidb_enable_instance_plan_cache = ON")
    rc.execute("SET GLOBAL tidb_enable_non_prepared_plan_cache = ON")
    rc.execute("SET GLOBAL tidb_enable_stmt_summary = OFF")
    rc.execute("SET GLOBAL tidb_enable_stmt_summary = ON")  # clears statements_summary
    c = pymysql.connect(host=a.host, port=a.port, user="root", database="inbench", autocommit=True)
    cur = c.cursor()
    rng = random.Random(7)

    def vals(n):
        return rng.sample(range(1, 2_000_000), n)

    def warn():
        cur.execute("SHOW WARNINGS")
        return "; ".join(w[2] for w in cur.fetchall())

    print("| mode | col | n | 2nd exec from cache | skip reason (1st exec) | plan root |")
    print("|---|---|---:|---|---|---|")
    groups = []

    def settle():
        time.sleep(16)
        return plan_num(a.status_port)

    before = settle()
    for col in ["id", "k"]:
        for n in LENGTHS:
            marks = ",".join("?" * n)
            cur.execute(f"PREPARE s FROM 'SELECT pad FROM t WHERE {col} IN ({marks})'")
            names = [f"@p{i}" for i in range(n)]
            for attempt in range(2):
                cur.execute("SET " + ",".join(f"{nm}={v}" for nm, v in zip(names, vals(n))))
                cur.execute("EXECUTE s USING " + ",".join(names))
                cur.fetchall()
                if attempt == 0:
                    w = warn()
            cur.execute("SELECT @@last_plan_from_cache")
            hit = cur.fetchone()[0]
            cur.execute(f"EXPLAIN SELECT pad FROM t WHERE {col} IN ({','.join(map(str, vals(n)))})")
            root_op = cur.fetchall()[0][0]
            print(f"| prepared | {col} | {n} | {hit} | {w or '-'} | {root_op} |")
            cur.execute("DEALLOCATE PREPARE s")
        after = settle()
        groups.append(("prepared", col, after - before))
        before = after
        for n in LENGTHS:
            for attempt in range(2):
                cur.execute(f"SELECT pad FROM t WHERE {col} IN ({','.join(map(str, vals(n)))})")
                cur.fetchall()
                if attempt == 0:
                    w = warn()
            cur.execute("SELECT @@last_plan_from_cache")
            hit = cur.fetchone()[0]
            print(f"| non-prepared | {col} | {n} | {hit} | {w or '-'} | |")
        after = settle()
        groups.append(("non-prepared", col, after - before))
        before = after
    print(f"\n| mode | col | instance plan-cache entries added ({len(LENGTHS)} lengths) |")
    print("|---|---|---:|")
    for g in groups:
        print(f"| {g[0]} | {g[1]} | {g[2]:.0f} |")
    rc.execute("SELECT DIGEST_TEXT, COUNT(*), SUM(EXEC_COUNT), COUNT(DISTINCT PLAN_DIGEST), SUM(PLAN_CACHE_HITS) "
               "FROM information_schema.statements_summary WHERE SCHEMA_NAME='inbench' AND DIGEST_TEXT LIKE 'select `pad`%' "
               "GROUP BY DIGEST_TEXT ORDER BY 1")
    print("\n| statements_summary digest text | rows | exec count | distinct plan digests | plan cache hits |")
    print("|---|---:|---:|---:|---:|")
    for r in rc.fetchall():
        print(f"| `{r[0]}` | {r[1]} | {r[2]} | {r[3]} | {r[4]} |")
    rc.execute("SET GLOBAL tidb_enable_instance_plan_cache = OFF")
    rc.execute("SET GLOBAL tidb_enable_non_prepared_plan_cache = OFF")


if __name__ == "__main__":
    main()
