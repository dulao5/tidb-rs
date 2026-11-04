#!/usr/bin/env python3
"""Convergence check: Rust provenance annotations <=> the Go functions of a porting block.

First version, written for the D01.2 (MyDecimal) process pilot; meant to move into the
tracker. Syntax: docs/porting/provenance.md (branch docs/porting-constitution).

    scripts/converge.py --block D01.2 \
        --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv \
        crates/types/src/mydecimal.rs crates/types/src/helper.rs

Checks, per Rust file:
  * `//! go-file: <path>` headers name the Go files the file ports;
  * every `// go: [<path>] <Symbol> <level>` names a symbol of func_blocks.tsv in one of
    those files (or in <path>), with a valid level (`L1`, `L1 [M-..]`, `L2 (reason)`, `L3`);
  * every function of the block in a go-file is annotated exactly once with `// go:`;
  * every `fn` outside `#[cfg(test)]` / test files carries some annotation
    (`go:`, `go-part:`, `go-std:`, `go-l3:`, `go-exception:`, `glue:`);
  * the `// go:` annotations follow Go's source order (reported, not an error).

Exit status 1 if anything is missing, extra, duplicated, malformed or unannotated.
"""

import argparse
import csv
import re
import sys
from collections import defaultdict

ANN_RE = re.compile(r"^\s*//\s*(go|go-part|go-std|go-l3|go-exception|go-fix|glue):\s*(.*)$")
FILE_RE = re.compile(r"^\s*//!\s*go-file:\s*(\S+)")
FN_RE = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?(const\s+)?(unsafe\s+)?fn\s+(\w+)")
LEVEL_RE = re.compile(r"^(L1(\s+\[M-[A-Z]+(,\s*M-[A-Z]+)*\])?|L2\s+\(.+\)|L3)$")


def load_funcs(path):
    """(file, symbol) -> (block, start_line)"""
    funcs = {}
    with open(path, newline="") as f:
        for row in csv.DictReader(f, delimiter="\t"):
            funcs[(row["file"], row["symbol"])] = (row["block"], int(row["start_line"]))
    return funcs


def parse_go_annotation(text, go_files, funcs):
    """`[<path>] <Symbol> [level...]` -> (file, symbol, level, error)"""
    parts = text.split(None, 1)
    if not parts:
        return None, None, None, "empty annotation"
    path = None
    if "/" in parts[0] and parts[0].endswith(".go"):
        path = parts[0]
        parts = parts[1].split(None, 1) if len(parts) > 1 else []
        if not parts:
            return path, None, None, "missing symbol"
    symbol = parts[0]
    level = parts[1].strip() if len(parts) > 1 else "L1"
    if not LEVEL_RE.match(level):
        return path, symbol, level, f"bad level {level!r}"
    candidates = [path] if path else go_files
    for f in candidates:
        if (f, symbol) in funcs:
            return f, symbol, level, None
    return path, symbol, level, f"symbol {symbol} not in {candidates} (func_blocks.tsv)"


def scan(path, funcs):
    lines = open(path).read().splitlines()
    go_files = [m.group(1) for line in lines if (m := FILE_RE.match(line))]
    anns = []  # (line_no, kind, text)
    errors = []
    unannotated = []
    in_test_mod = False
    for i, line in enumerate(lines):
        if re.match(r"^\s*#\[cfg\(test\)\]", line):
            in_test_mod = True
        m = ANN_RE.match(line)
        if m:
            anns.append((i + 1, m.group(1), m.group(2).strip()))
        fm = FN_RE.match(line)
        if fm and not in_test_mod:
            # Walk up over doc comments, comments and attributes looking for an annotation.
            j = i - 1
            found = False
            while j >= 0 and re.match(r"^\s*(//|#\[|#!\[)", lines[j]):
                if ANN_RE.match(lines[j]):
                    found = True
                    break
                j -= 1
            if not found:
                unannotated.append((i + 1, fm.group(5)))
    return go_files, anns, errors, unannotated


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--block", required=True)
    ap.add_argument("--func-blocks", required=True, help="tracker data/func_blocks.tsv")
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()

    funcs = load_funcs(args.func_blocks)
    expected = {k for k, (b, _) in funcs.items() if b == args.block}
    expected_files = {f for f, _ in expected}

    seen = defaultdict(list)  # (file, symbol) -> [rs locations]
    problems = []
    other_blocks = []
    kinds = defaultdict(int)
    levels = defaultdict(int)
    covered_files = set()
    for rs in args.files:
        go_files, anns, _, unannotated = scan(rs, funcs)
        covered_files.update(go_files)
        order = []
        for line_no, kind, text in anns:
            kinds[kind] += 1
            if kind != "go":
                continue
            f, sym, level, err = parse_go_annotation(text, go_files, funcs)
            loc = f"{rs}:{line_no}"
            if err:
                problems.append(f"{loc}: {err}")
                continue
            levels[level.split()[0]] += 1
            seen[(f, sym)].append(loc)
            block, start = funcs[(f, sym)]
            if block != args.block:
                other_blocks.append(f"{loc}: {f} {sym} belongs to block {block}")
            order.append((start, sym, line_no, f))
        for line_no, name in unannotated:
            problems.append(f"{rs}:{line_no}: fn {name} has no provenance annotation")
        # Go order within each Go file.
        by_file = defaultdict(list)
        for start, sym, line_no, f in order:
            by_file[f].append((start, sym, line_no))
        for f, items in by_file.items():
            prev = None
            for start, sym, line_no in items:
                if prev and start < prev[0]:
                    print(f"note: {rs}:{line_no} {sym} (Go line {start}) comes after {prev[1]} (Go line {prev[0]})")
                prev = (start, sym)

    missing = sorted(k for k in expected if k[0] in covered_files and k not in seen)
    not_covered = sorted(expected_files - covered_files)
    dups = {k: v for k, v in seen.items() if len(v) > 1}
    extra = [p for p in problems if "not in" in p]

    print(f"block {args.block}: {len(expected)} Go functions in {len(expected_files)} file(s)")
    print(f"annotations: " + ", ".join(f"{k}={v}" for k, v in sorted(kinds.items())))
    print(f"go: levels: " + ", ".join(f"{k}={v}" for k, v in sorted(levels.items())))
    print(f"matched {len(expected & set(seen))}/{len(expected)}, missing {len(missing)}, "
          f"extra {len(extra)}, duplicated {len(dups)}")
    for f in not_covered:
        print(f"MISSING FILE: no Rust file has `//! go-file: {f}`")
    for f, s in missing:
        print(f"MISSING: {f} {s} (Go line {funcs[(f, s)][1]})")
    for k, v in sorted(dups.items()):
        print(f"DUPLICATE: {k[1]} annotated at {', '.join(v)}")
    for p in problems:
        print(f"ERROR: {p}")
    for o in other_blocks:
        print(f"info: {o}")
    bad = missing or not_covered or dups or problems
    print("converge: FAIL" if bad else "converge: OK")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
