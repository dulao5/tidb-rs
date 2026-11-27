//! The prototype's prepared statements and Go's `skip prepared plan-cache: <reason>` warning
//! (#512; plan cache spec `specs/D13.1-plancache/spec.md` §4.4).
//!
//! Go decides at PREPARE whether a statement may use the plan cache
//! (`GeneratePlanCacheStmtWithAST`: "plan cache is disabled", `IsASTCacheable`, the
//! `Fix49736` override), warns there, keeps the reason in `PlanCacheStmt.UncacheableReason`,
//! and warns again on every EXECUTE (`GetPlanFromPlanCache` L214-216). The prototype has no
//! plan cache, but the statements it answers (`@@var`, `DATABASE()`, `USER()`, ... in every
//! `--new-stack` mode, and every prepared statement with `off`) must report the same
//! warnings. Nothing here is a translation: the decision is the ported
//! [`prepare_cacheability`] and the EXECUTE warning is the ported `PlanCacheTracker`.
//!
//! Inputs (maintainer decision on #512): with `fallback` / `strict` the router passes the new
//! session's synced variables and the ported domain's InfoSchema; with `off` there is neither,
//! so the prototype uses a session with Go's default variables (the prototype rejects every
//! `SET` of a variable the decision reads) and no InfoSchema, which leaves out the table checks
//! of `checkTableCacheable` (partitioned, generated-column and temporary tables). That is a
//! listed known difference of the prototype path.

use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::contextutil::{
    SQLWarn, SessionPrepared, WarnAppender, WarnLevelNote, WarnLevelWarning, new_plan_cache_tracker,
};
use tidb_mysql_protocol::SqlError;
use tidb_parser::ast;
use tidb_planner::core::plan_cache_utils::{InfoSchemaLookup, prepare_cacheability};
use tidb_sessionctx::variable::session::SessionVars;

/// What a PREPARE on the prototype keeps and reports about the plan cache.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrepareCacheability {
    /// `PlanCacheStmt.UncacheableReason`: warned again on every EXECUTE (empty: none).
    pub reason: String,
    /// The warnings of the PREPARE itself (`skip prepared plan-cache: ...` or
    /// `force plan-cache: ...`).
    pub warnings: Vec<SQLWarn>,
}

/// Go's prepare-time cacheability of `stmt` for a statement the prototype prepared. `is` is
/// `None` when there is no ported InfoSchema to check tables with (see the module doc). A
/// boundary stub the decision reaches (a node the Rust AST does not have) is returned as the
/// PREPARE's 1235, never skipped.
// glue: the prototype's PREPARE takes the uncacheable reason from the ported GeneratePlanCacheStmtWithAST decision (#512)
pub fn prepare_time_cacheability(
    vars: &SessionVars,
    stmt: &ast::Stmt,
    is: Option<&dyn InfoSchemaLookup>,
) -> Result<PrepareCacheability, SqlError> {
    let (_, reason, warnings) =
        prepare_cacheability(vars, true, stmt, is).map_err(|e| crate::router::sql_error(&e))?;
    Ok(PrepareCacheability {
        reason,
        warnings: warnings
            .into_iter()
            .map(|err| SQLWarn {
                level: WarnLevelWarning.into(),
                err,
            })
            .collect(),
    })
}

/// The warnings `GetPlanFromPlanCache` reports on an EXECUTE of a statement prepared with
/// `reason` (Go L204-216: the cache type is `SessionPrepared`, then
/// `WarnSkipPlanCache(stmt.UncacheableReason)` when the reason is not empty), through the
/// ported `PlanCacheTracker`.
// glue: GetPlanFromPlanCache's WarnSkipPlanCache on the prototype's EXECUTE (#512)
pub fn execute_warnings(reason: &str) -> Vec<SQLWarn> {
    if reason.is_empty() {
        return Vec::new();
    }
    let sink = Arc::new(Sink::default());
    let tracker = new_plan_cache_tracker(Some(Arc::clone(&sink) as _));
    tracker.set_cache_type(SessionPrepared);
    tracker.warn_skip_plan_cache(reason);
    std::mem::take(&mut *sink.0.lock())
}

/// The statement context's warning list as the tracker's warn handler.
#[derive(Default)]
struct Sink(Mutex<Vec<SQLWarn>>);

impl WarnAppender for Sink {
    // glue: interface method of contextutil.WarnAppender (StmtCtx.AppendWarning)
    fn append_warning(&self, err: Error) {
        self.0.lock().push(SQLWarn {
            level: WarnLevelWarning.into(),
            err,
        });
    }
    // glue: interface method of contextutil.WarnAppender (StmtCtx.AppendNote)
    fn append_note(&self, err: Error) {
        self.0.lock().push(SQLWarn {
            level: WarnLevelNote.into(),
            err,
        });
    }
}

#[cfg(test)]
mod tests {
    use tidb_session::{Dialect, Session};

    use super::*;
    use crate::router::newstack::DefaultGlobals;

    /// A session with Go's default variables, as the off-mode PREPARE uses.
    fn defaults() -> Session {
        let mut s = Session::new(Dialect::MySQL, Box::new(DefaultGlobals));
        s.load_common_global_variables_if_needed().unwrap();
        s
    }

    fn decide(s: &Session, sql: &str) -> PrepareCacheability {
        let stmt = tidb_sql::ir::parse_one(sql).unwrap();
        prepare_time_cacheability(s.vars(), &stmt, None).unwrap()
    }

    fn texts(warns: &[SQLWarn]) -> Vec<(String, String)> {
        warns
            .iter()
            .map(|w| {
                (
                    w.level.to_string(),
                    String::from_utf8_lossy(&w.err.error()).into_owned(),
                )
            })
            .collect()
    }

    /// The statements the prototype answers in every mode get Go's reasons (the texts of
    /// `cacheableChecker.Enter`), with one `skip prepared plan-cache` warning at PREPARE.
    #[test]
    fn uncacheable_reasons_of_the_prototype_statements() {
        let s = defaults();
        for (sql, reason) in [
            (
                "SELECT @@max_allowed_packet",
                "query has user-defined variables is un-cacheable",
            ),
            (
                "SELECT @a",
                "query has user-defined variables is un-cacheable",
            ),
            ("SELECT DATABASE()", "query has 'database' is un-cacheable"),
            ("SELECT USER()", "query has 'user' is un-cacheable"),
            (
                "SELECT CURRENT_USER()",
                "query has 'current_user' is un-cacheable",
            ),
        ] {
            let c = decide(&s, sql);
            assert_eq!(c.reason, reason, "{sql}");
            assert_eq!(
                texts(&c.warnings),
                [(
                    "Warning".to_owned(),
                    format!("skip prepared plan-cache: {reason}")
                )],
                "{sql}"
            );
        }
    }

    /// A cacheable statement: no reason, no warning, nothing on EXECUTE.
    #[test]
    fn cacheable_statement_has_no_warning() {
        let s = defaults();
        for sql in ["SELECT 1", "SELECT ? + 1", "SELECT c FROM t WHERE id = ?"] {
            let c = decide(&s, sql);
            assert_eq!(c, PrepareCacheability::default(), "{sql}");
            assert!(execute_warnings(&c.reason).is_empty(), "{sql}");
        }
    }

    /// With the prepared plan cache switched off, Go's reason is "plan cache is disabled":
    /// the PREPARE itself does not warn (Go L141-145 skip the warning branch), every EXECUTE
    /// does.
    #[test]
    fn plan_cache_disabled() {
        let mut s = defaults();
        s.vars_mut()
            .set_system_var("tidb_enable_prepared_plan_cache", "OFF")
            .unwrap();
        let c = decide(&s, "SELECT 1");
        assert_eq!(c.reason, "plan cache is disabled");
        assert!(c.warnings.is_empty());
        assert_eq!(
            texts(&execute_warnings(&c.reason)),
            [(
                "Warning".to_owned(),
                "skip prepared plan-cache: plan cache is disabled".to_owned()
            )]
        );
    }

    /// Every EXECUTE of an uncacheable statement warns once, with the same text.
    #[test]
    fn execute_warns_once_per_execution() {
        let reason = "query has 'database' is un-cacheable";
        for _ in 0..2 {
            let w = execute_warnings(reason);
            assert_eq!(
                texts(&w),
                [(
                    "Warning".to_owned(),
                    format!("skip prepared plan-cache: {reason}")
                )]
            );
        }
        assert!(execute_warnings("").is_empty());
    }

    /// SHOW WARNINGS shows the warning as Go does: `Warning | 1105 | <text>` (a plain error
    /// is shown with `ErrUnknown`); SHOW ERRORS shows none of it.
    #[test]
    fn show_warnings_rows() {
        let w = execute_warnings("query has 'user' is un-cacheable");
        let rs = crate::warnings_result_set(&w, false);
        assert_eq!(
            rs.rows,
            vec![vec![
                tidb_mysql_protocol::Value::Bytes(b"Warning".to_vec()),
                tidb_mysql_protocol::Value::Int(1105),
                tidb_mysql_protocol::Value::Bytes(
                    b"skip prepared plan-cache: query has 'user' is un-cacheable".to_vec()
                ),
            ]]
        );
        assert_eq!(rs.columns.len(), 3);
        assert!(crate::warnings_result_set(&w, true).rows.is_empty());
    }

    /// The statements of the prototype's corpora: the ab workloads and the parser's
    /// prototype list (one statement per line there; `;`-separated in the workloads).
    fn corpus() -> Vec<String> {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../..");
        let mut out = Vec::new();
        let workloads = format!("{root}/tools/ab/workloads");
        let mut files: Vec<_> = std::fs::read_dir(&workloads)
            .expect("tools/ab/workloads")
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no workload in {workloads}");
        let code = |l: &&str| {
            let t = l.trim_start();
            !(t.is_empty() || t.starts_with('#') || t.starts_with("--"))
        };
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let body: Vec<&str> = text.lines().filter(code).collect();
            out.extend(
                body.join("\n")
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
        let proto =
            std::fs::read_to_string(format!("{root}/crates/parser/tests/data/prototype.sql"))
                .unwrap();
        out.extend(
            proto
                .lines()
                .filter(code)
                .map(|l| l.trim().trim_end_matches(';').to_owned()),
        );
        out
    }

    /// Statements that reach the decision's boundary stubs (optimizer hints D13.3; DML and
    /// nodes the Rust AST does not have, D02.2).
    const BOUNDARY: &[&str] = &[
        "SELECT /*+ USE_INDEX(t, i) */ c FROM t WHERE id = ?",
        "SELECT /*+ IGNORE_PLAN_CACHE() */ c FROM t WHERE id = ?",
        "UPDATE t SET c = 1 WHERE id = ?",
        "INSERT INTO t VALUES (?)",
        "DELETE FROM t WHERE id = ?",
    ];

    /// In `--new-stack=off` the decision runs after the prototype accepted a PREPARE: it
    /// must never turn that PREPARE into a 1235. So every statement whose decision hits a
    /// boundary stub is one the prototype's lowering refuses, over the prototype's corpora
    /// and the known boundary statements: the PoC's refusal list and the decision's
    /// boundary list are pinned to each other. A PoC change that starts accepting, say,
    /// optimizer hints fails here instead of breaking PREPARE.
    #[test]
    fn decision_never_blocks_a_statement_the_prototype_prepares() {
        let s = defaults();
        let mut accepted = 0;
        let mut boundary = 0;
        let corpus = corpus();
        let all = corpus
            .iter()
            .map(String::as_str)
            .chain(BOUNDARY.iter().copied());
        for sql in all {
            let Ok(stmt) = tidb_sql::ir::parse_one(sql) else {
                continue;
            };
            let lowered = tidb_sql::ir::lower_statement(&stmt, sql);
            let decision = prepare_time_cacheability(s.vars(), &stmt, None);
            if let Err(e) = &decision {
                boundary += 1;
                assert!(
                    lowered.is_err(),
                    "{sql}: the prototype prepares it, but the plan-cache decision fails: {} {}",
                    e.code,
                    e.message
                );
            }
            if lowered.is_ok() {
                accepted += 1;
            }
        }
        // The pin means something only while both sides are exercised.
        assert!(accepted >= 100, "only {accepted} accepted statements");
        assert!(
            boundary >= BOUNDARY.len(),
            "only {boundary} statements reach a boundary stub"
        );
    }
}
