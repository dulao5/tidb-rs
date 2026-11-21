#!/usr/bin/env python3
"""Tests of check_stub_ratchet.py (run by CI)."""

import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import check_stub_ratchet as sr  # noqa: E402

SRC = '''#[cfg(test)]
mod idiom_tests;

// glue: stub of Foo.Bar, blocked-by:D10.1 (the physical plans)
fn bar() -> Result<(), Error> {
    // Prose about blocked-by:D09.13 in an ordinary comment is not a stub.
    Err(tidb_base::blocked::blocked("D11.2", "stats loading"))
}

#[cfg(test)]
mod tests {
    fn t() { let _ = blocked("D10.2", "an index path"); }
}
'''


class RatchetTest(unittest.TestCase):
    def tree(self, d):
        files = {
            "crates/planner/src/a.rs": SRC,
            "crates/poc/session/src/b.rs": 'fn f() { blocked("D06.5", "SET GLOBAL"); }\n',
            "crates/planner/tests/c.rs": SRC,  # integration tests are not counted
            "crates/planner/src/x_tests.rs": SRC,
        }
        for rel, text in files.items():
            os.makedirs(os.path.dirname(os.path.join(d, rel)), exist_ok=True)
            with open(os.path.join(d, rel), "w") as f:
                f.write(text)
        return list(files)

    def test_counts_stubs_not_prose_or_tests(self):
        with tempfile.TemporaryDirectory() as d:
            found = sr.stubs(d, self.tree(d))
            self.assertEqual([(a, b) for a, _, _, b in found],
                             [("planner", "D10.1"), ("planner", "D11.2"), ("poc/session", "D06.5")])

    def test_only_down(self):
        base = {"planner": 2, "poc/session": 1}
        self.assertEqual(sr.compare({"planner": 2, "poc/session": 1}, base), ([], []))
        # A new stub over the baseline, or in a new area: red.
        self.assertEqual(sr.compare({"planner": 3, "poc/session": 1}, base)[0], [("planner", 2, 3)])
        self.assertEqual(sr.compare({"planner": 2, "poc/session": 1, "executor": 1}, base)[0],
                         [("executor", 0, 1)])
        # Fewer: the baseline must be lowered (red until it is).
        self.assertEqual(sr.compare({"planner": 1}, base)[1], [("planner", 2, 1), ("poc/session", 1, 0)])

    def test_baseline_round_trip(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "b.tsv")
            sr.write_baseline({"planner": 2, "poc/session": 1}, p)
            self.assertEqual(sr.read_baseline(p), {"planner": 2, "poc/session": 1})

    def test_shipped_baseline_matches_the_tree(self):
        r = subprocess.run([sys.executable, os.path.join(HERE, "check_stub_ratchet.py")], capture_output=True,
                           text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)


if __name__ == "__main__":
    unittest.main()
