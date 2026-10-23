//! Accept/reject over the TiDB parser_test.go corpus (default parser mode).
mod common;

use tidb_parser::{ParseOptions, parse};

/// Invalid statements we must keep rejecting. The rest are grammar-valid in parser.y and
/// rejected by Go actions that are not ported yet (they become placeholders here).
const MIN_INVALID_REJECTED: usize = 616;

#[test]
fn corpus_acceptance() {
    let cases = common::default_mode();
    let (valid, invalid): (Vec<_>, Vec<_>) = cases.iter().partition(|c| c.ok);
    // Guard against silent corpus changes; update together with a regen.
    assert_eq!(
        (valid.len(), invalid.len()),
        (3434, 706),
        "corpus size changed"
    );

    let mut rejected_valid = Vec::new();
    for c in &valid {
        let opts = ParseOptions {
            window_functions: c.window,
        };
        if let Err(e) = parse(&c.src, &opts) {
            rejected_valid.push(format!("parser_test.go:{}: {:?} -> {e}", c.line, c.src));
        }
    }
    let rejected_invalid = invalid
        .iter()
        .filter(|c| {
            parse(
                &c.src,
                &ParseOptions {
                    window_functions: c.window,
                },
            )
            .is_err()
        })
        .count();

    eprintln!(
        "valid accepted: {}/{}, invalid rejected: {rejected_invalid}/{}",
        valid.len() - rejected_valid.len(),
        valid.len(),
        invalid.len()
    );
    assert!(
        rejected_valid.is_empty(),
        "{} valid statements rejected, first ones:\n{}",
        rejected_valid.len(),
        rejected_valid[..rejected_valid.len().min(20)].join("\n")
    );
    assert!(
        rejected_invalid >= MIN_INVALID_REJECTED,
        "only {rejected_invalid} invalid statements rejected (expected >= {MIN_INVALID_REJECTED})"
    );
}
