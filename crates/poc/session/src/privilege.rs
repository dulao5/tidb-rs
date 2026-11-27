//! Server-side privilege integration for `poc-session` (D04.1 Part 5/5):
//! - [`PrivilegeSource`] wraps the live [`tidb_privilege::Handle`] (and optional cluster
//!   [`poc_domain::PrivilegeSync`]).
//! - [`authenticate_user_record`] enforces fail-closed login rules against the loaded `mysql.user`
//!   and `mysql.global_priv` snapshot (refusing non-native plugins, `REQUIRE SSL/X509/CIPHER/SAN`
//!   or broken `global_priv`, expired or lifetime-limited passwords, and failed-login lockout
//!   policies while returning `account_locked` for Go-exact `3118` reporting after password check).
//! - [`check_poc_select_privilege`] enforces rulings R29, R30, R31, and R32 on the PoC fallback
//!   path (`Session::plan` / `Session::prepare_lowered`) using the exact same live
//!   [`tidb_privilege::PrivilegeManager`] bound to the new stack.

use std::collections::HashSet;
use std::sync::Arc;

use poc_domain::PrivilegeSync;
use tidb_base::mysql::SelectPriv;
use tidb_meta::InfoSchema;
use tidb_mysql_protocol::{AccountAuth, Credential, SqlError};
use tidb_parser::auth::RoleIdentity;
use tidb_privilege::{Handle, MySQLPrivilege, PrivilegeManager};
use tidb_sql::ir::{Expr, Field, Select};

/// Live privilege cache backing multi-account authentication and session privilege checks.
#[derive(Clone)]
pub struct PrivilegeSource {
    handle: Arc<Handle>,
    #[allow(dead_code)]
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
}

/// Resolves an account from a [`MySQLPrivilege`] snapshot for MySQL handshake authentication.
///
/// Fails closed (`None` -> `1045`) for any account that relies on an unported authentication or
/// login-restriction feature:
/// - `mysql.user.plugin` other than `mysql_native_password` (or `""`, normalized on load),
///   preventing empty-password bypass on `auth_socket`, `tidb_auth_token`, LDAP, etc.
/// - `mysql.global_priv` rows with `REQUIRE SSL/X509/CIPHER/SAN` or broken JSON.
/// - `mysql.user.password_expired = 'Y'` or non-zero `password_lifetime`.
/// - `mysql.user.User_attributes` failed-login lockout policy or auto-lock state.
///
/// Locked accounts (`Account_locked = 'Y'`) are returned with `account_locked = true` so the
/// protocol layer verifies the password first (`1045` on mismatch) and then returns Go's exact
/// `3118 (HY000) Access denied for user '<user>'@'<host>'. Account is locked.`.
pub fn authenticate_user_record(
    snap: &MySQLPrivilege,
    user: &str,
    host: &str,
) -> Option<AccountAuth> {
    let rec = snap.match_user(user, host)?;
    if !rec.auth_plugin.is_empty() && rec.auth_plugin != "mysql_native_password" {
        return None;
    }
    let lifetime_active = match rec.password_lifetime {
        // `PASSWORD EXPIRE NEVER` explicitly overrides global `default_password_lifetime`.
        Some(0) => false,
        Some(_) => true,
        // SQL `NULL` (CREATE USER default) inherits `@@global.default_password_lifetime`.
        None => snap.default_password_lifetime() > 0,
    };
    if rec.password_expired || lifetime_active || rec.has_failed_login_policy {
        return None;
    }
    if let Some(gp) = snap.match_global_priv(&rec.base.user, &rec.base.host)
        && (gp.broken || gp.require_ssl)
    {
        return None;
    }
    let credential = Credential::native_from_authentication_string(&rec.auth_string)?;
    Some(AccountAuth {
        credential,
        auth_user: Some(rec.base.user.clone()),
        auth_host: Some(rec.base.host.clone()),
        account_locked: rec.account_locked,
    })
}

/// Enforces R31 (missing-table `1142` vs `1146` oracle check) during the PoC preprocess phase,
/// **before** [`tidb_sql::plan_select`] runs.
///
/// Mirrors Go's `preprocess::tableByName`: if the target table does not exist in `is`, an
/// unauthorized caller gets `1142` instead of `1146`. When the table *does* exist, this returns
/// `Ok(())` so [`tidb_sql::plan_select`] runs first and reports plan-build errors (e.g. `1054`
/// unknown column, `only_full_group_by`) before post-build [`check_poc_select_privilege`] runs.
pub fn check_poc_missing_table_privilege(
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
    if is.table_by_name(db, table).is_none()
        && !pm.request_verification(active_roles, db, table, "*", SelectPriv)
    {
        return Err(SqlError::table_access_denied(
            "SELECT", auth_user, auth_host, table,
        ));
    }
    Ok(())
}

/// Enforces rulings R29, R30, R31, and R32 on a PoC `SELECT` statement after logical planning
/// (matching Go's `preprocess -> build_logical_plan -> check_privilege` error order).
///
/// Matches the new stack's planner (`tidb-planner`) semantics:
/// 1. Missing table (R31): requires `SelectPriv` with `column = "*"` -> returns `1142` when denied;
///    otherwise returns `Ok(())` so `tidb_sql::plan_select` reports `1146`.
/// 2. Existing table baseline (R29 + R30): requires `SelectPriv` with `column = "*"` FIRST ->
///    returns `1142` (`for table '...'`) before any column check (`1143`).
/// 3. Wildcard (`*`, `t.*`, `db.t.*`): requires `SelectPriv` on every public column of the table,
///    returning `1142` (`for table '...'`, matching Go `ReportTableErrOption`).
/// 4. Explicit column references (`SELECT`, `WHERE`, `GROUP BY`, `ORDER BY`):
///    - Pass 1 checks ordinary public columns first (skipping unqualified `ORDER BY` aliases that
///      resolve to a `SELECT`-list alias, never skipping `GROUP BY` columns, and skipping unknown
///      columns so `plan_select` reports `1054`).
///    - Pass 2 checks hidden `_tidb_rowid` (R32, only when `!tbl.pk_is_handle && !tbl.is_common_handle`)
///      with `column = ""`, returning `1143` (`for column '_tidb_rowid' in table '...'`) only after
///      all ordinary columns pass.
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

    // Collect SELECT-list aliases for `ORDER BY` only: `tidb_sql::plan` resolves `GROUP BY`
    // directly against table columns (matching Go when a real table column shares the name), so
    // `GROUP BY` must never skip names that happen to match a `SELECT` alias.
    let mut select_aliases: HashSet<String> = HashSet::new();
    let mut cols: Vec<(&str, bool)> = Vec::new(); // (col_name, is_unqualified)
    for field in &sel.fields {
        if let Field::Expr { expr, alias, .. } = field {
            if !alias.is_empty() {
                select_aliases.insert(alias.to_lowercase());
            }
            collect_expr_cols(expr, &mut cols);
        }
    }
    if let Some(w) = &sel.where_ {
        collect_expr_cols(w, &mut cols);
    }
    for item in &sel.group_by {
        collect_expr_cols(&item.expr, &mut cols);
    }
    let mut order_cols: Vec<(&str, bool)> = Vec::new();
    for item in &sel.order_by {
        collect_expr_cols(&item.expr, &mut order_cols);
    }
    for (col_name, unqualified) in order_cols {
        if unqualified && select_aliases.contains(&col_name.to_lowercase()) {
            continue;
        }
        cols.push((col_name, unqualified));
    }

    let has_hidden_rowid = !tbl.pk_is_handle && !tbl.is_common_handle;

    // Pass 1: check all ordinary columns in Go's order first; defer R32 `_tidb_rowid` so
    // queries like `SELECT a FROM t WHERE _tidb_rowid = 1` report Go's denied column (`a`).
    let mut saw_rowid = false;
    for (col_name, _) in cols {
        if has_hidden_rowid && col_name.eq_ignore_ascii_case("_tidb_rowid") {
            saw_rowid = true;
            continue;
        }
        let col_lower = col_name.to_lowercase();
        let Some(col_info) = tbl
            .columns
            .iter()
            .find(|c| c.is_public() && !c.hidden && c.name.l == col_lower)
        else {
            // Unknown column (or `_tidb_rowid` on a PK-handle/clustered table): let `plan_select`
            // report 1054.
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

    // Pass 2: R32 `_tidb_rowid` requires full-table SELECT (`column = ""`).
    if saw_rowid && !pm.request_verification(active_roles, db, &tbl.name.l, "", SelectPriv) {
        return Err(SqlError::column_access_denied(
            "SELECT",
            auth_user,
            auth_host,
            "_tidb_rowid",
            &tbl.name.o,
        ));
    }

    Ok(())
}

fn collect_expr_cols<'a>(expr: &'a Expr, out: &mut Vec<(&'a str, bool)>) {
    match expr {
        Expr::Column(c) => out.push((c.name.as_str(), c.schema.is_empty() && c.table.is_empty())),
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
        ColumnsPrivRecord, DbRecord, GlobalPrivRecord, MySQLPrivilege, TablesPrivRecord,
        UserPrivileges, UserRecord,
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
            field_type: tidb_meta::model::FieldType {
                tp: 3,   // MYSQL_TYPE_LONG (INT)
                flag: 1, // NOT_NULL_FLAG
                ..Default::default()
            },
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
        let t_pk = TableInfo {
            id: 11,
            name: CIStr {
                o: "t_pk".to_owned(),
                l: "t_pk".to_owned(),
            },
            columns: vec![col(1, 0, "id"), col(2, 1, "a")],
            pk_is_handle: true,
            state: SchemaState::Public,
            ..Default::default()
        };
        InfoSchema::new(1, vec![(db, vec![t, t_pk])])
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
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_cola", "%", "test", "t_pk", "a", SelectPriv,
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

        // u_cola: passes COUNT(*), SELECT 1, SELECT a, and `SELECT a AS b ... ORDER BY b`;
        // gets 1143 on b & _tidb_rowid; 1142 on *; passes `_tidb_rowid` on pk_is_handle table
        // so plan_select reports 1054 instead of 1143.
        for ok_sql in [
            "SELECT 1 FROM test.t",
            "SELECT COUNT(*) FROM test.t",
            "SELECT SUM(1) FROM test.t",
            "SELECT a FROM test.t",
            "SELECT a AS b FROM test.t ORDER BY b",
            "SELECT _tidb_rowid FROM test.t_pk",
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

        // Ordinary column 1143 precedes R32 `_tidb_rowid` 1143 (matching Go).
        let mixed_a = check(
            &handle,
            &is,
            "u_mixed",
            "SELECT a FROM test.t WHERE _tidb_rowid = 1",
        )
        .unwrap_err();
        assert_eq!(mixed_a.code, 1143);
        assert!(
            mixed_a.message.contains("for column 'a' in table 't'"),
            "unexpected message: {}",
            mixed_a.message
        );
        let mixed_rowid_first =
            check(&handle, &is, "u_mixed", "SELECT _tidb_rowid, a FROM test.t").unwrap_err();
        assert_eq!(mixed_rowid_first.code, 1143);
        assert!(
            mixed_rowid_first
                .message
                .contains("for column 'a' in table 't'"),
            "unexpected message: {}",
            mixed_rowid_first.message
        );
        let mixed_b = check(
            &handle,
            &is,
            "u_mixed",
            "SELECT b FROM test.t WHERE _tidb_rowid = 1",
        )
        .unwrap_err();
        assert_eq!(mixed_b.code, 1143);
        assert!(
            mixed_b
                .message
                .contains("for column '_tidb_rowid' in table 't'"),
            "unexpected message: {}",
            mixed_b.message
        );

        // GROUP BY resolves directly to table columns (never skipped by a SELECT alias):
        // u_cola (SELECT(a) only) running `SELECT COUNT(*) AS b FROM test.t GROUP BY b` must
        // fail with 1143 for column 'b'.
        let gb_alias = check(
            &handle,
            &is,
            "u_cola",
            "SELECT COUNT(*) AS b FROM test.t GROUP BY b",
        )
        .unwrap_err();
        assert_eq!(gb_alias.code, 1143);
        assert!(
            gb_alias.message.contains("for column 'b' in table 't'"),
            "unexpected message: {}",
            gb_alias.message
        );
        // ORDER BY bare alias resolves to the SELECT item (no check on table column 'b').
        assert!(
            check(
                &handle,
                &is,
                "u_cola",
                "SELECT a AS b FROM test.t ORDER BY b"
            )
            .is_ok()
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
    fn fail_closed_login_rules_reject_non_native_ssl_expired_and_lockout() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(
            UserRecord::new("%", "root", SelectPriv)
                .with_auth_string("*23AE809DDACAF96AF0FD78ED04B6A265E05AA257"),
        );
        mp.add_user(UserRecord::new("10.%", "u_net", SelectPriv));
        mp.add_user(UserRecord::new("%", "u_locked", SelectPriv).with_account_locked(true));
        mp.add_user(
            UserRecord::new("%", "u_token", SelectPriv).with_auth_plugin("tidb_auth_token"),
        );
        mp.add_user(UserRecord::new("%", "u_socket", SelectPriv).with_auth_plugin("auth_socket"));
        mp.add_user(
            UserRecord::new("%", "u_ldap", SelectPriv)
                .with_auth_plugin("authentication_ldap_simple"),
        );
        mp.add_user(UserRecord::new("%", "u_expired", SelectPriv).with_password_expired(true));
        mp.add_user(
            UserRecord::new("%", "u_lifetime", SelectPriv).with_password_lifetime(Some(30)),
        );
        mp.add_user(UserRecord::new("%", "u_never_lt", SelectPriv).with_password_lifetime(Some(0)));
        mp.add_user(UserRecord::new("%", "u_lockpol", SelectPriv).with_failed_login_policy(true));
        mp.add_user(UserRecord::new("%", "u_ssl", SelectPriv));
        mp.add_global_priv(GlobalPrivRecord::new("u_ssl", "%", true, false));
        mp.add_user(UserRecord::new("%", "u_broken_gp", SelectPriv));
        mp.add_global_priv(GlobalPrivRecord::new("u_broken_gp", "%", true, true));

        // Normal native user & IPv4-mapped IPv6 host match succeed when default_password_lifetime == 0.
        let root_auth = authenticate_user_record(&mp, "root", "127.0.0.1").unwrap();
        assert!(!root_auth.account_locked);
        assert_eq!(root_auth.auth_user.as_deref(), Some("root"));
        assert_eq!(root_auth.auth_host.as_deref(), Some("%"));
        assert!(authenticate_user_record(&mp, "u_net", "::ffff:10.0.0.5").is_some());
        assert!(authenticate_user_record(&mp, "u_never_lt", "127.0.0.1").is_some());

        // Locked account returns Some(AccountAuth { account_locked: true }) so password check runs
        // before returning 3118.
        let locked = authenticate_user_record(&mp, "u_locked", "127.0.0.1").unwrap();
        assert!(locked.account_locked);

        // Every unported auth plugin / login restriction fails closed (None -> 1045).
        for denied in [
            "u_token",
            "u_socket",
            "u_ldap",
            "u_expired",
            "u_lifetime",
            "u_lockpol",
            "u_ssl",
            "u_broken_gp",
        ] {
            assert!(
                authenticate_user_record(&mp, denied, "127.0.0.1").is_none(),
                "{denied} must fail closed"
            );
        }

        // When global `default_password_lifetime > 0`, accounts with `password_lifetime IS NULL`
        // (such as `root` / `u_net`) fail closed, while `PASSWORD EXPIRE NEVER` (`Some(0)`) is allowed.
        mp.set_default_password_lifetime(90);
        assert!(authenticate_user_record(&mp, "root", "127.0.0.1").is_none());
        assert!(authenticate_user_record(&mp, "u_net", "10.0.0.5").is_none());
        assert!(authenticate_user_record(&mp, "u_never_lt", "127.0.0.1").is_some());
    }

    #[test]
    fn poc_plan_error_order_matches_go_preprocess_build_then_privilege() {
        let is = test_schema();
        let handle = test_handle();
        let plan_with_priv = |user: &str, sql: &str| -> Result<tidb_sql::Plan, SqlError> {
            let lowered = tidb_sql::parse_and_lower(sql).unwrap();
            let Statement::Select(sel) = &lowered.stmt else {
                panic!("expected SELECT");
            };
            let pm = UserPrivileges::new(user, "%", Arc::clone(&handle));
            // 1. Preprocess (R31 missing-table check)
            check_poc_missing_table_privilege(&pm, &[], user, "%", Some("test"), sel, &is)?;
            // 2. Plan build (unknown column 1054, only_full_group_by 1235, etc.)
            let plan = tidb_sql::plan_select(
                sel,
                &is,
                tidb_sql::PlanContext {
                    current_db: Some("test"),
                    params: &[],
                    describe_only: false,
                },
            )?;
            // 3. Post-build privilege check (R29/R30/R32)
            check_poc_select_privilege(&pm, &[], user, "%", Some("test"), sel, &is)?;
            Ok(plan)
        };

        // Plan-build error (only_full_group_by / unknown column) precedes privilege errors,
        // matching Go's `build_logical_plan` before `check_privilege`.
        for u in ["u_none", "u_ins", "u_cola", "u_mixed"] {
            let err = plan_with_priv(u, "SELECT a FROM test.t GROUP BY b").unwrap_err();
            assert_eq!(
                err.code, 1235,
                "{u} only_full_group_by must fail at plan-build before privilege check: {err}"
            );
            let err = plan_with_priv(u, "SELECT nosuch FROM test.t").unwrap_err();
            assert_eq!(
                err.code, 1054,
                "{u} unknown column must fail at plan-build before privilege check: {err}"
            );
        }

        // R31 missing-table check still runs in preprocess (1142 instead of 1146).
        for u in ["u_none", "u_ins", "u_db_ins"] {
            let err = plan_with_priv(u, "SELECT 1 FROM test.nosuch").unwrap_err();
            assert_eq!(err.code, 1142, "{u} missing table must return 1142: {err}");
        }
        assert_eq!(
            plan_with_priv("u_db_sel", "SELECT 1 FROM test.nosuch")
                .unwrap_err()
                .code,
            1146
        );

        // Valid plans still enforce R29/R30/column checks.
        assert_eq!(
            plan_with_priv("u_none", "SELECT a FROM test.t")
                .unwrap_err()
                .code,
            1142
        );
        assert_eq!(
            plan_with_priv("u_cola", "SELECT COUNT(*) AS b FROM test.t GROUP BY b")
                .unwrap_err()
                .code,
            1143
        );
        assert!(plan_with_priv("u_cola", "SELECT a FROM test.t").is_ok());
    }
}
