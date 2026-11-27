//! Golden Matrix 2: Error raw bytes, stack traces, `disable-error-stack` (R33 / #453),
//! `ErrVerboseLogger` + `replace_logger` sync, redaction parity, `terror`, `panic_guard`, and
//! subprocess unhandled panic verification (`specs/D00.3-logging/spec.md` §2, §8).

use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::sync::Mutex;

use chrono::{FixedOffset, TimeZone};
use tidb_base::config::LogConfig;
use tidb_base::errors::{self, RedactLogDisable, RedactLogEnable, RedactLogMarker};
use tidb_base::panic_guard::{
    self, PanicMode, catch_unwind_capture, catch_unwind_future, catch_unwind_silent,
};
use tidb_base::structured_log::{LogField, LogLevel};
use tidb_base::{go_args, terror};
use tidb_log::encoder::{self, EncoderConfig, Entry};
use tidb_log::sink::MemorySink;

static GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn sample_time() -> chrono::DateTime<FixedOffset> {
    let off = FixedOffset::east_opt(0).unwrap();
    off.with_ymd_and_hms(2026, 11, 27, 1, 22, 37).unwrap()
}

fn assert_single_line(bytes: &[u8]) {
    assert_eq!(bytes.last(), Some(&b'\n'));
    let body = &bytes[..bytes.len() - 1];
    assert!(!body.contains(&b'\n'), "raw \\n inside line");
    assert!(!body.contains(&b'\r'), "raw \\r inside line");
}

#[test]
fn matrix2_error_raw_bytes_has_stack_trace_annotate_and_r33_json_parity() {
    // Raw non-UTF-8 bytes in Error must encode as 6-byte ASCII `\ufffd`, not lossy U+FFFD.
    let raw_err = terror::ClassKV
        .new_std(tidb_base::errno::ErrDupEntry)
        .fast_gen(b"raw=\xff\xfea".as_slice(), vec![]);
    assert!(!raw_err.has_stack());
    assert_eq!(raw_err.error_verbose(), raw_err.error());

    let mut out = Vec::new();
    let cfg_no_verbose = EncoderConfig {
        disable_timestamp: true,
        disable_caller: true,
        disable_error_verbose: true,
        force_error_verbose: false,
    };
    let entry = Entry {
        time: sample_time(),
        level: LogLevel::Error,
        logger_name: "",
        file: "",
        line: 0,
        msg: "m",
        bound_fields: &[],
        fields: &[LogField::new("error", &raw_err)],
        stack: "",
    };
    encoder::text::encode_entry(cfg_no_verbose, &entry, &mut out);
    assert_single_line(&out);
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains(r#"raw=\ufffd\ufffda"#),
        "must use 6-byte \\ufffd: {s}"
    );
    assert!(!s.contains('\u{FFFD}'), "must not emit UTF-8 U+FFFD: {s}");
    assert!(!s.contains("errorVerbose"), "{s}");

    // FastGen* wrapped by trace() / annotate() re-captures a stack (Go SuspendStack parity).
    let traced = errors::trace(raw_err.clone());
    assert!(traced.has_stack());
    assert_ne!(traced.error_verbose(), traced.error());

    let annotated = errors::annotate(raw_err, "outer ctx");
    assert!(annotated.has_stack());
    let ann_err = String::from_utf8_lossy(&annotated.error()).into_owned();
    assert!(ann_err.starts_with("outer ctx: "), "{ann_err}");
    let ann_verbose = String::from_utf8_lossy(&annotated.error_verbose()).into_owned();
    assert!(
        ann_verbose.contains("\nouter ctx\n"),
        "cause first then pre-stack annotation then frames: {ann_verbose}"
    );

    // Annotate on an error that ALREADY has a stack puts the annotation AFTER the stack frames
    // (Go pingcap/errors juju_adaptor.go:19-35), and error_equal compares underlying cause.
    let plain_base = errors::new("plain cause");
    let plain_ann = errors::annotate(plain_base.clone(), "post stack ctx");
    assert!(errors::error_equal(Some(&plain_base), Some(&plain_ann)));
    let plain_ann_verbose = String::from_utf8_lossy(&plain_ann.error_verbose()).into_owned();
    assert!(
        plain_ann_verbose.ends_with("\npost stack ctx"),
        "post-stack annotation must appear after stack frames: {plain_ann_verbose}"
    );

    // GenWithStack* always captures stack (owner decision D4 = B).
    let stacked = terror::ClassKV
        .new_std(tidb_base::errno::ErrDupEntry)
        .gen_with_stack_by_args(go_args!["k", "idx"]);
    assert!(stacked.has_stack());

    // R33 (#453): both text AND JSON honor disable_error_verbose = true, and both emit
    // errorVerbose when disable_error_verbose = false or force_error_verbose = true.
    let e_entry = Entry {
        time: sample_time(),
        level: LogLevel::Error,
        logger_name: "",
        file: "",
        line: 0,
        msg: "err",
        bound_fields: &[],
        fields: &[LogField::new("error", &stacked)],
        stack: "",
    };

    let mut text_disabled = Vec::new();
    let mut json_disabled = Vec::new();
    encoder::text::encode_entry(cfg_no_verbose, &e_entry, &mut text_disabled);
    encoder::json::encode_entry(cfg_no_verbose, &e_entry, &mut json_disabled);
    assert_single_line(&text_disabled);
    assert_single_line(&json_disabled);
    assert!(
        !String::from_utf8(text_disabled)
            .unwrap()
            .contains("errorVerbose")
    );
    assert!(
        !String::from_utf8(json_disabled)
            .unwrap()
            .contains("errorVerbose")
    );

    let cfg_forced = EncoderConfig {
        force_error_verbose: true,
        ..cfg_no_verbose
    };
    let mut text_forced = Vec::new();
    let mut json_forced = Vec::new();
    encoder::text::encode_entry(cfg_forced, &e_entry, &mut text_forced);
    encoder::json::encode_entry(cfg_forced, &e_entry, &mut json_forced);
    assert_single_line(&text_forced);
    assert_single_line(&json_forced);
    assert!(
        String::from_utf8(text_forced)
            .unwrap()
            .contains("[errorVerbose=")
    );
    assert!(
        String::from_utf8(json_forced)
            .unwrap()
            .contains(r#""errorVerbose":"#)
    );
}

#[test]
fn matrix2_redaction_3_modes_and_gen_with_stack_unredacted_parity() {
    let _g = GLOBAL_TEST_LOCK.lock().unwrap();
    let proto = errors::normalize(
        "Duplicate entry '%s' for key '%s'",
        [
            errors::redact_args(&[0_usize, 1_usize][..]),
            errors::rfc_code_text("kv:1062"),
            errors::mysql_error_code(1062),
        ],
    );

    for (mode, expected_by_args) in [
        (
            RedactLogDisable,
            "[kv:1062]Duplicate entry 'secretVal' for key '‹k›'",
        ),
        (RedactLogEnable, "[kv:1062]Duplicate entry '?' for key '?'"),
        (
            RedactLogMarker,
            "[kv:1062]Duplicate entry '‹secretVal›' for key '‹‹‹k›››'",
        ),
    ] {
        errors::redact_log_enabled_store(mode);
        let e_by_args = proto.gen_with_stack_by_args(go_args!["secretVal", "‹k›"]);
        assert_eq!(
            String::from_utf8(e_by_args.error()).unwrap(),
            expected_by_args,
            "mode={mode}"
        );
        // Go parity: GenWithStack / FastGen do NOT redact (keep // TODO: RedactErrorArg).
        let e_raw = proto.gen_with_stack("val=%s", go_args!["secretVal"]);
        assert_eq!(
            String::from_utf8(e_raw.error()).unwrap(),
            "[kv:1062]val=secretVal",
            "GenWithStack must stay unredacted in mode={mode}"
        );
        let e_fast = proto.fast_gen("val=%s", go_args!["secretVal"]);
        assert_eq!(
            String::from_utf8(e_fast.error()).unwrap(),
            "[kv:1062]val=secretVal",
            "FastGen must stay unredacted in mode={mode}"
        );
    }
    errors::redact_log_enabled_store(RedactLogDisable);
}

#[test]
fn matrix2_err_verbose_logger_and_terror_call_log() {
    let _g = GLOBAL_TEST_LOCK.lock().unwrap();
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "info".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: false,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    tidb_log::init_logger_with_sinks(&cfg, "", sink.clone(), None).unwrap();

    let err = errors::new("boom with stack");
    // Normal logger omits errorVerbose when disable_error_verbose = true.
    tidb_base::log_error!("normal", "error" => &err);
    let line1 = sink.take();
    assert_single_line(&line1);
    let s1 = String::from_utf8(line1).unwrap();
    assert!(!s1.contains("errorVerbose"), "{s1}");

    // ErrVerboseLogger forces errorVerbose even when disable_error_verbose = true.
    tidb_base::log_err_verbose!(LogLevel::Error, "verbose", "error" => &err);
    let line2 = sink.take();
    assert_single_line(&line2);
    let s2 = String::from_utf8(line2).unwrap();
    assert!(s2.contains("[errorVerbose="), "{s2}");

    // terror::call & terror::log emit structured single-line ERROR with [error=...] [stack="..."].
    terror::call(|| Err(errors::new("call failed")));
    let line3 = sink.take();
    assert_single_line(&line3);
    let s3 = String::from_utf8(line3).unwrap();
    assert!(s3.contains("[\"function call errored\"]"), "{s3}");
    assert!(s3.contains("[error=\"call failed\"]"), "{s3}");
    assert!(s3.contains("[stack="), "{s3}");

    terror::log(Some(&err));
    let line4 = sink.take();
    assert_single_line(&line4);
    let s4 = String::from_utf8(line4).unwrap();
    assert!(s4.contains("[\"encountered error\"]"), "{s4}");
    assert!(s4.contains("[stack="), "{s4}");
}

#[test]
fn matrix2_panic_guard_nesting_async_and_ok_err_isolation() {
    let _g = GLOBAL_TEST_LOCK.lock().unwrap();
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "info".to_owned(),
        format: "text".to_owned(),
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    tidb_log::init_logger_with_sinks(&cfg, "", sink.clone(), None).unwrap();
    tidb_log::set_panic_hook();

    assert_eq!(panic_guard::current_panic_mode(), PanicMode::FatalLog);

    // Nested catch_unwind_silent inside catch_unwind_capture: inner silent panic does NOT emit
    // [FATAL] and restores outer CaptureBacktrace on exit.
    let outer = catch_unwind_capture(AssertUnwindSafe(|| {
        let inner = catch_unwind_silent(|| {
            panic!("silent inner");
        });
        assert!(inner.is_err());
        assert_eq!(
            panic_guard::current_panic_mode(),
            PanicMode::CaptureBacktrace
        );
        panic!("outer recoverable");
    }));
    let (payload, bt) = outer.unwrap_err();
    assert_eq!(
        panic_guard::panic_payload_message(payload.as_ref()),
        "outer recoverable"
    );
    assert!(
        bt.as_deref().is_some_and(|s| !s.is_empty()),
        "backtrace must be captured"
    );
    assert!(
        sink.snapshot().is_empty(),
        "recoverable/silent panics must not emit [FATAL]"
    );
    assert_eq!(panic_guard::current_panic_mode(), PanicMode::FatalLog);

    // Async catch_unwind_future: TLS mode is NOT leaked across Pending polls, and Ok(Err(...))
    // never touches any backtrace slot.
    let mut yielded = false;
    let fut = std::future::poll_fn(|cx| {
        assert_eq!(
            panic_guard::current_panic_mode(),
            PanicMode::CaptureBacktrace
        );
        if !yielded {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(Err::<(), _>("normal error"))
        }
    });
    let mut wrapped = Box::pin(catch_unwind_future(fut));
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    assert!(matches!(
        wrapped.as_mut().poll(&mut cx),
        std::task::Poll::Pending
    ));
    // Outside poll(), TLS mode MUST be back to FatalLog!
    assert_eq!(panic_guard::current_panic_mode(), PanicMode::FatalLog);
    match wrapped.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Ok(Err("normal error"))) => {}
        other => panic!("unexpected poll result: {other:?}"),
    }
    assert_eq!(panic_guard::current_panic_mode(), PanicMode::FatalLog);
    assert!(panic_guard::take_recovered_backtrace().is_none());
}

/// Subprocess verification of global `set_panic_hook()` on an unhandled panic:
/// stdout contains a single-line `[FATAL] ... ["unhandled panic"]` entry with escaped stack,
/// and stderr contains zero multi-line raw Rust panic backtraces.
#[test]
fn matrix2_unhandled_panic_subprocess_single_line() {
    if std::env::var("TIDB_LOG_TEST_UNHANDLED_PANIC").as_deref() == Ok("1") {
        let cfg = LogConfig {
            level: "info".to_owned(),
            format: "text".to_owned(),
            disable_error_verbose: true,
            ..LogConfig::default()
        };
        tidb_log::init_logger(&cfg, "").unwrap();
        panic!("boom\nline2");
    }

    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .arg("--exact")
        .arg("matrix2_unhandled_panic_subprocess_single_line")
        .arg("--nocapture")
        .env("TIDB_LOG_TEST_UNHANDLED_PANIC", "1")
        .env("RUST_BACKTRACE", "1")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "child must exit non-zero on unhandled panic"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fatal_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("[FATAL]") && l.contains("[\"unhandled panic\"]"))
        .collect();
    assert_eq!(
        fatal_lines.len(),
        1,
        "expected 1 single-line FATAL log on stdout:\n{stdout}"
    );
    let line = fatal_lines[0];
    assert!(line.contains(r#"[panic="boom\nline2"]"#), "{line}");
    assert!(line.contains("[location="), "{line}");
    assert!(line.contains("[stack="), "{line}");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("stack backtrace:"),
        "stderr must not contain raw multiline backtrace:\n{stderr}"
    );
}

/// Regression test for #462 live release check: `Error::error_verbose` / `format_backtrace_frames`
/// MUST include Go-style `\n\t<file>:<line>` locations (including in `--release`, guaranteed by
/// `[profile.release] debug = "line-tables-only"` in Cargo.toml, spec §2.1).
#[test]
fn error_stack_frames_include_file_and_line() {
    let err = errors::new("BIGINT value is out of range");
    let verbose = String::from_utf8(err.error_verbose()).unwrap();
    let loc_lines: Vec<&str> = verbose.lines().filter(|l| l.starts_with('\t')).collect();
    assert!(
        !loc_lines.is_empty(),
        "stack trace must contain \\t<file>:<line> lines (check release debuginfo):\n{verbose}"
    );
    assert!(
        loc_lines.iter().any(|l| {
            let trimmed = l.trim_start_matches('\t');
            trimmed.contains(".rs:")
                && trimmed.rsplit_once(':').is_some_and(|(_, line)| {
                    !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit())
                })
        }),
        "expected at least one .rs:<line> frame location:\n{verbose}"
    );
}
