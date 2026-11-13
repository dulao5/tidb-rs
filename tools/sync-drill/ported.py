#!/usr/bin/env python3
"""List the Go functions that tidb-rs has ported, from the Rust provenance annotations.

A function counts as ported when some Rust file carries `// go: [<path>] <Symbol> <level>`
for it (or `// go-part:`, a helper split out of it), resolved against the file's
`//! go-file:` headers and the tracker's func_blocks.tsv exactly as scripts/converge.py does.
Spike / PoC crates, tests and benches are skipped. `go-std` / `go-dep` (Go stdlib, third-party modules) are out of scope of a TiDB sync.

    tools/sync-drill/ported.py --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv \
        crates > ported.tsv

Set GO_SRC to the Go TiDB v8.5.8 source tree to also resolve package-level vars
(defaultSysVars, noopSysVars ...) whose closures are ported with `go-part`.

Output (TSV): go_file, symbol, block ('-' for a var), kinds (go,go-part), rs_files.
"""

import argparse
import os
import re
import sys
from collections import defaultdict

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "scripts"))
import converge  # noqa: E402


# Not shipped code: throw-away spikes / PoCs, integration tests and benches.
SKIP_RE = re.compile(r"(^|/)(spike|poc|tests|benches)/")
GO_SRC = os.environ.get("GO_SRC", "")  # Go TiDB v8.5.8 source, for package-level vars


def resolve(text, go_files, funcs):
    """`[<path>] <Symbol> ...` -> (go_file, symbol, is_func). Unlike converge, the level and
    any trailing words are ignored (they may wrap onto the next line); a symbol that is not a
    function (a package-level var such as defaultSysVars whose closures are ported) is looked
    up in the Go source of the candidate files."""
    parts = text.split()
    if not parts:
        return None, None, False
    cands = go_files
    if "/" in parts[0] and parts[0].endswith(".go"):
        cands = [parts[0]]
        parts = parts[1:]
        if not parts:
            return None, None, False
    sym = parts[0]
    for f in cands:
        if (f, sym) in funcs:
            return f, sym, True
    if GO_SRC and re.fullmatch(r"\w+", sym):
        pat = re.compile(rf"^(var\s+)?\s*{sym}\s*(=|\[\]|map\[|\*?\w+\s*=)", re.M)
        for f in cands:
            p = os.path.join(GO_SRC, f)
            if f.endswith(".go") and os.path.exists(p) and pat.search(open(p, encoding="utf-8").read()):
                return f, sym, False
    return None, sym, False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--func-blocks", required=True)
    ap.add_argument("roots", nargs="+")
    args = ap.parse_args()
    funcs = converge.load_funcs(args.func_blocks)

    found = defaultdict(lambda: (set(), set()))  # (file, symbol) -> (kinds, rs_files)
    unresolved = []
    for root in args.roots:
        for dirpath, _, names in os.walk(root):
            for name in sorted(names):
                path = os.path.join(dirpath, name)
                if not name.endswith(".rs") or name.endswith("_test.rs") or SKIP_RE.search(path):
                    continue
                lines = open(path, encoding="utf-8").read().splitlines()
                go_files = [m.group(1) for line in lines if (m := converge.FILE_RE.match(line))]
                for line in lines:
                    m = converge.ANN_RE.match(line)
                    if not m or m.group(1) not in ("go", "go-part"):
                        continue
                    f, sym, decl = resolve(m.group(2), go_files, funcs)
                    if not f or not f.startswith("pkg/"):
                        unresolved.append(f"{path}: {m.group(1)}: {m.group(2)}")
                        continue
                    kinds, rs = found[(f, sym)]
                    kinds.add(m.group(1))
                    rs.add(path)
    print("go_file\tsymbol\tblock\tkinds\trs_files")
    for (f, sym), (kinds, rs) in sorted(found.items()):
        print(f"{f}\t{sym}\t{funcs.get((f, sym), ('-',))[0]}\t{','.join(sorted(kinds))}\t{','.join(sorted(rs))}")
    print(f"ported.py: {len(found)} symbols, {len(unresolved)} annotations skipped "
          "(unresolved or not under pkg/):", file=sys.stderr)
    for u in unresolved:
        print("  " + u, file=sys.stderr)


if __name__ == "__main__":
    main()
