//! go-file: pkg/util/hint/hint_processor.go
//!
//! The part of the hint processor the optimizer driver calls (block D13.3):
//! `ExtractTableHintsFromStmtNode` and `extractHintWarns`. The binding-related functions
//! (`ParseHintsSet`, `BindHint`, `CollectHint`, ...) are SQL bindings' (D13.2).

use tidb_base::errno as errcode;
use tidb_base::terror::ClassParser;
use tidb_base::{Error, errors};
use tidb_parser::ast::{SetOprNode, Stmt};
use tidb_parser::gen_types::TableOptimizerHint;

/// ExtractTableHintsFromStmtNode extracts table hints from this node.
///
/// The Rust AST has SELECT and set operations of the statements Go handles here; UPDATE,
/// DELETE and INSERT are not in it (D02.2), and the warning handler is only used by the
/// INSERT arm (`checkInsertStmtHintDuplicated`), so it is not a parameter. A set operation
/// whose operand is a nested list (`SetOprSelectList`) contributes nothing, as in Go (the
/// list is not a `SelectStmt`).
// go: ExtractTableHintsFromStmtNode L2 (UPDATE / DELETE / INSERT are not in the Rust AST)
pub fn extract_table_hints_from_stmt_node(node: &Stmt) -> Vec<&TableOptimizerHint> {
    match node {
        Stmt::Select(x) => x.table_hints.iter().collect(),
        Stmt::SetOpr(x) => {
            let mut result = Vec::new();
            for s in &x.select_list.selects {
                if let SetOprNode::Select(sel) = s {
                    result.extend(sel.table_hints.iter());
                }
            }
            result
        }
        _ => Vec::new(),
    }
}

/// The codes of the parser's error values `extractHintWarns` compares with
/// (`pkg/parser/yy_parser.go` / `lexer.go` variables, `terror.ClassParser.NewStd(code)`).
/// The Rust parser does not define them (D02.1) and the error registry of the parser class
/// does not hold these codes, so `Equal` is evaluated on the error's identity, as pingcap
/// errors' `Equal` does for normalized errors: the RFC code `parser:<code>`.
// glue: the parser's error variables (D02.1)
const PARSER_HINT_ERROR_CODES: [u16; 7] = [
    errcode::ErrParse,
    errcode::ErrWarnOptimizerHintUnsupportedHint,
    errcode::ErrWarnOptimizerHintInvalidToken,
    errcode::ErrWarnMemoryQuotaOverflow,
    errcode::ErrWarnOptimizerHintParseError,
    errcode::ErrWarnOptimizerHintInvalidInteger,
    errcode::ErrWarnOptimizerHintWrongPos,
];

/// `parser.ErrXxx.Equal(w)` for one of [`PARSER_HINT_ERROR_CODES`].
// glue: pingcap errors Error.Equal of a normalized error (ids compared)
fn is_parser_hint_error(w: &Error) -> bool {
    let w = errors::cause(w);
    w.is_normalized()
        && PARSER_HINT_ERROR_CODES
            .iter()
            .any(|c| w.id() == format!("{}:{c}", ClassParser.string()))
}

// go: extractHintWarns L1
pub fn extract_hint_warns(warns: &[Error]) -> Vec<Error> {
    for w in warns {
        if is_parser_hint_error(w) {
            // Just one warning is enough, however we use a slice here to stop golint complaining
            // "error should be the last type when returning multiple items" for `ParseHintsSet`.
            return vec![w.clone()];
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_parser::{ParseOptions, parse};

    fn stmt(sql: &str) -> Stmt {
        parse(sql, &ParseOptions::default())
            .unwrap()
            .stmts
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn statements_without_hints_have_none() {
        for sql in [
            "select a from t",
            "select a from t union select b from t2",
            "begin",
        ] {
            assert!(
                extract_table_hints_from_stmt_node(&stmt(sql)).is_empty(),
                "{sql}"
            );
        }
    }

    #[test]
    fn hint_warns_keep_the_first_parser_hint_warning() {
        let parser_err = |code: u16| {
            errors::normalize(
                "w",
                [
                    errors::rfc_code_text(format!("parser:{code}")),
                    errors::mysql_error_code(code as _),
                ],
            )
        };
        let other = parser_err(errcode::ErrUnknown);
        let plain = Error::plain("parser:8064");
        let w = parser_err(errcode::ErrWarnOptimizerHintParseError);
        assert!(extract_hint_warns(&[other.clone(), plain]).is_empty());
        let got = extract_hint_warns(&[other, w.clone(), parser_err(errcode::ErrParse)]);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].code(),
            errcode::ErrWarnOptimizerHintParseError as isize
        );
    }
}
