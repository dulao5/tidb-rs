#!/usr/bin/env python3
"""Tests of scripts/check_one_translation.py. Run in CI next to the check.

    python3 scripts/test_check_one_translation.py
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True  # no __pycache__ in scripts/
sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_one_translation as c  # noqa: E402


def findings(files: dict[str, str]) -> dict[str, list[str]]:
    """Findings of a fake repository: {path under crates/: source} -> {key: [site files]}."""
    with tempfile.TemporaryDirectory() as d:
        for rel, src in files.items():
            p = Path(d) / "crates" / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(src, encoding="utf-8")
        old, c.ROOT = c.ROOT, d
        try:
            return {k.replace("\t", " "): [s.file for s in v] for k, v in c.findings().items()}
        finally:
            c.ROOT = old


DECIMAL = "//! go-file: pkg/types/mydecimal.go\n\n// go: MyDecimal.Round L1\npub fn round() {}\n"


class OneTranslation(unittest.TestCase):
    def test_single_translation_passes(self):
        self.assertEqual(findings({"types/src/mydecimal.rs": DECIMAL}), {})

    def test_function_in_two_crates(self):
        other = "//! go-file: pkg/types/mydecimal.go\n\n// go: MyDecimal.Round L1\npub fn round2() {}\n"
        got = findings({"types/src/mydecimal.rs": DECIMAL, "executor/src/x.rs": other})
        self.assertEqual(list(got), ["func pkg/types MyDecimal.Round"])

    def test_explicit_path_resolves_package(self):
        other = "//! go-file: pkg/executor/a.go\n\n// go: pkg/types/mydecimal.go MyDecimal.Round L1\nfn r() {}\n"
        self.assertEqual(list(findings({"types/src/mydecimal.rs": DECIMAL, "executor/src/x.rs": other})),
                         ["func pkg/types MyDecimal.Round"])

    def test_same_name_other_package_passes(self):
        other = "//! go-file: pkg/executor/a.go\n\n// go: MyDecimal.Round L1\nfn r() {}\n"
        self.assertEqual(findings({"types/src/mydecimal.rs": DECIMAL, "executor/src/x.rs": other}), {})

    def test_cfg_variants_are_one_site(self):
        src = ("//! go-file: pkg/types/a.go\n\n// go: F L1 [M-FMA]\n#[cfg(target_arch = \"aarch64\")]\nfn f() {}\n"
               "// go: F L1 [M-FMA]\n#[cfg(not(target_arch = \"aarch64\"))]\nfn f() {}\n")
        self.assertEqual(findings({"types/src/a.rs": src}), {})

    def test_same_method_name_in_two_impls_is_two_sites(self):
        src = ("//! go-file: pkg/types/a.go\n\nimpl A {\n    // go: A.New L1\n    fn new() {}\n}\n"
               "impl B {\n    // go: A.New L1\n    fn new() {}\n}\n")
        self.assertEqual(list(findings({"types/src/a.rs": src})), ["func pkg/types A.New"])

    def test_init_is_per_go_file(self):
        a = "//! go-file: pkg/types/a.go\n\n// go: init L1\nfn init_a() {}\n"
        b = "//! go-file: pkg/types/b.go\n\n// go: init L1\nfn init_b() {}\n"
        self.assertEqual(findings({"types/src/a.rs": a, "types/src/b.rs": b}), {})

    def test_go_dup_marks_a_deliberate_copy(self):
        other = ("//! go-file: pkg/types/mydecimal.go\n\n// go-dup: hot path copy, see bench\n"
                 "// go: MyDecimal.Round L1\npub fn round2() {}\n")
        self.assertEqual(findings({"types/src/mydecimal.rs": DECIMAL, "executor/src/x.rs": other}), {})

    def test_go_std_twice(self):
        a = "// go-std: bytes.Compare L1\nfn a() {}\n"
        b = "// go-std: bytes.Compare L1 (no allocation)\nfn b() {}\n"
        self.assertEqual(list(findings({"kv/src/a.rs": a, "store/src/b.rs": b})), ["std bytes.Compare"])

    def test_type_split_across_crates(self):
        a = "//! go-file: pkg/types/field_type.go\n\nimpl FieldType {\n    // go: FieldType.Clone L1\n    fn clone() {}\n}\n"
        b = "//! go-file: pkg/types/field_type.go\n\nimpl FieldType {\n    // go: FieldType.Equal L1\n    fn equal() {}\n}\n"
        got = findings({"parser/src/a.rs": a, "model/src/b.rs": b})
        self.assertIn("type-split pkg/types FieldType", got)

    def test_method_as_free_function_is_not_a_split(self):
        a = "//! go-file: pkg/types/field_type.go\n\nimpl FieldType {\n    // go: FieldType.Clone L1\n    fn clone() {}\n}\n"
        b = "//! go-file: pkg/types/field_type.go\n\n// go: FieldType.Equal L1\npub fn field_type_equal() {}\n"
        self.assertEqual(findings({"parser/src/a.rs": a, "planner/src/b.rs": b}), {})

    def test_type_defined_in_two_crates(self):
        a = "//! go-file: pkg/meta/model/column.go\n\npub struct ColumnInfo {}\n"
        b = "//! go-file: pkg/meta/model/column.go\n\n#[derive(Clone)]\npub struct ColumnInfo {}\n"
        self.assertEqual(list(findings({"model/src/a.rs": a, "infoschema/src/b.rs": b})),
                         ["type-def pkg/meta/model ColumnInfo"])

    def test_hand_written_stand_in_of_ported_type(self):
        a = "//! go-file: pkg/meta/model/column.go\n\npub struct ColumnInfo {}\n"
        b = "/// A column (Go: `model.ColumnInfo`).\n#[derive(Clone)]\npub struct ColumnInfo {}\n"
        self.assertEqual(findings({"model/src/a.rs": a, "meta/src/b.rs": b}),
                         {"type-def pkg/meta/model ColumnInfo": ["crates/model/src/a.rs", "crates/meta/src/b.rs"]})

    def test_associated_type_is_not_a_definition(self):
        a = "//! go-file: pkg/kv/a.go\n\npub struct Iter {}\n"
        b = "//! go-file: pkg/kv/b.go\n\nimpl Iterator for X {\n    type Iter = u8;\n}\n"
        self.assertEqual(findings({"kv/src/a.rs": a, "store/src/b.rs": b}), {})

    def test_spike_crate_is_skipped(self):
        other = "//! go-file: pkg/types/mydecimal.go\n\n// go: MyDecimal.Round L1\npub fn round2() {}\n"
        self.assertEqual(findings({"types/src/mydecimal.rs": DECIMAL, "spike/x/src/a.rs": other}), {})


if __name__ == "__main__":
    unittest.main()
