#!/usr/bin/env python3
"""Tests of prototype_not_supported.py (run by CI)."""

import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import prototype_not_supported as pns  # noqa: E402

SRC = '''
fn f() -> Result<(), SqlError> {
    Err(unsupported("joins"))?;
    Err(not_supported(format!("function {name}")))?;
    Err(Error::Unsupported("table scan without columns".to_owned()))
}

#[derive(Debug, thiserror::Error)]
pub enum Unsupported {
    #[error("table `{0}` is partitioned; partitioned tables are not supported")]
    Partitioned(String),
    #[error("`{0}` is a view")]
    View(String),
    #[error("table `{0}` is not public (state {1:?})")]
    NotPublic(String, SchemaState),
}

#[derive(Debug, thiserror::Error)]
pub enum Other {
    #[error("not a fixed answer {0}")]
    X(String),
}
'''

ANSWER = "^This version of TiDB doesn't yet support '(?:{})'$"


class GeneratorTest(unittest.TestCase):
    def test_calls_and_unsupported_enum(self):
        got = pns.texts(SRC)
        self.assertEqual(got, [
            "joins",
            "function {name}",
            "table scan without columns",
            "table `{0}` is partitioned; partitioned tables are not supported",
            "`{0}` is a view",
            "table `{0}` is not public (state {1:?})",
        ])
        # Another enum's #[error] texts are not fixed answers.
        self.assertNotIn("not a fixed answer {0}", got)

    def test_patterns_match_the_answers(self):
        pats = [pns.pattern(t) for t in pns.texts(SRC)]
        rx = re.compile(ANSWER.format("|".join(pats)))
        for msg in (
            "table `test.tp` is partitioned; partitioned tables are not supported",
            "`cte.v1` is a view",
            "table `t` is not public (state WriteOnly)",
            "function date_add",
        ):
            self.assertTrue(rx.match(f"This version of TiDB doesn't yet support '{msg}'"), msg)
        self.assertFalse(rx.match("This version of TiDB doesn't yet support 'other'"))

    def test_the_committed_list_has_the_meta_reasons(self):
        # The W48 weekly (week 48's run) counted these as mismatches because crates/meta was
        # not scanned.
        text = pns.generate()
        for p in (r"`.*` is a view",
                  r"table `.*` is partitioned; partitioned tables are not supported",
                  r"table `.*` has a clustered non\-integer primary key \(common handle\)"):
            self.assertIn(p + "\n", text)


if __name__ == "__main__":
    unittest.main()
