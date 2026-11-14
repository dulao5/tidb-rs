#!/usr/bin/env python3
"""The weekly / nightly build report (#128): one Markdown page from the parts' JSON.

    report.py --in DIR [--previous DIR] [--title T] [--commit SHA] --out report.md

DIR holds what tools/weekly/run.sh wrote for each part (ab.json, ab-classified.json,
sysbench.json, faults.json, faults-raw.json, locks.json, locks-raw.json, integrationtest.json and
ab-integrationtest/results.jsonl), possibly in
sub-directories (one per CI job). --previous is the same for last week's run: the report then
shows what changed (newly executable operators, agreement, QPS ratios).

Verdict (the report's first line, and the exit status):
- RED (1): a part found a wrong result, a mismatch or a new inconsistency;
- INCOMPLETE (2): a part did not run fully (skipped, failed to set up, missing);
- REGRESSED (2): against the previous run, an operator or a TPC-H query tidb-rs executed is no
  longer executable, `unsupported` statements increased (tidb-ab workloads, or the integration
  tests both runs ran), or a sysbench B/A ratio dropped by more than 15%;
- GREEN (0): none of these.
"""

import argparse
import glob
import json
import os
import re
from collections import Counter, defaultdict

PARTS = ("ab", "sysbench", "faults", "locks", "tpch", "integrationtest")
# tidb-ab output directories (results.jsonl) of parts other than `ab`.
RESULT_DIRS = {"ab-tpch": "tpch", "ab-integrationtest": "integrationtest"}
# Accepted integration-test mismatches (tidb-rs bugs with an issue, nondeterministic results),
# keyed by "test:line": not RED, reported as known. --it-expected overrides the path.
IT_EXPECTED = os.path.join(os.path.dirname(os.path.abspath(__file__)), "integrationtest-expected.json")


def it_expected(path=None):
    """{"test:line": {"class", "reason", "issue"?}} from the accepted-mismatches file."""
    path = path or IT_EXPECTED
    if not os.path.exists(path):
        return {}
    with open(path) as f:
        return {k: v for k, v in json.load(f).items() if not k.startswith("_")}


def it_key(r):
    return f"{r.get('template')}:{r.get('line')}"


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


def wrong_results(base, expected=None):
    """Parts whose raw results show a wrong result, whatever their status says. Integration-test
    mismatches listed in `expected` (it_expected()) do not count."""
    expected = expected or {}
    out = {}
    # Counted from tidb-ab's results.jsonl themselves, not from the classified summary (which
    # a failed classify.py would leave out). ab-tpch/ belongs to the tpch part,
    # ab-integrationtest/ to the integrationtest part.
    for path in glob.glob(os.path.join(base, "**", "results.jsonl"), recursive=True):
        part = RESULT_DIRS.get(os.path.basename(os.path.dirname(path)), "ab")
        with open(path) as f:
            rows = [json.loads(line) for line in f if line.strip()]
        n = sum(1 for r in rows if r.get("verdict") == "mismatch"
                and not (part == "integrationtest" and it_key(r) in expected))
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


def integration(base, expected=None):
    """Summary of the integrationtest part from tidb-ab's ab-integrationtest/results.jsonl:
    {"totals": {verdict: n}, "tests": {test: {verdict: n}}, "reasons": Counter (unsupported),
    "errors": Counter (b_error), "mismatches": [record] (not in `expected`), "known": [record]
    (in `expected`), "fixed": [(key, verdict)] (in `expected`, ran on tidb-rs, no longer a mismatch)}, or None
    when there is none."""
    expected = expected or {}
    if not base:
        return None
    hits = sorted(glob.glob(os.path.join(base, "**", "ab-integrationtest", "results.jsonl"), recursive=True))
    if not hits:
        return None
    totals, tests, reasons, errors, mismatches, known = Counter(), defaultdict(Counter), Counter(), Counter(), [], []
    ran = {}
    with open(hits[0]) as f:
        for line in f:
            if not line.strip():
                continue
            r = json.loads(line)
            v = r.get("verdict", "?")
            totals[v] += 1
            tests[r.get("template", "?")][v] += 1
            ran[it_key(r)] = v
            if v == "unsupported":
                reasons[reason(r.get("detail") or r.get("kind") or "")] += 1
            elif v == "b_error":
                errors[f"{r.get('kind', '')}: {reason(r.get('detail') or '')}"] += 1
            elif v == "mismatch":
                (known if it_key(r) in expected else mismatches).append(r)
    # Fixed: the statement ran on tidb-rs this time and did not mismatch (not merely skipped).
    seen = {it_key(r) for r in known}
    fixed = sorted((k, ran[k]) for k in expected if k in ran and k not in seen)
    return {"totals": totals, "tests": tests, "reasons": reasons, "errors": errors,
            "mismatches": mismatches, "known": known, "fixed": fixed}


def reason(detail):
    """An unsupported message without its specifics, to group by: tidb-rs's 1235 text
    ("This version of TiDB doesn't yet support '<what>'") reduced to <what>, with quoted
    table / column names and AST node numbers (`ExplainStmt#11`) taken out."""
    m = re.fullmatch(r"This version of TiDB doesn't yet support '(.*)'", detail.strip(), re.S)
    d = m.group(1) if m else detail
    d = re.sub(r"'[^']*'|`[^`]*`", "?", d)
    d = re.sub(r"#\d+", "", d)
    return " ".join(d.split())[:120] or "(no message)"


def it_unsupported_growth(cur, prev):
    """(previous, current) unsupported counts over the tests both runs ran, or None."""
    if not cur or not prev:
        return None
    common = set(cur["tests"]) & set(prev["tests"])
    if not common:
        return None
    return (sum(prev["tests"][t]["unsupported"] for t in common),
            sum(cur["tests"][t]["unsupported"] for t in common))


def regressions(cur_ab, prev_ab, cur_sb, prev_sb, cur_tpch=None, prev_tpch=None, cur_it=None, prev_it=None):
    """What got worse against the previous run (each a short line)."""
    out = []
    g = it_unsupported_growth(cur_it, prev_it)
    if g and g[1] > g[0]:
        out.append(f"integration tests: `unsupported` statements up by {g[1] - g[0]} ({g[0]} → {g[1]})")
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


def section_integration(status, cur, prev, lines, expected=None):
    lines.append("## TiDB integration tests (v8.5.8 tests/integrationtest, hybrid mode)\n")
    if not cur:
        lines.append("_Not run._\n")
        return
    t = cur["totals"]
    ran = t["match"] + t["mismatch"] + t["b_error"]
    status = status or {}
    lines.append(
        f"{status.get('tests', '?')} test files selected, {len(cur['tests'])} with reads sent to tidb-rs"
        + (f", {status['seconds'] // 60} min" if status.get("seconds") else "")
        + f". {sum(t.values())} reads sent to tidb-rs: {t['match']} match, {t['mismatch']} **mismatch**, "
        f"{t['b_error']} tidb-rs error, {t['unsupported']} unsupported. Agreement on what tidb-rs "
        f"executed (match / match + mismatch + error): **{pct(t['match'], ran)}**.\n"
    )
    lines.append(
        f"Mismatches: **{len(cur['mismatches'])} new**, {len(cur['known'])} known "
        f"(tools/weekly/integrationtest-expected.json"
        + (", " + ", ".join(f"{n} {c}" for c, n in sorted(Counter(
            (expected or {}).get(it_key(r), {}).get("class", "?") for r in cur["known"]).items()))
           if cur["known"] else "")
        + ").\n"
    )
    if cur["fixed"]:
        lines.append("Fixed, remove from integrationtest-expected.json: "
                     + ", ".join(f"`{k}` (now {v})" for k, v in cur["fixed"]) + ".\n")
    g = it_unsupported_growth(cur, prev)
    if g:
        lines.append(f"Unsupported over the tests both runs ran: {g[0]} last run, {g[1]} now.\n")
    if cur["mismatches"]:
        lines.append("New mismatches:\n")
        for r in cur["mismatches"][:20]:
            q = " ".join(r.get("query", "").split())[:100].replace("|", "/")
            lines.append(f"- `{r.get('template')}:{r.get('line', '?')}` ({r.get('kind', '')}) `{q}`")
        if len(cur["mismatches"]) > 20:
            lines.append(f"- … {len(cur['mismatches']) - 20} more (ab-integrationtest/mismatches.jsonl)")
        lines.append("")
    for title, counter in (("top unsupported reasons", cur["reasons"]), ("top tidb-rs errors", cur["errors"])):
        if counter:
            lines.append(f"| {title} | statements |")
            lines.append("|---|---|")
            for why, n in counter.most_common(10):
                lines.append(f"| {why.replace('|', '/')} | {n} |")
            lines.append("")
    lines.append("| test | reads | match | mismatch | error | unsupported |")
    lines.append("|---|---|---|---|---|---|")
    top = sorted(cur["tests"].items(), key=lambda kv: (-kv[1]["mismatch"], -kv[1]["unsupported"], kv[0]))
    for name, c in top[:15]:
        lines.append(f"| {name} | {sum(c.values())} | {c['match']} | {c['mismatch']} | {c['b_error']} | "
                     f"{c['unsupported']} |")
    if len(top) > 15:
        lines.append(f"| … {len(top) - 15} more | | | | | |")
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
    ap.add_argument("--it-expected", help=f"accepted integration-test mismatches (default {IT_EXPECTED})")
    args = ap.parse_args()

    expected = [p for p in args.parts.split(",") if p]
    status = {}
    for p in expected:
        j = load(args.inp, f"{p}.json")
        status[p] = j["status"] if j else "missing"
    # A wrong result in the raw data is RED whatever a part's exit-code arithmetic said
    # (#140 review: a mismatch in one workload plus an error in another came out INCOMPLETE).
    accepted = it_expected(args.it_expected)
    # The integrationtest part's own status counts every mismatch (ab exits 1); here only the
    # ones not accepted in the expected list do.
    if status.get("integrationtest") == "fail" and integration(args.inp) is not None:
        status["integrationtest"] = "ok"
    for p, why in wrong_results(args.inp, accepted).items():
        if p in status:
            status[p] = "fail"
            print(f"{p}: {why}")

    cur_ab, prev_ab = load(args.inp, "ab-classified.json"), load(args.previous, "ab-classified.json")
    cur_sb, prev_sb = load(args.inp, "sysbench.json"), load(args.previous, "sysbench.json")
    cur_it, prev_it = integration(args.inp, accepted), integration(args.previous, accepted)
    regressed = regressions(cur_ab, prev_ab, cur_sb, prev_sb,
                            load(args.inp, "tpch-time.json"), load(args.previous, "tpch-time.json"),
                            cur_it, prev_it)
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
    if "integrationtest" in expected:
        section_integration(load(args.inp, "integrationtest.json"), cur_it, prev_it, lines, accepted)
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
