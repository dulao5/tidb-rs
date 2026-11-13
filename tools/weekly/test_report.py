#!/usr/bin/env python3
"""Tests of report.py's verdict (run by CI: `python3 tools/weekly/test_report.py`)."""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
OPS = {"Point_Get": {"match": 10, "mismatch": 0, "unsupported": 0, "both_error": 0}}


def classified(mismatch=0, unsupported=0):
    ops = {k: dict(v) for k, v in OPS.items()}
    ops["Point_Get"]["mismatch"] = mismatch
    return {"statements": 10 + mismatch + unsupported,
            "totals": {"match": 10, "mismatch": mismatch, "unsupported": unsupported, "both_error": 0},
            "operators": ops, "templates": {}}


def write(d, name, obj):
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "w") as f:
        json.dump(obj, f)


def report(inp, prev=None, parts="ab"):
    out = os.path.join(inp, "report.md")
    cmd = [sys.executable, os.path.join(HERE, "report.py"), "--in", inp, "--parts", parts, "--out", out]
    if prev:
        cmd += ["--previous", prev]
    rc = subprocess.run(cmd, capture_output=True, text=True).returncode
    with open(out) as f:
        return rc, f.readline().strip()


class VerdictTest(unittest.TestCase):
    def test_mismatch_plus_error_is_red(self):
        # One workload mismatched (exit 1), another errored (exit 2): run.sh may record the
        # part as incomplete, but the raw results hold a mismatch.
        with tempfile.TemporaryDirectory() as d:
            write(f"{d}/part-ab", "ab.json", {"part": "ab", "status": "incomplete", "exit": 2})
            write(f"{d}/part-ab", "ab-classified.json", classified(mismatch=1))
            rc, first = report(d)
            self.assertEqual(rc, 1)
            self.assertTrue(first.startswith("**RED**"), first)

    def test_faults_and_locks_raw_wrong_results_are_red(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "faults.json", {"part": "faults", "status": "ok"})
            write(d, "faults-raw.json", {"results": [{"scenario": "kill_tikv", "summary": {
                "A": {"point": {"wrong": 0, "errors": {}, "max_stall_ms": 1}},
                "B": {"point": {"wrong": 2, "errors": {}, "max_stall_ms": 1}}}}],
                "new_inconsistencies": [], "incomplete": []})
            write(d, "locks.json", {"part": "locks", "status": "ok"})
            write(d, "locks-raw.json", [{"case": "c", "shape": "s", "same": False}])
            rc, first = report(d, parts="faults,locks")
            self.assertEqual(rc, 1)
            self.assertTrue(first.startswith("**RED**"), first)

    def test_incomplete_and_green(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "ab.json", {"part": "ab", "status": "incomplete"})
            write(d, "ab-classified.json", classified())
            self.assertEqual(report(d)[0], 2)
        with tempfile.TemporaryDirectory() as d:
            write(d, "ab.json", {"part": "ab", "status": "ok"})
            write(d, "ab-classified.json", classified())
            rc, first = report(d)
            self.assertEqual(rc, 0)
            self.assertTrue(first.startswith("**GREEN**"), first)

    def test_more_unsupported_than_baseline_is_regressed(self):
        with tempfile.TemporaryDirectory() as d:
            write(f"{d}/cur", "ab.json", {"part": "ab", "status": "ok"})
            write(f"{d}/cur", "ab-classified.json", classified(unsupported=3))
            write(f"{d}/prev", "ab-classified.json", classified())
            rc, first = report(f"{d}/cur", f"{d}/prev")
            self.assertEqual(rc, 2)
            self.assertTrue(first.startswith("**REGRESSED**"), first)


if __name__ == "__main__":
    unittest.main()
