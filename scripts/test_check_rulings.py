#!/usr/bin/env python3
"""Tests of check_rulings.py, including the checks that must go red (run by CI)."""

import os
import subprocess
import sys
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import check_rulings as cr  # noqa: E402

BASE = """
[audit]
runs = 3
"""

MATCHED = """
[[ruling]]
id = "R1"
name = "a-matched-one"
class = "cosmetic"
plane = "error"
layer = "integrationtest"
unit = '^executor/foo$'
stmt = '(?i)^select @@x'
example = "executor/foo:2"
source = "s"
evidence = "e"
rationale = "r"
approver = "maintainer"
approved = "2026-11-21"
reopen = "o"
"""

UNMATCHED = """
[[ruling]]
id = "R2"
name = "not-matched"
class = "divergence"
plane = "behaviour"
layer = "none"
source = "s"
evidence = "e"
rationale = "r"
approver = "pending-owner"
reopen = "o"
"""


def problems(text, tests_dir=None):
    with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as f:
        f.write(text)
    try:
        doc, p = cr.load(f.name)
        return p + (cr.check(doc, tests_dir) if doc is not None else [])
    finally:
        os.unlink(f.name)


def tests_tree(d):
    os.makedirs(f"{d}/executor")
    with open(f"{d}/executor/foo.test", "w") as f:
        f.write("# a comment\nselect @@x;\nselect\n  2;\n")
    return d


class SchemaTest(unittest.TestCase):
    def test_shipped_ledger_is_well_formed(self):
        r = subprocess.run([sys.executable, os.path.join(HERE, "check_rulings.py")], capture_output=True,
                           text=True)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_good_ledger(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(problems(BASE + MATCHED + UNMATCHED, tests_tree(d)), [])

    def test_bad_fields_are_refused(self):
        cases = {
            # Only cosmetic rulings may expire or be approved by a maintainer.
            "only cosmetic rulings may expire": UNMATCHED.replace('reopen = "o"', 'reopen = "o"\nexpires = "2027-01-01"'),
            "only cosmetic rulings may be approved by a maintainer":
                UNMATCHED.replace('approver = "pending-owner"', 'approver = "maintainer"\napproved = "2026-11-21"'),
            "a pending ruling has no approval date": UNMATCHED.replace('reopen = "o"', 'reopen = "o"\napproved = "2026-11-21"'),
            "approved must be the approval date": MATCHED.replace('approved = "2026-11-21"\n', ""),
            "missing reopen": UNMATCHED.replace('reopen = "o"\n', ""),
            "unknown plane": UNMATCHED.replace('"behaviour"', '"vibes"'),
            "is not one the comparator reports": MATCHED.replace('plane = "error"', 'plane = "session"'),
            "a matched ruling needs example": MATCHED.replace('example = "executor/foo:2"\n', ""),
            "without a test layer": UNMATCHED.replace('layer = "none"', "layer = \"none\"\nstmt = 'x'"),
            "stmt:": MATCHED.replace("'(?i)^select @@x'", "'(unclosed'"),
            "id must be R<n>": UNMATCHED.replace('"R2"', '"X2"'),
            "no fn nope": UNMATCHED.replace('reopen = "o"', 'reopen = "o"\ntest = "scripts/check_rulings.rs::nope"')
                .replace("check_rulings.rs", "../tools/ab/src/rulings.rs"),
        }
        for want, body in cases.items():
            with self.subTest(want):
                p = problems(BASE + body)
                self.assertTrue(any(want in x for x in p), p)

    def test_duplicate_ids(self):
        p = problems(BASE + UNMATCHED + UNMATCHED.replace("not-matched", "other"))
        self.assertTrue(any("duplicate id" in x for x in p), p)

    def test_outside_the_subset_the_comparator_reads(self):
        for body in ('evidence = """multi\nline"""', 'evidence = ["a", "b"]', "evidence = { a = 1 }"):
            with self.subTest(body):
                p = problems(BASE + UNMATCHED.replace('evidence = "e"', body))
                self.assertTrue(any("outside the TOML subset" in x for x in p), p)

    def test_example_must_be_covered_by_the_rule(self):
        # The negative control of "a ruling that can never match": its example (a statement of
        # the pinned test tree) is not covered by its own rule.
        with tempfile.TemporaryDirectory() as d:
            tests_tree(d)
            p = problems(BASE + MATCHED.replace("'(?i)^select @@x'", "'^select @@never'"), d)
            self.assertTrue(any("does not match its example" in x for x in p), p)
            p = problems(BASE + MATCHED.replace("executor/foo:2", "executor/foo:4"), d)
            self.assertTrue(any("no statement starts at line 4" in x for x in p), p)
            p = problems(BASE + MATCHED.replace("executor/foo:2", "executor/gone:2"), d)
            self.assertTrue(any("no such file" in x for x in p), p)


class AuditTest(unittest.TestCase):
    def doc(self, body=MATCHED):
        return tomllib.loads(BASE + body + UNMATCHED)

    def test_zero_hits_over_n_runs_is_red(self):
        # The negative control: a ruling that never matches over the last 3 full runs.
        errors, _, table = cr.audit(self.doc(), [{"R1": 0}, {"R1": 0}, {"R1": 0}, {"R1": 9}], "2026-11-21")
        self.assertEqual(len(errors), 1)
        self.assertIn("R1", errors[0])
        self.assertEqual(table, [("R1", "maintainer", [0, 0, 0])])

    def test_a_hit_in_the_window_passes(self):
        errors, _, _ = cr.audit(self.doc(), [{"R1": 0}, {"R1": 1}, {"R1": 0}], "2026-11-21")
        self.assertEqual(errors, [])

    def test_runs_before_the_ruling_do_not_count(self):
        # Only two runs list R1 (it is newer than the third): not audited yet.
        errors, _, _ = cr.audit(self.doc(), [{"R1": 0}, {"R1": 0}, {}], "2026-11-21")
        self.assertEqual(errors, [])

    def test_unmatched_rulings_are_not_audited(self):
        errors, _, table = cr.audit(self.doc(), [{}, {}, {}], "2026-11-21")
        self.assertEqual([t[0] for t in table], ["R1"])

    def test_expired_cosmetic_ruling_that_still_fires_warns(self):
        doc = self.doc(MATCHED.replace('reopen = "o"', 'reopen = "o"\nexpires = "2026-11-01"'))
        _, warnings, _ = cr.audit(doc, [{"R1": 4}], "2026-11-21")
        self.assertTrue(any("expired on 2026-11-01 and still fires" in w for w in warnings), warnings)
        _, warnings, _ = cr.audit(doc, [{"R1": 4}], "2026-10-31")
        self.assertEqual(warnings, [])

    def test_cli_audit_reads_hits_files(self):
        with tempfile.TemporaryDirectory() as d:
            ledger = os.path.join(d, "rulings.toml")
            with open(ledger, "w") as f:
                f.write(BASE + MATCHED + UNMATCHED)
            runs = []
            for i in range(3):
                run = os.path.join(d, f"run{i}", "part-integrationtest", "ab-integrationtest")
                os.makedirs(run)
                with open(os.path.join(run, cr.HITS_FILE), "w") as f:
                    f.write("id\tstatus\tlayer\thits\nR1\tapproved\tintegrationtest\t0\n")
                runs.append(os.path.join(d, f"run{i}"))
            r = subprocess.run([sys.executable, os.path.join(HERE, "check_rulings.py"), "--ledger", ledger,
                                "--audit", *runs], capture_output=True, text=True)
            self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
            self.assertIn("no hit in the last 3 full runs", r.stderr)


if __name__ == "__main__":
    unittest.main()
