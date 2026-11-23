#!/usr/bin/env python3
"""Summarise a macOS `sample` call tree of tidb-server by area.

  sample <pid> 10 -file cpu.txt        # while the point-select load runs
  tools/perf/cpu_report.py cpu.txt

Busy samples are all samples except threads parked in the runtime (tokio park / condvar wait /
kevent). Each area's share counts a sample once when any frame of its stack matches one of the
area's patterns (Rust v0-mangled names are matched by substring); areas overlap, so the shares
do not add up to 100%. The second table is the self time ("top of stack") of the busiest
functions.
"""

import re
import sys

# Leaf frames of a thread that is blocked waiting for work.
IDLE = ("__psynch_cvwait", "kevent")
AREAS = [
    ("network syscalls (recv/send/writev)", ("__recvfrom", "__sendto", "writev", "readv", "__read_nocancel", "__write_nocancel")),
    ("TiKV client / gRPC (client-rust, tonic, h2, hyper)", ("11tikv_client", "5tonic", "2h2", "5hyper")),
    ("arc_swap debt payment (ArcSwap store waits for readers)", ("8arc_swap4debt",)),
    ("allocator (jemalloc malloc/free)", ("_rjem_",)),
    ("memcpy / memmove", ("_platform_memmove", "memcpy")),
    ("router checkpoint / restore (PoC glue)", ("10checkpoint", "15checkpoint_copy", "7restore")),
    ("eval context (new_eval_context)", ("16new_eval_context",)),
    ("result column metadata", ("24col_names2_result_fields", "17write_column_info", "11exec_result")),
    ("system variable lookups", ("11get_sys_var", "14get_system_var", "32get_session_or_global_system_var")),
    ("planner / plan cache", ("16planner_optimize", "24get_plan_from_plan_cache")),
    ("compile (incl. planner)", ("7compile",)),
    ("statement context reset", ("21reset_context_of_stmt",)),
    ("point get executor", ("9point_get", "8PointGet")),
    ("mutex / condvar signalling", ("pthread_mutex", "__psynch_mutex", "__psynch_cvsignal", "pthread_cond_signal")),
]


def parse(path):
    """Yields the stack ([depth, name, count, children count]) at every node of the call graph."""
    stack = []
    in_graph = False
    for line in open(path):
        if line.startswith("Call graph:"):
            in_graph = True
            continue
        if not in_graph:
            continue
        if not line.strip():
            break
        m = re.match(r"^(\s*[+!:| ]*)(\d+)\s+(\S+)", line)
        if not m:
            continue
        depth = len(m.group(1))
        count = int(m.group(2))
        name = m.group(3)
        while stack and stack[-1][0] >= depth:
            stack.pop()
        stack.append([depth, name, count, 0])
        if len(stack) > 1:
            stack[-2][3] += count
        yield stack


def main():
    # A node's self count (count - children) is known only after its subtree: keep references.
    nodes = [([n[1] for n in st], st[-1]) for st in parse(sys.argv[1])]
    total = busy = 0
    area = {a: 0 for a, _ in AREAS}
    self_t = {}
    for names, node in nodes:
        self_count = node[2] - node[3]
        if self_count <= 0:
            continue
        total += self_count
        text = " ".join(names)
        if names[-1] in IDLE:
            continue
        busy += self_count
        for a, pats in AREAS:
            if any(p in text for p in pats):
                area[a] += self_count
        self_t[names[-1]] = self_t.get(names[-1], 0) + self_count
    print(f"samples: {total}, busy (not parked): {busy}")
    print("\n== share of busy samples by area (overlapping)")
    for a, v in sorted(area.items(), key=lambda x: -x[1]):
        print(f"{100 * v / busy:6.1f}%  {a}")
    print("\n== self time, top 25")
    for k, v in sorted(self_t.items(), key=lambda x: -x[1])[:25]:
        print(f"{100 * v / busy:6.1f}%  {k[:150]}")


if __name__ == "__main__":
    main()
