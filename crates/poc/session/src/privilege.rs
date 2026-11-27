//! Server-side privilege integration for `poc-session` (D04.1 Part 5/5):
//! - [`PrivilegeSource`] wraps the live [`tidb_privilege::Handle`] (and optional cluster
//!   [`poc_domain::PrivilegeSync`] for on-demand reload).
//! - [`check_poc_select_privilege`] enforces rulings R29, R30, R31, and R32 on the PoC fallback
//!   path (`Session::plan` / `Session::prepare_lowered`) using the exact same live
//!   [`tidb_privilege::PrivilegeManager`] bound to the new stack, so statements routed to the
//!   PoC (e.g. `SELECT COUNT(*)`, `SELECT SUM(1)`, or `--new-stack=off`) never bypass privilege
//!   verification.

use std::sync::Arc;

use poc_domain::PrivilegeSync;
use tidb_base::mysql::SelectPriv;
use tidb_meta::InfoSchema;
use tidb_mysql_protocol::SqlError;
use tidb_parser::auth::RoleIdentity;
use tidb_privilege::{Handle, PrivilegeManager};
use tidb_sql::ir::{Expr, Field, Select};

/// Live privilege cache backing multi-account authentication and session privilege checks.
#[derive(Clone)]
pub struct PrivilegeSource {
    handle: Arc<Handle>,
    sync: Option<PrivilegeSync>,
}

impl PrivilegeSource {
    /// Creates a privilege source backed by a live cluster [`PrivilegeSync`].
    pub fn from_sync(sync: PrivilegeSync) -> Self {
        let handle = Arc::clone(sync.handle());
        Self {
            handle,
            sync: Some(sync),
        }
    }

    /// Creates a privilege source backed by an in-memory [`Handle`] (for unit tests).
    pub fn from_handle(handle: Arc<Handle>) -> Self {
        Self { handle, sync: None }
    }

    /// Returns the shared lock-free [`Handle`].
    pub fn handle(&self) -> &Arc<Handle> {
        &self.handle
    }

    /// Triggers an on-demand reload from TiKV when backed by [`PrivilegeSync`].
    pub async fn reload_if_synced(&self) {
        if let Some(sync) = &self.sync
            && let Err(e) = sync.reload_now().await
        {
            log::warn!("on-demand privilege reload failed: {e}");
        }
    }
}

/// Enforces rulings R29, R30, R31, and R32 on a PoC `SELECT` statement before planning/reading.
///
/// Matches the new stack's planner (`tidb-planner`) semantics:
/// 1. Missing table (R31): requires `SelectPriv` with `column = "*"` -> returns `1142` when denied;
///    otherwise returns `Ok(())` so `tidb_sql::plan_select` reports `1146`.
/// 2. Existing table baseline (R29 + R30): requires `SelectPriv` with `column = "*"` FIRST ->
///    returns `1142` (`for table '...'`) before any column check (`1143`).
/// 3. Wildcard (`*`, `t.*`, `db.t.*`): requires `SelectPriv` on every public column of the table,
///    returning `1142` (`for table '...'`, matching Go `ReportTableErrOption`).
/// 4. Explicit column references (`SELECT`, `WHERE`, `GROUP BY`, `ORDER BY`):
///    - Hidden `_tidb_rowid` (R32): requires full-table `SelectPriv` (`column = ""`), returning
///      `1143` (`for column '_tidb_rowid' in table '...'`).
///    - Regular public columns: requires `SelectPriv` on the column, returning `1143`.
///    - Unknown columns are skipped here so `tidb_sql::plan_select` reports `1054 Unknown column`.
pub fn check_poc_select_privilege(
    pm: &dyn PrivilegeManager,
    active_roles: &[RoleIdentity],
    auth_user: &str,
    auth_host: &str,
    current_db: Option<&str>,
    sel: &Select,
    is: &InfoSchema,
) -> Result<(), SqlError> {
    let Some(from) = &sel.from else {
        return Ok(());
    };
    let db = if from.schema.is_empty() {
        let Some(cur) = current_db else {
            return Err(SqlError::new(
                tidb_mysql_protocol::error::code::ER_NO_DB,
                "No database selected",
            ));
        };
        cur
    } else {
        from.schema.as_str()
    };
    let table = from.name.as_str();

    let Some(tbl) = is.table_by_name(db, table) else {
        // R31: missing table requires statement-appropriate SelectPriv ("*") before leaking 1146.
        if !pm.request_verification(active_roles, db, table, "*", SelectPriv) {
            return Err(SqlError::table_access_denied(
                "SELECT", auth_user, auth_host, table,
            ));
        }
        return Ok(());
    };

    // R29 + R30: table-read baseline ("*") checked before any column check (1142 before 1143).
    if !pm.request_verification(active_roles, db, &tbl.name.l, "*", SelectPriv) {
        return Err(SqlError::table_access_denied(
            "SELECT",
            auth_user,
            auth_host,
            &tbl.name.o,
        ));
    }

    // Wildcards (`*`, `t.*`, `db.t.*`) require SELECT on every public non-hidden column (1142).
    let has_wildcard = sel
        .fields
        .iter()
        .any(|f| matches!(f, Field::Wildcard { .. }));
    if has_wildcard {
        for col in tbl.columns.iter().filter(|c| c.is_public() && !c.hidden) {
            if !pm.request_verification(active_roles, db, &tbl.name.l, &col.name.l, SelectPriv) {
                return Err(SqlError::table_access_denied(
                    "SELECT",
                    auth_user,
                    auth_host,
                    &tbl.name.o,
                ));
            }
        }
    }

    // Collect explicit column references across SELECT, WHERE, GROUP BY, and ORDER BY.
    let mut cols: Vec<&str> = Vec::new();
    for field in &sel.fields {
        if let Field::Expr { expr, .. } = field {
            collect_expr_cols(expr, &mut cols);
        }
    }
    if let Some(w) = &sel.where_ {
        collect_expr_cols(w, &mut cols);
    }
    for item in &sel.group_by {
        collect_expr_cols(&item.expr, &mut cols);
    }
    for item in &sel.order_by {
        collect_expr_cols(&item.expr, &mut cols);
    }

    for col_name in cols {
        if col_name.eq_ignore_ascii_case("_tidb_rowid") {
            // R32: `_tidb_rowid` requires full-table SELECT (`column = ""`).
            if !pm.request_verification(active_roles, db, &tbl.name.l, "", SelectPriv) {
                return Err(SqlError::column_access_denied(
                    "SELECT",
                    auth_user,
                    auth_host,
                    "_tidb_rowid",
                    &tbl.name.o,
                ));
            }
            continue;
        }
        let col_lower = col_name.to_lowercase();
        let Some(col_info) = tbl
            .columns
            .iter()
            .find(|c| c.is_public() && !c.hidden && c.name.l == col_lower)
        else {
            // Unknown column: let `tidb_sql::plan_select` report 1054.
            continue;
        };
        if !pm.request_verification(active_roles, db, &tbl.name.l, &col_info.name.l, SelectPriv) {
            return Err(SqlError::column_access_denied(
                "SELECT",
                auth_user,
                auth_host,
                &col_info.name.o,
                &tbl.name.o,
            ));
        }
    }

    Ok(())
}

fn collect_expr_cols<'a>(expr: &'a Expr, out: &mut Vec<&'a str>) {
    match expr {
        Expr::Column(c) => out.push(c.name.as_str()),
        Expr::Neg(inner) => collect_expr_cols(inner, out),
        Expr::Cmp(_, l, r) | Expr::And(l, r) => {
            collect_expr_cols(l, out);
            collect_expr_cols(r, out);
        }
        Expr::Between { expr, low, high } => {
            collect_expr_cols(expr, out);
            collect_expr_cols(low, out);
            collect_expr_cols(high, out);
        }
        Expr::Agg { arg, .. } => collect_expr_cols(arg, out),
        Expr::Literal(_)
        | Expr::Param(_)
        | Expr::SysVar { .. }
        | Expr::Func { .. }
        | Expr::Position(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::mysql::{InsertPriv, PrivilegeType, UpdatePriv};
    use tidb_meta::model::{CIStr, ColumnInfo, DbInfo, SchemaState, TableInfo};
    use tidb_privilege::{
        ColumnsPrivRecord, DbRecord, MySQLPrivilege, TablesPrivRecord, UserPrivileges, UserRecord,
    };
    use tidb_sql::Statement;

    fn test_schema() -> InfoSchema {
        let db = DbInfo {
            id: 1,
            name: CIStr {
                o: "test".to_owned(),
                l: "test".to_owned(),
            },
            state: SchemaState::Public,
            ..Default::default()
        };
        let col = |id: i64, offset: usize, name: &str| ColumnInfo {
            id,
            name: CIStr {
                o: name.to_owned(),
                l: name.to_lowercase(),
            },
            offset,
            state: SchemaState::Public,
            ..Default::default()
        };
        let t = TableInfo {
            id: 10,
            name: CIStr {
                o: "t".to_owned(),
                l: "t".to_owned(),
            },
            columns: vec![col(1, 0, "a"), col(2, 1, "b")],
            state: SchemaState::Public,
            ..Default::default()
        };
        InfoSchema::new(1, vec![(db, vec![t])])
    }

    fn test_handle() -> Arc<Handle> {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "root", SelectPriv));
        mp.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "u_ins", PrivilegeType(0)));
        mp.add_table_priv(TablesPrivRecord::new("u_ins", "%", "test", "t", InsertPriv));
        mp.add_user(UserRecord::new("%", "u_cola", PrivilegeType(0)));
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_cola", "%", "test", "t", "a", SelectPriv,
        ));
        mp.add_user(UserRecord::new("%", "u_mixed", PrivilegeType(0)));
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_mixed", "%", "test", "t", "a", UpdatePriv,
        ));
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_mixed", "%", "test", "t", "b", SelectPriv,
        ));
        mp.add_user(UserRecord::new("%", "u_db_ins", PrivilegeType(0)));
        mp.add_db(DbRecord::new("u_db_ins", "%", "test", InsertPriv));
        mp.add_user(UserRecord::new("%", "u_db_sel", PrivilegeType(0)));
        mp.add_db(DbRecord::new("u_db_sel", "%", "test", SelectPriv));
        Arc::new(Handle::new(mp))
    }

    fn check(handle: &Arc<Handle>, is: &InfoSchema, user: &str, sql: &str) -> Result<(), SqlError> {
        let lowered = tidb_sql::parse_and_lower(sql).unwrap();
        let Statement::Select(sel) = &lowered.stmt else {
            panic!("not select: {sql}");
        };
        let pm = UserPrivileges::new(user, "%", Arc::clone(handle));
        check_poc_select_privilege(&pm, &[], user, "%", None, sel, is)
    }

    #[test]
    fn poc_fallback_enforces_r29_r30_r31_r32_and_live_revoke() {
        let is = test_schema();
        let handle = test_handle();

        // Zero-SELECT users fail with 1142 on COUNT(*), SUM(1), SELECT 1, SELECT a, SELECT *.
        for user in ["u_none", "u_ins", "u_db_ins"] {
            for sql in [
                "SELECT 1 FROM test.t",
                "SELECT COUNT(*) FROM test.t",
                "SELECT SUM(1) FROM test.t",
                "SELECT a FROM test.t",
                "SELECT * FROM test.t",
                "SELECT 1 FROM test.nosuch",
            ] {
                let err = check(&handle, &is, user, sql).expect_err(sql);
                assert_eq!(err.code, 1142, "{user} on `{sql}`: {err}");
                assert_eq!(err.state, "42000");
            }
        }

        // u_cola: passes COUNT(*), SELECT 1, SELECT a; gets 1143 on b & _tidb_rowid; 1142 on *.
        for ok_sql in [
            "SELECT 1 FROM test.t",
            "SELECT COUNT(*) FROM test.t",
            "SELECT SUM(1) FROM test.t",
            "SELECT a FROM test.t",
        ] {
            assert!(check(&handle, &is, "u_cola", ok_sql).is_ok(), "{ok_sql}");
        }
        assert_eq!(
            check(&handle, &is, "u_cola", "SELECT b FROM test.t")
                .unwrap_err()
                .code,
            1143
        );
        assert_eq!(
            check(&handle, &is, "u_cola", "SELECT COUNT(b) FROM test.t")
                .unwrap_err()
                .code,
            1143
        );
        assert_eq!(
            check(&handle, &is, "u_cola", "SELECT _tidb_rowid FROM test.t")
                .unwrap_err()
                .code,
            1143
        );
        assert_eq!(
            check(&handle, &is, "u_cola", "SELECT * FROM test.t")
                .unwrap_err()
                .code,
            1142
        );

        // u_db_sel and root pass missing-table gate (letting plan_select report 1146).
        assert!(check(&handle, &is, "u_db_sel", "SELECT 1 FROM test.nosuch").is_ok());
        assert!(check(&handle, &is, "root", "SELECT 1 FROM test.nosuch").is_ok());
        assert!(check(&handle, &is, "root", "SELECT _tidb_rowid FROM test.t").is_ok());

        // Live revoke via Handle::replace immediately revokes u_cola's access.
        let mut next = MySQLPrivilege::new();
        next.add_user(UserRecord::new("%", "u_cola", PrivilegeType(0)));
        handle.replace(next);
        assert_eq!(
            check(&handle, &is, "u_cola", "SELECT a FROM test.t")
                .unwrap_err()
                .code,
            1142
        );
    }

    #[test]
    fn sql_states_and_account_locking_match_go() {
        let e1044 = SqlError::db_access_denied("u_none", "%", "test");
        assert_eq!(e1044.code, 1044);
        assert_eq!(e1044.state, "42000");
        assert_eq!(
            e1044.message,
            "Access denied for user 'u_none'@'%' to database 'test'"
        );

        let e1142 = SqlError::table_access_denied("SELECT", "u_none", "%", "t");
        assert_eq!(e1142.code, 1142);
        assert_eq!(e1142.state, "42000");
        assert_eq!(
            e1142.message,
            "SELECT command denied to user 'u_none'@'%' for table 't'"
        );

        let e1143 = SqlError::column_access_denied("SELECT", "u_cola", "%", "b", "t");
        assert_eq!(e1143.code, 1143);
        assert_eq!(e1143.state, "42000");
        assert_eq!(
            e1143.message,
            "SELECT command denied to user 'u_cola'@'%' for column 'b' in table 't'"
        );

        // Verify locked accounts (roles) and password hash lookup via PrivilegeSource.
        let mut mp = MySQLPrivilege::new();
        mp.add_user(
            UserRecord::new("%", "root", SelectPriv)
                .with_auth_string("*23AE809DDACAF96AF0FD78ED04B6A265E05AA257"),
        );
        mp.add_user(UserRecord::new("%", "u_nopw", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "r_locked", PrivilegeType(0)).with_account_locked(true));
        let src = PrivilegeSource::from_handle(Arc::new(Handle::new(mp)));
        let snap = src.handle().get();

        let root = snap.match_user("root", "127.0.0.1").unwrap();
        assert!(!root.account_locked);
        assert_eq!(
            tidb_mysql_protocol::Credential::native_from_authentication_string(&root.auth_string),
            Some(tidb_mysql_protocol::Credential::native_from_password("123"))
        );

        let nopw = snap.match_user("u_nopw", "127.0.0.1").unwrap();
        assert!(!nopw.account_locked);
        assert_eq!(
            tidb_mysql_protocol::Credential::native_from_authentication_string(&nopw.auth_string),
            Some(tidb_mysql_protocol::Credential::NativePassword(None))
        );

        let locked = snap.match_user("r_locked", "127.0.0.1").unwrap();
        assert!(locked.account_locked);
    }
}
