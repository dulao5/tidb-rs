//! go-file: pkg/util/hint/hint_query_block.go
//!
//! The query block handler (block D13.3): numbers the SELECT blocks of a statement (Go
//! writes `SelectStmt.QueryBlockOffset` in `Enter`) and resolves query block names.
//!
//! Mapping:
//! - The Rust AST has no `QueryBlockOffset` field: the handler keeps the offsets keyed by the
//!   address of each `SelectStmt` it visited ([`QBHintHandler::query_block_offset`]); a
//!   SELECT it never visited reads 0, Go's unset field (`BuildLogicalPlanForTest` never
//!   walks the handler). The handler is built per statement and dropped with it, so an
//!   address is not reused while it lives.
//! - Go keeps the warning handler (the statement context) in the handler; here the methods
//!   that may warn take it as an argument (`M-CTX`), `None` being Go's nil handler.
//! - The hint lists are opaque (`ast::TableOptimizerHint`, the parser's hint grammar is not
//!   ported, D02.2): wherever Go reads a hint's contents the function is a boundary stub
//!   `blocked-by:D02.2`; the empty-list paths are Go's. The hints kept in the maps are AST
//!   node addresses (Go's pointers) and are never set while hints are not parsed.
//! - `ast.Node.Accept(handler)` is [`QBHintHandler::accept`] over the plan-cache AST walker
//!   (`Walker`, Go's `Accept` order).

use std::collections::{HashMap, HashSet};

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_parser::ast::{SelectStmt, Stmt};
use tidb_parser::gen_types::TableOptimizerHint;
use tidb_parser::model::CIStr;
use tidb_sessionctx::stmtctx::StatementContext;

use crate::core::expression_rewriter::node_addr;
use crate::core::plan_cacheable_checker::{CNode, Visit, Walker};
use crate::hint::hint::HintTable;

/// The address of an `ast.TableOptimizerHint` in the statement's AST (Go's pointer).
// glue: Go *ast.TableOptimizerHint as a map value
pub type HintAddr = usize;

/// hintWarnHandler is used to handle the warning when parsing hints.
pub trait HintWarnHandler {
    // glue: interface method of hintWarnHandler
    fn set_hint_warning(&self, warn: &str);
    // glue: interface method of hintWarnHandler
    fn set_hint_warning_from_error(&self, err: Error);
}

impl HintWarnHandler for StatementContext {
    // glue: StatementContext implements hintWarnHandler
    fn set_hint_warning(&self, warn: &str) {
        StatementContext::set_hint_warning(self, warn);
    }
    // glue: StatementContext implements hintWarnHandler
    fn set_hint_warning_from_error(&self, err: Error) {
        StatementContext::set_hint_warning_from_error(self, err);
    }
}

/// QBHintHandler is used to handle hints at different query blocks.
/// See the 2 examples below:
///
/// 1) `select /*+ use_index(@sel_2 t2, a) */ * from t1, (select a*2 as b from t2) tx where a>b`;
///
/// 2) `select /*+ use_index(@qb_xxx t2, a) */ * from t1, (select /*+ qb_name(qb_xxx) */ a*2 as b from t2) tx where a>b`;
///
/// In both cases, the `use_index` hint doesn't take effect directly, since a specific qb_name is specified, and this
/// QBHintHandler is used to handle this cases.
#[derive(Debug, Default)]
pub struct QBHintHandler {
    /// map[QBName]SelectOffset
    pub qb_name_to_sel_offset: Option<HashMap<String, isize>>,

    // Used for the view's hint
    /// map[QBName]HintedTable
    pub view_qb_name_to_table: Option<HashMap<String, Vec<HintTable>>>,
    /// map[QBName]Hints
    pub view_qb_name_to_hints: Option<HashMap<String, Vec<HintAddr>>>,

    select_stmt_offset: isize,

    /// `SelectStmt.QueryBlockOffset` of each visited SELECT, by node address (module doc).
    query_block_offsets: HashMap<usize, isize>,
    /// A boundary reached during [`QBHintHandler::accept`].
    walk_err: Option<Error>,
}

/// QBHintBuildState stores the per-build runtime state for a QBHintHandler.
/// The handler itself only keeps AST-derived metadata and can be shared safely.
#[derive(Debug, Default)]
pub struct QBHintBuildState {
    /// map[QueryBlockOffset]Hints
    pub qb_offset_to_hints: Option<HashMap<isize, Vec<HintAddr>>>,
    /// map[QBName]Used
    pub view_qb_name_used: Option<HashSet<String>>,
}

/// NewQBHintHandler creates a QBHintHandler.
///
/// Go stores the warning handler; it is passed to the methods that warn (module doc).
// go: NewQBHintHandler L2 (the warning handler is an argument of the methods, M-CTX)
pub fn new_qb_hint_handler() -> QBHintHandler {
    QBHintHandler::default()
}

const DEFAULT_UPDATE_BLOCK_NAME: &str = "upd_1";
const DEFAULT_DELETE_BLOCK_NAME: &str = "del_1";
const DEFAULT_SELECT_BLOCK_PREFIX: &str = "sel_";

impl QBHintHandler {
    /// NewBuildState creates the per-build runtime state for the handler.
    // go: QBHintHandler.NewBuildState L1
    pub fn new_build_state(&self) -> QBHintBuildState {
        let mut state = QBHintBuildState::default();
        let Some(m) = &self.view_qb_name_to_table else {
            return state;
        };
        if m.is_empty() {
            return state;
        }
        state.view_qb_name_used = Some(HashSet::with_capacity(m.len()));
        state
    }

    /// MaxSelectStmtOffset returns the current stmt offset.
    // go: QBHintHandler.MaxSelectStmtOffset L1
    pub fn max_select_stmt_offset(&self) -> isize {
        self.select_stmt_offset
    }

    /// `node.Accept(handler)`: walks the statement in Go's `Accept` order, numbering the
    /// SELECT blocks. A node the Rust AST does not have stops the walk (`blocked-by:D02.2`:
    /// the offsets of the blocks after it would be unknown).
    // glue: ast.Node.Accept(QBHintHandler)
    pub fn accept(
        &mut self,
        node: &Stmt,
        warn_handler: Option<&dyn HintWarnHandler>,
    ) -> Result<(), Error> {
        let mut v = AcceptVisitor {
            h: self,
            warn_handler,
        };
        Walker(&mut v).stmt(node);
        match self.walk_err.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Go's `sel.QueryBlockOffset` (module doc): 0 for a SELECT the handler did not visit.
    // glue: SelectStmt.QueryBlockOffset (not in the Rust AST)
    pub fn query_block_offset(&self, sel: &SelectStmt) -> isize {
        self.query_block_offsets
            .get(&node_addr(sel))
            .copied()
            .unwrap_or(0)
    }

    /// Enter implements Visitor interface.
    ///
    /// The Rust AST has no UPDATE / DELETE (their `checkQueryBlockHints(node.TableHints, 0)`),
    /// EXPLAIN or CREATE BINDING statements (Go skips the children of those two).
    // go: QBHintHandler.Enter L2 (UPDATE / DELETE / EXPLAIN / CREATE BINDING are not in the Rust AST)
    fn enter(
        &mut self,
        n: CNode<'_>,
        warn_handler: Option<&dyn HintWarnHandler>,
    ) -> Result<bool, Error> {
        if let CNode::Select(node) = n {
            self.select_stmt_offset += 1;
            self.query_block_offsets
                .insert(node_addr(node), self.select_stmt_offset);
            // Handle the view hints and update the left hint.
            let hints: Vec<&TableOptimizerHint> = node.table_hints.iter().collect();
            let left = self.handle_view_hints(&hints, self.select_stmt_offset, warn_handler)?;
            // node.TableHints = left: the list is empty here (a non-empty one stopped above).
            self.check_query_block_hints(&left, self.select_stmt_offset, warn_handler)?;
        }
        Ok(false)
    }

    /// Leave implements Visitor interface.
    // go: QBHintHandler.Leave L1
    fn leave(&self) -> bool {
        true
    }

    /// checkQueryBlockHints checks the validity of query blocks and records the map of query block name to select offset.
    // go: QBHintHandler.checkQueryBlockHints L2 (reading a hint is blocked-by:D02.2)
    fn check_query_block_hints(
        &mut self,
        hints: &[&TableOptimizerHint],
        offset: isize,
        warn_handler: Option<&dyn HintWarnHandler>,
    ) -> Result<(), Error> {
        let _ = (offset, warn_handler);
        if !hints.is_empty() {
            // for _, hint := range hints { if hint.HintName.L != hintQBName { ... } }
            // glue: blocked-by:D02.2 (the parser's hint grammar: TableOptimizerHint is opaque)
            return Err(blocked(
                "D02.2",
                "query block hints (checkQueryBlockHints: the parser's hint grammar)",
            ));
        }
        // qbName == "": nothing to record.
        Ok(())
    }

    /// Returns Go's `leftHints` (nil for an empty list).
    // go: QBHintHandler.handleViewHints L2 (reading a hint is blocked-by:D02.2)
    fn handle_view_hints<'h>(
        &mut self,
        hints: &[&'h TableOptimizerHint],
        offset: isize,
        warn_handler: Option<&dyn HintWarnHandler>,
    ) -> Result<Vec<&'h TableOptimizerHint>, Error> {
        let _ = (offset, warn_handler);
        if hints.is_empty() {
            return Ok(Vec::new());
        }
        // handle the query block name hints for view
        // glue: blocked-by:D02.2 (the parser's hint grammar: TableOptimizerHint is opaque)
        Err(blocked(
            "D02.2",
            "view hints (handleViewHints: the parser's hint grammar)",
        ))
    }

    /// HandleUnusedViewHints handle the unused view hints.
    // go: QBHintHandler.HandleUnusedViewHints L1 [M-CTX]
    pub fn handle_unused_view_hints(
        &self,
        state: Option<&QBHintBuildState>,
        warn_handler: Option<&dyn HintWarnHandler>,
        mut warn: Vec<String>,
    ) -> Vec<String> {
        let Some(state) = state else {
            return warn;
        };

        warn.clear();
        if let Some(m) = &self.view_qb_name_to_table {
            for qb_name in m.keys() {
                let ok = state
                    .view_qb_name_used
                    .as_ref()
                    .is_some_and(|u| u.contains(qb_name));
                if !ok && warn_handler.is_some() {
                    warn.push(format!(
                        "The qb_name hint {qb_name} is unused, please check whether the table list in the qb_name hint {qb_name} is correct"
                    ));
                }
            }
        }
        warn
    }

    /// getBlockName finds the offset of query block name. It uses 0 as offset for top level update or delete,
    /// -1 for invalid block name.
    // go: QBHintHandler.getBlockOffset L1
    fn get_block_offset(&self, block_name: &CIStr) -> isize {
        if let Some(m) = &self.qb_name_to_sel_offset
            && let Some(level) = m.get(&block_name.l)
        {
            return *level;
        }
        // Handle the default query block name.
        if block_name.l == DEFAULT_UPDATE_BLOCK_NAME || block_name.l == DEFAULT_DELETE_BLOCK_NAME {
            return 0;
        }
        if let Some(suffix) = block_name.l.strip_prefix(DEFAULT_SELECT_BLOCK_PREFIX) {
            let (level, err) = tidb_base::gostd::strconv::parse_int(suffix.as_bytes(), "ParseInt");
            if err.is_err() || level > self.select_stmt_offset as i64 {
                return -1;
            }
            return level as isize;
        }
        -1
    }

    /// SetWarns set the warning from a list of strings.
    // go: QBHintHandler.SetWarns L1 [M-CTX]
    pub fn set_warns(&self, warn_handler: Option<&dyn HintWarnHandler>, warns: &[String]) {
        let Some(warn_handler) = warn_handler else {
            return;
        };
        for one in warns {
            warn_handler.set_hint_warning(one);
        }
    }

    /// GetHintOffset gets the offset of stmt that the hints take effects.
    // go: QBHintHandler.GetHintOffset L1
    pub fn get_hint_offset(&self, qb_name: &CIStr, current_offset: isize) -> isize {
        if !qb_name.l.is_empty() {
            return self.get_block_offset(qb_name);
        }
        current_offset
    }

    /// Called by `GetCurrentStmtHints` for each parsed hint (none while hints are opaque).
    // go: QBHintHandler.checkTableQBName L1
    #[cfg_attr(not(test), allow(dead_code))]
    fn check_table_qb_name(&self, tables: &[HintTable]) -> bool {
        for table in tables {
            if !table.qb_name.l.is_empty() && self.get_block_offset(&table.qb_name) < 0 {
                return false;
            }
        }
        true
    }

    /// Reads the hint's QB name and tables (`blocked-by:D02.2`).
    // go: QBHintHandler.isHint4View L2 (reading a hint is blocked-by:D02.2)
    pub fn is_hint4_view(&self, hint: &TableOptimizerHint) -> Result<bool, Error> {
        let _ = hint;
        // if hint.QBName.L != "" { ... }
        // glue: blocked-by:D02.2 (the parser's hint grammar: TableOptimizerHint is opaque)
        Err(blocked(
            "D02.2",
            "a view hint (isHint4View: the parser's hint grammar)",
        ))
    }

    /// GetCurrentStmtHints extracts all hints that take effects at current stmt.
    ///
    /// The hints the state holds for `current_offset` are never set while hints are not
    /// parsed (module doc), so the result for an empty list is empty.
    // go: QBHintHandler.GetCurrentStmtHints L2 (reading a hint is blocked-by:D02.2)
    pub fn get_current_stmt_hints<'h>(
        &self,
        hints: &[&'h TableOptimizerHint],
        current_offset: isize,
        state: Option<&mut QBHintBuildState>,
        warn_handler: Option<&dyn HintWarnHandler>,
    ) -> Result<Vec<&'h TableOptimizerHint>, Error> {
        let _ = warn_handler;
        let mut local = QBHintBuildState::default();
        let state = state.unwrap_or(&mut local);
        let m = state.qb_offset_to_hints.get_or_insert_with(HashMap::new);
        if !hints.is_empty() {
            // for _, hint := range hints { if hint.HintName.L == hintQBName { ... } }
            // glue: blocked-by:D02.2 (the parser's hint grammar: TableOptimizerHint is opaque)
            return Err(blocked(
                "D02.2",
                "the hints of a query block (GetCurrentStmtHints: the parser's hint grammar)",
            ));
        }
        // return state.QBOffsetToHints[currentOffset]: addresses of parsed hints, never set.
        debug_assert!(m.get(&current_offset).is_none_or(|v| v.is_empty()));
        Ok(Vec::new())
    }

    /// MarkViewQBNameUsed records that the named view qb hint was used in the current build.
    // go: QBHintHandler.MarkViewQBNameUsed L1
    pub fn mark_view_qb_name_used(&self, qb_name: &str, state: Option<&mut QBHintBuildState>) {
        let Some(state) = state else {
            return;
        };
        let Some(used) = &mut state.view_qb_name_used else {
            return;
        };
        used.insert(qb_name.to_owned());
    }
}

/// The handler with the warning handler of one walk (`Visit` has no extra arguments).
// glue: ast.Visitor (QBHintHandler.Enter / Leave)
struct AcceptVisitor<'h, 'w> {
    h: &'h mut QBHintHandler,
    warn_handler: Option<&'w dyn HintWarnHandler>,
}

impl Visit for AcceptVisitor<'_, '_> {
    // glue: ast.Visitor
    fn enter(&mut self, n: CNode<'_>) -> bool {
        match self.h.enter(n, self.warn_handler) {
            Ok(skip) => skip,
            Err(e) => {
                self.fail(e);
                true
            }
        }
    }
    // glue: ast.Visitor
    fn leave(&mut self, _: CNode<'_>) -> bool {
        self.h.walk_err.is_none() && self.h.leave()
    }
    // glue: ast.Visitor
    fn fail(&mut self, e: Error) {
        if self.h.walk_err.is_none() {
            self.h.walk_err = Some(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::blocked::blocked_by;
    use tidb_parser::ast::{Expr, ResultSetNode, SetOprNode};
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
    fn select_blocks_are_numbered_in_walk_order() {
        // Go: the outer SELECT is 1, then the derived table in FROM, then the subquery in
        // WHERE (SelectStmt.Accept visits Fields, From, Where, ...).
        let s = stmt("select a from (select b from t2) x where a > (select max(c) from t3)");
        let mut h = new_qb_hint_handler();
        h.accept(&s, None).unwrap();
        assert_eq!(h.max_select_stmt_offset(), 3);
        let Stmt::Select(outer) = &s else { panic!() };
        assert_eq!(h.query_block_offset(outer), 1);
        let from = outer.from.as_ref().unwrap();
        let derived = match from.table_refs.left.as_ref() {
            Some(ResultSetNode::TableSource(ts)) => match &ts.source {
                ResultSetNode::Select(s) => s,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(h.query_block_offset(derived), 2);
        let Some(Expr::Binary(b)) = &outer.where_ else {
            panic!()
        };
        let Expr::Subquery(sq) = &b.r else { panic!() };
        let ResultSetNode::Select(sub) = &sq.query else {
            panic!()
        };
        assert_eq!(h.query_block_offset(sub), 3);
        // A SELECT the handler never visited reads Go's unset field.
        let other = stmt("select 1");
        let Stmt::Select(o) = &other else { panic!() };
        assert_eq!(h.query_block_offset(o), 0);
    }

    #[test]
    fn set_operation_blocks_are_numbered_left_to_right() {
        let s = stmt("select a from t union all select b from t2 union select c from t3");
        let mut h = new_qb_hint_handler();
        h.accept(&s, None).unwrap();
        assert_eq!(h.max_select_stmt_offset(), 3);
        let Stmt::SetOpr(so) = &s else { panic!() };
        let offs: Vec<isize> = so
            .select_list
            .selects
            .iter()
            .map(|n| match n {
                SetOprNode::Select(s) => h.query_block_offset(s),
                _ => -1,
            })
            .collect();
        assert_eq!(offs, vec![1, 2, 3]);
    }

    #[test]
    fn non_empty_hint_lists_are_blocked() {
        let h = new_qb_hint_handler();
        let th = TableOptimizerHint::default();
        let e = h.get_current_stmt_hints(&[&th], 1, None, None).unwrap_err();
        assert_eq!(blocked_by(&e), Some("D02.2"));
        assert_eq!(
            blocked_by(&h.is_hint4_view(&th).unwrap_err()),
            Some("D02.2")
        );
        let mut h = new_qb_hint_handler();
        assert_eq!(
            blocked_by(&h.check_query_block_hints(&[&th], 1, None).unwrap_err()),
            Some("D02.2")
        );
        assert_eq!(
            blocked_by(&h.handle_view_hints(&[&th], 1, None).unwrap_err()),
            Some("D02.2")
        );
        // The empty lists.
        assert!(
            h.get_current_stmt_hints(&[], 1, None, None)
                .unwrap()
                .is_empty()
        );
        let mut st = h.new_build_state();
        assert!(st.view_qb_name_used.is_none());
        h.get_current_stmt_hints(&[], 1, Some(&mut st), None)
            .unwrap();
        assert!(st.qb_offset_to_hints.is_some());
    }

    #[test]
    fn block_names_resolve_like_go() {
        let mut h = new_qb_hint_handler();
        h.accept(&stmt("select a from (select b from t2) x"), None)
            .unwrap();
        assert_eq!(h.get_hint_offset(&CIStr::new(""), 7), 7);
        assert_eq!(h.get_hint_offset(&CIStr::new("sel_2"), 1), 2);
        assert_eq!(h.get_hint_offset(&CIStr::new("SEL_3"), 1), -1);
        assert_eq!(h.get_hint_offset(&CIStr::new("sel_x"), 1), -1);
        assert_eq!(h.get_hint_offset(&CIStr::new("upd_1"), 1), 0);
        assert_eq!(h.get_hint_offset(&CIStr::new("del_1"), 1), 0);
        assert_eq!(h.get_hint_offset(&CIStr::new("qb"), 1), -1);
        h.qb_name_to_sel_offset = Some(HashMap::from([("qb".to_owned(), 2)]));
        assert_eq!(h.get_hint_offset(&CIStr::new("qb"), 1), 2);
        let tables = [HintTable {
            qb_name: CIStr::new("sel_9"),
            ..Default::default()
        }];
        assert!(!h.check_table_qb_name(&tables));
        assert!(h.check_table_qb_name(&[HintTable::default()]));
    }

    #[test]
    fn unused_view_hints_and_build_state() {
        let mut h = new_qb_hint_handler();
        assert_eq!(
            h.handle_unused_view_hints(None, None, vec!["x".into()]),
            vec!["x".to_owned()]
        );
        h.view_qb_name_to_table = Some(HashMap::from([("v".to_owned(), vec![])]));
        let mut st = h.new_build_state();
        assert!(st.view_qb_name_used.is_some());
        let warn = h.handle_unused_view_hints(
            Some(&st),
            Some(&tidb_sessionctx::stmtctx::new_stmt_ctx()),
            vec![],
        );
        assert_eq!(warn.len(), 1);
        h.mark_view_qb_name_used("v", Some(&mut st));
        let warn = h.handle_unused_view_hints(
            Some(&st),
            Some(&tidb_sessionctx::stmtctx::new_stmt_ctx()),
            vec![],
        );
        assert!(warn.is_empty());
        // No warning handler: no warning (Go checks p.warnHandler != nil).
        let st = h.new_build_state();
        assert!(
            h.handle_unused_view_hints(Some(&st), None, vec![])
                .is_empty()
        );
    }
}
