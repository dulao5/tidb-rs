#!/usr/bin/env python3
"""Ratchet on temporary boundary stubs (`blocked-by:<block>`, constitution article 4.3; #262).

    scripts/check_stub_ratchet.py            check: exit 1 if an area has more stubs than its
                                             baseline, or fewer (lower the baseline)
    scripts/check_stub_ratchet.py --write    rewrite the baseline from the tree
    scripts/check_stub_ratchet.py --list     every stub, by area

A stub is counted as check_one_translation.py and the tracker's stub scan count them: in a
non-test Rust file under crates/ (not crates/spike, not tests/, tests.rs, *_tests.rs), a
`// glue: ...` comment naming `blocked-by:<block>`, or a `blocked("<block>", ...)` call. Prose
mentioning blocked-by in other comments is not a stub, nor is a `#[cfg(test)] mod` (the unit
tests at the end of a file). An area is a crate directory
(`crates/<name>` or `crates/poc/<name>`).

The baseline (scripts/stub_ratchet.tsv) holds the count per area. The count may only go down:
a PR that adds a stub over the baseline fails here (raising the baseline is a visible, reviewed
change of that file); a PR that removes stubs lowers the baseline with --write, so the slack
cannot be refilled later.
"""

import argparse
import os
import re
import sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from check_one_translation import BLOCKED_RE, ROOT, crate_of, rust_files  # noqa: E402

BASELINE = os.path.join(HERE, "stub_ratchet.tsv")


def is_test_file(rel):
    base = os.path.basename(rel)
    return f"{os.sep}tests{os.sep}" in rel or base == "tests.rs" or base.endswith("_tests.rs")


def stubs(root=ROOT, files=None):
    """[(area, file, line, block)] of every temporary boundary stub."""
    out = []
    for rel in files if files is not None else rust_files():
        if is_test_file(rel):
            continue
        with open(os.path.join(root, rel), encoding="utf-8") as f:
            lines = f.read().splitlines()
        for n, line in enumerate(lines, 1):
            s = line.lstrip()
            # The unit tests at the end of a file (`#[cfg(test)] mod ...`) are not stubs.
            # (`#[cfg(test)] mod name;` only declares a file: keep scanning.)
            if s.startswith("#[cfg(test)]") and n < len(lines) \
                    and re.match(r"\s*(pub(\([^)]*\))?\s+)?mod\s+\w+\s*\{", lines[n]):
                break
            if s.startswith("//") and "glue:" not in s:
                continue
            for m in BLOCKED_RE.finditer(line):
                out.append((crate_of(rel), rel, n, m.group(1) or m.group(2)))
    return out


def read_baseline(path=BASELINE):
    base = {}
    if not os.path.exists(path):
        return base
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("#") or not line.strip() or line.startswith("area\t"):
                continue
            area, n = line.rstrip("\n").split("\t")
            base[area] = int(n)
    return base


def write_baseline(counts, path=BASELINE):
    with open(path, "w", encoding="utf-8") as f:
        f.write("# Temporary boundary stubs (blocked-by) per area: may only go down "
                "(scripts/check_stub_ratchet.py, #262).\n")
        f.write("# Regenerate with scripts/check_stub_ratchet.py --write after removing stubs.\n")
        f.write("area\tstubs\n")
        for area in sorted(counts):
            f.write(f"{area}\t{counts[area]}\n")


def compare(counts, base):
    """(over, under): areas above their baseline, areas below it."""
    over = sorted((a, base.get(a, 0), n) for a, n in counts.items() if n > base.get(a, 0))
    under = sorted((a, b, counts.get(a, 0)) for a, b in base.items() if counts.get(a, 0) < b)
    return over, under


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--write", action="store_true")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--baseline", default=BASELINE)
    args = ap.parse_args()
    found = stubs()
    counts = Counter(a for a, _, _, _ in found)
    by_block = Counter(b for _, _, _, b in found)
    if args.list:
        per = defaultdict(list)
        for a, f, n, b in found:
            per[a].append(f"{f}:{n} blocked-by:{b}")
        for a in sorted(per):
            print(f"{a} ({len(per[a])})")
            for s in per[a]:
                print(f"  {s}")
    if args.write:
        write_baseline(counts, args.baseline)
        print(f"stub ratchet: baseline written: {sum(counts.values())} stubs in {len(counts)} areas")
        return 0
    base = read_baseline(args.baseline)
    over, under = compare(counts, base)
    print(f"stub ratchet: {sum(counts.values())} blocked-by stubs in {len(counts)} areas "
          f"(baseline {sum(base.values())}); by block: "
          + ", ".join(f"{b} {n}" for b, n in by_block.most_common(8)))
    for a, b, n in over:
        print(f"stub ratchet: {a}: {n} blocked-by stubs, baseline {b}: the count may only go down "
              f"(the stubs this change adds: `git diff -U0 origin/main -- crates/{a} | grep blocked`; "
              f"all of them: --list)", file=sys.stderr)
    for a, b, n in under:
        print(f"stub ratchet: {a}: {n} blocked-by stubs, baseline {b}: lower the baseline "
              f"(scripts/check_stub_ratchet.py --write)", file=sys.stderr)
    return 1 if over or under else 0


if __name__ == "__main__":
    sys.exit(main())
