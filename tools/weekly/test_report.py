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


def report(inp, prev=None, parts="ab", expected=None):
    """Runs report.py; `expected` is the accepted integration-test mismatches (default: none)."""
    out = os.path.join(inp, "report.md")
    exp = os.path.join(inp, "it-expected.json")
    with open(exp, "w") as f:
        json.dump(expected or {}, f)
    cmd = [sys.executable, os.path.join(HERE, "report.py"), "--in", inp, "--parts", parts, "--out", out,
           "--it-expected", exp]
    if prev:
        cmd += ["--previous", prev]
    rc = subprocess.run(cmd, capture_output=True, text=True).returncode
    with open(out) as f:
        return rc, f.readline().strip()


def it_results(d, rows, status="ok"):
    """An integrationtest part: integrationtest.json and ab-integrationtest/results.jsonl from
    (test, verdict) pairs; the statement of the i-th pair is at line i."""
    write(d, "integrationtest.json", {"part": "integrationtest", "status": status, "tests": 2, "seconds": 60})
    os.makedirs(f"{d}/ab-integrationtest", exist_ok=True)
    with open(f"{d}/ab-integrationtest/results.jsonl", "w") as f:
        for i, (test, verdict) in enumerate(rows):
            r = {"template": test, "protocol": "text", "verdict": verdict, "query": "select 1", "line": i}
            if verdict != "match":
                r.update(kind="", detail="not supported yet: function 'x'")
            f.write(json.dumps(r) + "\n")


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

    def test_mismatch_in_results_jsonl_is_red_without_classified(self):
        # classify.py failed: no ab-classified.json, but tidb-ab's results hold a mismatch.
        with tempfile.TemporaryDirectory() as d:
            write(f"{d}/part-ab", "ab.json", {"part": "ab", "status": "incomplete"})
            os.makedirs(f"{d}/part-ab/ab-sysbench")
            with open(f"{d}/part-ab/ab-sysbench/results.jsonl", "w") as f:
                f.write(json.dumps({"template": "t", "protocol": "text", "verdict": "mismatch", "query": "q"}) + "\n")
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

    def test_tpch_query_no_longer_executable_is_regressed(self):
        with tempfile.TemporaryDirectory() as d:
            write(f"{d}/cur", "tpch.json", {"part": "tpch", "status": "ok"})
            write(f"{d}/cur", "tpch-time.json", {"Q6": {"a_ms": 1.0, "b_ms": None, "b_error": "1235: x"}})
            write(f"{d}/prev", "tpch-time.json", {"Q6": {"a_ms": 1.0, "b_ms": 2.0, "b_error": None}})
            rc, first = report(f"{d}/cur", f"{d}/prev", parts="tpch")
            self.assertEqual(rc, 2)
            self.assertTrue(first.startswith("**REGRESSED**"), first)


    def test_integrationtest_mismatch_is_red(self):
        # Even though run.sh said ok: the raw results hold a mismatch.
        with tempfile.TemporaryDirectory() as d:
            it_results(f"{d}/part-integrationtest", [("a/t", "match"), ("a/t", "mismatch")])
            rc, first = report(d, parts="integrationtest")
            self.assertEqual(rc, 1)
            self.assertTrue(first.startswith("**RED**"), first)

    def test_integrationtest_unsupported_is_not_a_failure(self):
        with tempfile.TemporaryDirectory() as d:
            it_results(d, [("a/t", "match"), ("a/t", "unsupported"), ("b/t", "unsupported")])
            rc, first = report(d, parts="integrationtest")
            self.assertEqual(rc, 0)
            self.assertTrue(first.startswith("**GREEN**"), first)

    def test_integrationtest_more_unsupported_than_baseline_is_regressed(self):
        with tempfile.TemporaryDirectory() as d:
            it_results(f"{d}/cur", [("a/t", "unsupported"), ("a/t", "unsupported")])
            it_results(f"{d}/prev", [("a/t", "match"), ("a/t", "unsupported")])
            rc, first = report(f"{d}/cur", f"{d}/prev", parts="integrationtest")
            self.assertEqual(rc, 2)
            self.assertTrue(first.startswith("**REGRESSED**"), first)

    def test_integrationtest_unsupported_in_a_new_test_is_not_regressed(self):
        # A test the baseline did not run (the selection changed) is not compared.
        with tempfile.TemporaryDirectory() as d:
            it_results(f"{d}/cur", [("a/t", "match"), ("b/t", "unsupported")])
            it_results(f"{d}/prev", [("a/t", "match")])
            rc, first = report(f"{d}/cur", f"{d}/prev", parts="integrationtest")
            self.assertEqual(rc, 0)
            self.assertTrue(first.startswith("**GREEN**"), first)

    def test_integrationtest_expected_mismatch_is_not_red(self):
        # run.sh says fail (ab exits 1 on any mismatch); the only mismatch is accepted.
        with tempfile.TemporaryDirectory() as d:
            it_results(d, [("a/t", "match"), ("a/t", "mismatch")], status="fail")
            rc, first = report(d, parts="integrationtest",
                               expected={"a/t:1": {"class": "bug", "reason": "r", "issue": "TODO"}})
            self.assertEqual(rc, 0)
            self.assertTrue(first.startswith("**GREEN**"), first)
            with open(f"{d}/report.md") as f:
                self.assertIn("**0 new**, 1 known", f.read())

    def test_integrationtest_unlisted_mismatch_is_red(self):
        with tempfile.TemporaryDirectory() as d:
            it_results(d, [("a/t", "mismatch"), ("a/t", "mismatch")], status="fail")
            rc, first = report(d, parts="integrationtest",
                               expected={"a/t:0": {"class": "nondeterministic", "reason": "r"}})
            self.assertEqual(rc, 1)
            self.assertTrue(first.startswith("**RED**"), first)

    def test_integrationtest_stale_expected_entry_is_reported(self):
        # a/t:0 ran and matched: listed as fixed, not a failure. b/t:5 did not run: not listed.
        with tempfile.TemporaryDirectory() as d:
            it_results(d, [("a/t", "match")])
            rc, first = report(d, parts="integrationtest", expected={
                "a/t:0": {"class": "bug", "reason": "r", "issue": "TODO"},
                "b/t:5": {"class": "bug", "reason": "r", "issue": "TODO"}})
            self.assertEqual(rc, 0)
            with open(f"{d}/report.md") as f:
                text = f.read()
            self.assertIn("Fixed, remove from integrationtest-expected.json: `a/t:0` (now match)", text)
            self.assertNotIn("b/t:5", text)

    def test_integrationtest_results_do_not_count_for_ab(self):
        # A mismatch of the integration tests must not turn the ab part red.
        with tempfile.TemporaryDirectory() as d:
            write(f"{d}/part-ab", "ab.json", {"part": "ab", "status": "ok"})
            write(f"{d}/part-ab", "ab-classified.json", classified())
            it_results(f"{d}/part-integrationtest", [("a/t", "mismatch")])
            rc, first = report(d, parts="ab")
            self.assertEqual(rc, 0)
            self.assertTrue(first.startswith("**GREEN**"), first)

    def test_shipped_integrationtest_expected_is_well_formed(self):
        # Only tidb-rs bugs (with an issue) and nondeterministic answers, each with a reason.
        path = os.path.join(HERE, "integrationtest-expected.json")
        with open(path) as f:
            entries = json.load(f)
        for key, e in entries.items():
            if key.startswith("_"):
                continue
            self.assertRegex(key, r"^[\w/.-]+:\d+$")
            self.assertIn(e.get("class"), ("bug", "nondeterministic"), key)
            self.assertTrue(e.get("reason"), key)
            if e["class"] == "bug":
                self.assertTrue(e.get("issue"), key)


if __name__ == "__main__":
    unittest.main()
