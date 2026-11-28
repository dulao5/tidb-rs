#!/usr/bin/env python3
"""Test-only dependency check (ADR 0007 point 3, test-only exception; owner decision 2026-11-28).

The vendored TiKV evaluator `tidb_query_expr` is kept only for the coprocessor evaluation
oracle (crates/query-smoke/tests/copr_eval_oracle.rs). It must never reach the production
build. This check reads `cargo metadata` and fails when any first-party workspace crate (one
not under third_party/) reaches a test-only crate through normal or build dependencies,
directly or through other workspace crates (for example a vendored crate that depends on it
normally). Dev-dependencies are allowed: they are not part of what a crate ships.

    scripts/check_test_only_deps.py              # check the workspace
    scripts/check_test_only_deps.py --print      # also list who uses each test-only crate
"""

import argparse
import json
import subprocess
import sys

# Crates that may only be used as dev-dependencies by first-party code.
TEST_ONLY = ["tidb_query_expr"]


def load_metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def is_third_party(pkg, root):
    rel = pkg["manifest_path"][len(root):].lstrip("/") if pkg["manifest_path"].startswith(root) else pkg["manifest_path"]
    return rel.startswith("third_party/")


def violations(meta, test_only=TEST_ONLY):
    """Returns one message per (first-party crate, test-only crate) reached outside dev-deps.

    Each message names the dependency chain, so the offending edge can be found.
    """
    root = meta["workspace_root"]
    pkgs = {p["name"]: p for p in meta["packages"]}
    # Production edges between workspace packages, and to test-only crates even when they are
    # not workspace members. A dependency's `name` is the package name (a rename is in `rename`).
    edges = {}
    for p in meta["packages"]:
        edges[p["name"]] = [
            d["name"] for d in p["dependencies"]
            if d.get("kind") != "dev" and (d["name"] in pkgs or d["name"] in test_only)
        ]
    out = []
    for name in sorted(pkgs):
        p = pkgs[name]
        if is_third_party(p, root) or name in test_only:
            continue
        # Breadth-first search with parents, to report the chain.
        parent = {name: None}
        queue = [name]
        while queue:
            cur = queue.pop(0)
            for nxt in edges.get(cur, []):
                if nxt in parent:
                    continue
                parent[nxt] = cur
                queue.append(nxt)
        for t in test_only:
            if t in parent:
                chain = [t]
                while parent[chain[-1]] is not None:
                    chain.append(parent[chain[-1]])
                out.append(
                    f"{name} reaches test-only crate {t} outside dev-dependencies: "
                    + " -> ".join(reversed(chain))
                )
    return out


def users(meta, test_only=TEST_ONLY):
    res = []
    for p in meta["packages"]:
        for d in p["dependencies"]:
            if d["name"] in test_only:
                res.append(f"{d['name']}: used by {p['name']} ({d.get('kind') or 'normal'})")
    return sorted(res)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--print", action="store_true", help="list every user of a test-only crate")
    args = ap.parse_args()
    meta = load_metadata()
    if args.print:
        for line in users(meta):
            print(line)
    errs = violations(meta)
    for e in errs:
        print(f"error: {e}", file=sys.stderr)
    if errs:
        print(f"test-only dependencies: {len(errs)} error(s)", file=sys.stderr)
        return 1
    print(f"test-only dependencies: ok ({', '.join(TEST_ONLY)} only in dev-dependencies of first-party crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
