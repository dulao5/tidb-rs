#!/usr/bin/env python3
"""Tests of faults.py's error classification (run by CI: python3 tools/ab/faults/test_faults.py)."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    import pymysql  # noqa: F401
except ImportError:  # CI's check job has no PyMySQL; classification does not use it.
    import types

    sys.modules["pymysql"] = types.ModuleType("pymysql")
import faults  # noqa: E402


class MySQLError(Exception):
    pass


# A message as tidb-rs returns it for a client-rust internal error (#142: the first published
# weekly run lost everything after the source path).
REAL = (
    "TiKV client error: [/home/runner/.cargo/git/checkouts/client-rust-6fc2d0a9e1f3b4c5/82fd763/"
    "src/pd/timestamp.rs:72]: TimestampRequest channel is closed"
)


class ClassifyTest(unittest.TestCase):
    def test_client_rust_internal_error_keeps_file_and_message(self):
        kind = faults.classify(MySQLError(1105, REAL))
        self.assertEqual(kind, "1105: TiKV client error: [timestamp.rs:N]: TimestampRequest channel is closed")

    def test_message_keeps_everything(self):
        self.assertEqual(faults.message(MySQLError(1105, REAL)), "1105: " + REAL)

    def test_kind_is_stable_across_line_numbers_and_paths(self):
        other = REAL.replace("timestamp.rs:72", "timestamp.rs:80").replace("/home/runner", "/root")
        self.assertEqual(faults.classify(MySQLError(1105, REAL)), faults.classify(MySQLError(1105, other)))

    def test_sample_is_recorded_once_per_kind(self):
        stats = faults.Stats()
        for i in range(3):
            e = MySQLError(1105, REAL.replace(":72]", f":{70 + i}]"))
            stats.record("B", "point-tso", False, False, faults.classify(e), 0.1, 0.1, sample=faults.message(e))
        s = stats.summary()["B"]["point-tso"]
        self.assertEqual(list(s["errors"].values()), [3])
        self.assertIn("timestamp.rs:70]: TimestampRequest channel is closed", list(s["error_samples"].values())[0])


if __name__ == "__main__":
    unittest.main()
