//! Golden test of the stack frames in `errorVerbose` / `[stack=...]` (spec D00.3 §2.1, the
//! unified log format): Go's `pingcap/errors` `%+v` prints `<func>\n\t<file>:<line>` with the
//! qualified function name (`runtime.FuncForPC(pc).Name()`) and the absolute build path (the
//! Linux `tidb-server` build passes no `-trimpath`), starting at the frame Go's `callers()`
//! starts at.
//!
//! Meant to pass under the release profile as well as the test profile; CI runs it as
//! `cargo test --release -p tidb-log --test stack_frame_golden`, since production binaries are
//! release builds (`[profile.release] debug = "limited"` keeps names and line tables).
//!
//! The `make_*` helpers pass their result through `black_box`: a helper that ends in a tail
//! call would otherwise lose its own frame in an optimised build.

use std::hint::black_box;

use tidb_base::errors::{self, Error};
use tidb_base::go_args;

/// The repository root at build time (`crates/log` is two levels below it).
fn repo_root() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
        .strip_suffix("/crates/log")
        .expect("tidb-log lives at <repo>/crates/log")
}

/// Replaces the repository root with `<ROOT>` and the line number of each location with
/// `<LINE>`.
fn normalize(verbose: &str) -> Vec<String> {
    verbose
        .lines()
        .map(|line| match line.strip_prefix('\t') {
            Some(loc) => {
                let (path, line_no) = loc
                    .rsplit_once(':')
                    .unwrap_or_else(|| panic!("location without line: {loc:?}"));
                assert!(
                    !line_no.is_empty() && line_no.bytes().all(|b| b.is_ascii_digit()),
                    "location must end in :<line>, got {loc:?}"
                );
                let path = path.replacen(repo_root(), "<ROOT>", 1);
                format!("\t{path}:<LINE>")
            }
            None => line.to_owned(),
        })
        .collect()
}

/// Every frame after the pinned prefix: a function line, optionally followed by one
/// `\t<path>:<line>` location. Rust sources show the absolute build path (std's are
/// `/rustc/<hash>/...`); the C library's frames keep the paths its own debug info records.
fn assert_well_formed_frames(lines: &[String], verbose: &str) {
    let mut prev_was_func = false;
    for line in lines {
        if let Some(loc) = line.strip_prefix('\t') {
            assert!(
                prev_was_func,
                "location without a function line:\n{verbose}"
            );
            assert!(
                !loc.contains(".rs:") || loc.starts_with('/') || loc.starts_with("<ROOT>/"),
                "frame path must be the absolute build path, got {loc:?}:\n{verbose}"
            );
            prev_was_func = false;
        } else {
            assert!(!line.is_empty(), "empty frame line:\n{verbose}");
            assert!(
                !line.contains("fill_line_and_file") && !line.starts_with("std::backtrace"),
                "capture machinery frame leaked: {line:?}:\n{verbose}"
            );
            prev_was_func = true;
        }
    }
}

fn check(verbose: &str, expected_prefix: &[&str]) {
    let lines = normalize(verbose);
    assert!(
        lines.len() >= expected_prefix.len(),
        "too few lines in:\n{verbose}"
    );
    assert_eq!(
        &lines[..expected_prefix.len()],
        expected_prefix,
        "full output:\n{verbose}"
    );
    assert_well_formed_frames(&lines[1..], verbose);
}

fn prototype() -> Error {
    errors::normalize(
        "%s value is out of range in '%s'",
        [
            errors::rfc_code_text("types:1690"),
            errors::mysql_error_code(1690),
        ],
    )
}

#[inline(never)]
fn make_gen_with_stack_by_args(proto: &Error) -> Error {
    black_box(proto.gen_with_stack_by_args(go_args!["BIGINT", "(1 + 2)"]))
}

#[inline(never)]
fn make_new() -> Error {
    black_box(errors::new("plain failure"))
}

#[inline(never)]
fn make_trace(err: Error) -> Error {
    black_box(errors::trace(err))
}

#[inline(never)]
fn make_annotate(err: Error) -> Error {
    black_box(errors::annotate(err, "while reading"))
}

#[inline(never)]
fn capture_stack_string() -> String {
    black_box(errors::format_backtrace_string(
        &std::backtrace::Backtrace::force_capture(),
    ))
}

/// `GenWithStackByArgs` goes through `AddStack`, so Go's top frame is the constructor itself.
#[test]
fn gen_with_stack_by_args_starts_at_the_constructor() {
    let err = make_gen_with_stack_by_args(&prototype());
    let verbose = String::from_utf8(err.error_verbose()).unwrap();
    check(
        &verbose,
        &[
            "[types:1690]BIGINT value is out of range in '(1 + 2)'",
            "<tidb_base::errors::Error>::gen_with_stack_by_args",
            "\t<ROOT>/crates/base/src/errors.rs:<LINE>",
            "stack_frame_golden::make_gen_with_stack_by_args",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
            "stack_frame_golden::gen_with_stack_by_args_starts_at_the_constructor",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
        ],
    );
}

/// `errors.New` calls `callers()` itself, so Go's top frame is the caller of `New`.
#[test]
fn new_starts_at_its_caller() {
    let verbose = String::from_utf8(make_new().error_verbose()).unwrap();
    check(
        &verbose,
        &[
            "plain failure",
            "stack_frame_golden::make_new",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
            "stack_frame_golden::new_starts_at_its_caller",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
        ],
    );
}

/// `errors.Trace` on an error without a stack (`FastGen*`) records one with `errors.Trace` on
/// top (through `AddStack`).
#[test]
fn trace_starts_at_trace() {
    let err = make_trace(prototype().fast_gen_by_args(go_args!["BIGINT", "x"]));
    let verbose = String::from_utf8(err.error_verbose()).unwrap();
    check(
        &verbose,
        &[
            "[types:1690]BIGINT value is out of range in 'x'",
            "tidb_base::errors::trace",
            "\t<ROOT>/crates/base/src/errors.rs:<LINE>",
            "stack_frame_golden::make_trace",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
            "stack_frame_golden::trace_starts_at_trace",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
        ],
    );
}

/// `errors.Annotate` on an error without a stack calls `callers()` itself: Go's top frame is
/// the caller of `Annotate`, and the annotation precedes the frames.
#[test]
fn annotate_starts_at_its_caller() {
    let err = make_annotate(prototype().fast_gen_by_args(go_args!["BIGINT", "y"]));
    let verbose = String::from_utf8(err.error_verbose()).unwrap();
    check(
        &verbose,
        &[
            "[types:1690]BIGINT value is out of range in 'y'",
            "while reading",
            "stack_frame_golden::make_annotate",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
            "stack_frame_golden::annotate_starts_at_its_caller",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
        ],
    );
}

/// `[stack=...]` (Go `zap.Stack`): the frames start at the function that captured the stack,
/// with no leading newline.
#[test]
fn stack_field_starts_at_the_capturing_function() {
    let stack = capture_stack_string();
    let lines = normalize(&stack);
    assert_eq!(
        &lines[..4],
        [
            "stack_frame_golden::capture_stack_string",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
            "stack_frame_golden::stack_field_starts_at_the_capturing_function",
            "\t<ROOT>/crates/log/tests/stack_frame_golden.rs:<LINE>",
        ],
        "full output:\n{stack}"
    );
    assert_well_formed_frames(&lines, &stack);
}
