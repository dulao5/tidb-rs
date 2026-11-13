#!/usr/bin/env python3
"""Relative time of the TPC-H queries on Go TiDB (A) and tidb-rs (B) (weekly build, tpch part).

Runs each query of tools/weekly/tpch/queries.sql `--runs` times on each side, alternating, and
keeps the median. A query B rejects (an error) is timed on A only and reported as such; the
result comparison itself is tidb-ab's (results.jsonl).

    tpch_time.py --queries tools/weekly/tpch/queries.sql --a host:port --b host:port --json out.json
"""

import argparse
import json
import re
import statistics
import time

import pymysql


def queries(path):
    """[(name, sql)] from the `-- Qn` sections (the setup section is skipped)."""
    text = open(path).read()
    text = text[text.index("-- ab:test"):]
    out = []
    for m in re.finditer(r"-- (Q\d+)\n(.*?;)", text, re.S):
        out.append((m.group(1), m.group(2).strip().rstrip(";")))
    return out


def connect(ep, db):
    host, port = ep.split(":")
    return pymysql.connect(host=host, port=int(port), user="root", database=db, autocommit=True,
                           connect_timeout=5, read_timeout=600)


def timed(conn, sql):
    t0 = time.monotonic()
    try:
        with conn.cursor() as cur:
            cur.execute(sql)
            cur.fetchall()
    except pymysql.MySQLError as e:
        return None, f"{e.args[0]}: {str(e.args[1])[:80]}" if len(e.args) > 1 else str(e)
    return time.monotonic() - t0, None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--queries", required=True)
    ap.add_argument("--a", default="127.0.0.1:27000")
    ap.add_argument("--b", default="127.0.0.1:27001")
    ap.add_argument("--db", default="tpch")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--json", required=True)
    args = ap.parse_args()
    a, b = connect(args.a, args.db), connect(args.b, args.db)
    res = {}
    for name, sql in queries(args.queries):
        ta, tb, eb = [], [], None
        for _ in range(args.runs):
            t, e = timed(a, sql)
            if t is not None:
                ta.append(t)
            if eb is None:
                t, eb = timed(b, sql)
                if t is not None:
                    tb.append(t)
        r = {"a_ms": round(statistics.median(ta) * 1000, 1) if ta else None,
             "b_ms": round(statistics.median(tb) * 1000, 1) if tb else None,
             "b_error": eb}
        if r["a_ms"] and r["b_ms"]:
            r["ratio_b_over_a"] = round(r["b_ms"] / r["a_ms"], 2)
        res[name] = r
        print(name, r, flush=True)
    json.dump(res, open(args.json, "w"), indent=1)


if __name__ == "__main__":
    main()
