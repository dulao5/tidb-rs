//! Whole-line golden parity tests for D00.3 Batch 1 structured logs in `tidb-parser` and
//! `tidb-types`. Dedicated integration test binary (installs global `MemorySink`).

use std::borrow::Cow;
use std::sync::Arc;

use tidb_base::config::LogConfig;
use tidb_base::panic_guard::catch_unwind_silent;
use tidb_log::sink::MemorySink;
use tidb_types::collate;
use tidb_types::datum::Datum;
use tidb_types::json::BinaryJSON;
use tidb_types::mydecimal::new_dec_from_string_for_test;

fn normalize_caller(line: &str) -> String {
    assert!(
        !line.contains('\n') && !line.contains('\r'),
        "log line must be single-line: {line:?}"
    );
    // Format with disable_timestamp = true: `[LEVEL] [file.rs:123] ...`
    let Some((level_part, rest)) = line.split_once("] [") else {
        return line.to_owned();
    };
    let Some((caller, tail)) = rest.split_once("] ") else {
        return line.to_owned();
    };
    let Some((file, line_no)) = caller.rsplit_once(':') else {
        return line.to_owned();
    };
    assert!(
        !line_no.is_empty() && line_no.bytes().all(|b| b.is_ascii_digit()),
        "expected numeric caller line in {line:?}"
    );
    format!("{level_part}] [{file}:<LINE>] {tail}")
}

fn normalize_caller_and_stack(line: &str) -> String {
    let base = normalize_caller(line);
    let (prefix, stack_quoted) = base
        .split_once(" [stack=\"")
        .unwrap_or_else(|| panic!("missing [stack=\"...\"] field in: {base}"));
    let stack_body = stack_quoted
        .strip_suffix("\"]")
        .unwrap_or_else(|| panic!("stack must be the final field in: {base}"));
    assert!(
        !stack_body.is_empty() && stack_body.contains("\\n\\t"),
        "expected non-empty Go-style \\n\\t stack frames in: {stack_body:?}"
    );
    format!("{prefix} [stack=\"<STACK>\"]")
}

#[test]
fn parser_and_types_structured_logs_match_go_whole_line() {
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "debug".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    let log_sink = Arc::clone(&sink);
    tidb_log::init_logger_with_sinks(&cfg, "", log_sink, None).unwrap();

    // 1. tidb-parser: GetCharsetInfoByID (Go pkg/parser/charset/charset.go:186-189)
    let _ = tidb_parser::gocharset::get_charset_info_by_id(9999);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller_and_stack(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"unable to get collation name from collation ID, return default charset and collation instead\"] [ID=9999] [stack=\"<STACK>\"]"
    );

    // 2. tidb-types::collate: RewriteNewCollationIDIfNeeded & RestoreCollationIDIfNeeded
    assert_eq!(collate::rewrite_new_collation_id_if_needed(-46), -46);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"Unexpected negative collation ID for rewrite.\"] [ID=-46]"
    );

    assert_eq!(collate::restore_collation_id_if_needed(46), 46);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"Unexpected positive collation ID for restore.\"] [ID=46]"
    );

    // 3. tidb-types::collate: GetCollator & GetCollatorByID (with stack)
    let _ = collate::get_collator("bogus_coll");
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller_and_stack(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"Unable to get collator by name, use binCollator instead.\"] [name=bogus_coll] [stack=\"<STACK>\"]"
    );

    let _ = collate::get_collator_by_id(9999);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller_and_stack(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"Unable to get collator by ID, use binCollator instead.\"] [ID=9999] [stack=\"<STACK>\"]"
    );

    // 4. tidb-types::collate: SubstituteMissingCollationToDefault, CollationToProto, ProtoToCollation
    let _ = collate::substitute_missing_collation_to_default("bogus_coll");
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"The collation bogus_coll specified on connection is not supported when new collation is enabled, switch to the default collation: utf8mb4_bin\"]"
    );

    let _ = collate::collation_to_proto("bogus_coll");
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"Unable to get collation ID by name, use ID of the default collation instead\"] [name=bogus_coll] [\"default collation ID\"=-46] [\"default collation\"=utf8mb4_bin]"
    );

    let _ = collate::proto_to_collation(-9999);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[WARN] [mod.rs:<LINE>] [\"Unable to get collation name from ID, use name of the default collation instead\"] [id=-9999] [\"default collation ID\"=46] [\"default collation\"=utf8mb4_bin]"
    );

    // 5. tidb-types::datum: GetBinaryStringEncoded
    let mut d = Datum::default();
    d.set_string(b"abc".to_vec(), "bogus_coll");
    assert_eq!(d.get_binary_string_encoded(), b"abc");
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[WARN] [datum.rs:<LINE>] [\"unknown collation\"] [error=\"[ddl:1273]Unknown collation: 'bogus_coll'\"]"
    );

    // 6. tidb-types::json: BinaryJSON::get_value
    let bj = BinaryJSON {
        type_code: 99,
        value: Cow::Borrowed(&[]),
    };
    assert!(bj.get_value().is_none());
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[ERROR] [json_binary.rs:<LINE>] [\"unreachable JSON type\"] [type=99]"
    );

    // 7. tidb-types::mydecimal: NewDecFromStringForTest emits [PANIC] before panicking
    let panicked = catch_unwind_silent(|| {
        let _ = new_dec_from_string_for_test("not-a-decimal");
    });
    assert!(panicked.is_err());
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[PANIC] [mydecimal.rs:<LINE>] [\"encountered error\"] [error=\"[types:1292]Truncated incorrect DECIMAL value: 'not-a-decimal'\"] [DecimalStr=not-a-decimal]"
    );
}
