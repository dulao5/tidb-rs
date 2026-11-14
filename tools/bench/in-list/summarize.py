#!/usr/bin/env python3
"""Turn bench.py JSON results into a Markdown table (medians of the timed reps).

  python3 tools/bench/in-list/summarize.py results1.json [results2.json ...]

Columns: plan = operators of the EXPLAIN ANALYZE run (ids without the _N suffix, tree glyphs removed);
wall = statement latency seen by the client; load = temporary-table load (multi-row INSERTs of
20,000 rows); TiKV/TiDB CPU = process CPU time consumed during the statement (ps delta);
Process_time / Compile_time = slow-log fields; RPCs = sum of num_rpc over the plan's operators.
"""
import json
import re
import statistics
import sys


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def plan_shape(rows):
    names = []
    for r in rows:
        op = re.sub(r"^[\s│├└─]+", "", r["id"] or "")
        op = re.sub(r"_\d+", "", op)
        names.append(op)
    return " / ".join(names)


def rpcs(rows):
    return sum(int(m) for r in rows for m in re.findall(r"num_rpc:(\d+)", r["execution info"] or ""))


def f(x, nd=3):
    return "-" if x is None else f"{x:.{nd}f}"


def main():
    res = []
    for p in sys.argv[1:]:
        res += json.load(open(p))
    res.sort(key=lambda r: (r["variant"].split("_")[0], r["n"], r["variant"]))
    print("| variant | n | plan | wall s | load s | TiKV CPU s | TiDB CPU s | Process_time s | Compile_time s | RPCs |")
    print("|---|---:|---|---:|---:|---:|---:|---:|---:|---:|")
    for r in res:
        slow = [x["slow"] or {} for x in r["runs"]]
        print(f"| {r['variant']} | {r['n']} | {plan_shape(r['explain_analyze'])} | {f(r['median_wall_s'])} | "
              f"{f(r['median_load_s'])} | {f(r['median_tikv_cpu_s'], 2)} | {f(r['median_tidb_cpu_s'], 2)} | "
              f"{f(med([s.get('Process_time') for s in slow]))} | {f(med([s.get('Compile_time') for s in slow]))} | "
              f"{rpcs(r['explain_analyze'])} |")
    warns = sorted({w for r in res for x in r["runs"] for w in x["warnings"]})
    if warns:
        print("\nWarnings seen:")
        for w in warns:
            print(f"- {w[:200]}")


if __name__ == "__main__":
    main()
