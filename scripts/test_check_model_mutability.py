#!/usr/bin/env python3
"""Tests of scripts/check_model_mutability.py (N13 §3 rule 3). Run in CI next to the check.

    python3 scripts/test_check_model_mutability.py
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True  # no __pycache__ in scripts/
sys.path.insert(0, str(Path(__file__).resolve().parent))

from check_model_mutability import check_file  # noqa: E402


def errors(src: str) -> list[str]:
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "x.rs"
        p.write_text(src, encoding="utf-8")
        return check_file(p)


class CheckModelMutability(unittest.TestCase):
    def test_struct_field_fails(self):
        for ty in ["RefCell<u8>", "Cell<u8>", "std::sync::Mutex<u8>", "AtomicU64", "OnceLock<u8>"]:
            with self.subTest(ty=ty):
                self.assertEqual(len(errors(f"pub struct A {{\n    pub x: {ty},\n}}\n")), 1)

    def test_single_line_static_fails_unless_marked(self):
        self.assertEqual(len(errors("static S: Mutex<u8> = Mutex::new(0);\n")), 1)
        self.assertEqual(len(errors("pub static F: AtomicBool = AtomicBool::new(false);\n")), 1)
        # An init-once static wrapping interior mutability still fails.
        self.assertEqual(len(errors("static S: LazyLock<Mutex<u8>> = LazyLock::new(|| Mutex::new(0));\n")), 1)
        marked = "static S: Mutex<u8> = Mutex::new(0); // N13-allow: process-wide cache\n"
        self.assertEqual(errors(marked), [])

    def test_init_once_static_passes(self):
        self.assertEqual(errors("pub static N: LazyLock<CIStr> = LazyLock::new(|| CIStr::new(\"a\"));\n"), [])
        self.assertEqual(errors("pub static N: LazyLock<Vec<u8>> =\n    LazyLock::new(|| vec![1]);\n"), [])

    def test_marker_passes(self):
        self.assertEqual(errors("pub struct A {\n    pub x: RefCell<u8>, // N13-allow: reviewed\n}\n"), [])

    def test_comments_and_use_pass(self):
        src = (
            "use std::sync::{Arc, Mutex};\n"
            "// a Mutex in a line comment\n"
            "/// a RefCell in a doc comment\n"
            "/* a Cell in a block comment */ pub struct A;\n"
            "/*\n * Go's sync.Mutex guards this\n * AtomicU64 too\n */\n"
            "pub struct B { pub x: u8 }\n"
        )
        self.assertEqual(errors(src), [])
        # Code after a closed block comment on the same line is still checked.
        self.assertEqual(len(errors("/* ok */ pub struct C { pub x: Cell<u8> }\n")), 1)

    def test_repository_scope_passes(self):
        from check_model_mutability import REPO, SCOPE

        for p in SCOPE:
            root = REPO / p
            files = [root] if root.is_file() else sorted(root.rglob("*.rs"))
            self.assertTrue(files, p)
            for f in files:
                self.assertEqual(check_file(f), [], f)


if __name__ == "__main__":
    unittest.main()
