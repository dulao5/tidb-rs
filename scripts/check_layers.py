#!/usr/bin/env python3
"""Crate layering check (N0, specs/exceptions/N0-crate-layering/spec.md).

Reads layers.toml and `cargo metadata`, and fails if a workspace dependency edge
(normal, dev or build) breaks the layering:

  * a layered crate depends on a layered crate whose rank is not strictly lower;
  * a layered crate depends on a legacy (PoC) crate;
  * a layered crate depends on a third_party/ crate not allowed in [third_party.users];
  * a workspace member is not registered, or a crate marked `planned` already exists;
  * a block is assigned to two crates; with --blocks, also unknown block IDs and blocks of
    the listed phases that are in no crate and not in [unassigned].

Legacy crates may depend on anything (they adopt ported code while being replaced).
third_party/ crates are recognised by path.

    scripts/check_layers.py [--layers layers.toml] [--blocks ../tidb-rs-tracker/data/blocks.tsv]
    scripts/check_layers.py --print     # rank table of existing crates and their edges
    scripts/check_layers.py --block-edges ../tidb-rs-tracker/data/block_edges.tsv
        # informational: Go block references that go upward / sideways under this layering
        # (each must be handled by moving a type down or an interface inversion, N0 §4)
"""

import argparse
import collections
import csv
import json
import subprocess
import sys
import tomllib
from pathlib import Path


def load_metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--layers", default="layers.toml")
    ap.add_argument("--blocks", help="tracker data/blocks.tsv; checks that block IDs exist")
    ap.add_argument("--print", action="store_true", help="print the existing crates by rank")
    ap.add_argument("--block-edges", help="tracker data/block_edges.tsv; report upward Go references and exit")
    args = ap.parse_args()

    cfg = tomllib.loads(Path(args.layers).read_text())
    crates = cfg.get("crate", {})
    legacy = set(cfg.get("legacy", {}).get("crates", []))
    tp_users = cfg.get("third_party", {}).get("users", {})
    errors = []

    # Blocks: each in exactly one crate.
    owner = {}
    for name, c in crates.items():
        if not isinstance(c.get("rank"), int):
            errors.append(f"layers.toml: crate {name}: rank must be an integer")
        for b in c.get("blocks", []):
            if b in owner:
                errors.append(f"layers.toml: block {b} is in both {owner[b]} and {name}")
            owner[b] = name
    if args.blocks:
        with open(args.blocks, newline="") as f:
            phase = {row["block"]: row["phase"] for row in csv.DictReader(f, delimiter="\t")}
        for b in sorted(set(owner) - set(phase)):
            errors.append(f"layers.toml: block {b} ({owner[b]}) is not in {args.blocks}")
        un = cfg.get("unassigned", {})
        excluded = set(un.get("blocks", []))
        for b in sorted(excluded & set(owner)):
            errors.append(f"layers.toml: block {b} is both in [unassigned] and in crate {owner[b]}")
        for b in sorted(excluded - set(phase)):
            errors.append(f"layers.toml: [unassigned] block {b} is not in {args.blocks}")
        wanted = set(un.get("phases", []))
        for b in sorted(b for b, p in phase.items() if p in wanted and b not in owner and b not in excluded):
            errors.append(f"layers.toml: block {b} (phase {phase[b]}) is in no crate and not in [unassigned]")

    if args.block_edges:
        return report_block_edges(args.block_edges, crates, owner)

    meta = load_metadata()
    root = Path(meta["workspace_root"])
    members = set(meta["workspace_members"])
    pkgs = {p["name"]: p for p in meta["packages"] if p["id"] in members}
    by_dir = {Path(p["manifest_path"]).parent.resolve(): n for n, p in pkgs.items()}

    def kind(name):
        p = pkgs[name]
        rel = Path(p["manifest_path"]).relative_to(root)
        if rel.parts[0] == "third_party":
            return "third_party"
        if name in legacy:
            return "legacy"
        if name in crates:
            return "layered"
        return None

    for name in sorted(pkgs):
        k = kind(name)
        if k is None:
            errors.append(f"{name}: workspace member not registered in {args.layers}")
        elif k == "layered" and crates[name].get("planned"):
            errors.append(f"{name}: exists but is marked planned in {args.layers}; remove `planned`")
    for name in sorted(legacy - set(pkgs)):
        errors.append(f"layers.toml: legacy crate {name} is not a workspace member; remove it")

    edges = []
    for name in sorted(pkgs):
        if kind(name) != "layered":
            continue
        rank = crates[name]["rank"]
        for d in pkgs[name]["dependencies"]:
            # Workspace dependencies are path dependencies onto a workspace member. Match by
            # path, not by name: a crates.io or out-of-tree crate may share a member's name.
            dep = by_dir.get(Path(d["path"]).resolve()) if d.get("path") else None
            if dep is None:
                continue  # crates.io, git, or a path outside the workspace
            dk = kind(dep)
            label = f"{name} -> {dep}" + (f" ({d['kind']})" if d["kind"] else "")
            edges.append(label)
            if dk == "legacy":
                errors.append(f"{label}: layered crate depends on legacy (PoC) crate")
            elif dk == "third_party":
                if dep not in tp_users.get(name, []):
                    errors.append(f"{label}: third_party crate not allowed for {name} ([third_party.users])")
            elif dk == "layered":
                drank = crates[dep]["rank"]
                if drank >= rank:
                    what = "same rank" if drank == rank else "upward"
                    errors.append(f"{label}: {what} dependency (rank {rank} -> {drank})")

    if args.print:
        for name in sorted((n for n in pkgs if kind(n) == "layered"), key=lambda n: (crates[n]["rank"], n)):
            print(f"rank {crates[name]['rank']:>2}  {name}")
        for e in edges:
            print("  " + e)
        print("legacy: " + ", ".join(sorted(n for n in pkgs if kind(n) == "legacy")))

    for e in errors:
        print("ERROR: " + e)
    n_layered = sum(1 for n in pkgs if kind(n) == "layered")
    print(f"layers: {n_layered} layered, {len(legacy & set(pkgs))} legacy, {len(edges)} layered edges, "
          f"{len(errors)} error(s): {'FAIL' if errors else 'OK'}")
    return 1 if errors else 0


def report_block_edges(path, crates, owner):
    """Go block -> block references (tracker), aggregated per crate pair, that do not go downward."""
    down = 0
    bad = collections.Counter()
    detail = collections.defaultdict(list)
    with open(path, newline="") as f:
        for row in csv.DictReader(f, delimiter="\t"):
            a, b, n = owner.get(row["from"]), owner.get(row["to"]), int(row["refs"])
            if a is None or b is None or a == b:
                continue  # unassigned (X, util, ...) or intra-crate
            if crates[a]["rank"] > crates[b]["rank"]:
                down += n
            else:
                bad[(a, b)] += n
                detail[(a, b)].append((n, row["from"], row["to"]))
    total = down + sum(bad.values())
    print(f"cross-crate Go references: {total}, downward {down} ({100 * down / max(total, 1):.2f}%), "
          f"upward/same-rank {sum(bad.values())}")
    for (a, b), n in bad.most_common():
        same = " (same rank)" if crates[a]["rank"] == crates[b]["rank"] else ""
        top = ", ".join(f"{x}->{y}({m})" for m, x, y in sorted(detail[(a, b)], reverse=True)[:3])
        print(f"{n:>5}  {a} -> {b}{same}  {top}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
