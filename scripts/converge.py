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
    (`go:`, `go-part:`, `go-std:`, `go-dep:`, `go-l3:`, `go-exception:`, `glue:`);
  * the `// go:` annotations follow Go's source order (reported, not an error);
  * every mapping rule ID in `L1 [M-..]` is a rule of docs/porting/mappings.md;
  * with --fma-sites (tracker data/go_fma_sites.tsv, mapping rule M-FMA): every Go line
    that gc fuses into FMA on some target, in a Go file the Rust file ports, has a
    `[M-FMA] <file>.go:<line>` comment in the Rust file, and the `go:` annotation of its
    function carries M-FMA; every such comment names a registered line;
  * every lint in a module-level `#![allow(...)]` is in the workspace list
    `[workspace.metadata.porting] style-lint-allow` of Cargo.toml; item-level `#[allow]`s
    are listed for the reviewer (reported, not an error).

Besides `// go:`, a function of the block counts as accounted for when it is
  * replaced by an exception spec: `// go-exception: <EXC> <Symbol> (<reason>)`, e.g.
    `// go-exception: EXC.B SortItem.Hash64 (cascades)`; or
  * a boundary stub: `// glue: stub of <Symbol>, blocked-by:<block> (<what>)` above the stub
    function (registered in the tracker's data/stubs.tsv);
both are reported on a separate line ("accounted"); "matched" counts ported functions only.
With --blocks-done (tracker data/blocks_done.tsv), a block marked done may have no stub
blocked-by itself.

A block ported in parts (VS1 plan §0 item 3, as D06.2 / D06.3 "part 1"): with
`--part <functions.tsv> --item <ID>` only the block's functions listed for that work item
(columns `item`, `block`, `file`, `function`, e.g. specs/VS1-vertical-slice/functions.tsv;
an optional `not_ported` column gives the reason a listed function is not ported: such rows
are exempt, printed with their reason, neither required nor counted as `blocked-by`)
must converge; the rest of the block is reported as `blocked-by:<block>` (its boundary
stubs are registered in the tracker's data/stubs.tsv).

    scripts/converge.py --block D09.0 --part specs/VS1-vertical-slice/functions.tsv \
        --item VS1-10 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/planner/src/...

Exit status 1 if anything is missing, extra, duplicated, malformed or unannotated.
"""

import argparse
import csv
import os
import re
import sys
import tomllib
from collections import defaultdict

ANN_RE = re.compile(r"^\s*//\s*(go|go-part|go-std|go-dep|go-l3|go-exception|go-fix|glue):\s*(.*)$")
FILE_RE = re.compile(r"^\s*//!\s*go-file:\s*(\S+)")
FN_RE = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?(const\s+)?(unsafe\s+)?fn\s+(\w+)")
ALLOW_RE = re.compile(r"^\s*#(!?)\[allow\(")
LEVEL_RE = re.compile(r"^(L1(\s+\[M-[A-Z][A-Z0-9]*(,\s*M-[A-Z][A-Z0-9]*)*\])?|L2\s+\(.+\)|L3)$")
RULES_RE = re.compile(r"\[(M-[A-Z][A-Z0-9]*(?:,\s*M-[A-Z][A-Z0-9]*)*)\]")
FMA_MARK_RE = re.compile(r"//.*\[M-FMA\]\s+(\S+\.go):(\d+)")
FMA_TARGETS_END = "instrs"  # go_fma_sites.tsv: target columns are between symbol and instrs
STUB_RE = re.compile(r"^stub of (\S+), blocked-by:(\S+)")


def load_rules(path):
    """Mapping rule IDs: first cell of the rule table rows of mappings.md."""
    with open(path, encoding="utf-8") as f:
        return {m.group(1) for line in f if (m := re.match(r"^\|\s*(M-[A-Z][A-Z0-9]*)\s*\|", line))}


def load_fma_sites(path):
    """(file, line) -> (symbol, fused targets) for lines fused on at least one target."""
    sites = {}
    with open(path, newline="") as f:
        rows = csv.DictReader(f, delimiter="\t")
        cols = rows.fieldnames
        targets = cols[cols.index("symbol") + 1: cols.index(FMA_TARGETS_END)]
        for row in rows:
            fused = [t for t in targets if row[t] == "yes"]
            if fused:
                sites[(row["file"], int(row["line"]))] = (row["symbol"], fused)
    return sites


def load_funcs(path):
    """(file, symbol) -> (block, start_line)"""
    funcs = {}
    with open(path, newline="") as f:
        for row in csv.DictReader(f, delimiter="\t"):
            funcs[(row["file"], row["symbol"])] = (row["block"], int(row["start_line"]))
    return funcs


def load_part(path, item, block):
    """(file, symbol) of the rows of work item `item` and block `block` in a part's function
    list (one item may port parts of several blocks), and those of them marked not ported
    (column `not_ported`: the reason, as in the tracker's exemptions), as
    (part, {(file, symbol): reason})."""
    part, not_ported = set(), {}
    with open(path, newline="") as f:
        for row in csv.DictReader(f, delimiter="\t"):
            if row["item"] != item or row["block"] != block:
                continue
            part.add((row["file"], row["function"]))
            if (row.get("not_ported") or "").strip():
                not_ported[(row["file"], row["function"])] = row["not_ported"].strip()
    return part, not_ported


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


def load_lint_allow(path):
    with open(path, "rb") as f:
        meta = tomllib.load(f).get("workspace", {}).get("metadata", {}).get("porting", {})
    if "style-lint-allow" not in meta:
        sys.exit(f"converge: {path} has no [workspace.metadata.porting] style-lint-allow")
    return set(meta["style-lint-allow"])


def scan_allows(path):
    """[(line_no, module_level, [lint, ...])] for every #[allow(...)] / #![allow(...)]."""
    lines = open(path).read().splitlines()
    out = []
    i = 0
    while i < len(lines):
        m = ALLOW_RE.match(lines[i])
        if not m:
            i += 1
            continue
        start = i
        text = lines[i]
        while ")]" not in text and i + 1 < len(lines):
            i += 1
            text += " " + lines[i]
        body = text.split("allow(", 1)[1].split(")]", 1)[0]
        lints = [x.strip() for x in body.split(",") if x.strip()]
        out.append((start + 1, m.group(1) == "!", lints))
        i += 1
    return out


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


def check_fma(rs, go_files, sym_rules, sites, problems):
    """Rule M-FMA, see the module doc. Returns the number of registered sites checked."""
    marks = set()
    for n, line in enumerate(open(rs).read().splitlines(), 1):
        m = FMA_MARK_RE.search(line)
        if not m:
            continue
        name, ln = m.group(1), int(m.group(2))
        files = [f for f in go_files if f == name or f.endswith("/" + name)]
        if not files or (files[0], ln) not in sites:
            problems.append(f"{rs}:{n}: [M-FMA] {name}:{ln} is not a fused line of go_fma_sites.tsv "
                            f"in this file's go-files")
            continue
        marks.add((files[0], ln))
    checked = 0
    for (f, ln), (sym, fused) in sorted(sites.items()):
        if f not in go_files:
            continue
        checked += 1
        if (f, ln) not in marks:
            problems.append(f"{rs}: {f}:{ln} ({sym}) is fused by gc on {', '.join(fused)}; "
                            f"no `// [M-FMA] {os.path.basename(f)}:{ln}` comment")
        for s in sym.split(","):
            if "M-FMA" not in sym_rules.get(s, ()):
                problems.append(f"{rs}: {s} contains fused line {f}:{ln}; its annotation lacks [M-FMA]")
    return checked


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--block", required=True)
    ap.add_argument("--func-blocks", required=True, help="tracker data/func_blocks.tsv")
    ap.add_argument("--workspace-toml",
                    default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "Cargo.toml"),
                    help="Cargo.toml with [workspace.metadata.porting] style-lint-allow")
    ap.add_argument("--mappings",
                    default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "docs", "porting",
                                         "mappings.md"),
                    help="docs/porting/mappings.md (mapping rule IDs)")
    ap.add_argument("--fma-sites", help="tracker data/go_fma_sites.tsv (rule M-FMA)")
    ap.add_argument("--part", metavar="FUNCTIONS_TSV",
                    help="function list of the block's parts (item, block, file, function); with --item, "
                         "only that part must converge")
    ap.add_argument("--item", help="the work item of --part (e.g. VS1-10)")
    ap.add_argument("--blocks-done", help="tracker data/blocks_done.tsv: a done block may have no stub "
                                          "blocked-by itself")
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()
    lint_allow = load_lint_allow(args.workspace_toml)
    rules = load_rules(args.mappings)
    fma_sites = load_fma_sites(args.fma_sites) if args.fma_sites else None
    fma_checked = 0

    funcs = load_funcs(args.func_blocks)
    expected = {k for k, (b, _) in funcs.items() if b == args.block}
    part_problems, rest = [], set()
    if bool(args.part) != bool(args.item):
        ap.error("--part and --item go together")
    if args.part:
        part, not_ported = load_part(args.part, args.item, args.block)
        if not part:
            ap.error(f"{args.part}: no function of item {args.item} in block {args.block}")
        part_problems = [f"{args.part}: {f} {s} ({args.item}) is not a function of block {args.block} "
                         f"in func_blocks.tsv" for f, s in sorted(part - expected)]
        rest = expected - part
        # Rows marked not ported are exempt: neither required nor reported as blocked-by.
        for (f, sym), why in sorted(not_ported.items()):
            print(f"not ported ({args.part}): {f} {sym}: {why}")
        expected &= part - set(not_ported)
    expected_files = {f for f, _ in expected}

    seen = defaultdict(list)  # (file, symbol) -> [rs locations]
    accounted = {}  # (file, symbol) -> "exception EXC.B" / "stub D10.1" (not ported, accounted for)
    problems = []
    other_blocks = []
    kinds = defaultdict(int)
    rule_uses = defaultdict(int)
    levels = defaultdict(int)
    covered_files = set()
    item_allows = []
    for rs in args.files:
        for line_no, module_level, lints in scan_allows(rs):
            if not module_level:
                item_allows.append(f"{rs}:{line_no}: #[allow({', '.join(lints)})]")
                continue
            for lint in lints:
                if lint not in lint_allow:
                    problems.append(f"{rs}:{line_no}: #![allow({lint})] is not in the workspace "
                                    f"style-lint-allow list")
        go_files, anns, _, unannotated = scan(rs, funcs)
        covered_files.update(go_files)
        order = []
        sym_rules = defaultdict(set)  # Go symbol -> rule IDs of its `go:` annotation
        for line_no, kind, text in anns:
            kinds[kind] += 1
            for m in RULES_RE.finditer(text):
                for rule in re.split(r",\s*", m.group(1)):
                    if rule not in rules:
                        problems.append(f"{rs}:{line_no}: mapping rule {rule} is not in {args.mappings}")
                    rule_uses[rule] += 1
            if kind == "go" and text.split():
                parts = text.split()
                sym = parts[1] if parts[0].endswith(".go") and len(parts) > 1 else parts[0]
                sym_rules[sym].update(r for m in RULES_RE.finditer(text) for r in re.split(r",\s*", m.group(1)))
            if kind in ("go-exception", "glue"):
                if kind == "go-exception":
                    parts_ = text.split()
                    what, sym = (f"exception {parts_[0]}", parts_[1]) if len(parts_) > 1 else (None, None)
                else:
                    m = STUB_RE.match(text)
                    what, sym = (f"stub {m.group(2).rstrip(',')}", m.group(1)) if m else (None, None)
                if sym:
                    for f in go_files:
                        if (f, sym) in funcs:
                            accounted[(f, sym)] = what
                            break
                    else:
                        problems.append(f"{rs}:{line_no}: {kind} names {sym}, which is not in {go_files} "
                                        f"(func_blocks.tsv)")
                continue
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
        if fma_sites is not None:
            fma_checked += check_fma(rs, go_files, sym_rules, fma_sites, problems)
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

    missing = sorted(k for k in expected if k[0] in covered_files and k not in seen and k not in accounted)
    both = sorted(k for k in accounted if k in seen)
    not_covered = sorted(expected_files - covered_files)
    dups = {k: v for k, v in seen.items() if len(v) > 1}
    extra = [p for p in problems if "not in" in p]

    if args.part:
        print(f"block {args.block} part {args.item}: {len(expected)} Go functions in {len(expected_files)} "
              f"file(s); the other {len(rest)} function(s) of the block are blocked-by:{args.block}")
    else:
        print(f"block {args.block}: {len(expected)} Go functions in {len(expected_files)} file(s)")
    problems += part_problems
    print(f"annotations: " + ", ".join(f"{k}={v}" for k, v in sorted(kinds.items())))
    print(f"go: levels: " + ", ".join(f"{k}={v}" for k, v in sorted(levels.items())))
    print(f"mapping rules: " + (", ".join(f"{k}={v}" for k, v in sorted(rule_uses.items())) or "none"))
    if fma_sites is not None:
        print(f"M-FMA: {fma_checked} registered site(s) in the ported Go files")
    acc = [k for k in expected if k in accounted and k not in seen]
    n_stubs = sum(1 for k in acc if accounted[k].startswith("stub"))
    n_exc = sum(1 for k in acc if accounted[k].startswith("exception"))
    # matched counts ported functions only; stubs and exceptions are reported apart.
    print(f"matched {len(expected & set(seen))}/{len(expected)}, missing {len(missing)}, "
          f"extra {len(extra)}, duplicated {len(dups)}")
    print(f"accounted (stubs {n_stubs} / exceptions {n_exc})")
    for k in sorted(acc):
        print(f"accounted: {k[0]} {k[1]}: {accounted[k]}")
    if args.blocks_done:
        with open(args.blocks_done, newline="") as f:
            done = {row["block"] for row in csv.DictReader(f, delimiter="\t")}
        if args.block in done:
            own = sorted(k for k in acc if accounted[k] == f"stub {args.block}")
            for k in own:
                problems.append(f"{k[0]} {k[1]}: stub blocked-by:{args.block}, but {args.block} is done "
                                f"(blocks_done.tsv)")
    for k in both:
        problems.append(f"{k[0]} {k[1]} is both ported (go:) and {accounted[k]}")
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
    for a in item_allows:
        print(f"info: item-level {a}")
    bad = missing or not_covered or dups or problems
    print("converge: FAIL" if bad else "converge: OK")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
