#!/usr/bin/env python3
"""Unit tests for scripts/check_log_parity.py."""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import check_log_parity as clp


class TestCheckLogParity(unittest.TestCase):
    def test_skips_inline_cfg_test_without_truncating_trailing_prod_code(self) -> None:
        src = """
fn before() {
    tidb_base::log_info!("clean message");
}

#[cfg(test)]
mod tests {
    #[test]
    fn ok_in_test() {
        println!("allowed in test");
        let _ = std::panic::catch_unwind(|| 1);
    }
}

fn after_test_module() {
    eprintln!("caught after cfg(test) module");
}
"""
        violations, _ = clp.scan_rust_file("tidb-test", "crates/test/src/lib.rs", src)
        self.assertEqual(len(violations), 1)
        self.assertEqual(violations[0].rule, "A")
        self.assertIn("eprintln!", violations[0].detail)

    def test_rule_a_forbids_bare_catch_unwind_but_allows_panic_guard_and_skip(self) -> None:
        src = """
fn a() {
    let _ = tidb_base::panic_guard::catch_unwind_silent(|| 1);
    // log-skip: sanctioned test helper
    eprintln!("ok");
    let _ = std::panic::catch_unwind(|| 2);
}
"""
        violations, _ = clp.scan_rust_file("tidb-test", "crates/test/src/lib.rs", src)
        self.assertEqual(len(violations), 1)
        self.assertEqual(violations[0].rule, "A")
        self.assertIn("catch_unwind", violations[0].detail)

    def test_rule_b_flags_interpolated_messages_and_allows_structured(self) -> None:
        src = """
fn f(err: &str, conn: u64) {
    tidb_base::log_warn!("compile SQL failed", "conn" => conn, "error" => err);
    log::warn!("compile SQL failed: {err}");
    tracing::warn!(conn = conn, "run statement failed: schemaVersion={}", 1);
    tidb_base::log_error!(&format!("bad {err}"));
}
"""
        violations, _ = clp.scan_rust_file("tidb-test", "crates/test/src/lib.rs", src)
        rule_b = [v for v in violations if v.rule == "B"]
        self.assertEqual(len(rule_b), 3)

    def test_string_and_comment_braces_do_not_break_test_range_detection(self) -> None:
        src = """
#[cfg(test)]
mod tests {
    fn helper() {
        let _ = "}}} not closing brace";
        // } comment brace
        println!("still inside test");
    }
}
fn prod() {
    tidb_base::log_info!("ok");
}
"""
        violations, _ = clp.scan_rust_file("tidb-test", "crates/test/src/lib.rs", src)
        self.assertEqual(violations, [])

    def test_extract_go_function_logs_supports_generic_receiver(self) -> None:
        go_src = """
package demo

func (c *Cache[T]) Reload() {
    logutil.BgLogger().Warn("reload failed", zap.Error(err), zap.Stack("stack"))
    log.Info("reload done")
}
"""
        calls = clp.extract_go_function_logs(go_src, "Cache.Reload")
        self.assertIsNotNone(calls)
        assert calls is not None
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], ("warn", "reload failed", True))
        self.assertEqual(calls[1], ("info", "reload done", False))

    def test_scan_rust_extracts_log_target_err_verbose_and_stack_field(self) -> None:
        src = """//! go-file: pkg/demo/demo.go
// go: Demo.Run
fn run() {
    tidb_base::log_target!(tidb_base::structured_log::LogTarget::Main, tidb_base::structured_log::LogLevel::Error, "panic caught", "stack" => "bt");
    tidb_base::log_err_verbose!(tidb_base::structured_log::LogLevel::Warn, "verbose msg", "k" => 1);
}
"""
        violations, sym_calls = clp.scan_rust_file("tidb-test", "crates/test/src/lib.rs", src)
        self.assertEqual(violations, [])
        calls = sym_calls[("pkg/demo/demo.go", "Demo.Run")]
        self.assertEqual(calls, [("error", "panic caught", True), ("warn", "verbose msg", False)])


if __name__ == "__main__":
    unittest.main()
