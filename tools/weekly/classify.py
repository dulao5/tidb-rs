#!/usr/bin/env python3
"""Per-operator agreement of a tidb-ab run (weekly report, #128).

Reads tidb-ab's results.jsonl (every compared statement and its verdict), asks Go TiDB (A) for
the plan of every text-protocol statement (EXPLAIN FORMAT='brief'), and counts the
verdicts per operator (TableReader, Point_Get, HashAgg, ...) and per template. A statement that
is not a query (SHOW, SET, ...) is classified by its first keyword.

    classify.py --results target/ab/results.jsonl --a 127.0.0.1:27000 --json out.json
"""

import argparse
import json
import re
from collections import defaultdict

import pymysql

VERDICTS = ("match", "mismatch", "unsupported", "both_error")


def operators(conn, query):
    """The operator names in Go TiDB's plan of `query`, or its first keyword."""
    head = query.lstrip().split(None, 1)[0].upper() if query.strip() else "?"
    if head not in ("SELECT", "WITH", "TABLE", "VALUES", "(") and not query.lstrip().startswith("("):
        return [head]
    try:
        with conn.cursor() as cur:
            cur.execute("EXPLAIN FORMAT='brief' " + query)
            rows = cur.fetchall()
    except Exception:  # noqa: BLE001 - not explainable (e.g. depends on session state)
        return ["(no plan)"]
    ops = []
    for r in rows:
        name = re.sub(r"^[\s│├└─]+", "", str(r[0]))
        name = re.sub(r"_\d+$", "", name.split("(")[0].strip())
        if name and name not in ops:
            ops.append(name)
    return ops or ["(no plan)"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True, nargs="+")
    ap.add_argument("--a", default="127.0.0.1:27000")
    ap.add_argument("--json", required=True)
    args = ap.parse_args()

    records = []
    for path in args.results:
        with open(path) as f:
            records += [json.loads(line) for line in f if line.strip()]

    # Each text-protocol statement is explained on A. A prepared statement uses the plan of the
    # same statement text, or else of its template's first text statement (sysbench templates
    # bind parameters; file workloads share one template name across statements).
    host, port = args.a.split(":")
    conn = pymysql.connect(host=host, port=int(port), user="root", autocommit=True,
                           database="sbtest", connect_timeout=5, read_timeout=60)
    ops_of_query, sample = {}, {}
    for r in records:
        if r["protocol"] == "text":
            sample.setdefault(r["template"], r["query"])
            if r["query"] not in ops_of_query:
                ops_of_query[r["query"]] = operators(conn, r["query"])
    conn.close()

    def ops(r):
        if r["query"] in ops_of_query:
            return ops_of_query[r["query"]]
        if r["template"] in sample:
            return ops_of_query[sample[r["template"]]]
        return ["(no text query)"]

    per_op = defaultdict(lambda: dict.fromkeys(VERDICTS, 0))
    per_template = defaultdict(lambda: dict.fromkeys(VERDICTS, 0))
    for r in records:
        v = r["verdict"]
        per_template[r["template"]][v] += 1
        for op in ops(r):
            per_op[op][v] += 1

    out = {
        "statements": len(records),
        "totals": {v: sum(1 for r in records if r["verdict"] == v) for v in VERDICTS},
        "operators": dict(sorted(per_op.items())),
        "templates": dict(sorted(per_template.items())),
    }
    with open(args.json, "w") as f:
        json.dump(out, f, indent=1)


if __name__ == "__main__":
    main()
