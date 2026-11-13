#!/usr/bin/env python3
"""Summarize an N4 §5 gate run (scripts/eval-gate.sh): minimum over processes, every metric
against its threshold (specs/exceptions/N4-expr-eval/spec.md §5, D08.1 plan.md).

    scripts/eval_gate_summary.py <outdir>      # reads go-*.txt, rust-*.txt, machine.txt

Thresholds:
  * vector driver, every signature: not slower than Go (Go ns / Rust ns >= 1.0); a benchmark
    whose Rust side allocates (ALLOCS_VEC > 0) may be at most 5% slower (the owner-confirmed
    rule for allocation-including benchmarks, D01.2 spec §5);
  * vector driver throughput >= 1.5x Go (N4 §5 target, reported per signature);
  * fixed-length results: 0 heap allocations per chunk outside the result column;
  * RpnProgram::compile <= 1 us per node.
Row drivers are reported with the same ratio but are not part of the N4 §5 gate (#75).
Exit status 1 if a gate metric fails.
"""

import glob
import os
import re
import sys

# Go "<bench>/<sub>" (GOMAXPROCS suffix removed) -> Rust case
GO_CASES = {
    "BenchmarkVectorizedBuiltinArithmeticFunc/builtinArithmeticPlusIntSig": "PlusInt-SS",
    "BenchmarkVectorizedBuiltinArithmeticFunc/builtinArithmeticPlusIntSig#01": "PlusInt-UU",
    "BenchmarkVectorizedBuiltinArithmeticFunc/builtinArithmeticPlusIntSig#02": "PlusInt-SU",
    "BenchmarkVectorizedBuiltinArithmeticFunc/builtinArithmeticPlusIntSig#03": "PlusInt-US",
    "BenchmarkVectorizedBuiltinCompareFunc/builtinLTIntSig": "LTInt",
    "BenchmarkVectorizedBuiltinCastFunc/builtinCastIntAsRealSig": "CastIntAsReal",
    "BenchmarkVectorizedBuiltinStringFunc/builtinConcatSig": "Concat",
    "BenchmarkVectorizedBuiltinLikeFunc/builtinLikeSig": "Like",
}
for d in (3, 768):
    for f in ("L1Distance", "L2Distance", "CosineDistance", "NegativeInnerProduct"):
        GO_CASES[f"BenchmarkGateVecDim{d}/builtinVec{f}Sig"] = f"Vec{f}-d{d}"

GO_LINE = re.compile(r"^(Benchmark\S+?)(?:-\d+)?\s+\d+\s+([\d.]+) ns/op(?:\s+(\d+) B/op\s+(\d+) allocs/op)?")
TIME = re.compile(r"time:\s+\[\S+ \S+ (\S+) (\S+) \S+ \S+\]")
UNIT = {"ps": 1e-3, "ns": 1.0, "µs": 1e3, "us": 1e3, "ms": 1e6, "s": 1e9}


def go_results(out):
    """(case, mode) -> (min ns over every sample of every process, allocs/op)."""
    res = {}
    for path in sorted(glob.glob(os.path.join(out, "go-*.txt"))):
        for line in open(path):
            m = GO_LINE.match(line)
            if not m:
                continue
            name, ns = m.group(1), float(m.group(2))
            allocs = int(m.group(4)) if m.group(4) else None
            mm = re.match(r"(.*)-(VecBuiltinFunc|NonVecBuiltinFunc)(#\d+)?$", name)
            if not mm:
                continue
            case = GO_CASES.get(mm.group(1) + (mm.group(3) or ""))
            if case is None:
                continue
            key = (case, "Vec" if mm.group(2) == "VecBuiltinFunc" else "Row")
            prev = res.get(key)
            res[key] = (min(ns, prev[0]) if prev else ns, allocs)
    return res


def rust_results(out):
    """bench id -> min point estimate (ns) over processes; ALLOCS_{VEC,ROW}; compile nodes."""
    res, allocs, nodes, nproc = {}, {}, None, 0
    for path in sorted(glob.glob(os.path.join(out, "rust-*.txt"))):
        nproc += 1
        pending = None
        for line in open(path):
            line = line.rstrip("\n")
            if line.startswith("ALLOCS_"):
                kind, case, n = line.split()
                allocs[(case, kind[len("ALLOCS_"):].capitalize())] = int(n)
                continue
            if line.startswith("COMPILE_NODES"):
                nodes = int(line.split()[1])
                continue
            m = TIME.search(line)
            if m:
                head = line[: line.index("time:")].strip()
                name = head or pending
                ns = float(m.group(1)) * UNIT[m.group(2)]
                res[name] = min(ns, res.get(name, ns))
                pending = None
            elif re.fullmatch(r"[A-Za-z][\w.\-]*", line.strip()):
                pending = line.strip()
    return res, allocs, nodes, nproc


def fmt_ns(ns):
    return f"{ns / 1000:,.2f} µs" if ns >= 10_000 else f"{ns:,.0f} ns"


def main():
    out = sys.argv[1]
    go = go_results(out)
    rust, allocs, nodes, nproc = rust_results(out)
    ngo = len(glob.glob(os.path.join(out, "go-*.txt")))
    if os.path.exists(os.path.join(out, "machine.txt")):
        print("```")
        print(open(os.path.join(out, "machine.txt")).read().rstrip())
        print("```\n")
    print(f"Minimum over {ngo} Go processes (3 samples each) and {nproc} Rust processes; ns per 1024-row batch.\n")
    failed = []
    for mode in ("Vec", "Row"):
        gated = mode == "Vec"
        print(f"### {'Vector driver (gate)' if gated else 'Row driver (not gated, #75)'}\n")
        print("| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |")
        print("|---|--:|--:|--:|--:|--:|--:|:-:|:-:|")
        cases = [n[: -len("-" + mode)] for n in rust if n.endswith("-" + mode)]
        for case in cases:
            base = case[: -len("-port")] if case.endswith("-port") else case
            g = go.get((base, mode))
            r = rust[f"{case}-{mode}"]
            a = allocs.get((case, mode))
            if g is None:
                print(f"| {case} | — | — | {fmt_ns(r)} | {a} | — | — | — | no Go number |")
                if gated:
                    failed.append(f"{case}-{mode}: no Go number")
                continue
            ratio = g[0] / r
            thr = 1 / 1.05 if a else 1.0
            ok = ratio >= thr
            verdict = ("pass" if ok else "**FAIL**") if gated else ("≥ Go" if ok else "below Go")
            if gated and not ok:
                failed.append(f"{case}-{mode}: {ratio:.2f}× < {thr:.2f}×")
            thr_s = "≥ 0.95× (allocates)" if a else "≥ 1.0×"
            print(f"| {case} | {fmt_ns(g[0])} | {g[1] if g[1] is not None else '—'} | {fmt_ns(r)} | {a} | {ratio:.2f}× | {thr_s} | {'yes' if ratio >= 1.5 else 'no'} | {verdict} |")
        print()
    print("### Other N4 §5 metrics\n")
    print("| metric | threshold | result | verdict |")
    print("|---|---|---|:-:|")
    fixed = [c for (c, m), n in allocs.items() if m == "Vec" and not (c.startswith("Concat") or c.startswith("Like"))]
    bad = [c for c in fixed if allocs[(c, "Vec")] != 0]
    got = ", ".join("%s: %d" % (c, allocs[(c, "Vec")]) for c in bad) if bad else "0 in every case"
    print(f"| heap allocations per chunk, fixed-length results ({len(fixed)} cases, vector driver) | 0 | "
          f"{got} | {'pass' if not bad else '**FAIL**'} |")
    if bad:
        failed.append("allocations per chunk: " + ", ".join(bad))
    if "Compile-all" in rust and nodes:
        per = rust["Compile-all"] / nodes
        ok = per <= 1000
        print(f"| `RpnProgram::compile` per node ({nodes} nodes) | ≤ 1 µs | {per:.1f} ns | {'pass' if ok else '**FAIL**'} |")
        if not ok:
            failed.append("compile per node")
    print()
    if failed:
        print("**Gate failed:** " + "; ".join(failed))
        sys.exit(1)
    print("**Gate passed.**")


if __name__ == "__main__":
    main()
