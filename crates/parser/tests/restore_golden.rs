//! Restore-golden test: every SELECT case that falls fully inside the hand-ported subset must
//! restore byte-identically to the `Restore()` text expected by TiDB's parser_test.go.
mod common;

use tidb_parser::{ParseOptions, ast::Stmt, parse};

/// Known mismatches, by source text. Each needs a reason.
const ALLOWLIST: &[(&str, &str)] = &[(
    // parser_test.go:2692 `{"select 111 as \xd6\xf7", true, "SELECT 111 AS `??`"}`
    "select 111 as \u{d6}\u{f7}",
    "the Go case feeds the raw latin1 bytes D6 F7 (invalid UTF-8), which TiDB restores as `??`; \
     a &str cannot hold them, so the corpus carries the code points U+00D6 U+00F7 instead, \
     which we restore unchanged",
)];

/// Number of SELECT cases fully inside the ported subset at the time of the port; it may
/// only grow as more actions are ported.
const MIN_COVERED: usize = 296;

fn restore_all(stmts: &[Stmt]) -> Result<String, String> {
    let mut out = String::new();
    for (i, s) in stmts.iter().enumerate() {
        if i != 0 {
            out.push_str("; ");
        }
        s.restore(&mut out)?;
    }
    Ok(out)
}

#[test]
fn select_restore_matches_tidb() {
    let cases: Vec<_> = common::default_mode()
        .into_iter()
        .filter(|c| c.ok && c.restore.is_some())
        .filter(|c| {
            c.src
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("select")
        })
        .collect();
    let (mut covered, mut unported) = (0, 0);
    let mut mismatches = Vec::new();
    for c in &cases {
        let opts = ParseOptions {
            window_functions: c.window,
        };
        let out = parse(&c.src, &opts).unwrap_or_else(|e| panic!("{:?}: {e}", c.src));
        // Err = the statement touches a production that is still a placeholder.
        let Ok(got) = restore_all(&out.stmts) else {
            unported += 1;
            continue;
        };
        covered += 1;
        let want = c.restore.as_deref().unwrap();
        if got != want {
            mismatches.push((c.src.clone(), want.to_string(), got));
        }
    }
    eprintln!(
        "SELECT cases with expected restore: {}, inside ported subset: {covered}, \
         touching unported productions: {unported}, mismatches: {}",
        cases.len(),
        mismatches.len()
    );
    let unexpected: Vec<_> = mismatches
        .iter()
        .filter(|(src, ..)| !ALLOWLIST.iter().any(|(a, _)| a == src))
        .map(|(src, want, got)| format!("src : {src}\nwant: {want}\ngot : {got}"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "restore mismatches:\n{}",
        unexpected.join("\n\n")
    );
    for (src, _) in ALLOWLIST {
        assert!(
            mismatches.iter().any(|(s, ..)| s == src),
            "allowlisted case now restores correctly (or vanished), remove it: {src:?}"
        );
    }
    assert!(
        covered >= MIN_COVERED,
        "only {covered} cases inside the ported subset"
    );
}
