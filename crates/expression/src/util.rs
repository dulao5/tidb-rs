//! go-file: pkg/expression/util.go
//!
//! The parts of `util.go` (block D08.12) that earlier blocks need; the rest of the file
//! comes with D08.12.

use tidb_base::mysql::ParseToJSONFlag;

use crate::builtin_cast::with_type_mut;
use crate::context::EvalContext;
use crate::expression::{Expr, ExprRef};

/// DisableParseJSONFlag4Expr disables ParseToJSONFlag for `expr` except Column.
/// We should not *PARSE* a string as JSON under some scenarios. ParseToJSONFlag
/// is 0 for JSON column yet(as well as JSON correlated column), so we can skip
/// it. Moreover, Column.RetType refers to the infoschema, if we modify it, data
/// race may happen if another goroutine read from the infoschema at the same
/// time.
///
/// A scalar function's `RetType` is its signature's `tp` (one type, D08.1 spec §3.2), so
/// the signature sees the change as in Go: `CAST(c AS JSON)` stops parsing the string
/// (v8.5.8: `CAST('[1]' AS JSON) = '[1]'` is 0, Lucky review #94). A node shared with other
/// holders is copied first (spec §4, [`with_type_mut`]).
// go: DisableParseJSONFlag4Expr L2 (a shared node is copied before the change, D08.1 spec §4)
pub fn disable_parse_json_flag4_expr(_ctx: &dyn EvalContext, expr: &mut ExprRef) {
    if matches!(&**expr, Expr::Column(_) | Expr::CorrelatedColumn(_)) {
        return;
    }
    with_type_mut(expr, |tp| tp.set_flag(tp.get_flag() & !ParseToJSONFlag));
}
