#!/usr/bin/env python3
"""The weekly / nightly build report (#128): one Markdown page from the parts' JSON.

    report.py --in DIR [--previous DIR] [--title T] [--commit SHA] --out report.md

DIR holds what tools/weekly/run.sh wrote for each part (ab.json, ab-classified.json,
sysbench.json, faults.json, faults-raw.json, locks.json, locks-raw.json), possibly in
sub-directories (one per CI job). --previous is the same for last week's run: the report then
shows what changed (newly executable operators, agreement, QPS ratios).

Verdict (the report's first line, and the exit status):
- RED (1): a part found a wrong result, a mismatch or a new inconsistency;
- INCOMPLETE (2): a part did not run fully (skipped, failed to set up, missing);
- REGRESSED (2): against the previous run, an operator or a TPC-H query tidb-rs executed is no
  longer executable, `unsupported` statements increased, or a sysbench B/A ratio dropped by more
  than 15%;
- GREEN (0): none of these.
"""

import argparse
import glob
import json
import os

PARTS = ("ab", "sysbench", "faults", "locks", "tpch")


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


PERF_DROP = 0.15


def wrong_results(base):
    """Parts whose raw results show a wrong result, whatever their status says."""
    out = {}
    # Counted from tidb-ab's results.jsonl themselves, not from the classified summary (which
    # a failed classify.py would leave out). ab-tpch/ belongs to the tpch part.
    for path in glob.glob(os.path.join(base, "**", "results.jsonl"), recursive=True):
        part = "tpch" if os.path.basename(os.path.dirname(path)) == "ab-tpch" else "ab"
        with open(path) as f:
            n = sum(1 for line in f if line.strip() and json.loads(line).get("verdict") == "mismatch")
        if n:
            prev = int(out.get(part, "0 ").split()[0])
            out[part] = f"{prev + n} mismatches"
    # The classified summary too, in case only it survived.
    for part, name in (("ab", "ab-classified.json"), ("tpch", "tpch-classified.json")):
        c = load(base, name)
        if part not in out and c and c["totals"].get("mismatch", 0) > 0:
            out[part] = f"{c['totals']['mismatch']} mismatches"
    f = load(base, "faults-raw.json")
    if f:
        wrong = sum(v["wrong"] for r in f.get("results", []) for side in r.get("summary", {}).values()
                    for v in side.values())
        if wrong or f.get("new_inconsistencies"):
            out["faults"] = f"{wrong} wrong results, {len(f.get('new_inconsistencies', []))} new inconsistencies"
    locks = load(base, "locks-raw.json")
    if locks and any(not r.get("same", True) for r in locks):
        out["locks"] = "lock cases that differ"
    return out


def regressions(cur_ab, prev_ab, cur_sb, prev_sb, cur_tpch=None, prev_tpch=None):
    """What got worse against the previous run (each a short line)."""
    out = []
    if cur_tpch and prev_tpch:
        lost = sorted((q for q, r in prev_tpch.items() if r.get("b_ms") is not None
                       and cur_tpch.get(q, {}).get("b_ms") is None), key=lambda q: int(q[1:]))
        if lost:
            out.append("TPC-H queries no longer executable on tidb-rs: " + ", ".join(lost))
    if cur_ab and prev_ab:
        lost = executable(prev_ab) - executable(cur_ab)
        if lost:
            out.append("operators no longer executable on tidb-rs: " + ", ".join(f"`{o}`" for o in sorted(lost)))
        du = cur_ab["totals"]["unsupported"] - prev_ab["totals"]["unsupported"]
        if du > 0:
            out.append(f"`unsupported` statements up by {du} ({prev_ab['totals']['unsupported']} → "
                       f"{cur_ab['totals']['unsupported']})")
    if cur_sb and prev_sb and "results" in cur_sb and "results" in prev_sb:
        for w, rows in cur_sb["results"].items():
            for t in rows:
                r, p = ratio(rows, t), ratio(prev_sb["results"].get(w, {}), t)
                if r and p and r < p * (1 - PERF_DROP):
                    out.append(f"{w} {t} threads: B/A {p:.2f} → {r:.2f} (more than {PERF_DROP:.0%} lower)")
    return out


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
    lost_ops = executable(prev) - executable(cur)
    if prev:
        lines.append(
            "Newly executable operators since the last run: "
            + (", ".join(f"`{o}`" for o in sorted(new_ops)) if new_ops else "none")
            + ". No longer executable: "
            + (", ".join(f"**`{o}`**" for o in sorted(lost_ops)) if lost_ops else "none")
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
    lines.append("| workload | threads | Go TiDB QPS | tidb-rs QPS | B/A | last run |")
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


def section_tpch(classified, times, prev_times, lines):
    lines.append("## TPC-H (SF 1, queries 1-22)\n")
    if not times:
        lines.append("_Not run._\n")
        return
    if classified:
        t = classified["totals"]
        ran, agree = agreement(t)
        lines.append(
            f"tidb-ab: {t['match']} match, {t['mismatch']} **mismatch**, {t['both_error']} both error, "
            f"{t['unsupported']} unsupported (both protocols). Agreement on what tidb-rs executed: "
            f"{pct(agree, ran)}.\n"
        )
    runs = sum(1 for r in times.values() if r.get("b_ms") is not None)
    lines.append(f"tidb-rs executes {runs} / {len(times)} queries.\n")
    lines.append("| query | Go TiDB | tidb-rs | B/A | last run | tidb-rs error |")
    lines.append("|---|---|---|---|---|---|")
    for q, r in times.items():
        p = (prev_times or {}).get(q, {}).get("ratio_b_over_a")
        a = f"{r['a_ms']:.0f} ms" if r.get("a_ms") is not None else "—"
        b = f"{r['b_ms']:.0f} ms" if r.get("b_ms") is not None else "—"
        ratio_s = f"{r['ratio_b_over_a']:.2f}" if r.get("ratio_b_over_a") else "—"
        err = (r.get("b_error") or "").replace("|", "/")
        lines.append(f"| {q} | {a} | {b} | {ratio_s} | {f'{p:.2f}' if p else '—'} | {err} |")
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
    # A wrong result in the raw data is RED whatever a part's exit-code arithmetic said
    # (#140 review: a mismatch in one workload plus an error in another came out INCOMPLETE).
    for p, why in wrong_results(args.inp).items():
        if p in status:
            status[p] = "fail"
            print(f"{p}: {why}")

    cur_ab, prev_ab = load(args.inp, "ab-classified.json"), load(args.previous, "ab-classified.json")
    cur_sb, prev_sb = load(args.inp, "sysbench.json"), load(args.previous, "sysbench.json")
    regressed = regressions(cur_ab, prev_ab, cur_sb, prev_sb,
                            load(args.inp, "tpch-time.json"), load(args.previous, "tpch-time.json"))
    if any(s == "fail" for s in status.values()):
        verdict, code = "RED", 1
    elif any(s in ("incomplete", "missing") for s in status.values()):
        verdict, code = "INCOMPLETE", 2
    elif regressed:
        verdict, code = "REGRESSED", 2
    else:
        verdict, code = "GREEN", 0
    meaning = {
        "RED": "wrong results, mismatches or new inconsistencies",
        "INCOMPLETE": "some parts did not run fully; this is not a pass",
        "REGRESSED": "worse than the last run",
        "GREEN": "every part ran; no wrong results, no regressions",
    }[verdict]

    lines = [f"**{verdict}**: {meaning}.\n", f"# {args.title}\n"]
    if args.commit:
        lines.append(f"Commit `{args.commit}`.\n")
    lines.append("| part | status |")
    lines.append("|---|---|")
    icon = {"ok": "ok", "fail": "**FAIL**", "incomplete": "**INCOMPLETE**", "missing": "**MISSING**"}
    for p in expected:
        lines.append(f"| {p} | {icon.get(status[p], status[p])} |")
    lines.append("")
    if regressed:
        lines.append("Against the last run:")
        lines += [f"- {r}" for r in regressed]
        lines.append("")

    if "ab" in expected:
        section_ab(cur_ab, prev_ab, lines)
    if "sysbench" in expected:
        section_sysbench(cur_sb, prev_sb, lines)
    if "faults" in expected:
        section_faults(load(args.inp, "faults-raw.json"), lines)
    if "locks" in expected:
        section_locks(load(args.inp, "locks-raw.json"), lines)
    if "tpch" in expected:
        section_tpch(load(args.inp, "tpch-classified.json"), load(args.inp, "tpch-time.json"),
                     load(args.previous, "tpch-time.json"), lines)
    if not args.previous:
        lines.append("_No previous run to compare with._\n")

    with open(args.out, "w") as f:
        f.write("\n".join(lines))
    print("\n".join(lines))
    with open(os.path.join(os.path.dirname(os.path.abspath(args.out)), "verdict.txt"), "w") as f:
        f.write(verdict + "\n")
    raise SystemExit(code)


if __name__ == "__main__":
    main()
