#!/usr/bin/env python3
"""Tests of check_test_only_deps.py, including the cases that must go red (run by CI)."""

import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import check_test_only_deps as c  # noqa: E402

ROOT = "/ws"


def pkg(name, path, deps):
    return {
        "name": name,
        "manifest_path": f"{ROOT}/{path}/Cargo.toml",
        "dependencies": [{"name": n, "kind": k, "rename": r} for (n, k, r) in deps],
    }


def meta(*pkgs):
    return {"workspace_root": ROOT, "packages": list(pkgs)}


EXPR = pkg("tidb_query_expr", "third_party/tikv/tidb_query_expr", [])


class TestOnlyDeps(unittest.TestCase):
    def test_dev_dependency_is_allowed(self):
        m = meta(EXPR, pkg("tidb-query-smoke", "crates/query-smoke", [("tidb_query_expr", "dev", None)]))
        self.assertEqual(c.violations(m), [])

    def test_normal_dependency_fails(self):
        m = meta(EXPR, pkg("tidb-executor", "crates/executor", [("tidb_query_expr", None, None)]))
        errs = c.violations(m)
        self.assertEqual(len(errs), 1)
        self.assertIn("tidb-executor -> tidb_query_expr", errs[0])

    def test_build_dependency_fails(self):
        m = meta(EXPR, pkg("tidb-executor", "crates/executor", [("tidb_query_expr", "build", None)]))
        self.assertEqual(len(c.violations(m)), 1)

    def test_renamed_dependency_fails(self):
        m = meta(EXPR, pkg("tidb-executor", "crates/executor", [("tidb_query_expr", None, "rpn")]))
        self.assertEqual(len(c.violations(m)), 1)

    def test_not_a_member_still_fails(self):
        # The test-only crate need not be a workspace member to be caught.
        m = meta(pkg("tidb-executor", "crates/executor", [("tidb_query_expr", None, None)]))
        self.assertEqual(len(c.violations(m)), 1)

    def test_transitive_through_vendored_crate_fails(self):
        aggr = pkg("tidb_query_aggr", "third_party/tikv/tidb_query_aggr", [("tidb_query_expr", None, None)])
        m = meta(EXPR, aggr, pkg("tidb-executor", "crates/executor", [("tidb_query_aggr", None, None)]))
        errs = c.violations(m)
        self.assertEqual(len(errs), 1)
        self.assertIn("tidb-executor -> tidb_query_aggr -> tidb_query_expr", errs[0])

    def test_transitive_through_dev_edge_is_allowed(self):
        aggr = pkg("tidb_query_aggr", "third_party/tikv/tidb_query_aggr", [("tidb_query_expr", None, None)])
        m = meta(EXPR, aggr, pkg("tidb-query-smoke", "crates/query-smoke", [("tidb_query_aggr", "dev", None)]))
        self.assertEqual(c.violations(m), [])

    def test_vendored_crates_themselves_are_not_checked(self):
        aggr = pkg("tidb_query_aggr", "third_party/tikv/tidb_query_aggr", [("tidb_query_expr", None, None)])
        self.assertEqual(c.violations(meta(EXPR, aggr)), [])


if __name__ == "__main__":
    unittest.main()
