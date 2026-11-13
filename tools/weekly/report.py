#!/usr/bin/env python3
"""The weekly / nightly build report (#128): one Markdown page from the parts' JSON.

    report.py --in DIR [--previous DIR] [--title T] [--commit SHA] --out report.md

DIR holds what tools/weekly/run.sh wrote for each part (ab.json, ab-classified.json,
sysbench.json, faults.json, faults-raw.json, locks.json, locks-raw.json), possibly in
sub-directories (one per CI job). --previous is the same for last week's run: the report then
shows what changed (newly executable operators, agreement, QPS ratios).

Exit status: 1 (red) if any part found a wrong result or a new inconsistency, else 2 (yellow)
if any part is incomplete (skipped, failed to set up, or missing), else 0.
"""

import argparse
import glob
import json
import os

PARTS = ("ab", "sysbench", "faults", "locks")


def load(base, name):
    """The first `name` under `base` (the parts may sit in one sub-directory per CI job)."""
    if not base:
        return None
    hits = sorted(glob.glob(os.path.join(base, "**", name), recursive=True))
    if not hits:
        return None
    with open(hits[0]) as f:
        return json.load(f)


def pct(n, d):
    return f"{100 * n / d:.1f}%" if d else "—"


def agreement(c):
    """match + both_error over everything but unsupported (statements B could execute)."""
    ran = c["match"] + c["both_error"] + c["mismatch"]
    return ran, c["match"] + c["both_error"]


def executable(classified):
    """Operators B executed at least once (not only `unsupported`)."""
    if not classified:
        return set()
    return {op for op, c in classified["operators"].items() if agreement(c)[0] > 0}


def section_ab(cur, prev, lines):
    lines.append("## Result comparison (tools/ab)\n")
    if not cur:
        lines.append("_Not run._\n")
        return
    t = cur["totals"]
    ran, agree = agreement(t)
    lines.append(
        f"{cur['statements']} statements: {t['match']} match, {t['mismatch']} **mismatch**, "
        f"{t['both_error']} both error, {t['unsupported']} unsupported on tidb-rs. "
        f"Agreement on what tidb-rs executed: **{pct(agree, ran)}**.\n"
    )
    new_ops = executable(cur) - executable(prev)
    if prev:
        lines.append(
            "Newly executable operators this week: "
            + (", ".join(f"`{o}`" for o in sorted(new_ops)) if new_ops else "none")
            + ".\n"
        )
    lines.append("| operator (Go plan) | statements | agreement | mismatch | unsupported | change |")
    lines.append("|---|---|---|---|---|---|")
    for op, c in cur["operators"].items():
        ran, agree = agreement(c)
        total = ran + c["unsupported"]
        change = ""
        if prev and op in prev["operators"]:
            pr, pa = agreement(prev["operators"][op])
            if pr and ran:
                d = 100 * agree / ran - 100 * pa / pr
                change = f"{d:+.1f} pt" if abs(d) >= 0.05 else "="
        elif prev:
            change = "new"
        lines.append(f"| `{op}` | {total} | {pct(agree, ran)} | {c['mismatch']} | {c['unsupported']} | {change} |")
    lines.append("")


def ratio(rows, t):
    try:
        return float(rows[t]["B"]["qps"]) / float(rows[t]["A"]["qps"])
    except (KeyError, ValueError, ZeroDivisionError):
        return None


def section_sysbench(cur, prev, lines):
    lines.append("## Performance (sysbench, B/A QPS)\n")
    if not cur or "results" not in cur:
        lines.append("_Not run._\n")
        return
    lines.append("| workload | threads | Go TiDB QPS | tidb-rs QPS | B/A | last week |")
    lines.append("|---|---|---|---|---|---|")
    for w, rows in cur["results"].items():
        for t in sorted(rows, key=int):
            r = ratio(rows, t)
            p = ratio(prev["results"].get(w, {}), t) if prev and "results" in prev else None
            lines.append(
                f"| {w} | {t} | {rows[t].get('A', {}).get('qps', '—')} | {rows[t].get('B', {}).get('qps', '—')} | "
                f"{f'{r:.2f}' if r else '—'} | {f'{p:.2f}' if p else '—'} |"
            )
    lines.append("")


def section_faults(raw, lines):
    lines.append("## Fault scenarios (tools/ab/faults, 3 TiKV)\n")
    if not raw:
        lines.append("_Not run._\n")
        return
    lines.append("| scenario | wrong results | errors Go / tidb-rs | longest stall Go / tidb-rs |")
    lines.append("|---|---|---|---|")
    for r in raw["results"]:
        if "summary" not in r:
            lines.append(f"| {r['scenario']} | **skipped**: {r.get('skipped', '')} | | |")
            continue
        s = r["summary"]
        wrong = sum(v["wrong"] for side in s.values() for v in side.values())
        errs = [sum(sum(v["errors"].values()) for v in s.get(x, {}).values()) for x in "AB"]
        stall = [max((v["max_stall_ms"] for v in s.get(x, {}).values()), default=0) / 1000 for x in "AB"]
        lines.append(f"| {r['scenario']} | {wrong} | {errs[0]} / {errs[1]} | {stall[0]:.1f} s / {stall[1]:.1f} s |")
    for n in raw.get("new_inconsistencies", []):
        lines.append(f"- **new inconsistency**: {n}")
    for n in raw.get("incomplete", []):
        lines.append(f"- **incomplete**: {n}")
    lines.append("")


def section_locks(raw, lines):
    lines.append("## Lock resolution (tools/ab/locks)\n")
    if raw is None:
        lines.append("_Not run._\n")
        return
    same = sum(1 for r in raw if r["same"])
    lines.append(f"{same} / {len(raw)} lock cases read the same on both sides.")
    for r in raw:
        if not r["same"]:
            lines.append(f"- **mismatch**: {r['case']} / {r['shape']}")
    lines.append("")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="inp", required=True)
    ap.add_argument("--previous")
    ap.add_argument("--title", default="tidb-rs weekly build")
    ap.add_argument("--commit", default="")
    ap.add_argument("--parts", default=",".join(PARTS), help="parts expected in this run")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    expected = [p for p in args.parts.split(",") if p]
    status = {}
    for p in expected:
        j = load(args.inp, f"{p}.json")
        status[p] = j["status"] if j else "missing"

    lines = [f"# {args.title}\n"]
    if args.commit:
        lines.append(f"Commit `{args.commit}`.\n")
    lines.append("| part | status |")
    lines.append("|---|---|")
    icon = {"ok": "ok", "fail": "**FAIL**", "incomplete": "**INCOMPLETE**", "missing": "**MISSING**"}
    for p in expected:
        lines.append(f"| {p} | {icon.get(status[p], status[p])} |")
    lines.append("")

    if "ab" in expected:
        section_ab(load(args.inp, "ab-classified.json"), load(args.previous, "ab-classified.json"), lines)
    if "sysbench" in expected:
        section_sysbench(load(args.inp, "sysbench.json"), load(args.previous, "sysbench.json"), lines)
    if "faults" in expected:
        section_faults(load(args.inp, "faults-raw.json"), lines)
    if "locks" in expected:
        section_locks(load(args.inp, "locks-raw.json"), lines)
    if not args.previous:
        lines.append("_No previous run to compare with._\n")

    with open(args.out, "w") as f:
        f.write("\n".join(lines))
    print("\n".join(lines))
    if any(s == "fail" for s in status.values()):
        raise SystemExit(1)
    if any(s in ("incomplete", "missing") for s in status.values()):
        raise SystemExit(2)


if __name__ == "__main__":
    main()
