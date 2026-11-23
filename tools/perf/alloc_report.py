#!/usr/bin/env python3
"""Summarise an allocation profile written by tools/perf/allocprof.rs.

  tools/perf/alloc_report.py <stacks.txt> <statements> [top]

Prints allocations (+ reallocations) per statement grouped three ways:
  1. by area: the outermost tidb-rs function of a known statement step, or the crate of the
     innermost non-std frame when the stack has no tidb-rs frame (TiKV client, gRPC tasks);
  2. by the innermost tidb-rs call site (file:line) with its two most common caller chains;
  3. by the innermost tidb-rs function (file).
"""

import collections
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/"
STEPS = [
    "checkpoint", "restore", "parse_binary_params", "execute_prepared_stmt", "compile",
    "point_get", "next", "write_column_info", "write_result_set", "dump_binary_row",
    "reset_context_of_stmt", "finish_stmt", "close", "on_stmt_start", "get_stmt_read_ts",
    "set_process_info", "planner_optimize", "optimize", "new_eval_context", "columns",
]
# Areas: the first rule whose pattern occurs anywhere in the stack (function names) wins.
AREAS = [
    ("TiKV client / gRPC (inside client-rust, tonic, h2, hyper)", ("tikv_client", "tonic::", "tonic[", "h2::", "h2[", "hyper", "tower")),
    ("router checkpoint (PoC fallback glue)", ("checkpoint",)),
    ("eval context built per use (new_eval_context)", ("new_eval_context",)),
    ("result column metadata (fields / write_column_info)", ("col_names2_result_fields", "write_column_info", "exec_result", "::column::")),
    ("system variable lookups", ("get_sys_var", "get_system_var", "get_session_or_global_system_var", "get_charset_info")),
    ("plan cache lookup / planner", ("planner_optimize", "get_plan_from_plan_cache", "plan_cache")),
    ("statement context reset", ("reset_context_of_stmt", "new_stmt_ctx")),
    ("point get executor + snapshot", ("point_get", "PointGet", "snapshot")),
    ("transaction / session flow", ("txnmanager", "on_stmt_start", "get_stmt_read_ts", "finish_stmt", "sessiontxn")),
    ("result row encoding", ("dump_binary_row", "write_result_set", "result_encoder")),
    ("MySQL protocol read/write", ("mysql_protocol",)),
]
NOISE = {"clone", "poll", "to_owned", "to_string", "from", "into", "{closure#0}", "{closure#1}",
         "{async_fn#0}", "{async_block#0}", "new", "default", "collect", "extend", "push"}


def parse(path):
    stacks, cur = [], None
    for line in open(path):
        if line.startswith("@ "):
            a, r = map(int, line.split()[1:3])
            cur = [a + r, []]
            stacks.append(cur)
        elif cur is not None:
            cur[1].append(line.strip())
    return stacks


def loc(f):
    m = re.search(r"\[(.*)\]$", f)
    return m.group(1) if m else ""


def name(f):
    return re.sub(r"<.*", "", re.sub(r"\s+\[.*\]$", "", f))


def repo_frames(fs):
    out = []
    for f in fs:
        l = loc(f)
        if l.startswith(ROOT) and "/poc/server/src/allocprof.rs" not in l and "/poc/server/src/main.rs" not in l:
            out.append((name(f), l[len(ROOT):]))
    return out


def dep_crate(fs):
    for f in fs:
        l = loc(f)
        m = re.search(r"/(?:index\.crates\.io-[0-9a-f]+|checkouts/[^/]+/[0-9a-f]+)/([^/]+)/", l)
        if m and not m.group(1).startswith(("tokio-1", "futures")):
            return re.sub(r"-[\d.]+$", "", m.group(1))
    return "?"


def main():
    path, n = sys.argv[1], int(sys.argv[2])
    top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
    stacks = parse(path)
    total = sum(c for c, _ in stacks)
    print(f"total allocations per statement: {total / n:.1f}  ({len(stacks)} distinct stacks)")

    rules = collections.Counter()
    for c, fs in stacks:
        text = " ".join(re.sub(r"\s+\[.*\]$", "", f) for f in fs)
        rules[next((a for a, pats in AREAS if any(x in text for x in pats)), "other")] += c
    print("\n== by area (first matching rule, see AREAS)")
    for k, v in rules.most_common():
        print(f"{v / n:8.2f}  {k}")

    area, sites, files = collections.Counter(), collections.Counter(), collections.Counter()
    chains = {}
    for c, fs in stacks:
        rf = repo_frames(fs)
        if not rf:
            k = f"(no tidb-rs frame) {dep_crate(fs)}"
            area[k] += c
            sites[k] += c
            files[k] += c
            continue
        names = [n_ for n_, _ in rf]
        k = "other: " + next((x for x in reversed(names) if x not in NOISE), "?")
        for x in reversed(names):
            if x in STEPS:
                k = x
                break
        # Calls into a dependency from a tidb-rs frame: name the dependency too.
        first = next(i for i, f in enumerate(fs) if loc(f).startswith(ROOT) and "/poc/server/src/" not in loc(f))
        dep = dep_crate(fs[:first])
        if dep in ("tikv-client", "tonic", "h2", "hyper", "prost", "tower"):
            k += f" -> {dep}"
        area[k] += c
        site = rf[0][1]
        sites[site] += c
        ctx = []
        for x, _ in rf:
            if x not in NOISE and (not ctx or ctx[-1] != x):
                ctx.append(x)
            if len(ctx) >= 6:
                break
        chains.setdefault(site, collections.Counter())[" < ".join(ctx)] += c
        fn = next(((x, l) for x, l in rf if x not in NOISE), rf[0])
        files[f"{fn[0]} ({fn[1].rsplit(':', 1)[0]})"] += c

    print("\n== by outermost statement step")
    for k, v in area.most_common(top):
        print(f"{v / n:8.2f}  {k}")
    print("\n== by innermost tidb-rs call site")
    for k, v in sites.most_common(top):
        print(f"{v / n:8.2f}  {k}")
        for ch, cv in chains.get(k, collections.Counter()).most_common(2):
            print(f"            {cv / n:6.2f} {ch}")
    print("\n== by innermost named tidb-rs function")
    for k, v in files.most_common(top):
        print(f"{v / n:8.2f}  {k}")


if __name__ == "__main__":
    main()
