#!/usr/bin/env python3
"""The divergence ledger (docs/testing/rulings.toml; spec docs/testing/rulings.md).

    check_rulings.py [--ledger FILE] [--tests DIR]      schema check (CI)
    check_rulings.py --audit RUN [RUN ...] [--today D]  hit audit over full runs (weekly)

Schema check: the file uses only the TOML subset tools/ab/src/rulings.rs reads; every ruling
has the required fields with allowed values (approver `maintainer` and `expires` only for
cosmetic rulings, a date unless pending); regexes compile; a `test` names an existing test
function; a matched ruling's `example` is a statement its match rule covers (integrationtest
examples are looked up in the pinned test tree given with --tests, i.e.
`$(scripts/tidb-reference.sh)/tests/integrationtest/t`; without --tests only their form is
checked).

Audit: each RUN is a directory holding one full run's hits (the `rulings-hits.tsv` of each
tidb-ab part, summed, or the run's merged `rulings-run.tsv`), newest first. A ruling the
comparator can match that had no hit in each of the newest N runs listing it (N = [audit] runs)
fails. A run that does not list a ruling (older than the ruling) does not count, so a new
ruling is audited once N runs have seen it. An expired cosmetic ruling that still fires, or no
longer fires, is a warning.

Exit status: 0 ok, 1 problems found.
"""

import argparse
import datetime
import glob
import os
import re
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LEDGER = os.path.join(ROOT, "docs", "testing", "rulings.toml")
HITS_FILE = "rulings-hits.tsv"
# One full run's hits merged over its parts (written by tools/weekly/report.py, kept per run).
RUN_FILE = "rulings-run.tsv"

MATCHED_PLANES = ("rows", "order", "columns", "metadata", "error", "warnings", "affected_rows",
                  "shape", "b_error", "explain")
OTHER_PLANES = ("session", "protocol", "tool", "observability", "behaviour")
LAYERS = ("integrationtest", "workload", "none")
CLASSES = ("divergence", "cosmetic", "go-gap")
APPROVERS = ("zhigang-du", "maintainer", "pending-owner")
PENDING = "pending-owner"
REQUIRED = ("id", "name", "class", "plane", "layer", "source", "evidence", "rationale", "approver",
            "reopen")
MATCH_FIELDS = ("unit", "stmt", "detail", "example")
OPTIONAL = MATCH_FIELDS + ("test", "approved", "expires")

# The subset tools/ab/src/rulings.rs parses: tables, single-line strings, integers, comments.
SUBSET_LINE = re.compile(
    r"""^(\[audit\]|\[\[ruling\]\]|[A-Za-z0-9_]+ = ("(?:[^"\\]|\\["\\nt])*"|'[^']*'|[0-9]+))\s*(#.*)?$""")
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def load(path):
    with open(path, "rb") as f:
        raw = f.read()
    problems = []
    for n, line in enumerate(raw.decode("utf-8").splitlines(), 1):
        s = line.strip()
        if s and not s.startswith("#") and not SUBSET_LINE.match(s):
            problems.append(f"line {n}: outside the TOML subset the comparator reads: {s[:80]}")
    try:
        doc = tomllib.loads(raw.decode("utf-8"))
    except tomllib.TOMLDecodeError as e:
        return None, problems + [f"not valid TOML: {e}"]
    return doc, problems


def statements(path):
    """(first line, statement) of a mysql-tester .test or a ;-separated SQL file."""
    out, cur, start = [], [], None
    with open(path, encoding="utf-8", errors="replace") as f:
        for n, line in enumerate(f, 1):
            s = line.rstrip("\n")
            if not cur and (not s.strip() or s.lstrip().startswith(("#", "--"))):
                continue
            if not cur:
                start = n
            cur.append(s.strip())
            if s.rstrip().endswith(";"):
                out.append((start, " ".join(cur).rstrip(";").strip()))
                cur = []
    return out


def check_example(r, tests_dir):
    """Problems with a matched ruling's example (the statement its rule must cover)."""
    rid, ex = r["id"], r.get("example", "")
    m = re.fullmatch(r"(.+):(\d+)", ex)
    if not m:
        return [f"{rid}: example must be <test-or-path>:<line>, not {ex!r}"]
    unit, line = m.group(1), int(m.group(2))
    if r["layer"] == "integrationtest":
        if tests_dir is None:
            return []
        path = os.path.join(tests_dir, unit + ".test")
        name = unit
    else:
        path = os.path.join(ROOT, unit)
        name = os.path.splitext(os.path.basename(unit))[0]
    if not os.path.exists(path):
        return [f"{rid}: example {ex}: no such file {path}"]
    sql = dict(statements(path)).get(line)
    if sql is None:
        return [f"{rid}: example {ex}: no statement starts at line {line}"]
    problems = []
    if not re.search(r["unit"], name):
        problems.append(f"{rid}: unit {r['unit']!r} does not match its example's unit {name!r}")
    if not re.search(r["stmt"], sql):
        problems.append(f"{rid}: stmt {r['stmt']!r} does not match its example {ex}: {sql[:80]!r}")
    return problems


def check_test(rid, ref):
    m = re.fullmatch(r"([^:]+\.rs)::([A-Za-z_][A-Za-z0-9_]*)", ref)
    if not m:
        return [f"{rid}: test must be <path.rs>::<function>, not {ref!r}"]
    path = os.path.join(ROOT, m.group(1))
    if not os.path.exists(path):
        return [f"{rid}: test file {m.group(1)} does not exist"]
    with open(path, encoding="utf-8") as f:
        if not re.search(rf"\bfn {m.group(2)}\s*\(", f.read()):
            return [f"{rid}: no fn {m.group(2)} in {m.group(1)}"]
    return []


def check(doc, tests_dir=None):
    """Schema problems of a parsed ledger."""
    problems = []
    extra = set(doc) - {"audit", "ruling"}
    if extra:
        problems.append(f"unknown top-level keys: {sorted(extra)}")
    runs = doc.get("audit", {}).get("runs")
    # The weekly keeps each full run's hits 90 days: at most 12 weekly runs to look back on.
    if not isinstance(runs, int) or not 1 <= runs <= 12:
        problems.append("[audit] runs must be an integer from 1 to 12")
    if set(doc.get("audit", {})) - {"runs"}:
        problems.append(f"unknown [audit] keys: {sorted(set(doc['audit']) - {'runs'})}")
    rulings = doc.get("ruling", [])
    if not rulings:
        problems.append("no [[ruling]] entries")
    ids, names = set(), set()
    for i, r in enumerate(rulings, 1):
        rid = r.get("id", f"ruling #{i}")
        for k in REQUIRED:
            if not isinstance(r.get(k), str) or not r.get(k).strip():
                problems.append(f"{rid}: missing {k}")
        unknown = set(r) - set(REQUIRED) - set(OPTIONAL)
        if unknown:
            problems.append(f"{rid}: unknown fields {sorted(unknown)}")
        if not re.fullmatch(r"R[1-9][0-9]*", str(rid)):
            problems.append(f"{rid}: id must be R<n>")
        if rid in ids:
            problems.append(f"{rid}: duplicate id")
        ids.add(rid)
        name = r.get("name", "")
        if name and not re.fullmatch(r"[a-z0-9]+(-[a-z0-9]+)*", name):
            problems.append(f"{rid}: name must be lower-case words joined by '-'")
        if name in names:
            problems.append(f"{rid}: duplicate name {name}")
        names.add(name)
        cls, plane, layer, approver = (r.get(k) for k in ("class", "plane", "layer", "approver"))
        if cls not in CLASSES:
            problems.append(f"{rid}: class must be one of {CLASSES}")
        if plane not in MATCHED_PLANES + OTHER_PLANES:
            problems.append(f"{rid}: unknown plane {plane!r}")
        if layer not in LAYERS:
            problems.append(f"{rid}: layer must be one of {LAYERS}")
        if approver not in APPROVERS:
            problems.append(f"{rid}: approver must be one of {APPROVERS}")
        if approver == "maintainer" and cls != "cosmetic":
            problems.append(f"{rid}: only cosmetic rulings may be approved by a maintainer")
        if approver == PENDING:
            if "approved" in r:
                problems.append(f"{rid}: a pending ruling has no approval date")
        elif not DATE.match(str(r.get("approved", ""))):
            problems.append(f"{rid}: approved must be the approval date YYYY-MM-DD")
        if "expires" in r:
            if cls != "cosmetic":
                problems.append(f"{rid}: only cosmetic rulings may expire")
            if not DATE.match(str(r["expires"])):
                problems.append(f"{rid}: expires must be YYYY-MM-DD")
        if layer == "none":
            for k in MATCH_FIELDS:
                if k in r:
                    problems.append(f"{rid}: {k} without a test layer (layer = none)")
        elif layer in LAYERS:
            if plane not in MATCHED_PLANES:
                problems.append(f"{rid}: plane {plane!r} is not one the comparator reports")
            for k in ("unit", "stmt", "example"):
                if k not in r:
                    problems.append(f"{rid}: a matched ruling needs {k}")
            ok = True
            for k in ("unit", "stmt", "detail"):
                if k in r:
                    try:
                        re.compile(r[k])
                    except re.error as e:
                        problems.append(f"{rid}: {k}: {e}")
                        ok = False
            if ok and all(k in r for k in ("unit", "stmt", "example")):
                problems += check_example(r, tests_dir)
        if "test" in r:
            problems += check_test(rid, r["test"])
    return problems


def read_hits(run, name=HITS_FILE):
    """{id: hits} of one run: the file `run`, or the `name` files under the directory `run`,
    summed (one rulings-hits.tsv per tidb-ab part, or one merged rulings-run.tsv)."""
    out = {}
    files = [run] if os.path.isfile(run) else glob.glob(os.path.join(run, "**", name), recursive=True)
    for path in files:
        with open(path, encoding="utf-8") as f:
            header = f.readline().rstrip("\n").split("\t")
            for line in f:
                row = dict(zip(header, line.rstrip("\n").split("\t")))
                if row.get("id"):
                    out[row["id"]] = out.get(row["id"], 0) + int(row.get("hits") or 0)
    return out


def audit(doc, runs, today):
    """(errors, warnings, table) of the hit audit. `runs`: [{id: hits}], newest first."""
    n = doc["audit"]["runs"]
    errors, warnings, table = [], [], []
    for r in doc.get("ruling", []):
        if r.get("layer") == "none":
            continue
        rid = r["id"]
        seen = [run[rid] for run in runs if rid in run]
        last = seen[:n]
        table.append((rid, r.get("approver"), last))
        if len(last) >= n and sum(last) == 0:
            errors.append(f"{rid} ({r['name']}): no hit in the last {n} full runs: the divergence is "
                          f"gone or the match rule is wrong; remove the ruling or fix its rule")
        exp = r.get("expires")
        if exp and exp < today:
            if seen and seen[0] > 0:
                warnings.append(f"{rid} ({r['name']}): cosmetic ruling expired on {exp} and still fires "
                                f"({seen[0]} hits in the last run): fix the difference or renew it")
            else:
                warnings.append(f"{rid} ({r['name']}): cosmetic ruling expired on {exp}: remove it")
    return errors, warnings, table


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ledger", default=LEDGER)
    ap.add_argument("--tests", help="tests/integrationtest/t of the pinned reference tree")
    ap.add_argument("--audit", nargs="+", metavar="RUN", help="full runs, newest first")
    ap.add_argument("--today", default=datetime.date.today().isoformat())
    args = ap.parse_args()
    doc, problems = load(args.ledger)
    if doc is not None:
        problems += check(doc, args.tests)
    if problems:
        for p in problems:
            print(f"rulings: {p}", file=sys.stderr)
        print(f"rulings: {len(problems)} problem(s) in {args.ledger}", file=sys.stderr)
        return 1
    rulings = doc["ruling"]
    matched = sum(1 for r in rulings if r["layer"] != "none")
    pending = [r["id"] for r in rulings if r["approver"] == PENDING]
    print(f"rulings: {len(rulings)} rulings ({matched} matched by the comparator), "
          f"{len(pending)} pending the owner: {' '.join(pending) or '-'}")
    if not args.audit:
        return 0
    runs = [read_hits(r, RUN_FILE if glob.glob(os.path.join(r, "**", RUN_FILE), recursive=True)
                      else HITS_FILE) for r in args.audit]
    errors, warnings, table = audit(doc, runs, args.today)
    for rid, approver, last in table:
        print(f"  {rid} ({approver}): hits newest first {last}")
    for w in warnings:
        print(f"rulings: warning: {w}")
    for e in errors:
        print(f"rulings: {e}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
